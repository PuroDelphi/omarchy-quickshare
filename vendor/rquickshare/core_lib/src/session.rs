//! One Nearby/Quick Share protocol for LAN, RFCOMM and negotiated hotspot.
//! Based on rquickshare (GPLv3); BWU ordering follows Google Nearby (Apache-2.0)
//! bwu_manager.cc at d128b5fca30b3fa8d4642f8c75dd9e314e090e74.
//! Sharing frame ordering follows Google's incoming_frames_reader.cc and
//! nearby_sharing_service_impl.cc at the same revision.
use crate::channel::{ChannelAction, ChannelDirection, ChannelMessage, TransferType};
use crate::crypto::{Crypto, Handshake};
use crate::hdl::info::TransferMetadata;
use crate::hdl::{OutboundPayload, State};
use crate::lan_upgrade::{LanUpgradeHost, LanUpgradeRegistration};
use crate::location_nearby_connections as n;
use crate::network::{LanInterface, NetworkLease, NetworkManager};
use crate::qr::QrInvitation;
use crate::sharing_nearby as q;
use crate::storage::IncomingFiles;
use crate::transport::{FramedStream, TransportStream};
use crate::utils::{DeviceType, RemoteDeviceInfo};
use anyhow::{anyhow, bail, ensure, Result};
use n::bandwidth_upgrade_negotiation_frame::upgrade_path_info::{
    Medium, WifiHotspotCredentials, WifiLanSocket,
};
use n::bandwidth_upgrade_negotiation_frame::{
    ClientIntroduction, ClientIntroductionAck, EventType as BwuEvent, UpgradePathInfo,
};
use n::payload_transfer_frame::{
    control_message::EventType as ControlEvent, payload_header::PayloadType, PacketType,
    PayloadChunk, PayloadHeader,
};
use n::v1_frame::FrameType;
use parking_lot::RwLock;
use prost::Message;
use q::v1_frame::FrameType as ShareType;
use rand::Rng;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
const CHUNK: usize = 64 * 1024;
const CONTROL_LIMIT: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Connection,
    ClientInit,
    ClientFinish,
    ServerInit,
    ConnectionResponse,
    PairedEncryption,
    PairedResult,
    Introduction,
    Consent,
    Response,
    Transfer,
    Acknowledgments,
    Closing,
    Done,
}
#[derive(Debug)]
struct SendingFile {
    metadata: q::FileMetadata,
    file: File,
    offset: i64,
    last_sent: bool,
    acknowledged: bool,
}
#[derive(Debug)]
struct BytesPayload {
    size: i64,
    bytes: Vec<u8>,
}
enum UpgradeListener {
    Hotspot(TcpListener),
    Lan(LanUpgradeRegistration, Vec<LanInterface>),
}
impl UpgradeListener {
    async fn accept(&mut self) -> Result<(TcpStream, Option<ClientIntroduction>)> {
        match self {
            Self::Hotspot(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok((stream, None))
            }
            Self::Lan(registration, _) => {
                let candidate = registration
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("LAN upgrade admission closed"))?;
                Ok((candidate.stream, Some(candidate.introduction)))
            }
        }
    }
}
struct Upgrade {
    listener: Option<UpgradeListener>,
    next: Option<FramedStream>,
    lease: Option<NetworkLease>,
    info: UpgradePathInfo,
    deadline: Instant,
    last_sent: bool,
    last_received: bool,
}
pub struct Session {
    pub id: String,
    pub state: State,
    transport: String,
    outbound: bool,
    endpoint: String,
    remote_endpoint: Option<String>,
    local_name: String,
    socket: FramedStream,
    initial_request: Option<n::ConnectionRequestFrame>,
    phase: Phase,
    handshake: Option<Handshake>,
    crypto: Option<Crypto>,
    qr_invitation: Option<Arc<QrInvitation>>,
    sender: broadcast::Sender<ChannelMessage>,
    receiver: broadcast::Receiver<ChannelMessage>,
    metadata: TransferMetadata,
    files: BTreeMap<i64, SendingFile>,
    incoming_metadata: Vec<q::FileMetadata>,
    incoming: Option<IncomingFiles>,
    bytes: HashMap<i64, BytesPayload>,
    sent_control: HashMap<i64, i64>,
    pending_paired_result: Option<q::PairedKeyResultFrame>,
    pending_introduction: Option<q::IntroductionFrame>,
    download: Arc<RwLock<PathBuf>>,
    hotspot: bool,
    consent: Arc<AtomicBool>,
    remote_mediums: Vec<i32>,
    upgrade: Option<Upgrade>,
    lease: Option<NetworkLease>,
    pending_accept: bool,
    pending_lan: bool,
    deferred: Vec<Vec<u8>>,
    lan: bool,
    lan_upgrade_host: Option<LanUpgradeHost>,
    remote_accepted: bool,
    remote_disconnect_version: i32,
    close_deadline: Option<Instant>,
    last_activity: Instant,
    last_progress: Instant,
    last_bytes: Instant,
    phase_deadline: Instant,
    locally_verified: bool,
    cancellation: CancellationToken,
    pending_file: Option<(i64, u64)>,
}
fn offline(v1: n::V1Frame) -> n::OfflineFrame {
    n::OfflineFrame {
        version: Some(n::offline_frame::Version::V1.into()),
        v1: Some(v1),
    }
}
fn sharing(v1: q::V1Frame) -> q::Frame {
    q::Frame {
        version: Some(q::frame::Version::V1.into()),
        v1: Some(v1),
    }
}
fn bwu(event: BwuEvent, info: Option<UpgradePathInfo>) -> n::OfflineFrame {
    offline(n::V1Frame {
        r#type: Some(FrameType::BandwidthUpgradeNegotiation.into()),
        bandwidth_upgrade_negotiation: Some(n::BandwidthUpgradeNegotiationFrame {
            event_type: Some(event.into()),
            upgrade_path_info: info,
            ..Default::default()
        }),
        ..Default::default()
    })
}
fn disconnect() -> n::OfflineFrame {
    offline(n::V1Frame {
        r#type: Some(FrameType::Disconnection.into()),
        disconnection: Some(n::DisconnectionFrame::default()),
        ..Default::default()
    })
}
fn response() -> n::OfflineFrame {
    offline(n::V1Frame {
        r#type: Some(FrameType::ConnectionResponse.into()),
        connection_response: Some(n::ConnectionResponseFrame {
            // Version 1 supports clean disconnect, NOT auto-reconnect/resume (3+).
            status: Some(0),
            response: Some(n::connection_response_frame::ResponseStatus::Accept.into()),
            safe_to_disconnect_version: Some(1),
            os_info: Some(n::OsInfo {
                r#type: Some(n::os_info::OsType::Linux.into()),
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}
impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: String,
        stream: TransportStream,
        transport: &str,
        endpoint: [u8; 4],
        name: String,
        sender: broadcast::Sender<ChannelMessage>,
        download: Arc<RwLock<PathBuf>>,
        lan: bool,
        hotspot: bool,
        consent: Arc<AtomicBool>,
        outgoing: Option<OutboundPayload>,
        remote_endpoint: Option<String>,
        lan_upgrade_host: Option<LanUpgradeHost>,
    ) -> Result<Self> {
        let receiver = sender.subscribe();
        let outbound = outgoing.is_some();
        ensure!(
            remote_endpoint.as_ref().is_none_or(|id| id.len() == 4),
            "invalid discovered endpoint hint"
        );
        let mut files = BTreeMap::new();
        let mut names = Vec::new();
        let mut total = 0u64;
        if let Some(OutboundPayload::Files(selected)) = outgoing {
            ensure!(
                !selected.is_empty() && selected.len() <= crate::storage::MAX_FILES,
                "invalid file selection"
            );
            for selected in selected {
                let (file, name, size) = selected.into_parts();
                total = total
                    .checked_add(size)
                    .ok_or_else(|| anyhow!("transfer size overflow"))?;
                ensure!(
                    total <= crate::storage::MAX_TOTAL_BYTES,
                    "transfer exceeds size limit"
                );
                let mut id = rand::rng().random::<i64>();
                while files.contains_key(&id) {
                    id = rand::rng().random();
                }
                let mime = mime_guess::from_path(&name)
                    .first_or_octet_stream()
                    .to_string();
                let kind = if mime.starts_with("image/") {
                    q::file_metadata::Type::Image
                } else if mime.starts_with("video/") {
                    q::file_metadata::Type::Video
                } else if mime.starts_with("audio/") {
                    q::file_metadata::Type::Audio
                } else {
                    q::file_metadata::Type::Unknown
                };
                let metadata = q::FileMetadata {
                    name: Some(name.clone()),
                    payload_id: Some(id),
                    size: Some(size as i64),
                    mime_type: Some(mime),
                    r#type: Some(kind.into()),
                    ..Default::default()
                };
                files.insert(
                    id,
                    SendingFile {
                        metadata,
                        file,
                        offset: 0,
                        last_sent: false,
                        acknowledged: false,
                    },
                );
                names.push(name);
            }
        }
        let metadata = TransferMetadata {
            id: id.clone(),
            files: if outbound { Some(names) } else { None },
            total_bytes: total,
            ..Default::default()
        };
        Ok(Self {
            id,
            state: State::Initial,
            transport: transport.into(),
            outbound,
            endpoint: String::from_utf8(endpoint.to_vec())?,
            remote_endpoint,
            local_name: name,
            socket: FramedStream::new(stream),
            initial_request: None,
            phase: if outbound {
                Phase::ServerInit
            } else {
                Phase::Connection
            },
            handshake: None,
            crypto: None,
            qr_invitation: None,
            sender,
            receiver,
            metadata,
            files,
            incoming_metadata: vec![],
            incoming: None,
            bytes: HashMap::new(),
            sent_control: HashMap::new(),
            pending_paired_result: None,
            pending_introduction: None,
            download,
            hotspot,
            consent,
            remote_mediums: vec![],
            upgrade: None,
            lease: None,
            pending_accept: false,
            pending_lan: false,
            deferred: vec![],
            lan,
            lan_upgrade_host,
            remote_accepted: false,
            remote_disconnect_version: 0,
            close_deadline: None,
            last_activity: Instant::now(),
            last_progress: Instant::now(),
            last_bytes: Instant::now(),
            phase_deadline: Instant::now() + Duration::from_secs(90),
            locally_verified: false,
            cancellation: CancellationToken::new(),
            pending_file: None,
        })
    }
    pub(crate) fn set_initial_request(&mut self, request: n::ConnectionRequestFrame) {
        self.initial_request = Some(request);
    }
    pub(crate) fn set_qr_invitation(&mut self, invitation: Arc<QrInvitation>) -> Result<()> {
        ensure!(
            self.outbound && self.phase == Phase::ServerInit && self.crypto.is_none()
                && self.transport == "lan" && self.remote_endpoint.is_some()
                && self.qr_invitation.is_none(),
            "QR key requires an unstarted outbound LAN session"
        );
        self.qr_invitation = Some(invitation);
        Ok(())
    }
    pub fn event(&self, error: Option<String>) {
        let meta = if matches!(self.state, State::SendingFiles | State::ReceivingFiles) {
            // The frontend retains consent metadata; progress need not copy every filename.
            crate::hdl::info::TransferMetadata {
                total_bytes: self.metadata.total_bytes,
                ack_bytes: self.metadata.ack_bytes,
                ..Default::default()
            }
        } else {
            self.metadata.clone()
        };
        let _ = self.sender.send(ChannelMessage {
            id: self.id.clone(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(if self.outbound {
                TransferType::Outbound
            } else {
                TransferType::Inbound
            }),
            state: Some(self.state.clone()),
            meta: Some(meta),
            transport: Some(self.transport.clone()),
            error,
            ..Default::default()
        });
    }
    fn diagnostic(&self, error: String) {
        let _ = self.sender.send(ChannelMessage {
            direction: ChannelDirection::LibToFront,
            transport: Some(self.transport.clone()),
            error: Some(error),
            ..Default::default()
        });
    }
    async fn can_hotspot(&mut self) -> bool {
        match tokio::time::timeout(Duration::from_secs(5), NetworkManager::available()).await {
            Ok(Ok(available)) => available,
            Ok(Err(error)) => {
                self.diagnostic(format!("hotspot capability query: {error}"));
                false
            }
            Err(_) => {
                self.diagnostic("hotspot capability query timed out".into());
                false
            }
        }
    }
    fn transition(&mut self, state: State) {
        self.state = state;
        self.event(None);
    }
    fn checkpoint(&self) -> Result<()> {
        ensure!(!self.cancellation.is_cancelled(), "transfer cancelled");
        self.check_deadlines(Instant::now())
    }
    fn check_deadlines(&self, now: Instant) -> Result<()> {
        if self.phase == Phase::Done {
            return Ok(());
        }
        ensure!(
            now.duration_since(self.last_activity) < Duration::from_secs(90),
            "peer liveness timed out"
        );
        if self.phase == Phase::Transfer {
            ensure!(
                now.duration_since(self.last_bytes) < Duration::from_secs(90),
                "file bytes stalled"
            );
        } else if self.phase != Phase::Closing {
            ensure!(now < self.phase_deadline, "protocol phase deadline expired");
        }
        Ok(())
    }
    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_deadline = Instant::now()
            + if phase == Phase::Consent {
                Duration::from_secs(600)
            } else {
                Duration::from_secs(90)
            };
        if phase == Phase::Transfer {
            self.last_bytes = Instant::now();
        }
    }
    async fn plain(&mut self, frame: n::OfflineFrame) -> Result<()> {
        self.checkpoint()?;
        let bytes = frame.encode_to_vec();
        self.socket.queue_frame(bytes)?;
        tokio::select! {_=self.cancellation.cancelled()=>bail!("transfer cancelled"),result=self.socket.write_queued_frame()=>result}
    }
    async fn encrypted(&mut self, frame: n::OfflineFrame) -> Result<()> {
        self.checkpoint()?;
        let bytes = frame.encode_to_vec();
        let close_control = frame
            .v1
            .as_ref()
            .and_then(|v| v.bandwidth_upgrade_negotiation.as_ref())
            .is_some_and(|v| v.event_type() == BwuEvent::SafeToClosePriorChannel);
        if self.upgrade.as_ref().is_some_and(|u| u.last_sent) && !close_control {
            let pending: usize = self.deferred.iter().map(Vec::len).sum();
            ensure!(
                pending + bytes.len() <= CONTROL_LIMIT,
                "upgrade deferred output exceeds limit"
            );
            self.deferred.push(bytes);
            return Ok(());
        }
        let data = self
            .crypto
            .as_mut()
            .ok_or_else(|| anyhow!("session is not authenticated"))?
            .seal(bytes)?;
        self.socket.queue_frame(data)?;
        tokio::select! {_=self.cancellation.cancelled()=>bail!("transfer cancelled"),result=self.socket.write_queued_frame()=>result}
    }
    fn payload_frame(
        header: PayloadHeader,
        offset: i64,
        body: Vec<u8>,
        last: bool,
    ) -> n::OfflineFrame {
        offline(n::V1Frame {
            r#type: Some(FrameType::PayloadTransfer.into()),
            payload_transfer: Some(n::PayloadTransferFrame {
                packet_type: Some(PacketType::Data.into()),
                payload_header: Some(header),
                payload_chunk: Some(PayloadChunk {
                    offset: Some(offset),
                    flags: Some(if last { 1 } else { 0 }),
                    body: Some(body),
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    async fn payload(
        &mut self,
        header: PayloadHeader,
        offset: i64,
        body: Vec<u8>,
        last: bool,
    ) -> Result<()> {
        self.encrypted(Self::payload_frame(header, offset, body, last))
            .await
    }
    async fn share(&mut self, frame: q::Frame) -> Result<()> {
        let body = frame.encode_to_vec();
        let length = body.len() as i64;
        let mut id = rand::rng().random::<i64>();
        while self.files.contains_key(&id) || self.sent_control.contains_key(&id) {
            id = rand::rng().random();
        }
        let header = PayloadHeader {
            id: Some(id),
            r#type: Some(PayloadType::Bytes.into()),
            total_size: Some(length),
            ..Default::default()
        };
        self.payload(header.clone(), 0, body, false).await?;
        self.payload(header, length, vec![], true).await?;
        self.sent_control.insert(id, length);
        Ok(())
    }
    async fn paired_encryption(&mut self) -> Result<()> {
        // No Google identity/contact certificates: protocol explicitly falls back
        // to the UKEY PIN, never claims authenticated contact identity.
        let qr_code_handshake_data = self.qr_invitation.as_ref().map(|invitation| {
            let crypto = self.crypto.as_ref().ok_or_else(|| anyhow!("QR proof before UKEY2"))?;
            Ok::<_, anyhow::Error>(invitation.sign_auth_token(crypto.auth_token()).to_vec())
        }).transpose()?;
        self.share(sharing(q::V1Frame {
            r#type: Some(ShareType::PairedKeyEncryption.into()),
            paired_key_encryption: Some(q::PairedKeyEncryptionFrame {
                secret_id_hash: Some(crate::utils::gen_random(6)),
                signed_data: Some(crate::utils::gen_random(72)),
                qr_code_handshake_data,
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
    }
    async fn start(&mut self) -> Result<()> {
        if self.outbound {
            self.event(None);
        }
        if self.outbound {
            let mut mediums = vec![if self.transport == "bluetooth" {
                Medium::Bluetooth.into()
            } else {
                Medium::WifiLan.into()
            }];
            if self.lan && self.transport == "bluetooth" {
                mediums.push(Medium::WifiLan.into());
            }
            if self.hotspot && self.can_hotspot().await {
                mediums.push(Medium::WifiHotspot.into());
            }
            self.plain(offline(n::V1Frame {
                r#type: Some(FrameType::ConnectionRequest.into()),
                connection_request: Some(n::ConnectionRequestFrame {
                    endpoint_id: Some(self.endpoint.clone()),
                    endpoint_name: Some(self.local_name.clone().into_bytes()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: self.local_name.clone(),
                            device_type: DeviceType::Laptop,
                        }
                        .serialize(),
                    ),
                    mediums,
                    nonce: Some(rand::random()),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await?;
            let (hs, bytes) = Handshake::client()?;
            self.handshake = Some(hs);
            self.socket.write_frame(bytes).await?;
            self.transition(State::SentUkeyClientInit);
        }
        Ok(())
    }
    async fn notify_cancel(&mut self) -> Result<bool> {
        if self.crypto.is_none()
            || matches!(
                self.phase,
                Phase::Connection
                    | Phase::ClientInit
                    | Phase::ClientFinish
                    | Phase::ServerInit
                    | Phase::ConnectionResponse
                    | Phase::Done
            )
        {
            return Ok(false);
        }
        ensure!(
            !self
                .upgrade
                .as_ref()
                .is_some_and(|upgrade| upgrade.last_sent),
            "cannot signal cancellation after prior-channel last-write"
        );
        // Finish at most the already-queued frame. Dropping it would corrupt
        // framing and skip its encrypted sequence before the CANCEL control.
        let pending = self.socket.has_pending_frame();
        self.socket.finish_pending_frame().await?;
        if pending {
            if let Some((id, bytes)) = self.pending_file.take() {
                self.files
                    .get_mut(&id)
                    .expect("pending file is retained")
                    .offset += bytes as i64;
                self.metadata.ack_bytes += bytes;
            }
        }
        let crypto = self.crypto.as_mut().expect("authenticated cancellation");
        let socket = &mut self.socket;
        let controls = self
            .files
            .iter()
            .filter_map(|(&id, file)| {
                (!file.acknowledged && (file.offset > 0 || file.last_sent)).then_some((
                    id,
                    file.metadata.size(),
                    file.offset,
                ))
            })
            .chain(self.incoming_metadata.iter().filter_map(|file| {
                let id = file.payload_id?;
                let offset = self.incoming.as_ref()?.received_offset(id)?;
                (offset > 0).then_some((id, file.size(), offset))
            }));
        for (id, size, offset) in controls {
            let frame = offline(n::V1Frame {
                r#type: Some(FrameType::PayloadTransfer.into()),
                payload_transfer: Some(n::PayloadTransferFrame {
                    packet_type: Some(PacketType::Control.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(id),
                        r#type: Some(PayloadType::File.into()),
                        total_size: Some(size),
                        ..Default::default()
                    }),
                    control_message: Some(n::payload_transfer_frame::ControlMessage {
                        event: Some(ControlEvent::PayloadCanceled.into()),
                        offset: Some(offset),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
            socket
                .write_frame(crypto.seal(frame.encode_to_vec())?)
                .await?;
        }
        let body = sharing(q::V1Frame {
            r#type: Some(ShareType::Cancel.into()),
            ..Default::default()
        })
        .encode_to_vec();
        let length = body.len() as i64;
        let mut id = rand::rng().random::<i64>();
        while self.files.contains_key(&id) || self.sent_control.contains_key(&id) {
            id = rand::rng().random();
        }
        let header = PayloadHeader {
            id: Some(id),
            r#type: Some(PayloadType::Bytes.into()),
            total_size: Some(length),
            ..Default::default()
        };
        let frame = Self::payload_frame(header.clone(), 0, body, false);
        socket
            .write_frame(crypto.seal(frame.encode_to_vec())?)
            .await?;
        let frame = Self::payload_frame(header, length, Vec::new(), true);
        socket
            .write_frame(crypto.seal(frame.encode_to_vec())?)
            .await?;
        Ok(true)
    }
    pub async fn run(&mut self, ctk: CancellationToken) -> Result<()> {
        self.cancellation = ctk.clone();
        let mut result = self.drive(ctk.clone()).await;
        let restoration_failed = result
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<crate::network::RestorationFailure>());
        let mut cancellation_error = None;
        if ctk.is_cancelled() && !restoration_failed {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            match tokio::time::timeout_at(deadline, self.notify_cancel()).await {
                Ok(Ok(true)) => {
                    // The initiator gives the peer a bounded processing window;
                    // EOF ends it immediately. This is not a cleanup receipt.
                    let _ = tokio::time::timeout_at(deadline, async {
                        while self.socket.read_frame().await.is_ok() {}
                    })
                    .await;
                }
                Ok(Ok(false)) => {}
                Ok(Err(error)) => {
                    cancellation_error = Some(format!(
                        "remote cancellation could not be signalled: {error}"
                    ))
                }
                Err(_) => {
                    cancellation_error = Some("remote cancellation signalling timed out".into())
                }
            }
            self.state = State::Cancelled;
            self.phase = Phase::Done;
            result = Ok(());
        }
        // Cleanup completion precedes terminal state so the UI cannot advertise
        // success while the user's temporary Wi-Fi connection remains active.
        let mut cleanup_error = None;
        if let Some(lease) = self.upgrade.as_mut().and_then(|u| u.lease.as_mut()) {
            if let Err(e) = lease.close().await {
                cleanup_error = Some(e);
            }
        }
        if let Some(lease) = self.lease.as_mut() {
            if let Err(e) = lease.close().await {
                cleanup_error = Some(e);
            }
        }
        self.upgrade.take();
        self.lease.take();
        self.incoming.take();
        if let Some(e) = cleanup_error {
            self.state = State::Disconnected;
            self.event(Some(format!("network restoration failed: {e}")));
            return Err(e);
        }
        if ctk.is_cancelled() && !restoration_failed {
            self.state = State::Cancelled;
            self.phase = Phase::Done;
            result = Ok(());
        }
        match &result {
            Ok(()) => self.event(cancellation_error),
            Err(e) => {
                if self.outbound || self.phase != Phase::Connection {
                    self.state = State::Disconnected;
                    self.metadata.destination = None;
                    self.event(Some(e.to_string()));
                }
            }
        }
        result
    }
    async fn drive(&mut self, ctk: CancellationToken) -> Result<()> {
        self.start().await?;
        if let Some(request) = self.initial_request.take() {
            self.checkpoint()?;
            self.process_connection_request(request)?;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if self.phase == Phase::Done {
                return Ok(());
            }
            let sending = self.outbound
                && self.phase == Phase::Transfer
                && self.upgrade.is_none()
                && !self.pending_lan;
            let accepting = self
                .upgrade
                .as_ref()
                .is_some_and(|u| u.listener.is_some() && u.next.is_none());
            let upgrade_listener = self.upgrade.as_mut().and_then(|u| u.listener.as_mut());
            enum Input {
                Cancel,
                Message(Result<ChannelMessage, broadcast::error::RecvError>),
                Frame(Result<Vec<u8>>),
                Tick,
                Upgrade(Result<(TcpStream, Option<ClientIntroduction>)>),
            }
            let input = tokio::select! {
                _=ctk.cancelled()=>Input::Cancel,
                msg=self.receiver.recv()=>Input::Message(msg),
                frame=self.socket.read_frame()=>Input::Frame(frame),
                _=tick.tick()=>Input::Tick,
                accepted=async {upgrade_listener.expect("guarded listener").accept().await},if accepting=>Input::Upgrade(accepted),
            };
            match input {
                Input::Cancel => {
                    self.state = State::Cancelled;
                    return Ok(());
                }
                Input::Message(Ok(message))
                    if message.direction == ChannelDirection::FrontToLib
                        && message.id == self.id =>
                {
                    match message.action {
                        Some(ChannelAction::AcceptTransfer) if self.phase == Phase::Consent => {
                            self.accept().await?
                        }
                        Some(ChannelAction::RejectTransfer)
                            if matches!(self.phase, Phase::Consent | Phase::Response) =>
                        {
                            if self.outbound {
                                self.share(sharing(q::V1Frame {
                                    r#type: Some(ShareType::Cancel.into()),
                                    ..Default::default()
                                }))
                                .await?;
                            } else {
                                self.share_response(q::connection_response_frame::Status::Reject)
                                    .await?;
                            }
                            self.state = State::Rejected;
                            return Ok(());
                        }
                        Some(ChannelAction::CancelTransfer) => {
                            self.cancellation.cancel();
                            self.state = State::Cancelled;
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Input::Message(Err(broadcast::error::RecvError::Lagged(_))) => {
                    bail!("control channel lagged; transfer stopped safely")
                }
                Input::Message(_) => {}
                Input::Frame(frame) => match frame {
                    Ok(frame) => {
                        self.checkpoint()?;
                        self.last_activity = Instant::now();
                        self.process(&frame).await?;
                    }
                    Err(_) if self.phase == Phase::Closing => {
                        self.state = State::Finished;
                        self.phase = Phase::Done;
                    }
                    Err(error) => return Err(error),
                },
                Input::Upgrade(accepted) => {
                    let (stream, introduction) = accepted?;
                    self.checkpoint()?;
                    if let Err(error) = self.accept_upgrade(stream, introduction).await {
                        if self.upgrade.as_ref().is_some_and(|u| u.next.is_some()) {
                            return Err(error);
                        }
                        self.diagnostic("rejected invalid upgrade channel introduction".into());
                    }
                }
                Input::Tick => {
                    if let Some(upgrade) = &self.upgrade {
                        if Instant::now() > upgrade.deadline {
                            self.fail_upgrade().await?;
                        }
                    }
                    if self
                        .close_deadline
                        .is_some_and(|deadline| Instant::now() > deadline)
                    {
                        self.state = State::Finished;
                        self.phase = Phase::Done;
                        continue;
                    }
                    self.checkpoint()?;
                    if sending {
                        self.send_next().await?;
                    }
                }
            }
        }
    }
    async fn process(&mut self, bytes: &[u8]) -> Result<()> {
        let phase = self.phase;
        self.process_frame(bytes)
            .await
            .map_err(|error| anyhow!("session frame rejected: phase={phase:?}: {error}"))
    }
    fn process_connection_request(&mut self, mut request: n::ConnectionRequestFrame) -> Result<()> {
        ensure!(self.phase == Phase::Connection, "unexpected connection request");
        ensure!(
            request.endpoint_id().len() == 4,
            "invalid remote endpoint id"
        );
        self.remote_endpoint = request.endpoint_id.take();
        self.remote_mediums = std::mem::take(&mut request.mediums);
        let info = request.endpoint_info();
        ensure!(
            info.len() >= 18 && info.len() >= 18 + info[17] as usize,
            "invalid endpoint info"
        );
        self.metadata.source = Some(RemoteDeviceInfo {
            name: std::str::from_utf8(&info[18..18 + info[17] as usize])?.into(),
            device_type: DeviceType::from_raw_value((info[0] >> 1) & 7),
        });
        self.phase = Phase::ClientInit;
        self.transition(State::ReceivedConnectionRequest);
        Ok(())
    }
    async fn process_frame(&mut self, bytes: &[u8]) -> Result<()> {
        match self.phase {
            Phase::Connection => {
                let frame = n::OfflineFrame::decode(bytes)?;
                let v1 = frame
                    .v1
                    .ok_or_else(|| anyhow!("missing connection request"))?;
                ensure!(
                    v1.r#type == Some(FrameType::ConnectionRequest.into()),
                    "expected connection request, received type={:?}/{:?}",
                    FrameType::try_from(v1.r#type.unwrap_or_default()).ok(),
                    v1.r#type
                );
                let request = v1
                    .connection_request
                    .ok_or_else(|| anyhow!("missing connection request body"))?;
                self.process_connection_request(request)?;
            }
            Phase::ClientInit => {
                let (hs, response) = Handshake::server(bytes)?;
                self.handshake = Some(hs);
                self.socket.write_frame(response).await?;
                self.phase = Phase::ClientFinish;
                self.transition(State::SentUkeyServerInit);
            }
            Phase::ClientFinish => {
                self.crypto = Some(
                    self.handshake
                        .take()
                        .ok_or_else(|| anyhow!("missing handshake"))?
                        .server_finish(bytes)?,
                );
                self.metadata.pin_code = self.crypto.as_ref().map(|c| c.pin.clone());
                self.phase = Phase::ConnectionResponse;
                self.transition(State::ReceivedUkeyClientFinish);
            }
            Phase::ServerInit => {
                let (crypto, finish) = self
                    .handshake
                    .take()
                    .ok_or_else(|| anyhow!("missing handshake"))?
                    .client_finish(bytes)?;
                self.crypto = Some(crypto);
                self.metadata.pin_code = self.crypto.as_ref().map(|c| c.pin.clone());
                self.socket.write_frame(finish).await?;
                self.plain(response()).await?;
                self.phase = Phase::ConnectionResponse;
                self.transition(State::SentUkeyClientFinish);
            }
            Phase::ConnectionResponse => {
                let frame = n::OfflineFrame::decode(bytes)?;
                let v1 = frame.v1.ok_or_else(|| anyhow!("missing response"))?;
                ensure!(
                    v1.r#type == Some(FrameType::ConnectionResponse.into()),
                    "expected connection response, received type={:?}/{:?}",
                    FrameType::try_from(v1.r#type.unwrap_or_default()).ok(),
                    v1.r#type
                );
                let response_body = v1
                    .connection_response
                    .ok_or_else(|| anyhow!("missing response body"))?;
                ensure!(
                    response_body.response()
                        == n::connection_response_frame::ResponseStatus::Accept
                        || (response_body.response.is_none() && response_body.status == Some(0)),
                    "connection rejected"
                );
                self.remote_disconnect_version = response_body.safe_to_disconnect_version();
                if !self.outbound {
                    self.plain(response()).await?;
                }
                self.paired_encryption().await?;
                self.phase = Phase::PairedEncryption;
                self.transition(State::SentPairedKeyEncryption);
            }
            _ => {
                let decrypted = self
                    .crypto
                    .as_mut()
                    .ok_or_else(|| anyhow!("missing secure context"))?
                    .open(bytes)?;
                if self.pending_lan {
                    // Public introduction routed the socket; only a valid
                    // continued UKEY2 record authenticates its actual peer.
                    self.pending_lan = false;
                    self.transport = "lan".into();
                    self.restore_state();
                }
                let frame = n::OfflineFrame::decode(decrypted.as_slice())?;
                ensure!(
                    frame.version == Some(n::offline_frame::Version::V1.into()),
                    "unsupported offline version={:?}",
                    frame.version
                );
                let v1 = frame.v1.ok_or_else(|| anyhow!("missing offline v1"))?;
                let raw_type = v1.r#type;
                let kind = raw_type.and_then(|value| FrameType::try_from(value).ok());
                let result = self.process_offline(v1).await;
                result.map_err(|error| {
                    anyhow!("offline frame rejected: type={kind:?}/{raw_type:?}: {error}")
                })?;
            }
        }
        Ok(())
    }
    async fn process_offline(&mut self, v1: n::V1Frame) -> Result<()> {
        let bodies = [
            v1.connection_request.is_some(),
            v1.connection_response.is_some(),
            v1.payload_transfer.is_some(),
            v1.bandwidth_upgrade_negotiation.is_some(),
            v1.keep_alive.is_some(),
            v1.disconnection.is_some(),
            v1.paired_key_encryption.is_some(),
            v1.bandwidth_upgrade_retry.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        ensure!(
            v1.r#type.is_some() && bodies == 1,
            "missing or multiple offline bodies"
        );
        match v1.r#type() {
            FrameType::PayloadTransfer => {
                self.receive_payload(
                    v1.payload_transfer
                        .ok_or_else(|| anyhow!("missing payload"))?,
                )
                .await?;
            }
            FrameType::KeepAlive => {
                if !v1
                    .keep_alive
                    .ok_or_else(|| anyhow!("missing keepalive"))?
                    .ack()
                {
                    self.encrypted(offline(n::V1Frame {
                        r#type: Some(FrameType::KeepAlive.into()),
                        keep_alive: Some(n::KeepAliveFrame { ack: Some(true) }),
                        ..Default::default()
                    }))
                    .await?;
                }
            }
            FrameType::BandwidthUpgradeNegotiation => {
                self.process_upgrade(
                    v1.bandwidth_upgrade_negotiation
                        .ok_or_else(|| anyhow!("missing BWU"))?,
                )
                .await?;
            }
            FrameType::BandwidthUpgradeRetry => {
                let mut retry = v1
                    .bandwidth_upgrade_retry
                    .ok_or_else(|| anyhow!("missing upgrade capability exchange"))?;
                ensure!(retry.supported_medium.len() <= 64, "too many upgrade media");
                let request = retry.is_request();
                if !request || !retry.supported_medium.is_empty() {
                    retry.supported_medium.retain(|medium| {
                        n::bandwidth_upgrade_retry_frame::Medium::try_from(*medium).is_ok_and(
                            |medium| {
                                medium != n::bandwidth_upgrade_retry_frame::Medium::UnknownMedium
                            },
                        )
                    });
                    self.remote_mediums = retry.supported_medium;
                }
                if request {
                    let mut supported_medium = Vec::new();
                    if self.lan {
                        supported_medium.push(Medium::WifiLan as i32);
                    }
                    if self.hotspot {
                        supported_medium.push(Medium::WifiHotspot as i32);
                    }
                    // Capabilities are not permission to upgrade. Actual routes
                    // still require local PIN consent and network authorization.
                    self.encrypted(offline(n::V1Frame {
                        r#type: Some(FrameType::BandwidthUpgradeRetry.into()),
                        bandwidth_upgrade_retry: Some(n::BandwidthUpgradeRetryFrame {
                            supported_medium,
                            is_request: Some(false),
                        }),
                        ..Default::default()
                    }))
                    .await?;
                }
            }
            FrameType::Disconnection => {
                let closing = v1
                    .disconnection
                    .ok_or_else(|| anyhow!("missing disconnection body"))?;
                let sent_all = self.outbound && self.files.values().all(|f| f.last_sent);
                ensure!(
                    sent_all || self.phase == Phase::Closing,
                    "peer disconnected before transfer confirmation"
                );
                if closing.request_safe_to_disconnect() {
                    self.encrypted(offline(n::V1Frame {
                        r#type: Some(FrameType::Disconnection.into()),
                        disconnection: Some(n::DisconnectionFrame {
                            request_safe_to_disconnect: Some(false),
                            ack_safe_to_disconnect: Some(true),
                        }),
                        ..Default::default()
                    }))
                    .await?;
                }
                self.state = State::Finished;
                self.phase = Phase::Done;
            }
            _ => bail!("unexpected encrypted frame type"),
        }
        Ok(())
    }
    async fn ack(&mut self, header: PayloadHeader, _offset: i64) -> Result<()> {
        // Modern Google ACK packets contain only a payload ID. Older peers
        // confirm a finished send with graceful encrypted disconnection.
        if self.remote_disconnect_version < 6 {
            return Ok(());
        }
        self.encrypted(offline(n::V1Frame {
            r#type: Some(FrameType::PayloadTransfer.into()),
            payload_transfer: Some(n::PayloadTransferFrame {
                packet_type: Some(PacketType::PayloadAck.into()),
                payload_header: Some(PayloadHeader {
                    id: header.id,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
    }
    async fn receive_payload(&mut self, transfer: n::PayloadTransferFrame) -> Result<()> {
        let phase = self.phase;
        let packet = transfer.packet_type;
        let payload = transfer
            .payload_header
            .as_ref()
            .and_then(|header| header.r#type);
        let event = transfer
            .control_message
            .as_ref()
            .and_then(|control| control.event);
        self.receive_payload_frame(transfer).await.map_err(|error| {
            anyhow!(
                "payload frame rejected: phase={phase:?} packet={:?}/{packet:?} payload={:?}/{payload:?} control={:?}/{event:?}: {error}",
                packet.and_then(|value| PacketType::try_from(value).ok()),
                payload.and_then(|value| PayloadType::try_from(value).ok()),
                event.and_then(|value| ControlEvent::try_from(value).ok())
            )
        })
    }
    async fn receive_payload_frame(&mut self, transfer: n::PayloadTransferFrame) -> Result<()> {
        let packet = PacketType::try_from(
            transfer
                .packet_type
                .ok_or_else(|| anyhow!("missing payload packet type"))?,
        )
        .map_err(|_| anyhow!("unknown payload packet type"))?;
        ensure!(
            match packet {
                PacketType::Data =>
                    transfer.payload_chunk.is_some() && transfer.control_message.is_none(),
                PacketType::Control =>
                    transfer.control_message.is_some() && transfer.payload_chunk.is_none(),
                PacketType::PayloadAck =>
                    transfer.control_message.is_none() && transfer.payload_chunk.is_none(),
                _ => false,
            },
            "missing, mismatched or multiple payload packet bodies"
        );
        if transfer.packet_type() == PacketType::PayloadAck {
            let header = transfer
                .payload_header
                .ok_or_else(|| anyhow!("missing acknowledgment header"))?;
            ensure!(header.id.is_some(), "missing acknowledgment id");
            // Nearby acknowledges BYTES controls too, not only offered files.
            // Transport receipt does not mean sharing ACCEPT or PIN consent.
            if self.sent_control.remove(&header.id()).is_some() {
                return Ok(());
            }
            let file = self
                .files
                .get_mut(&header.id())
                .ok_or_else(|| anyhow!("unknown file acknowledgment"))?;
            ensure!(file.last_sent, "premature file acknowledgment");
            file.acknowledged = true;
            if self.files.values().all(|f| f.acknowledged) {
                self.state = State::Finished;
                self.phase = Phase::Done;
            }
            return Ok(());
        }
        let packet_type = transfer.packet_type();
        let header = transfer
            .payload_header
            .ok_or_else(|| anyhow!("missing payload header"))?;
        ensure!(
            header.id.is_some() && header.total_size.is_some() && header.total_size() >= 0,
            "invalid payload id/size"
        );
        if packet_type == PacketType::Control {
            let control = transfer
                .control_message
                .ok_or_else(|| anyhow!("missing control packet"))?;
            ensure!(
                control.event.is_some()
                    && control
                        .offset
                        .is_some_and(|offset| { offset >= 0 && offset <= header.total_size() }),
                "invalid payload control event/offset"
            );
            match control.event() {
                ControlEvent::PayloadReceivedAck => {
                    if let Some(size) = self.sent_control.get(&header.id()).copied() {
                        ensure!(
                            control.offset == Some(size) && header.total_size() == size,
                            "invalid control payload acknowledgment"
                        );
                        self.sent_control.remove(&header.id());
                        return Ok(());
                    }
                    if let Some(file) = self.files.get_mut(&header.id()) {
                        ensure!(
                            file.last_sent
                                && control.offset == Some(file.metadata.size())
                                && header.total_size() == file.metadata.size(),
                            "invalid file acknowledgment"
                        );
                        file.acknowledged = true;
                        if self.files.values().all(|f| f.acknowledged) {
                            self.state = State::Finished;
                            self.phase = Phase::Done;
                        }
                    } else {
                        bail!("unknown payload acknowledgment");
                    }
                }
                ControlEvent::PayloadCanceled => {
                    self.state = State::Cancelled;
                    self.phase = Phase::Done;
                }
                ControlEvent::PayloadError => bail!("peer reported payload error"),
                _ => bail!("unknown payload control event"),
            }
            return Ok(());
        }
        ensure!(
            packet_type == PacketType::Data,
            "invalid payload packet type"
        );
        let chunk = transfer
            .payload_chunk
            .ok_or_else(|| anyhow!("missing payload chunk"))?;
        ensure!(
            chunk.offset.is_some()
                && chunk.offset() >= 0
                && chunk.flags.is_some()
                && chunk.flags() & !1 == 0
                && (chunk.body.is_some() || chunk.flags() & 1 != 0),
            "invalid payload chunk offset/flags/body"
        );
        match header.r#type() {
            PayloadType::Bytes => {
                ensure!(
                    header.total_size() <= CONTROL_LIMIT as i64,
                    "control payload exceeds limit"
                );
                let aggregate: usize = self.bytes.values().map(|p| p.bytes.len()).sum();
                ensure!(
                    aggregate
                        .checked_add(chunk.body().len())
                        .is_some_and(|n| n <= CONTROL_LIMIT),
                    "aggregate control memory exceeds limit"
                );
                ensure!(
                    self.bytes.contains_key(&header.id()) || self.bytes.len() < 8,
                    "too many control payloads"
                );
                let buffer = self
                    .bytes
                    .entry(header.id())
                    .or_insert_with(|| BytesPayload {
                        size: header.total_size(),
                        bytes: Vec::new(),
                    });
                ensure!(
                    buffer.size == header.total_size()
                        && chunk.offset() == buffer.bytes.len() as i64,
                    "invalid control payload size/offset"
                );
                ensure!(
                    buffer.bytes.len() + chunk.body().len() <= buffer.size as usize,
                    "control payload exceeds declared size"
                );
                buffer.bytes.extend_from_slice(chunk.body());
                if chunk.flags() & 1 != 0 {
                    let buffer = self
                        .bytes
                        .remove(&header.id())
                        .ok_or_else(|| anyhow!("missing control buffer"))?;
                    ensure!(
                        buffer.bytes.len() == buffer.size as usize,
                        "incomplete control payload"
                    );
                    self.receive_share(q::Frame::decode(buffer.bytes.as_slice())?)
                        .await?;
                    self.ack(header.clone(), header.total_size()).await?;
                }
            }
            PayloadType::File => {
                ensure!(
                    !self.outbound && self.locally_verified && self.phase == Phase::Transfer,
                    "file data before user consent"
                );
                let complete = self
                    .incoming
                    .as_mut()
                    .ok_or_else(|| anyhow!("no consented incoming files"))?
                    .write(
                        header.id(),
                        header.total_size(),
                        chunk.offset(),
                        chunk.body(),
                        chunk.flags() & 1 != 0,
                    )?;
                self.metadata.ack_bytes += chunk.body().len() as u64;
                if !chunk.body().is_empty() {
                    self.last_bytes = Instant::now();
                }
                self.progress();
                if complete {
                    let incoming = self
                        .incoming
                        .as_mut()
                        .ok_or_else(|| anyhow!("missing incoming storage"))?;
                    self.metadata.files = Some(incoming.publish()?);
                    self.metadata.destination =
                        Some(incoming.destination().to_string_lossy().into_owned());
                    self.phase = Phase::Closing;
                    self.close_deadline = Some(Instant::now() + Duration::from_secs(5));
                }
                if chunk.flags() & 1 != 0 {
                    self.ack(header.clone(), header.total_size()).await?;
                }
                if complete {
                    self.encrypted(offline(n::V1Frame {
                        r#type: Some(FrameType::Disconnection.into()),
                        disconnection: Some(n::DisconnectionFrame {
                            request_safe_to_disconnect: Some(self.remote_disconnect_version >= 1),
                            ack_safe_to_disconnect: Some(false),
                        }),
                        ..Default::default()
                    }))
                    .await?;
                }
            }
            _ => bail!("unsupported payload type"),
        }
        Ok(())
    }
    fn progress(&mut self) {
        if self.last_progress.elapsed() >= Duration::from_millis(100) {
            self.last_progress = Instant::now();
            self.event(None);
        }
    }
    async fn receive_share(&mut self, frame: q::Frame) -> Result<()> {
        let phase = self.phase;
        let version = frame.version;
        let raw_type = frame.v1.as_ref().and_then(|v1| v1.r#type);
        let kind = raw_type.and_then(|value| ShareType::try_from(value).ok());
        self.receive_share_frame(frame).await.map_err(|error| {
            anyhow!(
                "sharing frame rejected: direction={} phase={phase:?} version={version:?} type={kind:?}/{raw_type:?}: {error}",
                if self.outbound { "outbound" } else { "inbound" }
            )
        })
    }
    async fn receive_share_frame(&mut self, frame: q::Frame) -> Result<()> {
        ensure!(
            frame.version == Some(q::frame::Version::V1.into()),
            "unsupported sharing version"
        );
        let v1 = frame.v1.ok_or_else(|| anyhow!("missing sharing frame"))?;
        let kind = ShareType::try_from(v1.r#type.ok_or_else(|| anyhow!("missing sharing type"))?)
            .map_err(|_| anyhow!("unsupported sharing frame type"))?;
        let bodies = [
            v1.introduction.is_some(),
            v1.connection_response.is_some(),
            v1.paired_key_encryption.is_some(),
            v1.paired_key_result.is_some(),
            v1.certificate_info.is_some(),
            v1.progress_update.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        let matching_body = match kind {
            ShareType::Introduction => v1.introduction.is_some(),
            ShareType::Response => v1.connection_response.is_some(),
            ShareType::PairedKeyEncryption => v1.paired_key_encryption.is_some(),
            ShareType::PairedKeyResult => v1.paired_key_result.is_some(),
            ShareType::CertificateInfo => v1.certificate_info.is_some(),
            ShareType::ProgressUpdate => v1.progress_update.is_some(),
            ShareType::Cancel => bodies == 0,
            _ => bail!("unsupported sharing frame type"),
        };
        ensure!(
            matching_body && bodies == usize::from(kind != ShareType::Cancel),
            "missing, mismatched or multiple sharing bodies"
        );
        ensure!(
            matches!(
                self.phase,
                Phase::PairedEncryption
                    | Phase::PairedResult
                    | Phase::Introduction
                    | Phase::Consent
                    | Phase::Response
                    | Phase::Transfer
                    | Phase::Acknowledgments
                    | Phase::Closing
            ),
            "sharing control outside secure session"
        );
        if kind == ShareType::Cancel {
            self.pending_introduction.take();
            self.pending_paired_result.take();
            self.state = State::Cancelled;
            self.phase = Phase::Done;
            return Ok(());
        }
        if matches!(kind, ShareType::CertificateInfo | ShareType::ProgressUpdate) {
            // Google IncomingFramesReader caches ancillary frames during typed
            // paired-key reads; its incoming session consumer discards them.
            // We have no contact certificate store, and deprecated progress
            // (including start_transfer) grants neither consent nor identity.
            if let Some(progress) = &v1.progress_update {
                ensure!(
                    progress.progress.is_none_or(f32::is_finite),
                    "non-finite ancillary progress"
                );
            }
            return Ok(());
        }
        if let Some(response) = &v1.connection_response {
            let status = q::connection_response_frame::Status::try_from(
                response
                    .status
                    .ok_or_else(|| anyhow!("missing transfer response status"))?,
            )
            .map_err(|_| anyhow!("invalid transfer response status"))?;
            ensure!(
                status != q::connection_response_frame::Status::Unknown,
                "unknown transfer response status"
            );
            // A negative response/cancel can cross another control in flight.
            // Only ACCEPT is phase- and direction-restricted below.
            if status != q::connection_response_frame::Status::Accept {
                self.pending_introduction.take();
                self.pending_paired_result.take();
                self.state = State::Rejected;
                self.phase = Phase::Done;
                return Ok(());
            }
        }
        match (self.phase, kind) {
            (Phase::PairedEncryption, ShareType::PairedKeyResult) => {
                let result = v1.paired_key_result.expect("validated sharing body");
                Self::validate_paired_result(&result)?;
                ensure!(
                    self.pending_paired_result.is_none(),
                    "duplicate paired key result"
                );
                self.pending_paired_result = Some(result);
            }
            (Phase::PairedEncryption | Phase::PairedResult, ShareType::Introduction)
                if !self.outbound =>
            {
                // Nearby BYTES payload completion can precede another payload.
                // Keep one metadata offer, but require BOTH paired-key frames
                // before exposing it for local consent.
                ensure!(
                    self.pending_introduction.is_none(),
                    "duplicate introduction"
                );
                let intro = v1.introduction.expect("validated sharing body");
                if !intro.file_metadata.is_empty() {
                    IncomingFiles::validate(&intro.file_metadata)?;
                }
                self.pending_introduction = Some(intro);
            }
            (Phase::PairedEncryption, ShareType::PairedKeyEncryption) => {
                let encryption = v1.paired_key_encryption.expect("validated sharing body");
                ensure!(
                    encryption
                        .signed_data
                        .as_ref()
                        .is_some_and(|data| !data.is_empty())
                        && encryption
                            .secret_id_hash
                            .as_ref()
                            .is_some_and(|hash| !hash.is_empty()),
                    "missing paired key signature or hash"
                );
                self.share(sharing(q::V1Frame {
                    r#type: Some(ShareType::PairedKeyResult.into()),
                    paired_key_result: Some(q::PairedKeyResultFrame {
                        status: Some(q::paired_key_result_frame::Status::Unable.into()),
                    }),
                    ..Default::default()
                }))
                .await?;
                self.phase = Phase::PairedResult;
                self.transition(State::SentPairedKeyResult);
                if let Some(result) = self.pending_paired_result.take() {
                    self.finish_pairing(result).await?;
                }
            }
            (Phase::PairedResult, ShareType::PairedKeyResult) => {
                self.finish_pairing(v1.paired_key_result.expect("validated sharing body"))
                    .await?;
            }
            (Phase::Introduction, ShareType::Introduction) if !self.outbound => {
                self.receive_introduction(v1.introduction.expect("validated sharing body"))
                    .await?;
            }
            (Phase::Response | Phase::Consent, ShareType::Response) if self.outbound => {
                ensure!(!self.remote_accepted, "duplicate transfer acceptance");
                self.remote_accepted = true;
                if self.locally_verified {
                    self.enter(Phase::Transfer);
                    self.transition(State::SendingFiles);
                }
            }
            (_, ShareType::Response) if !self.outbound => {
                // Google's incoming consumer discards ACCEPT from the sender.
                // Native Android also emits it before introduction. It grants
                // neither receiver consent nor identity, storage or more time.
            }
            _ => bail!("unexpected sharing frame for session phase"),
        }
        Ok(())
    }
    fn validate_paired_result(result: &q::PairedKeyResultFrame) -> Result<()> {
        ensure!(
            matches!(
                result.status,
                Some(status) if status == q::paired_key_result_frame::Status::Unable as i32
                    || status == q::paired_key_result_frame::Status::Success as i32
            ),
            "peer rejected or sent invalid paired key result status={:?}",
            result.status
        );
        Ok(())
    }
    async fn finish_pairing(&mut self, result: q::PairedKeyResultFrame) -> Result<()> {
        Self::validate_paired_result(&result)?;
        // Metadata can reach native PIN UI before local confirmation. Neither
        // remote ACCEPT nor identifiers authorize bytes/hotspot without consent.
        if self.outbound {
            self.share(sharing(q::V1Frame {
                r#type: Some(ShareType::Introduction.into()),
                introduction: Some(q::IntroductionFrame {
                    file_metadata: self.files.values().map(|f| f.metadata.clone()).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await?;
            self.enter(Phase::Consent);
            self.transition(State::WaitingForUserConsent);
        } else {
            self.enter(Phase::Introduction);
            self.transition(State::ReceivedPairedKeyResult);
            if let Some(intro) = self.pending_introduction.take() {
                self.receive_introduction(intro).await.map_err(|error| {
                    anyhow!("queued introduction rejected: phase=Introduction type=Introduction/1: {error}")
                })?;
            }
        }
        Ok(())
    }
    async fn receive_introduction(&mut self, intro: q::IntroductionFrame) -> Result<()> {
        if !intro.text_metadata.is_empty()
            || !intro.wifi_credentials_metadata.is_empty()
            || !intro.app_metadata.is_empty()
            || !intro.stream_metadata.is_empty()
            || intro.continuation()
            || intro.expect_more()
            || intro.file_metadata.is_empty()
        {
            self.share_response(q::connection_response_frame::Status::UnsupportedAttachmentType)
                .await?;
            self.state = State::Rejected;
            self.phase = Phase::Done;
            return Ok(());
        }
        // Preview IDs are advisory on the Google desktop receiver. They create
        // no storage, consent or authorized payload IDs here.
        // prost rejects invalid UTF-8 strings before this point; storage rejects
        // separators, controls, traversal, duplicate IDs and invalid sizes.
        self.metadata.total_bytes = IncomingFiles::validate(&intro.file_metadata)?;
        self.metadata.files = Some(
            intro
                .file_metadata
                .iter()
                .map(|f| f.name().to_owned())
                .collect(),
        );
        self.incoming_metadata = intro.file_metadata;
        self.enter(Phase::Consent);
        self.transition(State::WaitingForUserConsent);
        Ok(())
    }
    async fn share_response(&mut self, status: q::connection_response_frame::Status) -> Result<()> {
        self.share(sharing(q::V1Frame {
            r#type: Some(ShareType::Response.into()),
            connection_response: Some(q::ConnectionResponseFrame {
                status: Some(status.into()),
            }),
            ..Default::default()
        }))
        .await
    }
    async fn accept(&mut self) -> Result<()> {
        self.checkpoint()?;
        self.locally_verified = true;
        if self.outbound {
            if self.remote_accepted {
                self.enter(Phase::Transfer);
                self.transition(State::SendingFiles);
            } else {
                self.enter(Phase::Response);
                self.transition(State::SentIntroduction);
            }
            return Ok(());
        }
        // Resolve preferences at consent, not while an unseen socket connects.
        let download = self.download.read().clone();
        self.incoming = Some(IncomingFiles::create(&download, &self.incoming_metadata)?);
        if self.transport == "bluetooth" && self.lan {
            match self.offer_lan().await {
                Ok(true) => {
                    self.pending_accept = true;
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    if self.cancellation.is_cancelled() || self.upgrade.is_some() {
                        return Err(error);
                    }
                    self.diagnostic(format!("LAN upgrade unavailable: {error}"));
                    // A healthy old channel remains usable; do not replace
                    // the user's existing Wi-Fi with an AP after LAN failure.
                    self.share_response(q::connection_response_frame::Status::Accept)
                        .await?;
                    self.enter(Phase::Transfer);
                    self.transition(State::ReceivingFiles);
                    return Ok(());
                }
            }
        }
        if self.transport == "bluetooth"
            && self.hotspot
            && self.remote_mediums.contains(&(Medium::WifiHotspot as i32))
            && self.can_hotspot().await
        {
            self.pending_accept = true;
            match self.offer_hotspot().await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if self.cancellation.is_cancelled()
                        || error.is::<crate::network::RestorationFailure>()
                    {
                        return Err(error);
                    }
                    self.diagnostic(format!("hotspot upgrade unavailable: {error}"));
                    self.fail_upgrade().await?;
                    return Ok(());
                }
            }
        }
        self.share_response(q::connection_response_frame::Status::Accept)
            .await?;
        self.enter(Phase::Transfer);
        self.transition(State::ReceivingFiles);
        Ok(())
    }
    async fn send_next(&mut self) -> Result<()> {
        ensure!(
            self.locally_verified && self.remote_accepted && !self.pending_lan,
            "file data before local PIN, remote acceptance, or LAN channel authentication"
        );
        let Some((&id, file)) = self.files.iter_mut().find(|(_, f)| !f.last_sent) else {
            self.enter(Phase::Acknowledgments);
            return Ok(());
        };
        let header = PayloadHeader {
            id: Some(id),
            r#type: Some(PayloadType::File.into()),
            total_size: Some(file.metadata.size()),
            file_name: file.metadata.name.clone(),
            ..Default::default()
        };
        let offset = file.offset;
        let left = file.metadata.size() - offset;
        let (body, last) = if left == 0 {
            file.last_sent = true;
            (vec![], true)
        } else {
            let mut bytes = vec![0; CHUNK.min(left as usize)];
            file.file.read_exact(&mut bytes)?;
            (bytes, false)
        };
        let bytes = body.len() as u64;
        self.pending_file = Some((id, bytes));
        self.payload(header, offset, body, last).await?;
        self.pending_file = None;
        self.files
            .get_mut(&id)
            .expect("sending file is retained")
            .offset += bytes as i64;
        self.metadata.ack_bytes += bytes;
        if bytes > 0 {
            self.last_bytes = Instant::now();
            self.last_activity = Instant::now();
        }
        self.progress();
        Ok(())
    }
    async fn offer_lan(&mut self) -> Result<bool> {
        self.checkpoint()?;
        ensure!(
            self.locally_verified && self.remote_endpoint.is_some(),
            "LAN upgrade before local PIN confirmation/endpoint hint"
        );
        if !self.lan || !self.remote_mediums.contains(&(Medium::WifiLan as i32)) {
            return Ok(false);
        }
        ensure!(
            self.upgrade.is_none()
                && self.lease.is_none()
                && !self.pending_lan
                && self.transport == "bluetooth",
            "upgrade already active"
        );
        let Some(host) = self.lan_upgrade_host.as_ref() else {
            return Ok(false);
        };
        let candidates = NetworkManager::lan_interfaces(&self.cancellation).await?;
        self.checkpoint()?;
        if candidates.is_empty() {
            return Ok(false);
        }
        let registration = host.register(
            self.remote_endpoint
                .as_deref()
                .ok_or_else(|| anyhow!("missing upgrade endpoint"))?,
        )?;
        let addresses: Vec<_> = candidates
            .iter()
            .map(|candidate| n::ServiceAddress {
                ip_address: Some(candidate.address.octets().to_vec()),
                port: Some(i32::from(host.port())),
            })
            .collect();
        let info = UpgradePathInfo {
            medium: Some(Medium::WifiLan as i32),
            wifi_lan_socket: Some(WifiLanSocket {
                ip_address: addresses[0].ip_address.clone(),
                wifi_port: addresses[0].port,
                address_candidates: addresses,
            }),
            supports_client_introduction_ack: Some(true),
            supports_disabling_encryption: Some(false),
            ..Default::default()
        };
        self.upgrade = Some(Upgrade {
            listener: Some(UpgradeListener::Lan(registration, candidates)),
            next: None,
            lease: None,
            info: info.clone(),
            deadline: Instant::now() + Duration::from_secs(30),
            last_sent: false,
            last_received: false,
        });
        self.transition(State::NegotiatingTransport);
        self.encrypted(bwu(BwuEvent::UpgradePathAvailable, Some(info)))
            .await?;
        Ok(true)
    }
    async fn offer_hotspot(&mut self) -> Result<()> {
        ensure!(
            self.locally_verified && self.remote_endpoint.is_some(),
            "hotspot requires local PIN consent and endpoint hint"
        );
        ensure!(
            self.upgrade.is_none() && self.lease.is_none() && self.transport == "bluetooth",
            "upgrade unavailable"
        );
        self.transition(State::NegotiatingTransport);
        // Install ownership immediately: bind/build/announce failures must close
        // the lease through run(), not an unawaited local Drop cleanup task.
        self.lease = Some(
            NetworkManager::create(self.consent.load(Ordering::Acquire), &self.cancellation)
                .await?,
        );
        self.checkpoint()?;
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| anyhow!("missing hotspot lease"))?;
        let socket = tokio::net::TcpSocket::new_v4()?;
        bind_interface(&socket, &lease.interface)?;
        socket.bind(SocketAddr::new(lease.gateway, 0))?;
        let listener = socket.listen(1)?;
        let port = listener.local_addr()?.port();
        let address = match lease.gateway {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(_) => bail!("hotspot must use IPv4"),
        };
        let info = UpgradePathInfo {
            medium: Some(Medium::WifiHotspot.into()),
            wifi_hotspot_credentials: Some(WifiHotspotCredentials {
                ssid: Some(lease.ssid.clone()),
                password: Some(lease.password.clone()),
                gateway: Some(lease.gateway.to_string()),
                port: Some(port as i32),
                frequency: Some(lease.frequency),
                address_candidates: vec![n::ServiceAddress {
                    ip_address: Some(address),
                    port: Some(port as i32),
                }],
            }),
            supports_client_introduction_ack: Some(true),
            supports_disabling_encryption: Some(false),
            ..Default::default()
        };
        self.upgrade = Some(Upgrade {
            listener: Some(UpgradeListener::Hotspot(listener)),
            next: None,
            lease: self.lease.take(),
            info: info.clone(),
            deadline: Instant::now() + Duration::from_secs(45),
            last_sent: false,
            last_received: false,
        });
        self.encrypted(bwu(BwuEvent::UpgradePathAvailable, Some(info)))
            .await
    }
    async fn accept_upgrade(
        &mut self,
        stream: TcpStream,
        introduction: Option<ClientIntroduction>,
    ) -> Result<()> {
        self.checkpoint()?;
        let upgrade = self
            .upgrade
            .as_ref()
            .ok_or_else(|| anyhow!("unexpected upgrade connection"))?;
        ensure!(upgrade.next.is_none(), "upgrade candidate already selected");
        if let Some(lease) = upgrade.lease.as_ref() {
            validate_hotspot_peer(
                &lease.interface,
                lease.gateway,
                stream.local_addr()?.ip(),
                stream.peer_addr()?.ip(),
            )?;
        } else {
            let Some(UpgradeListener::Lan(_, candidates)) = upgrade.listener.as_ref() else {
                bail!("upgrade has no owned LAN admission");
            };
            ensure!(upgrade.info.medium() == Medium::WifiLan, "unexpected LAN medium");
            let local = stream.local_addr()?.ip();
            let candidate = candidates
                .iter()
                .find(|candidate| local == IpAddr::V4(candidate.address))
                .ok_or_else(|| anyhow!("upgrade accepted on unadvertised LAN address"))?;
            let IpAddr::V4(peer) = stream.peer_addr()?.ip() else {
                bail!("LAN upgrade requires IPv4");
            };
            ensure!(
                shared_subnet(candidate.address, candidate.netmask, peer),
                "upgrade peer is outside advertised LAN subnet"
            );
            ensure!(
                NetworkManager::lan_interfaces(&self.cancellation)
                    .await?
                    .iter()
                    .any(|current| current == candidate),
                "advertised LAN interface is no longer active"
            );
            ensure!(
                !if_addrs::get_if_addrs()?.iter().any(|interface| {
                    matches!(&interface.addr, if_addrs::IfAddr::V4(address) if address.ip == peer)
                }),
                "LAN upgrade peer is this computer"
            );
            ensure!(
                introduction.is_some()
                    && self
                        .lan_upgrade_host
                        .as_ref()
                        .is_some_and(|host| stream.local_addr().is_ok_and(|address| address.port() == host.port())),
                "upgrade arrived outside the shared LAN listener"
            );
            bind_interface(&stream, &candidate.interface)?;
        }
        self.checkpoint()?;
        let mut next = FramedStream::new(Box::new(stream));
        let intro = if let Some(introduction) = introduction {
            introduction
        } else {
            let bytes = tokio::select! {
                _ = self.cancellation.cancelled() => bail!("transfer cancelled"),
                result = tokio::time::timeout(Duration::from_secs(5), next.read_frame()) => result??,
            };
            let frame = n::OfflineFrame::decode(bytes.as_slice())?;
            ensure!(
                frame.version == Some(n::offline_frame::Version::V1.into()),
                "invalid upgrade introduction version"
            );
            let v1 = frame
                .v1
                .ok_or_else(|| anyhow!("missing upgrade introduction"))?;
            ensure!(
                v1.r#type() == FrameType::BandwidthUpgradeNegotiation,
                "invalid upgrade introduction type"
            );
            let intro = v1
                .bandwidth_upgrade_negotiation
                .ok_or_else(|| anyhow!("missing upgrade introduction"))?;
            ensure!(
                intro.event_type() == BwuEvent::ClientIntroduction,
                "expected upgrade client introduction"
            );
            intro
                .client_introduction
                .ok_or_else(|| anyhow!("missing endpoint introduction"))?
        };
        ensure!(
            self.remote_endpoint.as_deref() == Some(intro.endpoint_id())
                && !intro.supports_disabling_encryption(),
            "upgrade endpoint mismatch/encryption downgrade"
        );
        let mut ack = bwu(BwuEvent::ClientIntroductionAck, None);
        ack.v1
            .as_mut()
            .and_then(|v| v.bandwidth_upgrade_negotiation.as_mut())
            .ok_or_else(|| anyhow!("missing ack frame"))?
            .client_introduction_ack = Some(ClientIntroductionAck {});
        tokio::select! {
            _ = self.cancellation.cancelled() => bail!("transfer cancelled"),
            result = tokio::time::timeout(Duration::from_secs(5), next.write_frame(ack.encode_to_vec())) => result??,
        }
        self.checkpoint()?;
        ensure!(
            self.upgrade
                .as_ref()
                .is_some_and(|u| Instant::now() < u.deadline),
            "upgrade introduction deadline expired"
        );
        let upgrade = self
            .upgrade
            .as_mut()
            .ok_or_else(|| anyhow!("unexpected upgrade connection"))?;
        upgrade.next = Some(next);
        if let Some(UpgradeListener::Lan(registration, _)) = upgrade.listener.as_mut() {
            registration.close_admission();
        }
        upgrade.listener.take();
        // Freeze one candidate. Native writes are paused until authenticated
        // old-channel drain finishes; the public introduction is not key proof.
        // New-channel UKEY2 input must still prove the selected socket's peer.
        Ok(())
    }
    async fn last_write(&mut self) -> Result<()> {
        ensure!(
            self.upgrade
                .as_ref()
                .is_some_and(|u| Instant::now() < u.deadline),
            "upgrade commit deadline expired"
        );
        self.encrypted(bwu(BwuEvent::LastWriteToPriorChannel, None))
            .await?;
        let upgrade = self
            .upgrade
            .as_mut()
            .ok_or_else(|| anyhow!("missing upgrade"))?;
        upgrade.last_sent = true;
        if upgrade.last_received {
            self.encrypted(bwu(BwuEvent::SafeToClosePriorChannel, None))
                .await?;
        }
        Ok(())
    }
    async fn process_upgrade(&mut self, frame: n::BandwidthUpgradeNegotiationFrame) -> Result<()> {
        match frame.event_type() {
            BwuEvent::UpgradePathAvailable => {
                let info = frame
                    .upgrade_path_info
                    .ok_or_else(|| anyhow!("missing upgrade path"))?;
                let supported = (info.medium() == Medium::WifiHotspot && self.hotspot)
                    || (info.medium() == Medium::WifiLan && self.lan);
                if !self.locally_verified
                    || self.upgrade.is_some()
                    || self.transport != "bluetooth"
                    || !supported
                {
                    self.encrypted(bwu(BwuEvent::UpgradeFailure, Some(info)))
                        .await?;
                    return Ok(());
                }
                self.transition(State::NegotiatingTransport);
                let result = self.join_upgrade(info.clone()).await;
                if let Err(error) = result {
                    if self.cancellation.is_cancelled()
                        || error.is::<crate::network::RestorationFailure>()
                    {
                        return Err(error);
                    }
                    if let Some(lease) = self.lease.as_mut() {
                        lease.close().await?;
                    }
                    self.lease.take();
                    self.diagnostic(format!("transport upgrade unavailable: {error}"));
                    self.encrypted(bwu(BwuEvent::UpgradeFailure, Some(info)))
                        .await?;
                    self.restore_state();
                }
            }
            BwuEvent::LastWriteToPriorChannel => {
                let upgrade = self
                    .upgrade
                    .as_mut()
                    .ok_or_else(|| anyhow!("unexpected last-write"))?;
                ensure!(
                    upgrade.next.is_some() && !upgrade.last_received,
                    "last-write without acknowledged replacement"
                );
                upgrade.last_received = true;
                if upgrade.last_sent {
                    self.encrypted(bwu(BwuEvent::SafeToClosePriorChannel, None))
                        .await?;
                } else {
                    self.last_write().await?;
                }
            }
            BwuEvent::SafeToClosePriorChannel => {
                let upgrade = self
                    .upgrade
                    .as_ref()
                    .ok_or_else(|| anyhow!("unexpected safe-to-close"))?;
                ensure!(
                    upgrade.last_sent && upgrade.last_received && upgrade.next.is_some(),
                    "premature safe-to-close"
                );
                // Keep the lease attached to Session across every fallible await.
                self.plain(disconnect()).await?;
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), self.socket.read_frame()).await;
                self.checkpoint()?;
                let mut upgrade = self
                    .upgrade
                    .take()
                    .ok_or_else(|| anyhow!("missing upgrade"))?;
                let mut next = upgrade
                    .next
                    .take()
                    .ok_or_else(|| anyhow!("missing upgraded channel"))?;
                std::mem::swap(&mut self.socket, &mut next);
                self.lease = upgrade.lease.take();
                self.pending_lan = upgrade.info.medium() == Medium::WifiLan;
                if !self.pending_lan {
                    self.transport = "hotspot".into();
                }
                for bytes in std::mem::take(&mut self.deferred) {
                    let encrypted = self
                        .crypto
                        .as_mut()
                        .ok_or_else(|| anyhow!("missing secure context"))?
                        .seal(bytes)?;
                    self.socket.write_frame(encrypted).await?;
                }
                if self.pending_accept {
                    self.pending_accept = false;
                    self.share_response(q::connection_response_frame::Status::Accept)
                        .await?;
                    self.enter(Phase::Transfer);
                }
                self.restore_state();
            }
            BwuEvent::UpgradeFailure => self.fail_upgrade().await?,
            BwuEvent::UpgradePathRequest => {
                if self.locally_verified
                    && self.transport == "bluetooth"
                    && self.hotspot
                    && self.upgrade.is_none()
                {
                    if let Err(error) = self.offer_hotspot().await {
                        if self.cancellation.is_cancelled()
                            || error.is::<crate::network::RestorationFailure>()
                        {
                            return Err(error);
                        }
                        if let Some(lease) = self.lease.as_mut() {
                            lease.close().await?;
                        }
                        self.lease.take();
                        self.encrypted(bwu(BwuEvent::UpgradeFailure, frame.upgrade_path_info))
                            .await?;
                        self.restore_state();
                    }
                } else {
                    self.encrypted(bwu(BwuEvent::UpgradeFailure, frame.upgrade_path_info))
                        .await?;
                }
            }
            _ => bail!("unexpected BWU event on encrypted session"),
        }
        Ok(())
    }
    fn restore_state(&mut self) {
        if self.pending_lan {
            self.transition(State::NegotiatingTransport);
            return;
        }
        let state = match self.phase {
            Phase::Transfer | Phase::Acknowledgments => {
                if self.outbound {
                    State::SendingFiles
                } else {
                    State::ReceivingFiles
                }
            }
            Phase::Consent => State::WaitingForUserConsent,
            Phase::Response => State::SentIntroduction,
            _ => State::SentPairedKeyResult,
        };
        self.transition(state);
    }
    async fn fail_upgrade(&mut self) -> Result<()> {
        ensure!(
            !self.pending_lan,
            "LAN channel failed after old-channel drain; restart required"
        );
        if let Some(upgrade) = self.upgrade.as_ref() {
            ensure!(
                !upgrade.last_sent && !upgrade.last_received,
                "upgrade failed after old-channel last-write; restart required"
            );
            let info = upgrade.info.clone();
            self.encrypted(bwu(BwuEvent::UpgradeFailure, Some(info)))
                .await?;
            if let Some(lease) = self.upgrade.as_mut().and_then(|u| u.lease.as_mut()) {
                lease.close().await?;
            }
            self.upgrade.take();
        }
        if let Some(lease) = self.lease.as_mut() {
            lease.close().await?;
        }
        self.lease.take();
        if self.pending_accept {
            self.pending_accept = false;
            self.share_response(q::connection_response_frame::Status::Accept)
                .await?;
            self.enter(Phase::Transfer);
        }
        self.restore_state();
        Ok(())
    }
    async fn join_upgrade(&mut self, info: UpgradePathInfo) -> Result<()> {
        ensure!(
            self.locally_verified && self.lease.is_none(),
            "upgrade before local PIN consent"
        );
        let candidates = match info.medium() {
            Medium::WifiHotspot => {
                let credentials = info
                    .wifi_hotspot_credentials
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing hotspot credentials"))?;
                let candidates = if credentials.address_candidates.is_empty() {
                    vec![SocketAddr::new(
                        credentials.gateway().parse()?,
                        u16::try_from(credentials.port())?,
                    )]
                } else {
                    service_addresses(&credentials.address_candidates)?
                };
                self.lease = Some(
                    NetworkManager::join(
                        credentials.ssid(),
                        credentials.password(),
                        credentials.frequency(),
                        self.consent.load(Ordering::Acquire),
                        &self.cancellation,
                    )
                    .await?,
                );
                candidates
            }
            Medium::WifiLan => {
                let credentials = info
                    .wifi_lan_socket
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing LAN credentials"))?;
                let candidates = if credentials.address_candidates.is_empty() {
                    service_addresses(&[n::ServiceAddress {
                        ip_address: credentials.ip_address.clone(),
                        port: credentials.wifi_port,
                    }])?
                } else {
                    service_addresses(&credentials.address_candidates)?
                };
                candidates
            }
            _ => bail!("unsupported upgrade medium"),
        };
        ensure!(
            !candidates.is_empty() && candidates.len() <= 8,
            "invalid address candidates"
        );
        let lan_interfaces = if self.lease.is_none() {
            NetworkManager::lan_interfaces(&self.cancellation).await?
        } else {
            Vec::new()
        };
        let mut connected = None;
        for candidate in candidates {
            self.checkpoint()?;
            ensure!(
                !candidate.ip().is_unspecified()
                    && !candidate.ip().is_loopback()
                    && !candidate.ip().is_multicast()
                    && candidate.port() != 0,
                "invalid upgrade destination"
            );
            let socket = match candidate {
                SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
                SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
            };
            if let Some(lease) = &self.lease {
                ensure!(
                    candidate.ip() == lease.gateway,
                    "hotspot destination is not the negotiated interface gateway"
                );
                bind_interface(&socket, &lease.interface)?;
            } else {
                let remote = match candidate.ip() {
                    IpAddr::V4(ip) => ip,
                    IpAddr::V6(_) => continue,
                };
                let Some(interface) = lan_interfaces.iter().find(|interface| {
                    shared_subnet(interface.address, interface.netmask, remote)
                }) else {
                    continue;
                };
                bind_interface(&socket, &interface.interface)?;
                socket.bind(SocketAddr::new(IpAddr::V4(interface.address), 0))?;
            }
            if let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_secs(5), socket.connect(candidate)).await
            {
                connected = Some(stream);
                break;
            }
        }
        let mut next = FramedStream::new(Box::new(
            connected.ok_or_else(|| anyhow!("no reachable upgrade address"))?,
        ));
        self.checkpoint()?;
        let mut introduction = bwu(BwuEvent::ClientIntroduction, None);
        introduction
            .v1
            .as_mut()
            .and_then(|v| v.bandwidth_upgrade_negotiation.as_mut())
            .ok_or_else(|| anyhow!("missing intro"))?
            .client_introduction = Some(ClientIntroduction {
            endpoint_id: Some(self.endpoint.clone()),
            supports_disabling_encryption: Some(false),
        });
        next.write_frame(introduction.encode_to_vec()).await?;
        if info.supports_client_introduction_ack() {
            let bytes = tokio::time::timeout(Duration::from_secs(5), next.read_frame()).await??;
            let ack = n::OfflineFrame::decode(bytes.as_slice())?;
            ensure!(
                ack.v1
                    .and_then(|v| v.bandwidth_upgrade_negotiation)
                    .is_some_and(|v| v.event_type() == BwuEvent::ClientIntroductionAck),
                "missing upgrade introduction ACK"
            );
        }
        self.checkpoint()?;
        self.upgrade = Some(Upgrade {
            listener: None,
            next: Some(next),
            lease: self.lease.take(),
            info,
            deadline: Instant::now() + Duration::from_secs(30),
            last_sent: false,
            last_received: false,
        });
        self.last_write().await
    }
}
fn bind_interface(socket: &impl std::os::fd::AsRawFd, interface: &str) -> Result<()> {
    let interface = std::ffi::CString::new(interface)?;
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            interface.as_ptr().cast(),
            interface.as_bytes_with_nul().len() as libc::socklen_t,
        )
    };
    ensure!(
        result == 0,
        "cannot bind upgrade to expected interface: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn shared_subnet(
    local: std::net::Ipv4Addr,
    mask: std::net::Ipv4Addr,
    remote: std::net::Ipv4Addr,
) -> bool {
    let mask = u32::from(mask);
    let host = !mask;
    let peer = u32::from(remote);
    !local.is_loopback()
        && !remote.is_loopback()
        && !remote.is_unspecified()
        && !remote.is_multicast()
        && local != remote
        && host > 1
        && peer & mask == u32::from(local) & mask
        && peer & host != 0
        && peer & host != host
}
fn validate_hotspot_peer(
    interface: &str,
    gateway: IpAddr,
    local: IpAddr,
    peer: IpAddr,
) -> Result<()> {
    ensure!(
        local == gateway,
        "upgrade accepted on unexpected local address"
    );
    let (gateway, peer) = match (gateway, peer) {
        (IpAddr::V4(gateway), IpAddr::V4(peer)) => (gateway, peer),
        _ => bail!("hotspot requires IPv4"),
    };
    ensure!(
        if_addrs::get_if_addrs()?
            .into_iter()
            .any(|address| address.name == interface
                && match address.addr {
                    if_addrs::IfAddr::V4(address) =>
                        address.ip == gateway && shared_subnet(gateway, address.netmask, peer),
                    _ => false,
                }),
        "upgrade peer is outside negotiated hotspot interface/subnet"
    );
    Ok(())
}
fn service_addresses(addresses: &[n::ServiceAddress]) -> Result<Vec<SocketAddr>> {
    ensure!(addresses.len() <= 8, "too many service addresses");
    addresses
        .iter()
        .map(|address| {
            let ip = match address.ip_address().len() {
                4 => IpAddr::V4(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(
                    address.ip_address(),
                )?)),
                16 => IpAddr::V6(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(
                    address.ip_address(),
                )?)),
                _ => bail!("invalid service address length"),
            };
            Ok(SocketAddr::new(ip, u16::try_from(address.port())?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Crypto, Crypto) {
        let (client, init) = Handshake::client().unwrap();
        let (server, reply) = Handshake::server(&init).unwrap();
        let (client, finish) = client.client_finish(&reply).unwrap();
        (client, server.server_finish(&finish).unwrap())
    }
    fn session() -> (Session, FramedStream) {
        session_with_outgoing(None)
    }
    fn session_with_outgoing(outgoing: Option<OutboundPayload>) -> (Session, FramedStream) {
        let (local, remote) = tokio::io::duplex(128 * 1024);
        let (sender, _) = broadcast::channel(32);
        let session = Session::new(
            "test".into(),
            Box::new(local),
            "bluetooth",
            *b"LOCL",
            "local".into(),
            sender,
            Arc::new(RwLock::new(PathBuf::from("/tmp"))),
            true,
            false,
            Arc::new(AtomicBool::new(false)),
            outgoing,
            Some("PEER".into()),
            None,
        )
        .unwrap();
        (session, FramedStream::new(Box::new(remote)))
    }
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "quickshare-pinned-session-{}", uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn sending(selected: crate::OutboundFile) -> (Session, FramedStream, Crypto) {
        let (mut session, socket) =
            session_with_outgoing(Some(OutboundPayload::Files(vec![selected])));
        let (crypto, peer) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::Transfer);
        session.locally_verified = true;
        session.remote_accepted = true;
        (session, socket, peer)
    }
    async fn read_file_packet(socket: &mut FramedStream, peer: &mut Crypto) -> n::PayloadTransferFrame {
        let wire = socket.read_frame().await.unwrap();
        let plaintext = peer.open(&wire).unwrap();
        n::OfflineFrame::decode(plaintext.as_slice())
            .unwrap().v1.unwrap().payload_transfer.unwrap()
    }

    #[tokio::test]
    async fn queued_selection_delivers_original_inode_after_private_symlink_swap() {
        let directory = Directory::new();
        let parent = directory.0.join("selected");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("public.txt");
        let original = b"Selected public content, not the sender's private content";
        std::fs::write(&path, original).unwrap();
        // Selection happens before connection/session construction.
        let selected = crate::OutboundFile::open(&path).unwrap();
        let moved = directory.0.join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::remove_file(moved.join("public.txt")).unwrap();
        let private = directory.0.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("public.txt"), b"PRIVATE SECRET").unwrap();
        std::os::unix::fs::symlink(&private, &parent).unwrap();
        let (mut session, mut socket, mut peer) = sending(selected);
        assert_eq!(session.metadata.total_bytes, original.len() as u64);
        assert_eq!(session.metadata.files.as_ref().unwrap(), &["public.txt"]);
        session.send_next().await.unwrap();
        let packet = read_file_packet(&mut socket, &mut peer).await;
        let header = packet.payload_header.unwrap();
        assert_eq!(header.total_size(), original.len() as i64);
        assert_eq!(header.file_name(), "public.txt");
        assert_eq!(packet.payload_chunk.unwrap().body(), original);
        session.send_next().await.unwrap();
        let packet = read_file_packet(&mut socket, &mut peer).await;
        let chunk = packet.payload_chunk.unwrap();
        assert!(chunk.body().is_empty());
        assert_eq!(chunk.offset(), original.len() as i64);
        assert!(session.files.values().all(|file| file.last_sent));
    }

    #[tokio::test]
    async fn selected_size_limits_data_after_in_place_growth() {
        let directory = Directory::new();
        let path = directory.0.join("public.txt");
        let original = b"selected";
        std::fs::write(&path, original).unwrap();
        let selected = crate::OutboundFile::open(&path).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(b" later bytes must not exceed declared size").unwrap();
        let (mut session, mut socket, mut peer) = sending(selected);
        session.send_next().await.unwrap();
        let packet = read_file_packet(&mut socket, &mut peer).await;
        assert_eq!(packet.payload_header.unwrap().total_size(), original.len() as i64);
        assert_eq!(packet.payload_chunk.unwrap().body(), original);
        session.send_next().await.unwrap();
        assert!(read_file_packet(&mut socket, &mut peer).await
            .payload_chunk.unwrap().body().is_empty());
        assert_eq!(session.metadata.ack_bytes, original.len() as u64);
    }

    #[tokio::test]
    async fn selected_file_truncation_fails_instead_of_sending_short_data() {
        let directory = Directory::new();
        let path = directory.0.join("public.txt");
        std::fs::write(&path, b"original selected bytes").unwrap();
        let selected = crate::OutboundFile::open(&path).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(1).unwrap();
        let (mut session, mut socket, _peer) = sending(selected);
        let error = session.send_next().await.unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof);
        assert_eq!(session.metadata.ack_bytes, 0);
        assert!(session.files.values().all(|file| file.offset == 0 && !file.last_sent));
        assert!(tokio::time::timeout(Duration::from_millis(10), socket.read_frame())
            .await.is_err());
    }
    fn accepted() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::Response.into()),
            connection_response: Some(q::ConnectionResponseFrame {
                status: Some(q::connection_response_frame::Status::Accept.into()),
            }),
            ..Default::default()
        })
    }
    fn introduction() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::Introduction.into()),
            introduction: Some(q::IntroductionFrame {
                file_metadata: vec![q::FileMetadata {
                    name: Some("native-ñ.jpg".into()),
                    payload_id: Some(42),
                    size: Some(392114),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    fn paired_result() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::PairedKeyResult.into()),
            paired_key_result: Some(q::PairedKeyResultFrame {
                status: Some(q::paired_key_result_frame::Status::Unable.into()),
            }),
            ..Default::default()
        })
    }
    fn encryption() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::PairedKeyEncryption.into()),
            paired_key_encryption: Some(q::PairedKeyEncryptionFrame {
                signed_data: Some(vec![1; 72]),
                secret_id_hash: Some(vec![2; 6]),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    fn certificate() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::CertificateInfo.into()),
            certificate_info: Some(q::CertificateInfoFrame::default()),
            ..Default::default()
        })
    }
    fn progress_update() -> q::Frame {
        sharing(q::V1Frame {
            r#type: Some(ShareType::ProgressUpdate.into()),
            progress_update: Some(q::ProgressUpdateFrame {
                progress: Some(0.0),
                start_transfer: Some(true),
            }),
            ..Default::default()
        })
    }
    async fn deliver_control(
        session: &mut Session,
        peer: &mut Crypto,
        id: i64,
        body: Vec<u8>,
    ) -> Result<()> {
        let length = body.len() as i64;
        for (offset, body, flags) in [(0, body, 0), (length, Vec::new(), 1)] {
            let frame = offline(n::V1Frame {
                r#type: Some(FrameType::PayloadTransfer.into()),
                payload_transfer: Some(n::PayloadTransferFrame {
                    packet_type: Some(PacketType::Data.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(id),
                        r#type: Some(PayloadType::Bytes.into()),
                        total_size: Some(length),
                        ..Default::default()
                    }),
                    payload_chunk: Some(PayloadChunk {
                        offset: Some(offset),
                        body: Some(body),
                        flags: Some(flags),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
            session.process(&peer.seal(frame.encode_to_vec())?).await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn qr_proof_is_bound_to_ukey2_and_never_replaces_either_consent_gate() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
        for with_qr in [false, true] {
            for remote_first in [false, true] {
                let (mut session, mut socket) = session();
                session.outbound = true;
                session.transport = "lan".into();
                session.enter(Phase::ServerInit);
                let invitation = Arc::new(QrInvitation::new().unwrap());
                if with_qr { session.set_qr_invitation(invitation.clone()).unwrap(); }
                let (crypto, mut peer) = pair();
                let auth = *peer.auth_token();
                session.crypto = Some(crypto);
                session.paired_encryption().await.unwrap();
                let bytes = peer.open(&socket.read_frame().await.unwrap()).unwrap();
                let frame = n::OfflineFrame::decode(bytes.as_slice()).unwrap();
                let payload = frame.v1.unwrap().payload_transfer.unwrap();
                let share = q::Frame::decode(payload.payload_chunk.unwrap().body()).unwrap();
                let encryption = share.v1.unwrap().paired_key_encryption.unwrap();
                if with_qr {
                    let wire = URL_SAFE_NO_PAD.decode(invitation.url().split_once("#key=").unwrap().1).unwrap();
                    let verifier = VerifyingKey::from_sec1_bytes(&wire[2..]).unwrap();
                    let proof = encryption.qr_code_handshake_data.unwrap();
                    assert_eq!(proof.len(), 64);
                    let signature = Signature::from_slice(&proof).unwrap();
                    verifier.verify(&auth, &signature).unwrap();
                    let (_, unrelated) = pair();
                    assert!(verifier.verify(unrelated.auth_token(), &signature).is_err());
                    assert!(verifier.verify(session.crypto.as_ref().unwrap().pin.as_bytes(), &signature).is_err());
                } else {
                    assert!(encryption.qr_code_handshake_data.is_none());
                }
                peer.open(&socket.read_frame().await.unwrap()).unwrap(); // terminal BYTES chunk
                assert!(!session.locally_verified);
                assert!(!session.remote_accepted);
                session.enter(Phase::Consent);
                if remote_first {
                    session.receive_share(accepted()).await.unwrap();
                    assert_eq!(session.phase, Phase::Consent);
                    assert!(session.send_next().await.is_err());
                    session.accept().await.unwrap();
                } else {
                    session.accept().await.unwrap();
                    assert_eq!(session.phase, Phase::Response);
                    assert!(session.send_next().await.is_err());
                    session.receive_share(accepted()).await.unwrap();
                }
                assert_eq!(session.phase, Phase::Transfer);
                assert!(session.locally_verified && session.remote_accepted);
            }
        }
    }

    #[tokio::test]
    async fn qr_key_attachment_rejects_inbound_started_and_non_lan_sessions() {
        let invitation = Arc::new(QrInvitation::new().unwrap());
        let (mut session, _socket) = session();
        assert!(session.set_qr_invitation(invitation.clone()).is_err());
        session.outbound = true;
        session.enter(Phase::ServerInit);
        assert!(session.set_qr_invitation(invitation.clone()).is_err());
        session.transport = "lan".into();
        session.crypto = Some(pair().0);
        assert!(session.set_qr_invitation(invitation.clone()).is_err());
        session.crypto = None;
        session.set_qr_invitation(invitation.clone()).unwrap();
        assert!(session.paired_encryption().await.is_err(), "cannot produce a QR signature before UKEY2");
        assert!(session.set_qr_invitation(invitation).is_err(), "cannot replace a transfer's key");
        assert!(!session.locally_verified && !session.remote_accepted);
    }

    #[tokio::test]
    async fn native_ancillary_controls_preserve_introduction_and_consent() {
        let (mut session, _socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::PairedResult);
        for (id, frame) in [
            (1, certificate()),
            (2, paired_result()),
            (3, progress_update()),
        ] {
            deliver_control(&mut session, &mut peer, id, frame.encode_to_vec())
                .await
                .unwrap();
        }
        assert_eq!(session.phase, Phase::Introduction);
        assert!(session.metadata.files.is_none());
        deliver_control(&mut session, &mut peer, 4, introduction().encode_to_vec())
            .await
            .unwrap();
        assert_eq!(session.phase, Phase::Consent);
        assert_eq!(
            session.metadata.files.as_deref(),
            Some(&["native-ñ.jpg".to_owned()][..])
        );
        assert_eq!(session.metadata.total_bytes, 392114);
        let deadline = session.phase_deadline;
        for (id, frame) in [(5, progress_update()), (6, certificate())] {
            deliver_control(&mut session, &mut peer, id, frame.encode_to_vec())
                .await
                .unwrap();
        }
        assert_eq!(session.phase, Phase::Consent);
        assert_eq!(session.phase_deadline, deadline);
        assert!(!session.locally_verified);
        assert!(!session.remote_accepted);
        assert!(session.incoming.is_none());
        assert!(session.offer_hotspot().await.is_err());
        let error = session
            .receive_payload(n::PayloadTransferFrame {
                packet_type: Some(PacketType::Data.into()),
                payload_header: Some(PayloadHeader {
                    id: Some(42),
                    r#type: Some(PayloadType::File.into()),
                    total_size: Some(392114),
                    ..Default::default()
                }),
                payload_chunk: Some(PayloadChunk {
                    offset: Some(0),
                    flags: Some(0),
                    body: Some(vec![1]),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("phase=Consent"));
        assert!(error.contains("file data before user consent"));
    }
    #[tokio::test]
    async fn early_native_metadata_and_result_wait_for_both_paired_key_frames() {
        for result_first in [false, true] {
            let (mut session, _socket) = session();
            let (crypto, mut peer) = pair();
            session.crypto = Some(crypto);
            session.enter(Phase::PairedEncryption);
            deliver_control(&mut session, &mut peer, 1, introduction().encode_to_vec())
                .await
                .unwrap();
            if result_first {
                deliver_control(&mut session, &mut peer, 2, paired_result().encode_to_vec())
                    .await
                    .unwrap();
            }
            assert_eq!(session.phase, Phase::PairedEncryption);
            assert!(session.metadata.files.is_none());
            assert!(!session.locally_verified);
            deliver_control(&mut session, &mut peer, 3, certificate().encode_to_vec())
                .await
                .unwrap();
            deliver_control(&mut session, &mut peer, 4, encryption().encode_to_vec())
                .await
                .unwrap();
            if !result_first {
                assert_eq!(session.phase, Phase::PairedResult);
                assert!(session.metadata.files.is_none());
                deliver_control(&mut session, &mut peer, 5, paired_result().encode_to_vec())
                    .await
                    .unwrap();
            }
            assert_eq!(session.phase, Phase::Consent);
            assert!(session.pending_introduction.is_none());
            assert!(session.pending_paired_result.is_none());
            assert!(!session.locally_verified);
            assert!(session.incoming.is_none());
        }
    }
    #[tokio::test]
    async fn controls_keep_exact_phase_diagnostics_and_reject_duplicates_and_unknown_types() {
        let (mut session, _socket) = session();
        session.enter(Phase::Introduction);
        session.metadata.pin_code = Some("private-pin-marker".into());
        session.receive_share(introduction()).await.unwrap();
        let duplicate = session
            .receive_share(introduction())
            .await
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("phase=Consent"));
        assert!(duplicate.contains("Some(Introduction)/Some(1)"));
        assert!(!duplicate.contains("native-ñ.jpg"));
        assert!(!duplicate.contains("private-pin-marker"));
        for raw in [8, 9, 999] {
            let error = session
                .receive_share(sharing(q::V1Frame {
                    r#type: Some(raw),
                    ..Default::default()
                }))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("/Some({raw})")));
            assert!(error.contains("unsupported sharing frame type"));
            assert_eq!(session.phase, Phase::Consent);
        }
        let malformed = session
            .receive_share(sharing(q::V1Frame {
                r#type: Some(ShareType::Cancel.into()),
                introduction: introduction().v1.unwrap().introduction,
                ..Default::default()
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(malformed.contains("multiple sharing bodies"));
        assert_eq!(session.phase, Phase::Consent);
        let missing = session
            .receive_share(sharing(q::V1Frame {
                r#type: Some(ShareType::ProgressUpdate.into()),
                ..Default::default()
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("Some(ProgressUpdate)/Some(7)"));
        assert_eq!(session.phase, Phase::Consent);
    }
    #[tokio::test]
    async fn pending_introduction_is_bounded_and_cannot_bypass_failed_authentication() {
        let (mut session, _socket) = session();
        session.enter(Phase::PairedEncryption);
        session.receive_share(introduction()).await.unwrap();
        assert!(session
            .receive_share(introduction())
            .await
            .unwrap_err()
            .to_string()
            .contains("duplicate introduction"));
        let mut failed = paired_result();
        failed
            .v1
            .as_mut()
            .unwrap()
            .paired_key_result
            .as_mut()
            .unwrap()
            .status = Some(q::paired_key_result_frame::Status::Fail.into());
        assert!(session.receive_share(failed).await.is_err());
        assert_eq!(session.phase, Phase::PairedEncryption);
        assert!(session.metadata.files.is_none());
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
    }
    #[tokio::test]
    async fn rejection_and_cancel_are_valid_across_secure_phase_transitions() {
        for phase in [
            Phase::PairedEncryption,
            Phase::PairedResult,
            Phase::Introduction,
            Phase::Consent,
            Phase::Response,
            Phase::Transfer,
        ] {
            for cancel in [false, true] {
                let (mut session, _socket) = session();
                session.enter(phase);
                let frame = if cancel {
                    sharing(q::V1Frame {
                        r#type: Some(ShareType::Cancel.into()),
                        ..Default::default()
                    })
                } else {
                    sharing(q::V1Frame {
                        r#type: Some(ShareType::Response.into()),
                        connection_response: Some(q::ConnectionResponseFrame {
                            status: Some(q::connection_response_frame::Status::Reject.into()),
                        }),
                        ..Default::default()
                    })
                };
                session.receive_share(frame).await.unwrap();
                assert_eq!(session.phase, Phase::Done);
                assert_eq!(
                    session.state,
                    if cancel {
                        State::Cancelled
                    } else {
                        State::Rejected
                    }
                );
                assert!(!session.locally_verified);
            }
        }
    }
    #[tokio::test]
    async fn control_payload_acknowledgments_do_not_accept_a_transfer() {
        let (mut session, _socket) = session();
        let (crypto, _) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::Consent);
        session.share(progress_update()).await.unwrap();
        let id = *session.sent_control.keys().next().unwrap();
        session
            .receive_payload(n::PayloadTransferFrame {
                packet_type: Some(PacketType::PayloadAck.into()),
                payload_header: Some(PayloadHeader {
                    id: Some(id),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(session.sent_control.is_empty());
        assert_eq!(session.phase, Phase::Consent);
        assert!(!session.remote_accepted);
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
    }
    #[tokio::test]
    async fn native_upgrade_capability_request_never_grants_file_or_radio_consent() {
        let (mut session, mut socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.lan = true;
        session.hotspot = false;
        session.enter(Phase::Consent);
        let deadline = session.phase_deadline;
        let request = offline(n::V1Frame {
            r#type: Some(FrameType::BandwidthUpgradeRetry.into()),
            bandwidth_upgrade_retry: Some(n::BandwidthUpgradeRetryFrame {
                supported_medium: vec![Medium::WifiDirect as i32, 999],
                is_request: Some(true),
            }),
            ..Default::default()
        });
        session
            .process(&peer.seal(request.encode_to_vec()).unwrap())
            .await
            .unwrap();
        let response_bytes = peer.open(&socket.read_frame().await.unwrap()).unwrap();
        let response = n::OfflineFrame::decode(response_bytes.as_slice()).unwrap();
        let retry = response.v1.unwrap().bandwidth_upgrade_retry.unwrap();
        assert!(!retry.is_request());
        assert_eq!(retry.supported_medium, vec![Medium::WifiLan as i32]);
        assert_eq!(session.phase, Phase::Consent);
        assert_eq!(session.phase_deadline, deadline);
        assert!(!session.locally_verified);
        assert!(!session.consent.load(Ordering::Relaxed));
        assert!(session.incoming.is_none());
        assert!(session.lease.is_none());
        assert!(session.upgrade.is_none());
        let mut response = request;
        response
            .v1
            .as_mut()
            .unwrap()
            .bandwidth_upgrade_retry
            .as_mut()
            .unwrap()
            .is_request = Some(false);
        session
            .process(&peer.seal(response.encode_to_vec()).unwrap())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), socket.read_frame())
                .await
                .is_err()
        );
        assert_eq!(session.phase_deadline, deadline);
        assert!(session.lease.is_none());
    }
    #[tokio::test]
    async fn modern_native_control_receipt_emits_only_transport_acknowledgment() {
        let (mut session, mut socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.remote_disconnect_version = 6;
        session.enter(Phase::Introduction);
        deliver_control(&mut session, &mut peer, 7, certificate().encode_to_vec())
            .await
            .unwrap();
        let bytes = peer.open(&socket.read_frame().await.unwrap()).unwrap();
        let frame = n::OfflineFrame::decode(bytes.as_slice()).unwrap();
        let payload = frame.v1.unwrap().payload_transfer.unwrap();
        assert_eq!(payload.packet_type(), PacketType::PayloadAck);
        assert_eq!(payload.payload_header.unwrap().id(), 7);
        assert_eq!(session.phase, Phase::Introduction);
        assert!(!session.locally_verified);
    }
    #[tokio::test]
    async fn native_metadata_rejects_unsafe_utf8_names_but_not_advisory_preview_ids() {
        let (mut session, _socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::Introduction);
        let mut unsafe_name = introduction();
        unsafe_name
            .v1
            .as_mut()
            .unwrap()
            .introduction
            .as_mut()
            .unwrap()
            .file_metadata[0]
            .name = Some("../private-name".into());
        let error = session
            .receive_share(unsafe_name)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe remote filename"));
        assert!(!error.contains("private-name"));
        let invalid_utf8 = vec![8, 1, 18, 9, 8, 1, 18, 5, 10, 3, 10, 1, 255];
        let error = deliver_control(&mut session, &mut peer, 1, invalid_utf8)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("phase=Introduction"));
        assert!(session.metadata.files.is_none());
        let mut preview = introduction();
        preview
            .v1
            .as_mut()
            .unwrap()
            .introduction
            .as_mut()
            .unwrap()
            .preview_payload_ids
            .push(99);
        let intro = preview.v1.as_mut().unwrap().introduction.as_mut().unwrap();
        intro.continuation = Some(false);
        intro.expect_more = Some(false);
        session.receive_share(preview).await.unwrap();
        assert_eq!(session.state, State::WaitingForUserConsent);
        assert_eq!(session.phase, Phase::Consent);
        assert_eq!(session.incoming_metadata.len(), 1);
        assert_eq!(session.incoming_metadata[0].payload_id(), 42);
        let unexpected_preview =
            deliver_control(&mut session, &mut peer, 99, vec![255, 216, 255, 217])
                .await
                .unwrap_err()
                .to_string();
        assert!(unexpected_preview.contains("phase=Consent"));
        assert_eq!(session.phase, Phase::Consent);
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
    }
    #[tokio::test]
    async fn unsupported_native_attachment_offers_are_not_silently_reduced_to_files() {
        for field in 0..4 {
            let (mut session, _socket) = session();
            let (crypto, _) = pair();
            session.crypto = Some(crypto);
            session.enter(Phase::Introduction);
            let mut offer = introduction();
            let intro = offer.v1.as_mut().unwrap().introduction.as_mut().unwrap();
            match field {
                0 => intro.app_metadata.push(q::AppMetadata::default()),
                1 => intro.stream_metadata.push(q::StreamMetadata::default()),
                2 => intro.continuation = Some(true),
                _ => intro.expect_more = Some(true),
            }
            session.receive_share(offer).await.unwrap();
            assert_eq!(session.state, State::Rejected);
            assert_eq!(session.phase, Phase::Done);
            assert!(session.metadata.files.is_none());
            assert!(!session.locally_verified);
            assert!(session.incoming.is_none());
        }
    }
    #[tokio::test]
    async fn native_sender_acceptance_cannot_replace_receiver_consent() {
        let (mut session, _socket) = session();
        let (crypto, _) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::PairedEncryption);
        let authentication_deadline = session.phase_deadline;
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::PairedEncryption);
        assert_eq!(session.phase_deadline, authentication_deadline);
        assert!(!session.locally_verified);
        assert!(session.incoming_metadata.is_empty());
        session.enter(Phase::Introduction);
        let introduction_deadline = session.phase_deadline;
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::Introduction);
        assert_eq!(session.phase_deadline, introduction_deadline);
        assert!(session.incoming_metadata.is_empty());
        session.receive_share(introduction()).await.unwrap();
        let deadline = session.phase_deadline;
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::Consent);
        assert_eq!(session.phase_deadline, deadline);
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
        let error = session
            .receive_payload(n::PayloadTransferFrame {
                packet_type: Some(PacketType::Data.into()),
                payload_header: Some(PayloadHeader {
                    id: Some(42),
                    r#type: Some(PayloadType::File.into()),
                    total_size: Some(392114),
                    ..Default::default()
                }),
                payload_chunk: Some(n::payload_transfer_frame::PayloadChunk {
                    offset: Some(0),
                    body: Some(vec![1]),
                    flags: Some(0),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("file data before user consent"));
        assert!(session.incoming.is_none());
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::Consent);
    }
    #[tokio::test]
    async fn malformed_nearby_controls_and_unknown_responses_never_change_consent() {
        let (mut session, _socket) = session();
        session.enter(Phase::Consent);
        for status in [None, Some(0), Some(999)] {
            let error = session
                .receive_share(sharing(q::V1Frame {
                    r#type: Some(ShareType::Response.into()),
                    connection_response: Some(q::ConnectionResponseFrame { status }),
                    ..Default::default()
                }))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("phase=Consent"));
            assert!(error.contains("Some(Response)/Some(2)"));
            assert_eq!(session.phase, Phase::Consent);
            assert!(!session.remote_accepted);
        }
        for (event, offset) in [(Some(999), Some(0)), (Some(3), None), (Some(2), Some(2))] {
            let error = session
                .receive_payload(n::PayloadTransferFrame {
                    packet_type: Some(PacketType::Control.into()),
                    payload_header: Some(PayloadHeader {
                        id: Some(42),
                        r#type: Some(PayloadType::File.into()),
                        total_size: Some(1),
                        ..Default::default()
                    }),
                    control_message: Some(n::payload_transfer_frame::ControlMessage {
                        event,
                        offset,
                    }),
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("phase=Consent"));
            assert!(error.contains("Some(Control)/Some(2)"));
            assert_eq!(session.phase, Phase::Consent);
        }
        let mixed = session
            .receive_payload(n::PayloadTransferFrame {
                packet_type: Some(PacketType::Control.into()),
                payload_header: Some(PayloadHeader {
                    id: Some(42),
                    total_size: Some(1),
                    ..Default::default()
                }),
                control_message: Some(n::payload_transfer_frame::ControlMessage {
                    event: Some(ControlEvent::PayloadCanceled.into()),
                    offset: Some(0),
                }),
                payload_chunk: Some(PayloadChunk {
                    offset: Some(0),
                    flags: Some(1),
                    body: None,
                }),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(mixed.contains("multiple payload packet bodies"));
        assert_eq!(session.phase, Phase::Consent);
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
    }
    #[tokio::test]
    async fn destination_changed_before_consent_receives_the_actual_file() {
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Directory(std::env::temp_dir().join(format!(
            "quickshare-destination-test-{}",
            uuid::Uuid::new_v4()
        )));
        let before = root.0.join("before");
        let selected = root.0.join("selected");
        std::fs::create_dir_all(&before).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let (mut session, _peer) = session();
        let (crypto, _) = pair();
        session.crypto = Some(crypto);
        session.download = Arc::new(RwLock::new(before.clone()));
        let body = b"Destination selected before consent";
        session.incoming_metadata = vec![q::FileMetadata {
            name: Some("chosen.txt".into()),
            payload_id: Some(7),
            size: Some(body.len() as i64),
            ..Default::default()
        }];
        session.metadata.total_bytes = body.len() as u64;
        session.enter(Phase::Consent);
        *session.download.write() = selected.clone();
        session.accept().await.unwrap();
        session
            .receive_payload(n::PayloadTransferFrame {
                packet_type: Some(PacketType::Data.into()),
                payload_header: Some(PayloadHeader {
                    id: Some(7),
                    r#type: Some(PayloadType::File.into()),
                    total_size: Some(body.len() as i64),
                    ..Default::default()
                }),
                payload_chunk: Some(PayloadChunk {
                    offset: Some(0),
                    flags: Some(1),
                    body: Some(body.to_vec()),
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(std::fs::read(selected.join("chosen.txt")).unwrap(), body);
        assert!(!before.join("chosen.txt").exists());
    }
    #[tokio::test]
    async fn remote_accept_and_discovery_hint_cannot_authorize_files_or_hotspot() {
        let (mut session, _peer) = session();
        session.outbound = true;
        session.enter(Phase::Consent);
        assert_eq!(session.remote_endpoint.as_deref(), Some("PEER"));
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::Consent);
        assert!(!session.locally_verified);
        assert!(session.send_next().await.is_err());
        assert!(session.offer_hotspot().await.is_err());
        session.accept().await.unwrap();
        assert_eq!(session.phase, Phase::Transfer);
    }
    #[tokio::test]
    async fn local_pin_consent_alone_does_not_authorize_file_bytes() {
        let (mut session, _peer) = session();
        session.outbound = true;
        session.enter(Phase::Consent);
        session.accept().await.unwrap();
        assert_eq!(session.phase, Phase::Response);
        assert!(session.send_next().await.is_err());
        session.receive_share(accepted()).await.unwrap();
        assert_eq!(session.phase, Phase::Transfer);
    }
    #[tokio::test]
    async fn introduction_reaches_peer_but_never_grants_local_consent() {
        let (mut session, mut socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.outbound = true;
        session.phase = Phase::PairedResult;
        session
            .receive_share(sharing(q::V1Frame {
                r#type: Some(ShareType::PairedKeyResult.into()),
                paired_key_result: Some(q::PairedKeyResultFrame {
                    status: Some(q::paired_key_result_frame::Status::Success.into()),
                }),
                ..Default::default()
            }))
            .await
            .unwrap();
        let bytes = peer.open(&socket.read_frame().await.unwrap()).unwrap();
        let frame = n::OfflineFrame::decode(bytes.as_slice()).unwrap();
        let payload = frame.v1.unwrap().payload_transfer.unwrap();
        let share = q::Frame::decode(payload.payload_chunk.unwrap().body()).unwrap();
        assert_eq!(share.v1.unwrap().r#type(), ShareType::Introduction);
        assert_eq!(session.state, State::WaitingForUserConsent);
        assert!(!session.locally_verified);
        assert!(session.send_next().await.is_err());
    }
    #[tokio::test]
    async fn encrypted_keepalive_cannot_extend_auth_consent_or_stalled_bytes() {
        let (mut session, _socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        let now = Instant::now();
        for phase in [Phase::PairedEncryption, Phase::Consent, Phase::Transfer] {
            session.enter(phase);
            let deadline = session.phase_deadline;
            let bytes = peer
                .seal(
                    offline(n::V1Frame {
                        r#type: Some(FrameType::KeepAlive.into()),
                        keep_alive: Some(n::KeepAliveFrame { ack: Some(true) }),
                        ..Default::default()
                    })
                    .encode_to_vec(),
                )
                .unwrap();
            session.process(&bytes).await.unwrap();
            assert_eq!(session.phase_deadline, deadline);
            session.last_activity = now;
            if phase == Phase::Transfer {
                session.last_bytes = now - Duration::from_secs(91);
            } else {
                session.phase_deadline = now - Duration::from_secs(1);
            }
            assert!(session.check_deadlines(now).is_err());
        }
    }
    #[tokio::test]
    async fn public_introduction_channel_does_not_commit_before_old_channel_proof() {
        let (mut session, _socket) = session();
        let (next, _) = tokio::io::duplex(1024);
        session.upgrade = Some(Upgrade {
            listener: None,
            next: Some(FramedStream::new(Box::new(next))),
            lease: None,
            info: UpgradePathInfo::default(),
            deadline: Instant::now() + Duration::from_secs(30),
            last_sent: false,
            last_received: false,
        });
        let (_, mut impostor) = pair();
        session.enter(Phase::Consent);
        let (_, unrelated) = pair();
        session.crypto = Some(unrelated);
        let bytes = impostor
            .seal(bwu(BwuEvent::LastWriteToPriorChannel, None).encode_to_vec())
            .unwrap();
        assert!(session.process(&bytes).await.is_err());
        assert!(!session.upgrade.as_ref().unwrap().last_sent);
        assert!(!session.upgrade.as_ref().unwrap().last_received);
    }
    #[tokio::test]
    async fn cancellation_interrupts_a_blocked_write_and_publishes_one_terminal() {
        let (local, _remote) = tokio::io::duplex(1);
        let (sender, mut events) = broadcast::channel(32);
        let mut session = Session::new(
            "test".into(),
            Box::new(local),
            "lan",
            *b"LOCL",
            "local".into(),
            sender,
            Arc::new(RwLock::new(PathBuf::from("/tmp"))),
            false,
            false,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            None,
        )
        .unwrap();
        session.outbound = true;
        let token = CancellationToken::new();
        let cancel = async {
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (result, ()) = tokio::join!(session.run(token.clone()), cancel);
        result.unwrap();
        assert_eq!(session.state, State::Cancelled);
        let mut terminals = 0;
        while let Ok(event) = events.try_recv() {
            if event.state == Some(State::Cancelled) {
                terminals += 1;
            }
        }
        assert_eq!(terminals, 1);
    }
    #[tokio::test]
    async fn local_cancel_preserves_queued_ciphertext_and_cancels_the_active_payload() {
        let (mut sender, mut socket) = session();
        let (crypto, mut peer) = pair();
        sender.crypto = Some(crypto);
        sender.phase = Phase::Transfer;
        sender.cancellation.cancel();
        sender.files.insert(
            42,
            SendingFile {
                metadata: q::FileMetadata {
                    payload_id: Some(42),
                    size: Some(32),
                    name: Some("cancel.bin".into()),
                    ..Default::default()
                },
                file: File::open("/dev/null").unwrap(),
                offset: 0,
                last_sent: false,
                acknowledged: false,
            },
        );
        let data = Session::payload_frame(
            PayloadHeader {
                id: Some(42),
                r#type: Some(PayloadType::File.into()),
                total_size: Some(32),
                ..Default::default()
            },
            0,
            b"abc".to_vec(),
            false,
        );
        let ciphertext = sender
            .crypto
            .as_mut()
            .unwrap()
            .seal(data.encode_to_vec())
            .unwrap();
        sender.socket.queue_frame(ciphertext).unwrap();
        sender.pending_file = Some((42, 3));
        // A cancelled select can skip polling the writer altogether. Its frame
        // and assigned crypto sequence must still precede both cancel controls.
        assert!(sender.notify_cancel().await.unwrap());
        assert_eq!(sender.metadata.ack_bytes, 3);
        assert_eq!(sender.files[&42].offset, 3);
        let mut decoded = Vec::new();
        for _ in 0..4 {
            let bytes = peer.open(&socket.read_frame().await.unwrap()).unwrap();
            decoded.push(
                n::OfflineFrame::decode(bytes.as_slice())
                    .unwrap()
                    .v1
                    .unwrap()
                    .payload_transfer
                    .unwrap(),
            );
        }
        assert_eq!(decoded[0].payload_header.as_ref().unwrap().id(), 42);
        assert_eq!(decoded[0].payload_chunk.as_ref().unwrap().body(), b"abc");
        assert_eq!(decoded[1].packet_type(), PacketType::Control);
        assert_eq!(decoded[1].payload_header.as_ref().unwrap().id(), 42);
        assert_eq!(
            decoded[1].control_message.as_ref().unwrap().event(),
            ControlEvent::PayloadCanceled
        );
        assert_eq!(decoded[1].control_message.as_ref().unwrap().offset(), 3);
        let sharing_header = decoded[2].payload_header.as_ref().unwrap();
        let sharing_chunk = decoded[2].payload_chunk.as_ref().unwrap();
        assert_eq!(sharing_header.r#type(), PayloadType::Bytes);
        assert_eq!(sharing_chunk.offset(), 0);
        assert_eq!(sharing_chunk.flags(), 0);
        let cancellation = q::Frame::decode(sharing_chunk.body()).unwrap();
        assert_eq!(
            cancellation.v1.as_ref().unwrap().r#type(),
            ShareType::Cancel
        );
        let last_header = decoded[3].payload_header.as_ref().unwrap();
        let last_chunk = decoded[3].payload_chunk.as_ref().unwrap();
        assert_eq!(last_header.id(), sharing_header.id());
        assert_eq!(last_chunk.offset(), sharing_header.total_size());
        assert_eq!(last_chunk.flags(), 1);
        assert!(last_chunk.body().is_empty());
        let (mut receiver, _) = session();
        receiver.phase = Phase::Transfer;
        receiver.receive_share(cancellation).await.unwrap();
        assert_eq!(receiver.state, State::Cancelled);
    }
    #[test]
    fn hotspot_binding_rejects_routed_public_peers_and_network_broadcast() {
        let local = "10.42.0.1".parse().unwrap();
        let mask = "255.255.255.0".parse().unwrap();
        assert!(shared_subnet(local, mask, "10.42.0.2".parse().unwrap()));
        for peer in [
            "192.168.1.2",
            "10.42.0.0",
            "10.42.0.255",
            "10.42.0.1",
            "127.0.0.1",
        ] {
            assert!(!shared_subnet(local, mask, peer.parse().unwrap()));
        }
    }
    #[tokio::test]
    async fn lan_candidate_requires_continuing_key_proof_without_granting_consent() {
        let (mut session, _socket) = session();
        let (crypto, mut peer) = pair();
        session.crypto = Some(crypto);
        session.enter(Phase::Consent);
        session.state = State::NegotiatingTransport;
        session.pending_lan = true;
        let keepalive = offline(n::V1Frame {
            r#type: Some(FrameType::KeepAlive.into()),
            keep_alive: Some(n::KeepAliveFrame { ack: Some(true) }),
            ..Default::default()
        })
        .encode_to_vec();
        let (_, mut impostor) = pair();
        for bytes in [
            keepalive.clone(),
            impostor.seal(keepalive.clone()).unwrap(),
        ] {
            assert!(session.process(&bytes).await.is_err());
            assert!(session.pending_lan);
            assert_eq!(session.transport, "bluetooth");
            assert_eq!(session.state, State::NegotiatingTransport);
        }
        let authenticated = peer.seal(keepalive).unwrap();
        session.process(&authenticated).await.unwrap();
        assert!(!session.pending_lan);
        assert_eq!(session.transport, "lan");
        assert_eq!(session.state, State::WaitingForUserConsent);
        assert!(!session.locally_verified);
        assert!(session.incoming.is_none());
        assert!(session.process(&authenticated).await.is_err());
    }
    #[tokio::test]
    async fn tentative_lan_blocks_file_data_despite_both_consents() {
        let directory = Directory::new();
        let path = directory.0.join("payload.bin");
        let body = b"File bytes require the replacement channel's key proof";
        std::fs::write(&path, body).unwrap();
        let selected = crate::OutboundFile::open(&path).unwrap();
        let (mut session, mut socket, mut peer) = sending(selected);
        let id = *session.files.keys().next().unwrap();
        session.pending_lan = true;
        assert!(session.send_next().await.is_err());
        assert_eq!(session.files[&id].offset, 0);
        assert_eq!(session.transport, "bluetooth");
        let proof = peer
            .seal(
                offline(n::V1Frame {
                    r#type: Some(FrameType::KeepAlive.into()),
                    keep_alive: Some(n::KeepAliveFrame { ack: Some(true) }),
                    ..Default::default()
                })
                .encode_to_vec(),
            )
            .unwrap();
        session.process(&proof).await.unwrap();
        session.send_next().await.unwrap();
        let wire = socket.read_frame().await.unwrap();
        let plaintext = peer.open(&wire).unwrap();
        let packet = n::OfflineFrame::decode(plaintext.as_slice())
            .unwrap()
            .v1
            .unwrap()
            .payload_transfer
            .unwrap();
        assert_eq!(packet.payload_chunk.unwrap().body(), body);
        assert_eq!(session.files[&id].offset, body.len() as i64);
        assert_eq!(session.transport, "lan");
    }
    #[tokio::test]
    async fn failed_tentative_lan_cannot_resume_the_drained_bluetooth_channel() {
        let (mut session, _socket) = session();
        session.enter(Phase::Transfer);
        session.state = State::NegotiatingTransport;
        session.pending_lan = true;
        assert!(session.fail_upgrade().await.is_err());
        assert!(session.pending_lan);
        assert_eq!(session.phase, Phase::Transfer);
        assert_eq!(session.state, State::NegotiatingTransport);
        assert_eq!(session.transport, "bluetooth");
    }
}
