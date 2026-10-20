#[macro_use]
extern crate log;
use anyhow::{anyhow, ensure, Result};
use base64::Engine;
use channel::{ChannelDirection, ChannelMessage};
use hdl::{MDnsDiscovery, MDnsServer};
use manager::{Manager, Route, Routes};
use parking_lot::RwLock;
use rand::{distr::Alphanumeric, Rng};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
#[cfg(target_os = "linux")]
mod bluetooth;
pub mod channel;
mod crypto;
mod hdl;
mod lan_upgrade;
mod manager;
mod network;
mod outbound;
mod peer_cache;
pub mod qr;
mod session;
mod storage;
pub(crate) mod transport;
mod utils;
pub use hdl::{EndpointInfo, OutboundPayload, State, Visibility};
pub use outbound::OutboundFile;
pub use storage::MAX_TOTAL_BYTES;
pub use manager::SendInfo;
pub use qr::{QrInvitation, QrPeer, QrSendContext, QR_INVITATION_TTL};
pub use utils::DeviceType;
pub use peer_cache::{DISCOVERY_PEER_LIMIT, LAN_PEER_TTL};
pub mod sharing_nearby {
    include!(concat!(env!("OUT_DIR"), "/sharing.nearby.rs"));
}
pub mod securemessage {
    include!(concat!(env!("OUT_DIR"), "/securemessage.rs"));
}
pub mod securegcm {
    include!(concat!(env!("OUT_DIR"), "/securegcm.rs"));
}
pub mod location_nearby_connections {
    include!(concat!(env!("OUT_DIR"), "/location.nearby.connections.rs"));
}

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub bluetooth: bool,
    pub lan: bool,
    pub hotspot: bool,
}
impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            bluetooth: true,
            lan: true,
            hotspot: true,
        }
    }
}
#[derive(Debug)]
pub struct RQS {
    tracker: Option<TaskTracker>,
    ctoken: Option<CancellationToken>,
    discovery_ctk: Option<CancellationToken>,
    visibility_sender: watch::Sender<Visibility>,
    visibility_receiver: watch::Receiver<Visibility>,
    port_number: Option<u32>,
    pub message_sender: broadcast::Sender<ChannelMessage>,
    options: EngineOptions,
    consent: Arc<AtomicBool>,
    download: Arc<RwLock<PathBuf>>,
    routes: Routes,
    qr_invitation: watch::Sender<Option<Arc<QrInvitation>>>,
    qr_matches: mpsc::Sender<QrPeer>,
    qr_receiver: Option<mpsc::Receiver<QrPeer>>,
    #[cfg(target_os = "linux")]
    bluetooth: Option<Arc<bluetooth::Bluetooth>>,
}
impl Default for RQS {
    fn default() -> Self {
        Self::new(Visibility::Invisible, None, None)
    }
}
impl RQS {
    pub fn new(
        visibility: Visibility,
        port_number: Option<u32>,
        download_path: Option<PathBuf>,
    ) -> Self {
        Self::with_options(
            visibility,
            port_number,
            download_path,
            EngineOptions::default(),
        )
    }
    pub fn with_options(
        visibility: Visibility,
        port_number: Option<u32>,
        download_path: Option<PathBuf>,
        options: EngineOptions,
    ) -> Self {
        // Progress is capped at 10Hz per transfer; this bounded queue retains
        // consent/terminal events during normal UI scheduling stalls.
        let (message_sender, _) = broadcast::channel(4096);
        let (visibility_sender, visibility_receiver) = watch::channel(visibility);
        let (qr_invitation, _) = watch::channel(None);
        let (qr_matches, qr_receiver) = mpsc::channel(16);
        Self {
            tracker: None,
            ctoken: None,
            discovery_ctk: None,
            visibility_sender,
            visibility_receiver,
            port_number,
            message_sender,
            options,
            consent: Arc::new(AtomicBool::new(false)),
            download: Arc::new(RwLock::new(
                download_path.unwrap_or_else(utils::get_download_dir),
            )),
            routes: Arc::new(RwLock::new(HashMap::new())),
            qr_invitation,
            qr_matches,
            qr_receiver: Some(qr_receiver),
            #[cfg(target_os = "linux")]
            bluetooth: None,
        }
    }
    pub fn set_network_consent(&self, allowed: bool) {
        self.consent.store(allowed, Ordering::Release);
    }
    pub fn set_qr_invitation(&self, invitation: Option<Arc<QrInvitation>>) -> Result<()> {
        if let Some(invitation) = &invitation {
            ensure!(
                self.options.lan
                    && self.ctoken.as_ref().is_some_and(|token| !token.is_cancelled()),
                "QR sending requires a running LAN engine"
            );
            ensure!(
                !invitation.is_expired_at(std::time::Instant::now()),
                "QR invitation has expired"
            );
        }
        self.qr_invitation.send_replace(invitation);
        Ok(())
    }
    pub fn take_qr_matches(&mut self) -> Result<mpsc::Receiver<QrPeer>> {
        self.qr_receiver.take().ok_or_else(|| anyhow!("QR match receiver already taken"))
    }
    fn background_error(events: &broadcast::Sender<ChannelMessage>, error: String) {
        let _ = events.send(ChannelMessage {
            direction: ChannelDirection::LibToFront,
            error: Some(error),
            ..Default::default()
        });
    }
    pub async fn run(&mut self) -> Result<mpsc::Sender<SendInfo>> {
        ensure!(self.tracker.is_none(), "engine is already running");
        ensure!(
            self.options.lan || self.options.bluetooth,
            "no session transport is enabled"
        );
        if let Err(error) = network::NetworkManager::recover().await {
            Self::background_error(
                &self.message_sender,
                format!("temporary network recovery: {error:#}"),
            );
        }
        let endpoint: [u8; 4] = {
            let mut rng = rand::rng();
            std::array::from_fn(|_| rng.sample(Alphanumeric))
        };
        let name = utils::get_hostname()?;
        let listener = if self.options.lan {
            Some(
                TcpListener::bind(("0.0.0.0", u16::try_from(self.port_number.unwrap_or(0))?))
                    .await?,
            )
        } else {
            None
        };
        let mdns = match &listener {
            Some(listener) => Some(MDnsServer::new(
                endpoint,
                listener.local_addr()?.port(),
                self.visibility_receiver.clone(),
                &name,
            )?),
            None => None,
        };
        let tracker = TaskTracker::new();
        let token = CancellationToken::new();
        let (incoming_sender, incoming_receiver) = mpsc::channel(16);
        #[cfg(target_os = "linux")]
        if self.options.bluetooth {
            match bluetooth::Bluetooth::new(
                endpoint,
                name.clone(),
                self.visibility_receiver.clone(),
                self.message_sender.clone(),
            )
            .await
            {
                Ok(bluetooth) => {
                    let bluetooth = Arc::new(bluetooth);
                    self.bluetooth = Some(bluetooth.clone());
                    let child = token.clone();
                    let events = self.message_sender.clone();
                    tracker.spawn(async move {
                        if let Err(error) = bluetooth.run(incoming_sender, child).await {
                            Self::background_error(
                                &events,
                                format!("Bluetooth transport: {error:#}"),
                            );
                        }
                    });
                }
                Err(error) => {
                    Self::background_error(
                        &self.message_sender,
                        format!("Bluetooth initialization: {error:#}"),
                    );
                    if !self.options.lan {
                        return Err(error);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        if self.options.bluetooth {
            Self::background_error(
                &self.message_sender,
                "Bluetooth backend requires Linux BlueZ".into(),
            );
            if !self.options.lan {
                return Err(anyhow!("Bluetooth unavailable"));
            }
        }
        if let Some(mut mdns) = mdns {
            let child = token.clone();
            let events = self.message_sender.clone();
            tracker.spawn(async move {
                if let Err(error) = mdns.run(child).await {
                    Self::background_error(&events, format!("LAN advertisement: {error:#}"));
                }
            });
        }
        let (send_sender, send_receiver) = mpsc::channel(16);
        let lan_upgrade_host = match &listener {
            Some(listener) => Some(lan_upgrade::LanUpgradeHost::new(
                listener.local_addr()?.port(),
            )),
            None => None,
        };
        let manager = Manager {
            endpoint,
            name,
            events: self.message_sender.clone(),
            visibility: self.visibility_receiver.clone(),
            download: self.download.clone(),
            consent: self.consent.clone(),
            lan: self.options.lan,
            hotspot: self.options.hotspot,
            routes: self.routes.clone(),
            lan_upgrade_host,
            #[cfg(target_os = "linux")]
            bluetooth: self.bluetooth.clone(),
        };
        let child = token.clone();
        let events = self.message_sender.clone();
        tracker.spawn(async move {
            if let Err(error) = manager
                .run(listener, incoming_receiver, send_receiver, child)
                .await
            {
                Self::background_error(&events, format!("session manager: {error:#}"));
            }
        });
        self.tracker = Some(tracker);
        self.ctoken = Some(token);
        Ok(send_sender)
    }
    pub fn discovery(&mut self, sender: broadcast::Sender<EndpointInfo>) -> Result<()> {
        self.stop_discovery();
        let tracker = self
            .tracker
            .as_ref()
            .ok_or_else(|| anyhow!("engine must start before discovery"))?;
        let token = CancellationToken::new();
        let (proxy, _) = broadcast::channel(256);
        let mut receive = proxy.subscribe();
        let mdns = if self.options.lan {
            Some(MDnsDiscovery::new(
                proxy.clone(), self.qr_invitation.subscribe(), self.qr_matches.clone(),
            )?)
        } else {
            None
        };
        self.discovery_ctk = Some(token.clone());
        if let Some(mdns) = mdns {
            let child = token.clone();
            let events = self.message_sender.clone();
            tracker.spawn(async move {
                if let Err(error) = mdns.run(child).await {
                    Self::background_error(&events, format!("LAN discovery: {error:#}"));
                }
            });
        }
        #[cfg(target_os = "linux")]
        if let Some(bluetooth) = &self.bluetooth {
            let bluetooth = bluetooth.clone();
            let child = token.clone();
            let events = self.message_sender.clone();
            tracker.spawn(async move {
                if let Err(error) = bluetooth.discover(proxy, child).await {
                    Self::background_error(&events, format!("Bluetooth discovery: {error:#}"));
                }
            });
        }
        let child = token.clone();
        let routes = self.routes.clone();
        let events = self.message_sender.clone();
        tracker.spawn(async move {
            let mut cache = peer_cache::PeerCache::default();
            let mut endpoints: HashMap<String, (String, String)> = HashMap::new();
            let mut expiry = tokio::time::interval(std::time::Duration::from_secs(1));
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = child.cancelled() => break,
                    _ = expiry.tick() => {
                        let mut map = routes.write();
                        let mut changed = false;
                        cache.expire(std::time::Instant::now(), |peer| {
                            changed |= update_discovery_index(&mut endpoints, &peer);
                            let _ = sender.send(peer);
                        });
                        if changed {
                            rebuild_discovery_routes(&cache, &endpoints, &mut map);
                        }
                    }
                    message = receive.recv() => {
                        let mut map = routes.write();
                        let mut changed = false;
                        match message {
                            Ok(peer) => cache.apply(peer, std::time::Instant::now(), |peer| {
                                changed |= update_discovery_index(&mut endpoints, &peer);
                                let _ = sender.send(peer);
                            }),
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                cache.clear(|peer| {
                                    changed |= update_discovery_index(&mut endpoints, &peer);
                                    let _ = sender.send(peer);
                                });
                                Self::background_error(&events, "discovery channel lagged; cached peers withdrawn".into());
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                cache.clear(|peer| {
                                    changed |= update_discovery_index(&mut endpoints, &peer);
                                    let _ = sender.send(peer);
                                });
                                map.clear();
                                break;
                            }
                        }
                        if changed {
                            rebuild_discovery_routes(&cache, &endpoints, &mut map);
                        }
                    }
                }
            }
        });
        Ok(())
    }
    pub fn stop_discovery(&mut self) {
        if let Some(token) = self.discovery_ctk.take() {
            token.cancel();
        }
        self.routes.write().clear();
    }
    pub fn change_visibility(&mut self, visibility: Visibility) {
        self.visibility_sender.send_replace(visibility);
    }
    pub async fn stop(&mut self) {
        self.stop_discovery();
        self.qr_invitation.send_replace(None);
        if let Some(token) = self.ctoken.take() {
            token.cancel();
        }
        if let Some(tracker) = self.tracker.take() {
            tracker.close();
            tracker.wait().await;
        }
        #[cfg(target_os = "linux")]
        {
            self.bluetooth = None;
        }
    }
    pub fn set_download_path(&self, path: Option<PathBuf>) {
        *self.download.write() = path.unwrap_or_else(utils::get_download_dir);
    }
}
fn update_discovery_index(
    endpoints: &mut HashMap<String, (String, String)>,
    peer: &EndpointInfo,
) -> bool {
    if peer.present != Some(true) {
        return endpoints.remove(&peer.id).is_some();
    }
    let Some(endpoint) = discovery_endpoint(peer) else {
        return endpoints.remove(&peer.id).is_some();
    };
    let transport = peer.transport.clone().unwrap_or_else(|| {
        if peer.id.starts_with("bt:") { "bluetooth" } else { "lan" }.into()
    });
    if endpoints
        .get(&peer.id)
        .is_some_and(|previous| previous.0 == endpoint && previous.1 == transport)
    {
        return false;
    }
    endpoints.insert(peer.id.clone(), (endpoint, transport));
    true
}

fn rebuild_discovery_routes(
    cache: &peer_cache::PeerCache,
    endpoints: &HashMap<String, (String, String)>,
    routes: &mut HashMap<String, Route>,
) {
    routes.clear();
    for peer in cache.peers() {
        let address = &peer.id;
        let Some((endpoint, transport)) = endpoints.get(address) else { continue; };
        let preferred = if transport == "bluetooth" {
            endpoints
                .iter()
                .find(|(_, peer)| peer.0 == *endpoint && peer.1 == "lan")
                .map(|(address, _)| address.clone())
        } else {
            None
        };
        let fallback = if transport == "lan" {
            endpoints
                .iter()
                .find(|(_, peer)| peer.0 == *endpoint && peer.1 == "bluetooth")
                .map(|(address, _)| address.clone())
        } else {
            None
        };
        routes.insert(address.clone(), Route { endpoint: endpoint.clone(), preferred, fallback });
    }
}

fn discovery_endpoint(peer: &EndpointInfo) -> Option<String> {
    if let Some(endpoint) = peer.fullname.strip_prefix("endpoint:") {
        return (endpoint.len() == 4).then(|| endpoint.to_owned());
    }
    if peer.transport.as_deref() != Some("lan") {
        return None;
    }
    let record = peer.fullname.split('.').next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(record)
        .ok()?;
    if bytes.len() < 8 || bytes[0] & 0x1f != 3 || bytes[5..8] != [0xfc, 0x9f, 0x5e] {
        return None;
    }
    String::from_utf8(bytes[1..5].to_vec()).ok()
}

#[cfg(test)]
mod discovery_route_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn qr_context_requires_running_lan_but_cleanup_and_receiver_take_are_independent() {
        let mut engine = RQS::default();
        let invitation = Arc::new(QrInvitation::new().unwrap());
        engine.set_qr_invitation(None).unwrap();
        assert!(engine.set_qr_invitation(Some(invitation.clone())).is_err());
        let _receive = engine.take_qr_matches().unwrap();
        assert!(engine.take_qr_matches().is_err());
        engine.ctoken = Some(CancellationToken::new());
        engine.set_qr_invitation(Some(invitation.clone())).unwrap();
        assert_eq!(engine.qr_invitation.borrow().as_ref().unwrap().id(), invitation.id());
        engine.set_qr_invitation(None).unwrap();
        assert!(engine.qr_invitation.borrow().is_none());
        engine.options.lan = false;
        assert!(engine.set_qr_invitation(Some(invitation.clone())).is_err());
        engine.options.lan = true;
        engine.ctoken.as_ref().unwrap().cancel();
        assert!(engine.set_qr_invitation(Some(invitation)).is_err());
        engine.set_qr_invitation(None).unwrap();
    }

    fn lan(endpoint: &str, address: &str) -> EndpointInfo {
        let mut record = [3, 0, 0, 0, 0, 0xfc, 0x9f, 0x5e];
        record[1..5].copy_from_slice(endpoint.as_bytes());
        EndpointInfo {
            fullname: format!("{}._FC9F5ED42C8A._tcp.local.", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(record)),
            id: address.into(),
            present: Some(true),
            transport: Some("lan".into()),
            ..Default::default()
        }
    }

    #[test]
    fn lan_replacement_and_expiry_remove_stale_preference_and_fallback_routes() {
        let at = Instant::now();
        let mut cache = peer_cache::PeerCache::default();
        let mut index = HashMap::new();
        let mut routes = HashMap::new();
        let bluetooth = EndpointInfo {
            fullname: "endpoint:PEER".into(), id: "bt:phone".into(),
            present: Some(true), transport: Some("bluetooth".into()),
            ..Default::default()
        };
        for peer in [bluetooth, lan("PEER", "192.0.2.1:1234"), lan("PEER", "192.0.2.2:5678")] {
            cache.apply(peer, at, |peer| { update_discovery_index(&mut index, &peer); });
            rebuild_discovery_routes(&cache, &index, &mut routes);
        }
        assert!(!routes.contains_key("192.0.2.1:1234"));
        assert_eq!(routes["bt:phone"].preferred.as_deref(), Some("192.0.2.2:5678"));
        assert_eq!(routes["192.0.2.2:5678"].fallback.as_deref(), Some("bt:phone"));
        cache.expire(at + LAN_PEER_TTL, |peer| { update_discovery_index(&mut index, &peer); });
        rebuild_discovery_routes(&cache, &index, &mut routes);
        assert_eq!(routes.len(), 1);
        assert!(routes["bt:phone"].preferred.is_none());
        assert!(!index.contains_key("192.0.2.2:5678"));
    }

    #[test]
    fn source_budget_and_lag_reset_bound_all_routing_indices() {
        let at = Instant::now();
        let mut cache = peer_cache::PeerCache::default();
        let mut index = HashMap::new();
        let mut routes = HashMap::new();
        for number in 0..(DISCOVERY_PEER_LIMIT * 3) {
            cache.apply(lan(&format!("{number:04x}"), &format!("192.0.2.1:{}", 10000 + number)), at,
                |peer| { update_discovery_index(&mut index, &peer); });
            rebuild_discovery_routes(&cache, &index, &mut routes);
            assert!(index.len() <= DISCOVERY_PEER_LIMIT && routes.len() <= DISCOVERY_PEER_LIMIT);
        }
        assert_eq!(routes.len(), DISCOVERY_PEER_LIMIT);
        cache.clear(|peer| { update_discovery_index(&mut index, &peer); });
        rebuild_discovery_routes(&cache, &index, &mut routes);
        assert!(index.is_empty() && routes.is_empty());
    }
}

