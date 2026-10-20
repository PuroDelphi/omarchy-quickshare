use crate::{
    channel::{ChannelAction, ChannelDirection, ChannelMessage, TransferType},
    hdl::{OutboundPayload, State, Visibility},
    lan_upgrade::{LanCandidate, LanUpgradeHost},
    location_nearby_connections as n,
    qr::{QrInvitation, QrSendContext},
    session::Session,
    transport::{FrameReader, TransportStream},
};
use anyhow::{anyhow, ensure, Result};
use parking_lot::RwLock;
use prost::Message;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Debug)]
pub struct SendInfo {
    pub id: String,
    pub addr: String,
    pub ob: OutboundPayload,
    pub cancellation: CancellationToken,
    pub qr: Option<QrSendContext>,
}
#[derive(Debug, Clone)]
pub struct Route {
    pub endpoint: String,
    pub preferred: Option<String>,
    pub fallback: Option<String>,
}
pub type Routes = Arc<RwLock<HashMap<String, Route>>>;
#[derive(Clone)]
pub struct Manager {
    pub endpoint: [u8; 4],
    pub name: String,
    pub events: broadcast::Sender<ChannelMessage>,
    pub visibility: watch::Receiver<Visibility>,
    pub download: Arc<RwLock<PathBuf>>,
    pub consent: Arc<AtomicBool>,
    pub lan: bool,
    pub hotspot: bool,
    pub lan_upgrade_host: Option<LanUpgradeHost>,
    pub routes: Routes,
    #[cfg(target_os = "linux")]
    pub bluetooth: Option<Arc<crate::bluetooth::Bluetooth>>,
}
impl Manager {
    pub async fn run(
        self,
        listener: Option<TcpListener>,
        mut incoming: mpsc::Receiver<TransportStream>,
        mut sends: mpsc::Receiver<SendInfo>,
        ctk: CancellationToken,
    ) -> Result<()> {
        let sessions = TaskTracker::new();
        let slots = Arc::new(Semaphore::new(16));
        let classifiers = Arc::new(Semaphore::new(16));
        let result = loop {
            enum Work {
                Stop,
                Send(SendInfo),
                IncomingBluetooth(TransportStream),
                IncomingLan(std::io::Result<TcpStream>),
            }
            let work = tokio::select! {
                _=ctk.cancelled()=>Work::Stop,
                Some(send)=sends.recv()=>Work::Send(send),
                Some(stream)=incoming.recv()=>Work::IncomingBluetooth(stream),
                connection=async {listener.as_ref().expect("guarded listener").accept().await},if listener.is_some()=>{
                    Work::IncomingLan(connection.map(|(stream, _)| stream))
                }
            };
            match work {
                Work::Stop => break Ok(()),
                Work::IncomingLan(Err(error)) => break Err(error.into()),
                Work::IncomingLan(Ok(stream)) => {
                    let permit = match classifiers.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            log::debug!("discarding LAN connection: classifier capacity reached");
                            continue;
                        }
                    };
                    let manager = self.clone();
                    let token = ctk.child_token();
                    let slots = slots.clone();
                    sessions.spawn(async move {
                        let classified = manager.classify_lan(stream, &token).await;
                        // Classification never consumes a file-session slot,
                        // and its own slot is released before an ordinary session.
                        drop(permit);
                        let Some((stream, request)) = classified else {
                            return;
                        };
                        let permit = match slots.try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                log::debug!("discarding LAN connection: session capacity reached");
                                return;
                            }
                        };
                        if token.is_cancelled()
                            || *manager.visibility.borrow() == Visibility::Invisible
                        {
                            log::debug!("discarding LAN connection: receive visibility expired");
                            return;
                        }
                        let _permit = permit;
                        manager
                            .incoming_session(Box::new(stream), "lan", Some(request), token)
                            .await;
                    });
                }
                Work::IncomingBluetooth(stream) => {
                    let permit = match slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    if *self.visibility.borrow() == Visibility::Invisible {
                        continue;
                    }
                    let manager = self.clone();
                    let token = ctk.child_token();
                    sessions.spawn(async move {
                        let _permit = permit;
                        manager
                            .incoming_session(stream, "bluetooth", None, token)
                            .await;
                    });
                }
                Work::Send(send) => {
                    let permit = match slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            self.failure(
                                &send.id,
                                true,
                                None,
                                "maximum active sessions reached".into(),
                            );
                            continue;
                        }
                    };
                    let manager = self.clone();
                    let token = ctk.child_token();
                    sessions.spawn(async move {
                        let _permit = permit;
                        manager.connect(send, token).await;
                    });
                }
            }
        };
        ctk.cancel();
        sessions.close();
        sessions.wait().await;
        result
    }
    async fn classify_lan(
        &self,
        mut stream: TcpStream,
        token: &CancellationToken,
    ) -> Option<(TcpStream, n::ConnectionRequestFrame)> {
        let mut reader = FrameReader::default();
        let bytes = tokio::select! {
            biased;
            _ = token.cancelled() => return None,
            result = tokio::time::timeout(Duration::from_secs(5), reader.read_frame(&mut stream)) => {
                match result {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(_)) => {
                        log::debug!("discarding LAN connection: first frame unavailable");
                        return None;
                    }
                    Err(_) => {
                        log::debug!("discarding LAN connection: first frame timed out");
                        return None;
                    }
                }
            }
        };
        match classify_initial_frame(&bytes) {
            Ok(InitialFrame::ConnectionRequest(request)) => Some((stream, request)),
            Ok(InitialFrame::Upgrade(introduction)) => {
                match &self.lan_upgrade_host {
                    Some(host) => {
                        if host.route(LanCandidate { stream, introduction }).is_err() {
                            log::debug!("discarding LAN connection: upgrade candidate not admitted");
                        }
                    }
                    None => {
                        log::debug!("discarding LAN connection: no upgrade host");
                    }
                }
                None
            }
            Err(_) => {
                log::debug!("discarding LAN connection: invalid initial frame");
                None
            }
        }
    }

    async fn incoming_session(
        &self,
        stream: TransportStream,
        transport: &str,
        initial_request: Option<n::ConnectionRequestFrame>,
        token: CancellationToken,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let watcher = self.watch_cancel(id.clone(), token.clone(), None);
        self.session(
            id,
            stream,
            transport,
            None,
            None,
            initial_request,
            None,
            token.clone(),
        )
        .await;
        token.cancel();
        let _ = watcher.await;
    }
    fn failure(&self, id: &str, outbound: bool, transport: Option<String>, error: String) {
        let _ = self.events.send(ChannelMessage {
            id: id.into(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(if outbound {
                TransferType::Outbound
            } else {
                TransferType::Inbound
            }),
            state: Some(State::Disconnected),
            transport,
            error: Some(error),
            ..Default::default()
        });
    }
    async fn connect(&self, send: SendInfo, ctk: CancellationToken) {
        if send.cancellation.is_cancelled() {
            ctk.cancel();
        }
        let watcher = self.watch_cancel(
            send.id.clone(),
            ctk.clone(),
            Some(send.cancellation.clone()),
        );
        let _ = self.events.send(ChannelMessage {
            id: send.id.clone(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(TransferType::Outbound),
            state: Some(State::Initial),
            ..Default::default()
        });
        // QR association is to this exact advertised LAN socket. A route-cache
        // preference/fallback must never redirect its private invitation proof.
        let route = send.qr.is_none().then(|| self.routes.read().get(&send.addr).cloned()).flatten();
        let (remote_endpoint, preferred, fallback) = match (&send.qr, route) {
            (Some(qr), _) => (Some(qr.endpoint_id.clone()), None, None),
            (None, Some(route)) => (Some(route.endpoint), route.preferred, route.fallback),
            (None, None) => (None, None, None),
        };
        let connection = async {
            if let Some(qr) = &send.qr {
                ensure!(self.lan, "QR sending requires LAN");
                let address = send.addr.parse::<std::net::SocketAddrV4>()?;
                ensure!(
                    address.port() != 0
                        && !address.ip().is_unspecified()
                        && !address.ip().is_multicast()
                        && !address.ip().is_broadcast()
                        && qr.endpoint_id.len() == 4
                        && qr.endpoint_id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
                    "invalid QR receiver endpoint"
                );
            }
            let destinations = [preferred, Some(send.addr), fallback];
            let mut error = anyhow::anyhow!("no usable route");
            for route in destinations.into_iter().flatten() {
                if route.starts_with("bt:") {
                    #[cfg(target_os = "linux")]
                    if let Some(bluetooth) = &self.bluetooth {
                        match bluetooth.connect(&route).await {
                            Ok(stream) => return Ok((stream, "bluetooth")),
                            Err(e) => {
                                error = e;
                                continue;
                            }
                        }
                    }
                    error = anyhow::anyhow!("Bluetooth is unavailable");
                } else {
                    match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&route))
                        .await
                    {
                        Ok(Ok(stream)) => return Ok((Box::new(stream) as TransportStream, "lan")),
                        Ok(Err(e)) => error = e.into(),
                        Err(e) => error = e.into(),
                    }
                }
            }
            Err(error)
        };
        match tokio::select! {biased;_=send.cancellation.cancelled()=>{ctk.cancel();Err(anyhow::anyhow!("connection cancelled"))},_=ctk.cancelled()=>Err(anyhow::anyhow!("connection cancelled")),result=connection=>result}
        {
            Ok((stream, transport)) => {
                self.session(
                    send.id,
                    stream,
                    transport,
                    Some(send.ob),
                    remote_endpoint,
                    None,
                    send.qr.map(|qr| qr.invitation),
                    ctk.clone(),
                )
                .await
            }
            Err(error) => {
                if ctk.is_cancelled() {
                    let _ = self.events.send(ChannelMessage {
                        id: send.id,
                        direction: ChannelDirection::LibToFront,
                        rtype: Some(TransferType::Outbound),
                        state: Some(State::Cancelled),
                        ..Default::default()
                    });
                } else {
                    self.failure(&send.id, true, None, error.to_string());
                }
            }
        }
        ctk.cancel();
        let _ = watcher.await;
    }
    async fn session(
        &self,
        id: String,
        stream: TransportStream,
        transport: &str,
        outgoing: Option<OutboundPayload>,
        remote_endpoint: Option<String>,
        initial_request: Option<n::ConnectionRequestFrame>,
        qr_invitation: Option<Arc<QrInvitation>>,
        ctk: CancellationToken,
    ) {
        let download = self.download.clone();
        let outgoing_flag = outgoing.is_some();
        let mut session = match Session::new(
            id.clone(),
            stream,
            transport,
            self.endpoint,
            self.name.clone(),
            self.events.clone(),
            download,
            self.lan,
            self.hotspot,
            self.consent.clone(),
            outgoing,
            remote_endpoint,
            self.lan_upgrade_host.clone(),
        ) {
            Ok(session) => session,
            Err(error) => {
                self.failure(
                    &id,
                    outgoing_flag,
                    Some(transport.into()),
                    error.to_string(),
                );
                return;
            }
        };
        if let Some(invitation) = qr_invitation {
            if let Err(error) = session.set_qr_invitation(invitation) {
                self.failure(&id, outgoing_flag, Some(transport.into()), error.to_string());
                return;
            }
        }
        if let Some(request) = initial_request {
            session.set_initial_request(request);
        }
        let _ = session.run(ctk).await;
    }
    fn watch_cancel(
        &self,
        id: String,
        token: CancellationToken,
        retained: Option<CancellationToken>,
    ) -> tokio::task::JoinHandle<()> {
        let mut messages = self.events.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {_=token.cancelled()=>break,_=async{retained.as_ref().expect("guarded retained cancellation").cancelled().await},if retained.is_some()=>{token.cancel();break;},message=messages.recv()=>match message{
                    Ok(message) if message.direction==ChannelDirection::FrontToLib && message.id==id && message.action==Some(ChannelAction::CancelTransfer)=>{token.cancel();break;},
                    Err(broadcast::error::RecvError::Closed)=>break,
                    Err(broadcast::error::RecvError::Lagged(_))=>{token.cancel();break;},_=>{}
                }}
            }
        })
    }
}

enum InitialFrame {
    ConnectionRequest(n::ConnectionRequestFrame),
    Upgrade(n::bandwidth_upgrade_negotiation_frame::ClientIntroduction),
}

fn classify_initial_frame(bytes: &[u8]) -> Result<InitialFrame> {
    use n::bandwidth_upgrade_negotiation_frame::EventType;
    use n::v1_frame::FrameType;

    let frame = n::OfflineFrame::decode(bytes)?;
    ensure!(
        frame.version == Some(n::offline_frame::Version::V1.into()),
        "unsupported LAN first-frame version"
    );
    let v1 = frame.v1.ok_or_else(|| anyhow!("missing LAN first-frame body"))?;
    let bodies = v1.connection_request.is_some() as usize
        + v1.connection_response.is_some() as usize
        + v1.payload_transfer.is_some() as usize
        + v1.bandwidth_upgrade_negotiation.is_some() as usize
        + v1.keep_alive.is_some() as usize
        + v1.disconnection.is_some() as usize
        + v1.paired_key_encryption.is_some() as usize
        + v1.bandwidth_upgrade_retry.is_some() as usize;
    ensure!(bodies == 1, "ambiguous LAN first-frame body");
    match v1.r#type {
        Some(kind) if kind == FrameType::ConnectionRequest as i32 => {
            let request = v1
                .connection_request
                .ok_or_else(|| anyhow!("missing LAN connection request"))?;
            ensure!(request.endpoint_id().len() == 4, "invalid LAN endpoint");
            Ok(InitialFrame::ConnectionRequest(request))
        }
        Some(kind) if kind == FrameType::BandwidthUpgradeNegotiation as i32 => {
            let upgrade = v1
                .bandwidth_upgrade_negotiation
                .ok_or_else(|| anyhow!("missing LAN upgrade introduction"))?;
            ensure!(
                upgrade.event_type == Some(EventType::ClientIntroduction.into())
                    && upgrade.upgrade_path_info.is_none()
                    && upgrade.client_introduction_ack.is_none(),
                "invalid LAN upgrade introduction"
            );
            let introduction = upgrade
                .client_introduction
                .ok_or_else(|| anyhow!("missing LAN client introduction"))?;
            ensure!(introduction.endpoint_id().len() == 4, "invalid LAN endpoint");
            Ok(InitialFrame::Upgrade(introduction))
        }
        _ => Err(anyhow!("unsupported LAN first-frame type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn lan_manager(
        visibility: watch::Receiver<Visibility>,
        host: LanUpgradeHost,
    ) -> (Manager, broadcast::Receiver<ChannelMessage>) {
        let (events, receive) = broadcast::channel(256);
        (
            Manager {
                endpoint: *b"TEST",
                name: "test".into(),
                events,
                visibility,
                download: Arc::new(RwLock::new(std::env::temp_dir())),
                consent: Arc::new(AtomicBool::new(false)),
                lan: true,
                hotspot: false,
                lan_upgrade_host: Some(host),
                routes: Arc::new(RwLock::new(HashMap::new())),
                #[cfg(target_os = "linux")]
                bluetooth: None,
            },
            receive,
        )
    }

    fn connection_request(endpoint: &str) -> n::OfflineFrame {
        n::OfflineFrame {
            version: Some(n::offline_frame::Version::V1.into()),
            v1: Some(n::V1Frame {
                r#type: Some(n::v1_frame::FrameType::ConnectionRequest.into()),
                connection_request: Some(n::ConnectionRequestFrame {
                    endpoint_id: Some(endpoint.into()),
                    endpoint_info: Some(
                        crate::utils::RemoteDeviceInfo {
                            name: "test peer".into(),
                            device_type: crate::utils::DeviceType::Phone,
                        }
                        .serialize(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn introduction(endpoint: &str) -> n::OfflineFrame {
        n::OfflineFrame {
            version: Some(n::offline_frame::Version::V1.into()),
            v1: Some(n::V1Frame {
                r#type: Some(n::v1_frame::FrameType::BandwidthUpgradeNegotiation.into()),
                bandwidth_upgrade_negotiation: Some(n::BandwidthUpgradeNegotiationFrame {
                    event_type: Some(
                        n::bandwidth_upgrade_negotiation_frame::EventType::ClientIntroduction.into(),
                    ),
                    client_introduction: Some(
                        n::bandwidth_upgrade_negotiation_frame::ClientIntroduction {
                            endpoint_id: Some(endpoint.into()),
                            supports_disabling_encryption: Some(false),
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn append_frame(wire: &mut Vec<u8>, bytes: &[u8]) {
        wire.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        wire.extend_from_slice(bytes);
    }

    async fn assert_peer_closed(peer: &mut TcpStream) {
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
            .await
            .expect("inadmissible connection must be dropped");
        match result {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("expected a closed peer, received {other:?}"),
        }
    }

    async fn open_ordinary_session(address: std::net::SocketAddr, endpoint: &str) -> TcpStream {
        let mut peer = TcpStream::connect(address).await.unwrap();
        let (_, client_init) = crate::crypto::Handshake::client().unwrap();
        let mut wire = Vec::new();
        append_frame(&mut wire, &connection_request(endpoint).encode_to_vec());
        append_frame(&mut wire, &client_init);
        peer.write_all(&wire).await.unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            FrameReader::default().read_frame(&mut peer),
        )
        .await
        .expect("visible ordinary session must receive a real UKEY2 server response")
        .unwrap();
        let response = crate::securegcm::Ukey2Message::decode(response.as_slice()).unwrap();
        assert_eq!(
            response.message_type,
            Some(crate::securegcm::ukey2_message::Type::ServerInit.into())
        );
        peer
    }

    struct Source(PathBuf);
    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_queued_send_never_opens_a_peer_connection() {
        let source = Source(
            std::env::temp_dir().join(format!("quickshare-queue-test-{}", uuid::Uuid::new_v4())),
        );
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source.0)
            .unwrap()
            .write_all(b"Do not send after cancellation")
            .unwrap();
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        let (events, mut receive) = broadcast::channel(64);
        let (_, visibility) = watch::channel(Visibility::Invisible);
        let manager = Manager {
            endpoint: *b"TEST",
            name: "test".into(),
            events,
            visibility,
            download: Arc::new(RwLock::new(std::env::temp_dir())),
            consent: Arc::new(AtomicBool::new(false)),
            lan: true,
            hotspot: false,
            lan_upgrade_host: None,
            routes: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(target_os = "linux")]
            bluetooth: None,
        };
        let (outgoing, sends) = mpsc::channel(1);
        let (_incoming, incoming) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        outgoing
            .try_send(SendInfo {
                id: "queued".into(),
                addr: peer.local_addr().unwrap().to_string(),
                ob: OutboundPayload::Files(vec![crate::OutboundFile::open(&source.0).unwrap()]),
                cancellation: cancellation.clone(),
                qr: None,
            })
            .unwrap();
        cancellation.cancel();
        let stop = CancellationToken::new();
        let worker = tokio::spawn(manager.run(None, incoming, sends, stop.clone()));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = receive.recv().await.unwrap();
                if event.id == "queued" && event.state == Some(State::Cancelled) {
                    break;
                }
            }
        })
        .await
        .expect("queued cancellation must reach a terminal state");
        stop.cancel();
        worker.await.unwrap().unwrap();
        assert_eq!(
            peer.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "a cancelled queued transfer must not contact the peer"
        );
    }

    #[tokio::test]
    async fn routes_prefer_available_network_and_fall_back_after_connection_failure() {
        let source = Source(
            std::env::temp_dir().join(format!("quickshare-route-test-{}", uuid::Uuid::new_v4())),
        );
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source.0)
            .unwrap()
            .write_all(b"Route selection still requires file consent")
            .unwrap();
        for prefer in [true, false] {
            let original = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            original.set_nonblocking(true).unwrap();
            let address = original.local_addr().unwrap().to_string();
            let candidate = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let candidate_address = candidate.local_addr().unwrap().to_string();
            let (events, _) = broadcast::channel(64);
            let (_, visibility) = watch::channel(Visibility::Invisible);
            let routes = Arc::new(RwLock::new(HashMap::from([(
                address.clone(),
                Route {
                    endpoint: "PEER".into(),
                    preferred: prefer.then(|| candidate_address.clone()),
                    fallback: (!prefer).then_some(candidate_address),
                },
            )])));
            let manager = Manager {
                endpoint: *b"TEST",
                name: "test".into(),
                events,
                visibility,
                download: Arc::new(RwLock::new(std::env::temp_dir())),
                consent: Arc::new(AtomicBool::new(false)),
                lan: true,
                hotspot: false,
                lan_upgrade_host: None,
                routes,
                #[cfg(target_os = "linux")]
                bluetooth: None,
            };
            let original = prefer.then_some(original); // closed selected route in fallback case
            let stop = CancellationToken::new();
            let worker_token = stop.clone();
            let selected = crate::OutboundFile::open(&source.0).unwrap();
            let worker = tokio::spawn(async move {
                manager
                    .connect(
                        SendInfo {
                            id: "route".into(),
                            addr: address,
                            ob: OutboundPayload::Files(vec![selected]),
                            cancellation: CancellationToken::new(),
                            qr: None,
                        },
                        worker_token,
                    )
                    .await;
            });
            let connection = tokio::time::timeout(Duration::from_secs(2), candidate.accept())
                .await
                .expect("must use the preferred available route or the backup after failure")
                .unwrap();
            stop.cancel();
            drop(connection);
            worker.await.unwrap();
            if let Some(original) = original {
                assert_eq!(
                    original.accept().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock,
                    "the selected route must not be used while the preferred network is available"
                );
            }
        }
    }

    #[tokio::test]
    async fn qr_sends_use_only_the_matched_lan_socket_without_preference_or_fallback() {
        let source = Source(
            std::env::temp_dir().join(format!("quickshare-qr-route-{}", uuid::Uuid::new_v4())),
        );
        std::fs::OpenOptions::new().write(true).create_new(true).open(&source.0)
            .unwrap().write_all(b"QR still requires ordinary file consent").unwrap();
        for matched_available in [true, false] {
            let matched = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = matched.local_addr().unwrap().to_string();
            let unrelated = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            unrelated.set_nonblocking(true).unwrap();
            let unrelated_address = unrelated.local_addr().unwrap().to_string();
            let (_, visible) = watch::channel(Visibility::Invisible);
            let (manager, _) = lan_manager(visible, LanUpgradeHost::new(1234));
            manager.routes.write().insert(address.clone(), Route {
                endpoint: "WRNG".into(),
                preferred: Some(unrelated_address.clone()),
                fallback: Some(unrelated_address),
            });
            let invitation = Arc::new(QrInvitation::new().unwrap());
            let send = SendInfo {
                id: "qr-route".into(), addr: address,
                ob: OutboundPayload::Files(vec![crate::OutboundFile::open(&source.0).unwrap()]),
                cancellation: CancellationToken::new(),
                qr: Some(QrSendContext { invitation, endpoint_id: "PEER".into() }),
            };
            let stop = CancellationToken::new();
            if matched_available {
                let worker_token = stop.clone();
                let worker = tokio::spawn(async move { manager.connect(send, worker_token).await; });
                let connection = tokio::time::timeout(Duration::from_secs(2), matched.accept())
                    .await.expect("QR must connect directly to the matching receiver").unwrap();
                stop.cancel();
                drop(connection);
                worker.await.unwrap();
            } else {
                drop(matched);
                tokio::time::timeout(Duration::from_secs(2), manager.connect(send, stop))
                    .await.expect("a failed QR socket must not try another route");
            }
            assert_eq!(unrelated.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        }
    }

    #[tokio::test]
    async fn visibility_expiry_rejects_new_sessions_but_preserves_pending_upgrade_admission() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = LanUpgradeHost::new(address.port());
        let mut registration = host.register("PEER").unwrap();
        let (visibility, visible) = watch::channel(Visibility::Visible);
        let (manager, mut events) = lan_manager(visible, host);
        let (_incoming, incoming) = mpsc::channel(1);
        let (_outgoing, sends) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let worker = tokio::spawn(manager.run(Some(listener), incoming, sends, stop.clone()));

        let mut ordinary = TcpStream::connect(address).await.unwrap();
        let request = connection_request("PEER").encode_to_vec();
        let prefix = (request.len() as u32).to_be_bytes();
        ordinary.write_all(&prefix[..3]).await.unwrap();
        visibility.send_replace(Visibility::Invisible);
        ordinary.write_all(&prefix[3..]).await.unwrap();
        ordinary.write_all(&request).await.unwrap();
        assert_peer_closed(&mut ordinary).await;

        let mut upgrade = TcpStream::connect(address).await.unwrap();
        let mut wire = Vec::new();
        append_frame(&mut wire, &introduction("PEER").encode_to_vec());
        append_frame(&mut wire, b"remaining bytes belong to the existing transfer");
        upgrade.write_all(&wire).await.unwrap();
        let mut candidate = tokio::time::timeout(Duration::from_secs(2), registration.recv())
            .await
            .expect("a pending upgrade must remain admissible while invisible")
            .unwrap();
        assert_eq!(candidate.introduction.endpoint_id(), "PEER");
        assert_eq!(candidate.stream.peer_addr().unwrap(), upgrade.local_addr().unwrap());
        assert_eq!(
            FrameReader::default().read_frame(&mut candidate.stream).await.unwrap(),
            b"remaining bytes belong to the existing transfer"
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("manager must join classifier tasks on cancellation")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn ordinary_tcp_admission_primes_the_request_once_without_losing_the_next_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_visibility, visible) = watch::channel(Visibility::Visible);
        let (manager, mut events) = lan_manager(visible, LanUpgradeHost::new(address.port()));
        let (_incoming, incoming) = mpsc::channel(1);
        let (_outgoing, sends) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let worker = tokio::spawn(manager.run(Some(listener), incoming, sends, stop.clone()));
        let _peer = open_ordinary_session(address, "PEER").await;
        let mut requests = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.unwrap();
                assert_eq!(event.rtype, Some(TransferType::Inbound));
                assert!(event.error.is_none());
                if event.state == Some(State::ReceivedConnectionRequest) {
                    requests += 1;
                }
                if event.state == Some(State::SentUkeyServerInit) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(requests, 1);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("manager must join the admitted session on cancellation")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn pending_upgrade_uses_classifier_capacity_when_all_file_sessions_are_active() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = LanUpgradeHost::new(address.port());
        let mut registration = host.register("PEER").unwrap();
        let (visibility, visible) = watch::channel(Visibility::Visible);
        let (manager, mut events) = lan_manager(visible, host);
        let (_incoming, incoming) = mpsc::channel(1);
        let (_outgoing, sends) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let worker = tokio::spawn(manager.run(Some(listener), incoming, sends, stop.clone()));
        let mut peers = Vec::new();
        for index in 0..16 {
            peers.push(open_ordinary_session(address, &format!("S{index:03}")).await);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let event = events.recv().await.unwrap();
                    assert!(event.error.is_none());
                    if event.state == Some(State::SentUkeyServerInit) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        let mut excess = TcpStream::connect(address).await.unwrap();
        let mut excess_wire = Vec::new();
        append_frame(&mut excess_wire, &connection_request("LAST").encode_to_vec());
        excess.write_all(&excess_wire).await.unwrap();
        assert_peer_closed(&mut excess).await;
        visibility.send_replace(Visibility::Invisible);
        let mut upgrade = TcpStream::connect(address).await.unwrap();
        let mut wire = Vec::new();
        append_frame(&mut wire, &introduction("PEER").encode_to_vec());
        upgrade.write_all(&wire).await.unwrap();
        let candidate = tokio::time::timeout(Duration::from_secs(2), registration.recv())
            .await
            .expect("upgrade routing must not acquire one of the occupied session slots")
            .unwrap();
        assert_eq!(candidate.stream.peer_addr().unwrap(), upgrade.local_addr().unwrap());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("all active file sessions must cancel and join")
            .unwrap()
            .unwrap();
        drop(peers);
    }

    #[tokio::test]
    async fn malformed_and_unregistered_public_introductions_do_not_create_transfers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = LanUpgradeHost::new(address.port());
        let mut registration = host.register("PEER").unwrap();
        let (_visibility, visible) = watch::channel(Visibility::Invisible);
        let (manager, mut events) = lan_manager(visible, host);
        let (_incoming, incoming) = mpsc::channel(1);
        let (_outgoing, sends) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let worker = tokio::spawn(manager.run(Some(listener), incoming, sends, stop.clone()));
        let mut wrong_version = introduction("PEER");
        wrong_version.version = Some(n::offline_frame::Version::UnknownVersion.into());
        let mut missing_type = introduction("PEER");
        missing_type.v1.as_mut().unwrap().r#type = None;
        let mut wrong_type = introduction("PEER");
        wrong_type.v1.as_mut().unwrap().r#type =
            Some(n::v1_frame::FrameType::KeepAlive.into());
        let mut missing_introduction = introduction("PEER");
        missing_introduction
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .client_introduction = None;
        let mut mixed_body = introduction("PEER");
        mixed_body.v1.as_mut().unwrap().connection_request =
            connection_request("PEER").v1.unwrap().connection_request;
        let mut mixed_upgrade = introduction("PEER");
        mixed_upgrade
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .upgrade_path_info = Some(Default::default());
        let mut wrong_event = introduction("PEER");
        wrong_event
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_negotiation
            .as_mut()
            .unwrap()
            .event_type = Some(
            n::bandwidth_upgrade_negotiation_frame::EventType::ClientIntroductionAck.into(),
        );
        for frame in [
            wrong_version,
            missing_type,
            wrong_type,
            missing_introduction,
            mixed_body,
            mixed_upgrade,
            wrong_event,
            introduction("NONE"),
            introduction("invalid"),
        ] {
            let mut peer = TcpStream::connect(address).await.unwrap();
            let mut wire = Vec::new();
            append_frame(&mut wire, &frame.encode_to_vec());
            peer.write_all(&wire).await.unwrap();
            assert_peer_closed(&mut peer).await;
            assert!(matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
        }
        let mut valid = TcpStream::connect(address).await.unwrap();
        let mut wire = Vec::new();
        append_frame(&mut wire, &introduction("PEER").encode_to_vec());
        valid.write_all(&wire).await.unwrap();
        let candidate = tokio::time::timeout(Duration::from_secs(2), registration.recv())
            .await
            .expect("rejected candidates must not freeze a pending registration")
            .unwrap();
        assert_eq!(candidate.stream.peer_addr().unwrap(), valid.local_addr().unwrap());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
