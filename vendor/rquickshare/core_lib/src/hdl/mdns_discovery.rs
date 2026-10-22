use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::peer_cache::{PeerCache, LAN_PEER_TTL};
use crate::qr::{parse_endpoint_metadata, QrInvitation, QrPeer, MAX_ENDPOINT_INFO};
use crate::utils::is_not_self_ip;
use crate::DeviceType;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EndpointInfo {
    pub fullname: String,
    pub id: String,
    pub name: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    pub rtype: Option<DeviceType>,
    pub present: Option<bool>,
    #[serde(default)]
    pub transport: Option<String>,
}

pub struct MDnsDiscovery {
    daemon: ServiceDaemon,
    sender: broadcast::Sender<EndpointInfo>,
    invitation: watch::Receiver<Option<Arc<QrInvitation>>>,
    qr_matches: mpsc::Sender<QrPeer>,
}

impl MDnsDiscovery {
    pub fn new(
        sender: broadcast::Sender<EndpointInfo>,
        invitation: watch::Receiver<Option<Arc<QrInvitation>>>,
        qr_matches: mpsc::Sender<QrPeer>,
    ) -> Result<Self, anyhow::Error> {
        let daemon = ServiceDaemon::new()?;

        Ok(Self { daemon, sender, invitation, qr_matches })
    }

    pub async fn run(mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("MDnsDiscovery: service starting");

        let service_type = "_FC9F5ED42C8A._tcp.local.";
        let mut cache = PeerCache::default();
        let mut receiver = None;
        let result = async {
            receiver = Some(self.daemon.browse(service_type)?);
            let mut expiry = tokio::time::interval(Duration::from_secs(1));
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Unchanged answers renew DNS TTL without a ServiceResolved event.
            // Re-browse before the application lease expires to obtain fresh
            // observations. Intrinsic dependency budgets bound raw DNS/state
            // allocations throughout each period; this refresh is not that bound.
            let refresh_period = LAN_PEER_TTL / 2;
            let mut refresh = tokio::time::interval_at(
                tokio::time::Instant::now() + refresh_period,
                refresh_period,
            );
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ctk.cancelled() => {
                        info!("MDnsDiscovery: tracker cancelled, breaking");
                        break;
                    }
                    _ = expiry.tick() => {
                        cache.expire(Instant::now(), |peer| { let _ = self.sender.send(peer); });
                    }
                    _ = refresh.tick() => {
                        self.daemon.stop_browse(service_type)?;
                        let stopped = self.daemon.shutdown()?;
                        let mut events_open = true;
                        // Drain and discard this generation until shutdown is
                        // acknowledged. Its late removals cannot reach a new
                        // generation, and its bounded channel cannot block exit.
                        tokio::select! {
                            _ = ctk.cancelled() => break,
                            result = tokio::time::timeout(Duration::from_secs(3), async {
                                loop {
                                    tokio::select! {
                                        result = stopped.recv_async() => break result,
                                        event = receiver.as_ref().expect("active browse").recv_async(), if events_open => {
                                            events_open = event.is_ok();
                                        }
                                        _ = expiry.tick() => {
                                            cache.expire(Instant::now(), |peer| { let _ = self.sender.send(peer); });
                                        }
                                    }
                                }
                            }) => { result??; }
                        }
                        self.daemon = ServiceDaemon::new()?;
                        receiver = Some(self.daemon.browse(service_type)?);
                    }
                    event = receiver.as_ref().expect("active browse").recv_async() => {
                        match event {
                            Ok(ServiceEvent::ServiceResolved(info)) => {
                                let fullname = info.get_fullname();
                                let invitation = self.invitation.borrow().clone();
                                let endpoint = resolved_endpoint(&info, invitation.as_ref(), Instant::now());
                                // Retire the old route before probing a changed
                                // address, including invalid/no-IPv4 metadata.
                                cache.replace_address(
                                    "lan", fullname, endpoint.as_ref().filter(|peer| peer.public).map(|peer| peer.peer.id.as_str()),
                                    |peer| { let _ = self.sender.send(peer); },
                                );
                                let Some(endpoint) = endpoint else { continue; };
                                let address = endpoint.peer.id.parse::<std::net::SocketAddr>()
                                    .expect("resolved IPv4 endpoint");
                                let probe = tokio::time::timeout(
                                    Duration::from_secs(3), TcpStream::connect(address),
                                );
                                tokio::pin!(probe);
                                loop {
                                    tokio::select! {
                                        _ = ctk.cancelled() => break,
                                        _ = expiry.tick() => {
                                            cache.expire(Instant::now(), |peer| { let _ = self.sender.send(peer); });
                                        }
                                        result = &mut probe => {
                                            if matches!(result, Ok(Ok(_))) {
                                                if let Some(matched) = endpoint.qr_match(self.invitation.borrow().as_ref(), Instant::now()) {
                                                    let _ = self.qr_matches.try_send(matched);
                                                }
                                                if endpoint.public {
                                                    cache.apply(endpoint.peer, Instant::now(), |peer| { let _ = self.sender.send(peer); });
                                                }
                                            } else {
                                                cache.replace_address("lan", fullname, None, |peer| { let _ = self.sender.send(peer); });
                                            }
                                            break;
                                        }
                                    }
                                }
                                if ctk.is_cancelled() { break; }
                            }
                            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                                cache.replace_address("lan", &fullname, None, |peer| { let _ = self.sender.send(peer); });
                            }
                            Ok(_) => {}
                            Err(error) => {
                                error!("MDnsDiscovery: receiver closed: {error}");
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        }.await;

        let _ = self.daemon.stop_browse(service_type);
        if let Ok(stopped) = self.daemon.shutdown() {
            // Cleanup also drains without applying old-generation events.
            let mut events_open = receiver.is_some();
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    tokio::select! {
                        _ = stopped.recv_async() => break,
                        event = async {
                            receiver.as_ref().expect("active browse").recv_async().await
                        }, if events_open => {
                            events_open = event.is_ok();
                        }
                    }
                }
            }).await;
        }
        cache.clear(|peer| { let _ = self.sender.send(peer); });
        result
    }
}

struct ResolvedEndpoint {
    peer: EndpointInfo,
    endpoint_id: String,
    public: bool,
    invitation: Option<Arc<QrInvitation>>,
}

impl ResolvedEndpoint {
    fn qr_match(&self, active: Option<&Arc<QrInvitation>>, now: Instant) -> Option<QrPeer> {
        let invitation = self.invitation.as_ref()?;
        let active = active?;
        // Probe completion must not revive a cleared, replaced or expired offer.
        if active.id() != invitation.id() || active.is_expired_at(now) { return None; }
        Some(QrPeer {
            invitation_id: invitation.id().to_owned(),
            peer: self.peer.clone(),
            endpoint_id: self.endpoint_id.clone(),
        })
    }
}

fn service_endpoint(fullname: &str) -> Option<String> {
    if fullname.is_empty() || fullname.len() > 254 { return None; }
    let (instance, suffix) = fullname.split_once('.')?;
    if instance.len() > 63 || !suffix.eq_ignore_ascii_case("_FC9F5ED42C8A._tcp.local.") {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(instance).ok()?;
    if bytes.len() != 10 || bytes[0] & 0x1f != 3
        || bytes[5..8] != [0xfc, 0x9f, 0x5e] || bytes[8..] != [0, 0]
        || !bytes[1..5].iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    std::str::from_utf8(&bytes[1..5]).ok().map(str::to_owned)
}

fn resolved_endpoint(
    info: &mdns_sd::ServiceInfo,
    invitation: Option<&Arc<QrInvitation>>,
    now: Instant,
) -> Option<ResolvedEndpoint> {
    let fullname = info.get_fullname();
    let endpoint_id = service_endpoint(fullname)?;
    let addresses = info.get_addresses_v4();
    let ip = addresses.iter().filter(|ip| {
        !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()
            && !ip.is_broadcast() && is_not_self_ip(ip)
    }).min()?;
    let encoded = info.get_property("n")?.val_str();
    // Bound allocation before decoding untrusted TXT data.
    if encoded.len() > MAX_ENDPOINT_INFO.div_ceil(3) * 4 { return None; }
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let metadata = parse_endpoint_metadata(&bytes).ok()?;
    let matched_name = invitation.and_then(|qr| qr.matched_name(&metadata, now));
    let matched = matched_name.is_some();
    let public = metadata.name.is_some();
    let name = matched_name.or_else(|| metadata.name.map(str::to_owned))?;
    let port = info.get_port();
    if port == 0 { return None; }
    Some(ResolvedEndpoint {
        peer: EndpointInfo {
            fullname: fullname.to_owned(),
            id: format!("{ip}:{port}"),
            name: Some(name),
            ip: Some(ip.to_string()),
            port: Some(port.to_string()),
            rtype: Some(metadata.device_type),
            present: Some(true),
            transport: Some("lan".into()),
        },
        endpoint_id,
        public,
        invitation: invitation.filter(|_| matched).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::{aead::{Aead, KeyInit, Payload}, Aes128Gcm, Nonce};
    use crate::utils::{gen_mdns_endpoint_info, gen_mdns_name, hkdf_extract_expand};
    use crate::QR_INVITATION_TTL;

    fn service(instance: &str, ip: &str, port: u16, metadata: &str) -> mdns_sd::ServiceInfo {
        mdns_sd::ServiceInfo::new(
            "_FC9F5ED42C8A._tcp.local.", instance, "host.local.", ip, port,
            &[("n", metadata)][..],
        ).unwrap()
    }

    fn qr_metadata(qr: &QrInvitation, hidden: bool) -> String {
        let wire = URL_SAFE_NO_PAD.decode(qr.url().split_once("#key=").unwrap().1).unwrap();
        let token = hkdf_extract_expand(&[], &wire, b"advertisingContext", 16).unwrap();
        let key = hkdf_extract_expand(&[], &wire, b"encryptionKey", 16).unwrap();
        let mut data = vec![if hidden { 0x12 } else { 2 }; 17];
        let value = if hidden {
            let nonce = [7u8; 12];
            let cipher = Aes128Gcm::new_from_slice(&key).unwrap();
            let mut value = nonce.to_vec();
            value.extend_from_slice(&cipher.encrypt(&Nonce::from(nonce), Payload {
                msg: "Pixel ñ".as_bytes(), aad: &token,
            }).unwrap());
            value
        } else {
            data.push("Pixel ñ".len() as u8);
            data.extend_from_slice("Pixel ñ".as_bytes());
            token
        };
        data.extend_from_slice(&[1, value.len() as u8]);
        data.extend_from_slice(&value);
        URL_SAFE_NO_PAD.encode(data)
    }

    #[test]
    fn resolution_selects_a_stable_ipv4() {
        let metadata = gen_mdns_endpoint_info(1, &"p".repeat(256));
        let info = service(&gen_mdns_name(*b"TEST"), "192.0.2.2,192.0.2.1", 1234, &metadata);
        let peer = resolved_endpoint(&info, None, Instant::now()).unwrap();
        assert_eq!(peer.peer.id, "192.0.2.1:1234");
        assert!(peer.public);
        assert!(peer.qr_match(None, Instant::now()).is_none());
    }

    #[test]
    fn invalid_metadata_and_ipv6_only_updates_withdraw_previous_route() {
        let metadata = gen_mdns_endpoint_info(1, "Phone");
        let instance = gen_mdns_name(*b"TEST");
        let original = service(&instance, "192.0.2.1", 1234, &metadata);
        for updated in [
            service(&instance, "192.0.2.2", 5678, "invalid"),
            service(&instance, "2001:db8::1", 5678, &metadata),
        ] {
            let mut cache = PeerCache::default();
            cache.apply(resolved_endpoint(&original, None, Instant::now()).unwrap().peer, Instant::now(), |_| {});
            let replacement = resolved_endpoint(&updated, None, Instant::now());
            assert!(replacement.is_none());
            let mut events = Vec::new();
            cache.replace_address(
                "lan", updated.get_fullname(),
                replacement.as_ref().map(|peer| peer.peer.id.as_str()),
                |peer| events.push(peer),
            );
            assert_eq!(cache.peers().count(), 0);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, "192.0.2.1:1234");
            assert_eq!(events[0].fullname, original.get_fullname());
            assert_eq!(events[0].present, Some(false));
        }
    }

    #[test]
    fn invalid_service_identity_address_port_and_oversized_metadata_are_rejected() {
        let metadata = gen_mdns_endpoint_info(1, "Phone");
        let instance = gen_mdns_name(*b"TEST");
        for info in [
            service(&"p".repeat(255), "192.0.2.1", 1234, &metadata),
            service("not-an-endpoint", "192.0.2.1", 1234, &metadata),
            service(&gen_mdns_name(*b"BAD!"), "192.0.2.1", 1234, &metadata),
            service(&instance, "127.0.0.1", 1234, &metadata),
            service(&instance, "0.0.0.0", 1234, &metadata),
            service(&instance, "224.0.0.1", 1234, &metadata),
            service(&instance, "192.0.2.1", 0, &metadata),
            service(&instance, "192.0.2.1", 1234, &"A".repeat(2048)),
        ] {
            assert!(resolved_endpoint(&info, None, Instant::now()).is_none());
        }
        let fullname = format!("{instance}._OTHER._tcp.local.");
        assert!(service_endpoint(&fullname).is_none());
    }

    #[test]
    fn visible_and_hidden_qr_peers_keep_endpoint_identity_and_hidden_records_private() {
        let now = Instant::now();
        let qr = Arc::new(QrInvitation::new().unwrap());
        let other = Arc::new(QrInvitation::new().unwrap());
        for hidden in [false, true] {
            let metadata = qr_metadata(&qr, hidden);
            let info = service(&gen_mdns_name(*b"PEER"), "192.0.2.1", 1234, &metadata);
            let resolved = resolved_endpoint(&info, Some(&qr), now).unwrap();
            assert_eq!(resolved.public, !hidden);
            assert_eq!(resolved.peer.name.as_deref(), Some("Pixel ñ"));
            let matched = resolved.qr_match(Some(&qr), now).unwrap();
            assert_eq!(matched.invitation_id, qr.id());
            assert_eq!(matched.endpoint_id, "PEER");
            assert_eq!(matched.peer.id, "192.0.2.1:1234");
            assert!(resolved.qr_match(None, now).is_none());
            assert!(resolved.qr_match(Some(&other), now).is_none());
            assert!(resolved.qr_match(Some(&qr), qr.expires_at()).is_none());
            assert!(resolved.qr_match(Some(&qr), now + QR_INVITATION_TTL + Duration::from_secs(1)).is_none());
            let without_invite = resolved_endpoint(&info, None, now);
            let wrong_invite = resolved_endpoint(&info, Some(&other), now);
            if hidden {
                assert!(without_invite.is_none());
                assert!(wrong_invite.is_none());
            } else {
                assert!(without_invite.unwrap().public);
                assert!(wrong_invite.unwrap().qr_match(Some(&other), now).is_none());
            }
        }
    }
}
