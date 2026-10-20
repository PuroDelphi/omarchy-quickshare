use crate::share_selection::ShareSelections;
use crate::ui_language::UiLanguage;
use crate::{integration::NautilusIntegration, BUS, INTERFACE, PATH};
use anyhow::{bail, Context, Result};
use directories::{ProjectDirs, UserDirs};
use base64::Engine as _;
use rqs_lib::channel::{ChannelAction, ChannelDirection, ChannelMessage};
use rqs_lib::{
    EndpointInfo, EngineOptions, OutboundFile, OutboundPayload, QrInvitation, QrPeer, QrSendContext, SendInfo,
    Visibility, DISCOVERY_PEER_LIMIT, LAN_PEER_TTL, MAX_TOTAL_BYTES, RQS,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot, watch, RwLock};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize, Deserialize)]
struct Preferences {
    download_directory: PathBuf,
}

impl Preferences {
    fn file() -> Result<PathBuf> {
        Ok(ProjectDirs::from("", "", "omarchy-quickshare")
            .context("No se encontró la carpeta de configuración del usuario")?
            .config_dir()
            .join("settings.toml"))
    }

    fn load() -> Result<Self> {
        let file = Self::file()?;
        match std::fs::read_to_string(&file) {
            Ok(contents) => toml::from_str(&contents).context("Configuración Quick Share inválida"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let dirs = UserDirs::new().context("No se encontró el directorio del usuario")?;
                let path = dirs
                    .download_dir()
                    .map(Path::to_owned)
                    .unwrap_or_else(|| dirs.home_dir().join("Downloads"));
                Ok(Self {
                    download_directory: path,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn save(&self) -> Result<()> {
        let file = Self::file()?;
        let directory = file.parent().context("Configuración sin directorio")?;
        tokio::fs::create_dir_all(directory).await?;
        let temporary = directory.join(format!("settings-{}.tmp", uuid::Uuid::new_v4()));
        let contents = toml::to_string(self)?;
        tokio::fs::write(&temporary, contents).await?;
        match tokio::fs::rename(&temporary, &file).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_file(temporary).await;
                Err(error.into())
            }
        }
    }
}

#[derive(Clone, Serialize)]
struct Device {
    id: String,
    name: String,
    transport: String,
    #[serde(skip)]
    fullname: String,
    #[serde(skip)]
    last_seen: Instant,
}

#[derive(Clone, Serialize)]
struct Transfer {
    id: String,
    direction: &'static str,
    state: &'static str,
    pin_code: Option<String>,
    device_name: String,
    files: Vec<String>,
    total_bytes: u64,
    transferred_bytes: u64,
    transport: Option<String>,
    error: Option<String>,
    #[serde(skip)]
    cancellation: Option<CancellationToken>,
}

fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "rejected" | "cancelled" | "failed")
}

#[derive(Serialize)]
struct Capabilities {
    lan: bool,
    bluetooth: bool,
    hotspot: bool,
}

#[derive(Serialize)]
struct Snapshot {
    version: u32,
    instance_id: String,
    visible: bool,
    visibility_expires_at: Option<u64>,
    network_consent: bool,
    download_directory: String,
    directory_picker_supported: bool,
    devices: Vec<Device>,
    transfers: Vec<Transfer>,
    capabilities: Capabilities,
    errors: Vec<String>,
    qr_send_supported: bool,
    qr_send: Option<QrOffer>,
}

#[derive(Clone, Serialize)]
struct QrView {
    id: String,
    url: String,
    image: String,
    expires_at: u64,
    file_count: usize,
    total_bytes: u64,
}

#[derive(Serialize)]
struct QrOffer {
    #[serde(flatten)]
    view: QrView,
    #[serde(skip)]
    invitation: Arc<QrInvitation>,
    #[serde(skip)]
    files: Vec<OutboundFile>,
}

impl Snapshot {
    fn expire_qr(&mut self, at: Instant) -> bool {
        if self.qr_send.as_ref().is_some_and(|offer| offer.invitation.is_expired_at(at)) {
            self.qr_send = None;
            true
        } else {
            false
        }
    }

    fn expire_devices(&mut self, at: Instant) -> bool {
        let previous = self.devices.len();
        self.devices.retain(|device| {
            device.transport != "lan"
                || at.saturating_duration_since(device.last_seen) < LAN_PEER_TTL
        });
        self.devices.len() != previous
    }

    fn apply_device(&mut self, event: EndpointInfo, at: Instant) -> bool {
        let transport = event.transport.unwrap_or_else(|| {
            if event.id.starts_with("bt:") {
                "bluetooth"
            } else {
                "lan"
            }
            .into()
        });
        if event.present != Some(true) {
            let previous = self.devices.len();
            self.devices.retain(|device| {
                device.id != event.id
                    || device.transport != transport
                    || (!event.fullname.is_empty() && device.fullname != event.fullname)
            });
            return self.devices.len() != previous;
        }

        let previous = self.devices.len();
        self.devices.retain(|device| {
            event.fullname.is_empty()
                || device.transport != transport
                || device.fullname != event.fullname
                || device.id == event.id
        });
        let mut changed = self.devices.len() != previous;
        let name = event.name.unwrap_or_else(|| "Android".into());
        if let Some(device) = self.devices.iter_mut().find(|device| device.id == event.id) {
            changed |= device.name != name
                || device.transport != transport
                || device.fullname != event.fullname;
            device.name = name;
            device.transport = transport;
            device.fullname = event.fullname;
            device.last_seen = at;
            return changed;
        }

        if self.devices.len() == DISCOVERY_PEER_LIMIT {
            if let Some((index, _)) = self
                .devices
                .iter()
                .enumerate()
                .min_by_key(|(_, device)| device.last_seen)
            {
                self.devices.remove(index);
            }
        }
        self.devices.push(Device {
            id: event.id,
            name,
            transport,
            fullname: event.fullname,
            last_seen: at,
        });
        true
    }
}
#[derive(Serialize)]
struct SnapshotEnvelope<'a> {
    revision: u64,
    #[serde(flatten)]
    snapshot: &'a Snapshot,
}

struct Shared {
    state: RwLock<Snapshot>,
    share_selections: RwLock<ShareSelections>,
    revisions: watch::Sender<u64>,
    commands: mpsc::Sender<Operation>,
}

impl Shared {
    fn changed(&self) {
        self.revisions
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    async fn expire_devices_at(&self, at: Instant) {
        let mut state = self.state.write().await;
        if state.expire_devices(at) {
            self.changed();
        }
    }
    async fn snapshot(&self) -> Result<String> {
        self.snapshot_at(Instant::now()).await
    }
    async fn snapshot_at(&self, at: Instant) -> Result<String> {
        let mut state = self.state.write().await;
        if state.expire_devices(at) | state.expire_qr(at) {
            self.changed();
        }
        Ok(serde_json::to_string(&SnapshotEnvelope {
            revision: *self.revisions.borrow(),
            snapshot: &state,
        })?)
    }
    async fn execute(&self, command: Action) -> zbus::fdo::Result<String> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Operation { command, reply: tx })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("El motor no está disponible".into()))?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("El motor terminó".into()))?
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}

struct Interface(Arc<Shared>);

#[zbus::interface(name = "io.omarchy.QuickShare1")]
impl Interface {
    async fn snapshot(&self) -> zbus::fdo::Result<String> {
        self.0
            .snapshot()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
    async fn set_visibility(&self, seconds: u32) -> zbus::fdo::Result<()> {
        if seconds > 600 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Visibilidad permitida: 0–600 segundos".into(),
            ));
        }
        self.0
            .execute(Action::Visibility(seconds))
            .await
            .map(|_| ())
    }
    async fn set_network_consent(&self, allowed: bool) -> zbus::fdo::Result<()> {
        self.0
            .execute(Action::NetworkConsent(allowed))
            .await
            .map(|_| ())
    }
    async fn send(&self, peer_id: String, files: Vec<String>) -> zbus::fdo::Result<String> {
        self.0.execute(Action::Send(peer_id, files)).await
    }
    async fn start_qr_send(&self, files: Vec<String>) -> zbus::fdo::Result<String> {
        self.0.execute(Action::QrSend(files)).await
    }
    async fn stop_qr_send(&self, id: String) -> zbus::fdo::Result<()> {
        self.0.execute(Action::QrStop(id)).await.map(|_| ())
    }
    async fn prepare_share(&self, files: Vec<String>) -> zbus::fdo::Result<String> {
        self.0
            .share_selections
            .write()
            .await
            .prepare(files, Instant::now())
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
    }
    async fn take_share(&self, token: String) -> zbus::fdo::Result<Vec<String>> {
        self.0
            .share_selections
            .write()
            .await
            .take(&token, Instant::now())
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
    }
    async fn respond(&self, id: String, accept: bool) -> zbus::fdo::Result<()> {
        self.0
            .execute(Action::Respond(id, accept))
            .await
            .map(|_| ())
    }
    async fn cancel(&self, id: String) -> zbus::fdo::Result<()> {
        self.0.execute(Action::Cancel(id)).await.map(|_| ())
    }
    async fn set_download_directory(&self, directory: String) -> zbus::fdo::Result<()> {
        self.0
            .execute(Action::Directory(directory))
            .await
            .map(|_| ())
    }
    #[zbus(signal)]
    async fn changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        snapshot: &str,
    ) -> zbus::Result<()>;
}

enum Action {
    Visibility(u32),
    NetworkConsent(bool),
    Send(String, Vec<String>),
    QrSend(Vec<String>),
    QrStop(String),
    Respond(String, bool),
    Cancel(String),
    Directory(String),
}
struct Operation {
    command: Action,
    reply: oneshot::Sender<Result<String>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn validate_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("La carpeta debe ser absoluta");
    }
    let path = tokio::fs::canonicalize(path).await?;
    if !tokio::fs::metadata(&path).await?.is_dir() {
        bail!("El destino no es una carpeta");
    }
    // Verify write permission without overwriting any existing file.
    let probe = path.join(format!(".quickshare-check-{}", uuid::Uuid::new_v4()));
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await?;
    drop(file);
    tokio::fs::remove_file(probe).await?;
    Ok(path)
}

async fn apply_operation(
    shared: &Shared,
    engine: &mut RQS,
    sender: &mpsc::Sender<SendInfo>,
    action: Action,
) -> Result<String> {
    apply_operation_at(shared, engine, sender, action, Instant::now()).await
}

async fn apply_operation_at(
    shared: &Shared,
    engine: &mut RQS,
    sender: &mpsc::Sender<SendInfo>,
    action: Action,
    at: Instant,
) -> Result<String> {
    match action {
        Action::Visibility(seconds) => {
            engine.change_visibility(if seconds == 0 {
                Visibility::Invisible
            } else {
                Visibility::Visible
            });
            let mut state = shared.state.write().await;
            state.visible = seconds != 0;
            state.visibility_expires_at = (seconds != 0).then(|| now() + seconds as u64);
        }
        Action::NetworkConsent(allowed) => {
            engine.set_network_consent(allowed);
            shared.state.write().await.network_consent = allowed;
        }
        Action::Send(peer_id, paths) => {
            let device_name = {
                let mut state = shared.state.write().await;
                if state.expire_devices(at) {
                    shared.changed();
                }
                ensure_send_capacity(&state)?;
                state.devices.iter().find(|device| device.id == peer_id)
                    .map(|device| device.name.clone())
                    .context("El dispositivo ya no está disponible")?
            };
            let (files, total_bytes) = prepare_files(paths).await?;
            return enqueue_send(shared, sender, peer_id, device_name, files, total_bytes, None).await;
        }
        Action::QrSend(paths) => {
            {
                let state = shared.state.read().await;
                ensure_send_capacity(&state)?;
                if !state.capabilities.lan {
                    bail!("El QR nativo requiere una red local disponible en ambos dispositivos");
                }
            }
            let (files, total_bytes) = prepare_files(paths).await?;
            let invitation = Arc::new(QrInvitation::new()?);
            let view = qr_view(&invitation, files.len(), total_bytes)?;
            let response = serde_json::to_string(&view)?;
            engine.set_qr_invitation(Some(invitation.clone()))?;
            shared.state.write().await.qr_send = Some(QrOffer { view, invitation, files });
            shared.changed();
            return Ok(response);
        }
        Action::QrStop(id) => {
            let mut state = shared.state.write().await;
            if state.qr_send.as_ref().is_some_and(|offer| offer.view.id == id) {
                state.qr_send = None;
                engine.set_qr_invitation(None)?;
                shared.changed();
            }
        }
        Action::Respond(id, accept) => {
            let state = shared.state.read().await;
            let transfer = state
                .transfers
                .iter()
                .find(|t| t.id == id)
                .context("Solicitud desconocida")?;
            if transfer.state != "waiting_acceptance" {
                bail!("La transferencia no está esperando aceptación");
            }
            engine
                .message_sender
                .send(ChannelMessage {
                    id,
                    direction: ChannelDirection::FrontToLib,
                    action: Some(if accept {
                        ChannelAction::AcceptTransfer
                    } else {
                        ChannelAction::RejectTransfer
                    }),
                    ..Default::default()
                })
                .context("El motor no está escuchando solicitudes")?;
        }
        Action::Cancel(id) => {
            let state = shared.state.read().await;
            let transfer = state
                .transfers
                .iter()
                .find(|t| t.id == id)
                .context("Transferencia desconocida")?;
            if terminal(transfer.state) {
                bail!("La transferencia ya terminó");
            }
            if let Some(cancellation) = &transfer.cancellation {
                cancellation.cancel();
                shared.changed();
                return Ok(String::new());
            }
            engine
                .message_sender
                .send(ChannelMessage {
                    id,
                    direction: ChannelDirection::FrontToLib,
                    action: Some(ChannelAction::CancelTransfer),
                    ..Default::default()
                })
                .context("El motor no está escuchando cancelaciones")?;
        }
        Action::Directory(directory) => {
            if shared
                .state
                .read()
                .await
                .transfers
                .iter()
                .any(|t| !terminal(t.state))
            {
                bail!("Espera a que terminen las transferencias antes de cambiar el destino");
            }
            let directory = validate_directory(Path::new(&directory)).await?;
            Preferences {
                download_directory: directory.clone(),
            }
            .save()
            .await?;
            engine.set_download_path(Some(directory.clone()));
            shared.state.write().await.download_directory =
                directory.to_string_lossy().into_owned();
        }
    }
    shared.changed();
    Ok(String::new())
}

fn ensure_send_capacity(state: &Snapshot) -> Result<()> {
    if state.transfers.iter().filter(|transfer| !terminal(transfer.state)).count() >= 16 {
        bail!("Hay demasiadas transferencias activas");
    }
    Ok(())
}

async fn prepare_files(paths: Vec<String>) -> Result<(Vec<OutboundFile>, u64)> {
    if paths.is_empty() || paths.len() > 1000 {
        bail!("Selecciona entre 1 y 1000 archivos");
    }
    // Open once, before network/QR waits. Only owned descriptors leave this
    // worker; paths are never used again to obtain the outgoing bytes.
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::with_capacity(paths.len());
        let mut total_bytes = 0_u64;
        for path in paths {
            let file = OutboundFile::open(path)?;
            total_bytes = total_bytes.checked_add(file.size())
                .context("Tamaño total no representable")?;
            if total_bytes > MAX_TOTAL_BYTES {
                bail!("La selección supera el tamaño máximo de transferencia");
            }
            files.push(file);
        }
        Ok((files, total_bytes))
    }).await.context("No se pudo preparar la selección de archivos")?
}

fn qr_view(invitation: &QrInvitation, file_count: usize, total_bytes: u64) -> Result<QrView> {
    let svg = qrcode::QrCode::new(invitation.url().as_bytes())?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    Ok(QrView {
        id: invitation.id().to_owned(),
        url: invitation.url().to_owned(),
        image: format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg)),
        expires_at: now() + invitation.expires_at().saturating_duration_since(Instant::now()).as_secs(),
        file_count,
        total_bytes,
    })
}

async fn enqueue_send(
    shared: &Shared,
    sender: &mpsc::Sender<SendInfo>,
    peer_id: String,
    device_name: String,
    files: Vec<OutboundFile>,
    total_bytes: u64,
    qr: Option<QrSendContext>,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let cancellation = CancellationToken::new();
    let mut state = shared.state.write().await;
    ensure_send_capacity(&state)?;
    state.transfers.push(Transfer {
        id: id.clone(), direction: "outbound", state: "connecting", pin_code: None,
        device_name, files: files.iter().map(|file| file.name().to_owned()).collect(), total_bytes, transferred_bytes: 0,
        transport: None, error: None, cancellation: Some(cancellation.clone()),
    });
    drop(state);
    if let Err(error) = sender.send(SendInfo {
        id: id.clone(), addr: peer_id, ob: OutboundPayload::Files(files), cancellation, qr,
    }).await {
        let mut state = shared.state.write().await;
        if let Some(transfer) = state.transfers.iter_mut().find(|transfer| transfer.id == id) {
            transfer.state = "failed";
            transfer.error = Some(error.to_string());
        }
        shared.changed();
        return Err(error.into());
    }
    shared.changed();
    Ok(id)
}

async fn claim_qr(
    shared: &Shared,
    engine: &RQS,
    sender: &mpsc::Sender<SendInfo>,
    matched: QrPeer,
    at: Instant,
) -> Result<Option<String>> {
    let mut state = shared.state.write().await;
    if state.expire_qr(at) {
        engine.set_qr_invitation(None)?;
        shared.changed();
    }
    if !state.qr_send.as_ref().is_some_and(|offer| offer.view.id == matched.invitation_id) {
        return Ok(None);
    }
    let offer = state.qr_send.take().expect("matched pending invitation");
    drop(state);
    engine.set_qr_invitation(None)?;
    shared.changed();
    let id = enqueue_send(shared, sender, matched.peer.id,
        matched.peer.name.unwrap_or_else(|| "Android".into()), offer.files, offer.view.total_bytes,
        Some(QrSendContext { invitation: offer.invitation, endpoint_id: matched.endpoint_id })).await?;
    Ok(Some(id))
}

fn normalized_state(state: &rqs_lib::State) -> &'static str {
    use rqs_lib::State::*;
    match state {
        Initial | ReceivedConnectionRequest => "connecting",
        WaitingForUserConsent => "waiting_acceptance",
        NegotiatingTransport => "negotiating",
        ReceivingFiles | SendingFiles => "transferring",
        Finished => "completed",
        Cancelled => "cancelled",
        Rejected => "rejected",
        Disconnected => "failed",
        _ => "authenticating",
    }
}

async fn handle_message(shared: &Shared, message: ChannelMessage) -> Option<Transfer> {
    if message.direction != ChannelDirection::LibToFront {
        return None;
    }
    let mut snapshot = shared.state.write().await;
    if message.id.is_empty() {
        if let Some(available) = message.bluetooth_available {
            snapshot.capabilities.bluetooth = available;
        }
        if let Some(error) = message.error {
            if snapshot.errors.len() >= 100 {
                snapshot.errors.remove(0);
            }
            snapshot.errors.push(error);
        }
        drop(snapshot);
        shared.changed();
        return None;
    }
    let index = match snapshot.transfers.iter().position(|t| t.id == message.id) {
        Some(index) => index,
        None => {
            snapshot.transfers.push(Transfer {
                id: message.id.clone(),
                direction: match message.rtype {
                    Some(rqs_lib::channel::TransferType::Outbound) => "outbound",
                    _ => "inbound",
                },
                state: "connecting",
                pin_code: None,
                device_name: "Dispositivo cercano".into(),
                files: Vec::new(),
                total_bytes: 0,
                transferred_bytes: 0,
                transport: None,
                error: None,
                cancellation: None,
            });
            snapshot.transfers.len() - 1
        }
    };
    let transfer = &mut snapshot.transfers[index];
    // Late socket/worker events cannot reopen or relabel a completed transfer.
    if terminal(transfer.state) {
        return None;
    }
    let previous = transfer.state;
    if let Some(state) = message.state {
        transfer.state = normalized_state(&state);
    }
    if let Some(meta) = message.meta {
        if let Some(source) = meta.source {
            transfer.device_name = source.name;
        }
        if let Some(pin) = meta.pin_code {
            transfer.pin_code = Some(pin);
        }
        if let Some(files) = meta.files {
            transfer.files = files;
        }
        transfer.total_bytes = meta.total_bytes;
        transfer.transferred_bytes = meta.ack_bytes;
    }
    if let Some(transport) = message.transport {
        transfer.transport = Some(transport);
    }
    if let Some(error) = message.error {
        transfer.error = Some(error);
        transfer.state = "failed";
    }
    if terminal(transfer.state) {
        transfer.cancellation = None;
    }
    let notification = (previous != transfer.state
        && (transfer.state == "waiting_acceptance" || terminal(transfer.state)))
    .then(|| transfer.clone());
    // Bound retained history; never discard an active request.
    while snapshot.transfers.len() > 100 {
        if let Some(index) = snapshot.transfers.iter().position(|t| terminal(t.state)) {
            snapshot.transfers.remove(index);
        } else {
            break;
        }
    }
    drop(snapshot);
    shared.changed();
    notification
}

async fn handle_device(shared: &Shared, event: EndpointInfo) {
    handle_device_at(shared, event, Instant::now()).await;
}

async fn handle_device_at(shared: &Shared, event: EndpointInfo, at: Instant) {
    let mut state = shared.state.write().await;
    if state.apply_device(event, at) {
        shared.changed();
    }
}

async fn handle_device_lagged(shared: &Shared, count: u64) {
    let mut state = shared.state.write().await;
    state.devices.clear();
    if state.errors.len() >= 100 {
        state.errors.remove(0);
    }
    state
        .errors
        .push(format!("Se perdieron {count} eventos de descubrimiento"));
    shared.changed();
}

async fn notify(connection: &zbus::Connection, transfer: &Transfer, config: &Path) -> Result<u32> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let language = UiLanguage::load(config).await?;
    let pending = transfer.state == "waiting_acceptance";
    let title = if pending && transfer.direction == "outbound" {
        language.text("Verify the PIN to send", "Verifica el PIN para enviar")
    } else if pending {
        language.text("Quick Share request", "Solicitud de Quick Share")
    } else {
        "Quick Share"
    };
    let mut body = format!(
        "{} — {}",
        transfer.device_name,
        language.state(transfer.state)
    );
    if let Some(pin) = &transfer.pin_code {
        body.push_str(&format!("\n{}: {pin}", language.text("Code", "Código")));
    }
    if pending {
        body.push_str(language.text(
            "\nConfirm that the code matches on both devices before accepting.",
            "\nConfirma que el código coincide en ambos dispositivos antes de aceptar.",
        ));
    }
    if let Some(error) = &transfer.error {
        body.push_str(&format!("\n{error}"));
    }
    // Notification servers may interpret markup; escape untrusted peer names and errors.
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let actions: &[&str] = match (language, pending) {
        (UiLanguage::English, true) => &[
            "default",
            "Open",
            "accept",
            "Confirm PIN",
            "reject",
            "Reject",
        ],
        (UiLanguage::Spanish, true) => &[
            "default",
            "Abrir",
            "accept",
            "Confirmar PIN",
            "reject",
            "Rechazar",
        ],
        (UiLanguage::English, false) => &["default", "Open"],
        (UiLanguage::Spanish, false) => &["default", "Abrir"],
    };
    let hints: BTreeMap<&str, zbus::zvariant::Value<'_>> = BTreeMap::new();
    Ok(proxy
        .call(
            "Notify",
            &(
                "Quick Share",
                0_u32,
                "document-send",
                title,
                body,
                actions,
                hints,
                if pending { 0_i32 } else { 5000_i32 },
            ),
        )
        .await?)
}

pub async fn serve(port: u16, download: Option<PathBuf>, options: EngineOptions) -> Result<()> {
    let preferences = Preferences::load()?;
    let shell_config = UserDirs::new()
        .context("Cannot find the user's home directory")?
        .home_dir()
        .join(".config/omarchy/shell.json");
    let directory = download.unwrap_or(preferences.download_directory);
    tokio::fs::create_dir_all(&directory).await?;
    let directory = validate_directory(&directory).await?;
    let mut engine = RQS::with_options(
        Visibility::Invisible,
        Some(port as u32),
        Some(directory.clone()),
        options.clone(),
    );
    let mut events = engine.message_sender.subscribe();
    let (devices_tx, mut devices_rx) = broadcast::channel(64);
    let (commands_tx, mut commands_rx) = mpsc::channel(32);
    let (revisions, mut revision_rx) = watch::channel(0_u64);
    let capabilities = local_capabilities(&options).await;
    let shared = Arc::new(Shared {
        state: RwLock::new(Snapshot {
            version: 1,
            visible: false,
            visibility_expires_at: None,
            network_consent: false,
            instance_id: uuid::Uuid::new_v4().to_string(),
            download_directory: directory.to_string_lossy().into_owned(),
            directory_picker_supported: true,
            devices: Vec::new(),
            transfers: Vec::new(),
            capabilities,
            errors: Vec::new(),
            qr_send_supported: true,
            qr_send: None,
        }),
        share_selections: RwLock::new(ShareSelections::default()),
        revisions,
        commands: commands_tx,
    });
    // Acquire the session name before starting radio tasks, so a second instance cannot advertise.
    let connection = zbus::connection::Builder::session()?
        .name(BUS)?
        .serve_at(PATH, Interface(shared.clone()))?
        .build()
        .await?;
    let integration = match NautilusIntegration::new() {
        Ok(integration) => integration,
        Err(error) => {
            shared.state.write().await.errors.push(format!(
                "No se pudo iniciar la retirada automática del menú Nautilus: {error:#}"
            ));
            shared.changed();
            None
        }
    };
    let integration_monitor = async move {
        match integration {
            Some(integration) => integration.watch().await,
            None => std::future::pending::<Result<()>>().await,
        }
    };
    tokio::pin!(integration_monitor);
    let mut integration_monitor_active = true;
    let outgoing = engine.run().await?;
    engine.discovery(devices_tx)?;
    let mut qr_matches = engine.take_qr_matches()?;
    let signal_connection = connection.clone();
    let signal_shared = shared.clone();
    let publisher = tokio::spawn(async move {
        while revision_rx.changed().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            revision_rx.borrow_and_update();
            match signal_shared.snapshot().await {
                Ok(snapshot) => {
                    if let Err(error) = signal_connection
                        .emit_signal(None::<&str>, PATH, INTERFACE, "Changed", &(snapshot,))
                        .await
                    {
                        tracing::warn!(%error, "No se pudo emitir estado D-Bus");
                    }
                }
                Err(error) => tracing::error!(%error, "No se pudo serializar estado"),
            }
        }
    });
    let mut expiry = tokio::time::interval(Duration::from_secs(1));
    let mut notifications = BTreeMap::<u32, (String, u64, bool)>::new();
    let notification_proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let mut actions = notification_proxy.receive_signal("ActionInvoked").await?;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        tokio::select! {
            result = &mut integration_monitor, if integration_monitor_active => {
                integration_monitor_active = false;
                if let Err(error) = result {
                    shared.state.write().await.errors.push(format!(
                        "La retirada automática del menú Nautilus dejó de funcionar: {error:#}"
                    ));
                    shared.changed();
                }
            }
            _ = &mut interrupt => break,
            Some(operation) = commands_rx.recv() => {
                let result = apply_operation(&shared, &mut engine, &outgoing, operation.command).await;
                let _ = operation.reply.send(result);
            }
            Some(matched) = qr_matches.recv() => {
                if let Err(error) = claim_qr(&shared, &engine, &outgoing, matched, Instant::now()).await {
                    let mut state = shared.state.write().await;
                    if state.errors.len() >= 100 { state.errors.remove(0); }
                    state.errors.push(format!("No se pudo iniciar el envío QR: {error}"));
                    shared.changed();
                }
            }
            event = events.recv() => match event {
                Ok(event) => if let Some(transfer) = handle_message(&shared, event).await {
                    match notify(&connection, &transfer, &shell_config).await {
                        Ok(id) => {
                            let pending = transfer.state == "waiting_acceptance";
                            notifications.insert(id, (transfer.id, now() + if pending { 600 } else { 60 }, pending));
                        }
                        Err(error) => tracing::warn!(%error, "Notificaciones no disponibles"),
                    }
                },
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    shared.state.write().await.errors.push(format!("Se perdieron {count} eventos del motor")); shared.changed();
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            event = devices_rx.recv() => match event {
                Ok(event) => handle_device(&shared, event).await,
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    handle_device_lagged(&shared, count).await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(action) = futures_util::StreamExt::next(&mut actions) => {
                if let Ok((id, action)) = action.body().deserialize::<(u32, String)>() {
                    if let Some((transfer, _, _)) = notifications.remove(&id) {
                        if action == "default" {
                            let payload = serde_json::json!({ "transfer_id": transfer }).to_string();
                            tokio::spawn(async move {
                                if let Err(error) = tokio::process::Command::new("omarchy-shell")
                                    .args(["shell", "summon", "macondo.quickshare", &payload]).status().await {
                                    tracing::warn!(%error, "No se pudo abrir el panel");
                                }
                            });
                        } else if matches!(action.as_str(), "accept" | "reject") {
                            if let Err(error) = apply_operation(&shared, &mut engine, &outgoing, Action::Respond(transfer, action == "accept")).await {
                                tracing::warn!(%error, "Solicitud ya no disponible");
                            }
                        }
                    }
                }
            },
            _ = expiry.tick() => {
                shared.share_selections.write().await.expire(Instant::now());
                shared.expire_devices_at(Instant::now()).await;
                let mut state = shared.state.write().await;
                if state.expire_qr(Instant::now()) { shared.changed(); }
                if state.qr_send.is_none() { let _ = engine.set_qr_invitation(None); }
                drop(state);
                let expired = shared.state.read().await.visibility_expires_at.is_some_and(|deadline| now() >= deadline);
                if expired { let _ = apply_operation(&shared, &mut engine, &outgoing, Action::Visibility(0)).await; }
                let stale: Vec<u32> = {
                    let state = shared.state.read().await;
                    notifications.iter().filter_map(|(id, (transfer, deadline, pending))| {
                        (now() >= *deadline || (*pending && !state.transfers.iter().any(|t| t.id == *transfer && t.state == "waiting_acceptance")))
                            .then_some(*id)
                    }).collect()
                };
                for id in stale {
                    notifications.remove(&id);
                    let _: zbus::Result<()> = notification_proxy.call("CloseNotification", &(id,)).await;
                }
            }
        }
    }
    engine.stop().await;
    publisher.abort();
    Ok(())
}

async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn local_capabilities(options: &EngineOptions) -> Capabilities {
    let hotspot = command_output(
        "nmcli",
        &["-t", "-f", "WIFI-PROPERTIES.AP", "device", "show"],
    )
    .await
    .is_some_and(|s| s.lines().any(|l| l.ends_with(":yes")));
    let lan = command_output("nmcli", &["-t", "-f", "TYPE,STATE", "device"])
        .await
        .is_some_and(|s| {
            s.lines()
                .any(|l| l.starts_with("wifi:connected") || l.starts_with("ethernet:connected"))
        });
    Capabilities {
        lan: options.lan && lan,
        // The backend publishes readiness and withdrawal, not merely radio power.
        bluetooth: false,
        hotspot: options.hotspot && hotspot,
    }
}

pub async fn doctor() -> Result<()> {
    let mut result = BTreeMap::new();
    for (name, program, args) in [
        ("omarchy", "omarchy", vec!["version"]),
        ("quickshell", "quickshell", vec!["--version"]),
        ("bluez", "bluetoothctl", vec!["--version"]),
        ("bluetooth", "btmgmt", vec!["info"]),
        ("wifi", "iw", vec!["list"]),
        (
            "network_permissions",
            "nmcli",
            vec!["general", "permissions"],
        ),
        ("android_usb", "adb", vec!["devices", "-l"]),
    ] {
        result.insert(
            name,
            command_output(program, &args)
                .await
                .unwrap_or_else(|| "No disponible o acceso denegado".into()),
        );
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFile(PathBuf);
    impl TestFile {
        fn new(name: &str, contents: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("quickshare-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
    }
    impl Drop for TestFile {
        fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
    }

    fn shared_for_test() -> (Arc<Shared>, mpsc::Receiver<Operation>) {
        let (revisions, _) = watch::channel(0);
        let (commands, receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            state: RwLock::new(Snapshot {
                version: 1,
                visible: false,
                visibility_expires_at: None,
                network_consent: false,
                instance_id: "test-instance".into(),
                download_directory: "/tmp".into(),
                directory_picker_supported: true,
                devices: vec![],
                transfers: vec![],
                capabilities: Capabilities {
                    lan: true,
                    bluetooth: false,
                    hotspot: false,
                },
                errors: vec![],
                qr_send_supported: true,
                qr_send: None,
            }),
            share_selections: RwLock::new(ShareSelections::default()),
            revisions,
            commands,
        });
        (shared, receiver)
    }

    fn peer(fullname: &str, id: &str, transport: Option<&str>) -> EndpointInfo {
        EndpointInfo {
            fullname: fullname.into(),
            id: id.into(),
            name: Some("Pixel".into()),
            present: Some(true),
            transport: transport.map(str::to_owned),
            ..Default::default()
        }
    }

    async fn snapshot_json_at(shared: &Shared, at: Instant) -> serde_json::Value {
        serde_json::from_str(&shared.snapshot_at(at).await.unwrap()).unwrap()
    }

    fn offer_for_test(files: Vec<OutboundFile>) -> QrOffer {
        let invitation = Arc::new(QrInvitation::new().unwrap());
        let view = qr_view(&invitation, files.len(), files.iter().map(OutboundFile::size).sum()).unwrap();
        QrOffer { view, invitation, files }
    }

    #[tokio::test]
    async fn qr_snapshot_keeps_selected_paths_and_signing_context_private_and_expires_at_boundary() {
        let (shared, _) = shared_for_test();
        let file = TestFile::new("selected-secret-name", b"QR selection");
        let offer = offer_for_test(vec![OutboundFile::open(&file.0).unwrap()]);
        let expires = offer.invitation.expires_at();
        shared.state.write().await.qr_send = Some(offer);
        let snapshot = snapshot_json_at(&shared, expires - Duration::from_nanos(1)).await;
        assert!(snapshot["qr_send"].is_object());
        assert!(snapshot["qr_send"].get("files").is_none());
        assert!(snapshot["qr_send"].get("invitation").is_none());
        assert!(!snapshot.to_string().contains("selected-secret-name"));
        assert!(snapshot_json_at(&shared, expires).await["qr_send"].is_null());
        assert!(shared.state.read().await.qr_send.is_none());
    }

    #[tokio::test]
    async fn stopping_an_old_qr_cannot_withdraw_a_replacement_or_cancel_a_transfer() {
        let (shared, _) = shared_for_test();
        let old = offer_for_test(vec![]);
        let current = offer_for_test(vec![]);
        let current_id = current.view.id.clone();
        shared.state.write().await.qr_send = Some(current);
        let mut engine = RQS::default();
        let (sender, _) = mpsc::channel(1);
        apply_operation(&shared, &mut engine, &sender, Action::QrStop(old.view.id)).await.unwrap();
        assert_eq!(shared.state.read().await.qr_send.as_ref().unwrap().view.id, current_id);
        apply_operation(&shared, &mut engine, &sender, Action::QrStop(current_id)).await.unwrap();
        let state = shared.state.read().await;
        assert!(state.qr_send.is_none());
        assert!(state.transfers.is_empty());
    }

    #[tokio::test]
    async fn oversized_qr_replacement_preserves_the_previous_selection() {
        let first = TestFile::new("large-first", b"x");
        let second = TestFile::new("large-second", b"y");
        std::fs::OpenOptions::new().write(true).open(&first.0).unwrap()
            .set_len(MAX_TOTAL_BYTES / 2).unwrap();
        let second_writer = std::fs::OpenOptions::new().write(true).open(&second.0).unwrap();
        second_writer.set_len(MAX_TOTAL_BYTES / 2).unwrap();
        let paths = vec![first.0.to_str().unwrap().to_owned(), second.0.to_str().unwrap().to_owned()];
        let (boundary, total) = prepare_files(paths.clone()).await.unwrap();
        assert_eq!(total, MAX_TOTAL_BYTES);
        drop(boundary);
        second_writer.set_len(MAX_TOTAL_BYTES / 2 + 1).unwrap();

        let selected = TestFile::new("previous-qr", b"previous selection");
        let previous = offer_for_test(vec![OutboundFile::open(&selected.0).unwrap()]);
        let previous_id = previous.view.id.clone();
        let (shared, _) = shared_for_test();
        shared.state.write().await.qr_send = Some(previous);
        let mut engine = RQS::default();
        let (sender, _) = mpsc::channel(1);
        assert!(apply_operation(&shared, &mut engine, &sender, Action::QrSend(paths)).await.is_err());
        let state = shared.state.read().await;
        assert_eq!(state.qr_send.as_ref().unwrap().view.id, previous_id);
        assert_eq!(state.qr_send.as_ref().unwrap().view.total_bytes, 18);
        assert!(state.transfers.is_empty());
    }

    #[tokio::test]
    async fn qr_claim_ignores_wrong_and_replayed_invitations_and_starts_only_one_transfer() {
        let file = TestFile::new("selected", b"QR selection");
        let (shared, _) = shared_for_test();
        let offer = offer_for_test(vec![OutboundFile::open(&file.0).unwrap()]);
        let id = offer.view.id.clone();
        shared.state.write().await.qr_send = Some(offer);
        let engine = RQS::default();
        let (sender, mut receiver) = mpsc::channel(2);
        let candidate = |id: String| QrPeer {
            invitation_id: id, peer: peer("", "127.0.0.2:47593", Some("lan")), endpoint_id: "PEER".into(),
        };
        assert!(claim_qr(&shared, &engine, &sender, candidate("old-invitation".into()), Instant::now()).await.unwrap().is_none());
        assert!(shared.state.read().await.transfers.is_empty());
        assert!(claim_qr(&shared, &engine, &sender, candidate(id.clone()), Instant::now()).await.unwrap().is_some());
        assert!(claim_qr(&shared, &engine, &sender, candidate(id), Instant::now()).await.unwrap().is_none());
        let state = shared.state.read().await;
        assert_eq!(state.transfers.len(), 1);
        assert!(state.qr_send.is_none());
        assert!(receiver.try_recv().is_ok());
        assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn address_churn_retires_old_ids_without_a_withdrawal_event() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        for port in 47593..47849 {
            let id = format!("192.0.2.10:{port}");
            handle_device_at(&shared, peer("pixel._tcp.local.", &id, Some("lan")), at).await;
            let snapshot = snapshot_json_at(&shared, at).await;
            assert_eq!(
                snapshot["devices"],
                serde_json::json!([{ "id": id, "name": "Pixel", "transport": "lan" }])
            );
        }
    }

    #[tokio::test]
    async fn discovery_count_is_bounded_across_lan_and_bluetooth_and_evicts_oldest() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        for index in 0..512 {
            let id = if index % 2 == 0 {
                format!("192.0.2.{}:{}", index % 254 + 1, 47000 + index)
            } else {
                format!("bt:AA:BB:CC:DD:{:02X}:{:02X}", index >> 8, index & 255)
            };
            let mut event = peer(&format!("peer-{index}"), &id, None);
            event.name = Some(format!("Peer {index}"));
            let seen = at + Duration::from_millis(index);
            handle_device_at(&shared, event, seen).await;
            let snapshot = snapshot_json_at(&shared, seen).await;
            assert_eq!(
                snapshot["devices"].as_array().unwrap().len(),
                ((index + 1) as usize).min(DISCOVERY_PEER_LIMIT)
            );
        }
        let retained_first = 512 - DISCOVERY_PEER_LIMIT;
        let id = format!(
            "192.0.2.{}:{}",
            retained_first % 254 + 1,
            47000 + retained_first
        );
        let mut refresh = peer(&format!("peer-{retained_first}"), &id, None);
        refresh.name = Some(format!("Peer {retained_first}"));
        handle_device_at(&shared, refresh, at + Duration::from_secs(1)).await;
        let mut newcomer = peer("newcomer", "192.0.2.254:49999", None);
        newcomer.name = Some("Newcomer".into());
        handle_device_at(&shared, newcomer, at + Duration::from_secs(2)).await;
        let snapshot = snapshot_json_at(&shared, at + Duration::from_secs(2)).await;
        let devices = snapshot["devices"].as_array().unwrap();
        assert_eq!(devices.len(), DISCOVERY_PEER_LIMIT);
        let expected_evicted = format!("Peer {}", retained_first + 1);
        let expected_retained = format!("Peer {retained_first}");
        assert!(devices.iter().any(|device| device["name"] == expected_retained));
        assert!(!devices.iter().any(|device| device["name"] == expected_evicted));
        assert!(devices.iter().any(|device| device["name"] == "Newcomer"));
    }

    #[tokio::test]
    async fn metadata_refresh_preserves_order_and_renews_the_lan_lease() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        let renewal = at + LAN_PEER_TTL / 2;
        let event = peer("pixel._tcp.local.", "192.0.2.10:47593", None);
        handle_device_at(&shared, event.clone(), at).await;
        handle_device_at(
            &shared,
            peer("other._tcp.local.", "192.0.2.11:47593", None),
            at,
        )
        .await;
        let before = snapshot_json_at(&shared, at).await;
        handle_device_at(&shared, event.clone(), renewal).await;
        assert_eq!(snapshot_json_at(&shared, renewal).await, before);
        let mut metadata = event;
        metadata.name = Some("Renamed Pixel".into());
        handle_device_at(&shared, metadata, renewal).await;
        let renamed = snapshot_json_at(&shared, renewal).await;
        assert_eq!(renamed["devices"][0]["id"], "192.0.2.10:47593");
        assert_eq!(renamed["devices"][0]["name"], "Renamed Pixel");
        assert_eq!(renamed["devices"][1]["id"], "192.0.2.11:47593");

        shared.expire_devices_at(at + LAN_PEER_TTL).await;
        let renewed = snapshot_json_at(&shared, at + LAN_PEER_TTL).await;
        assert_eq!(renewed["devices"].as_array().unwrap().len(), 1);
        assert_eq!(renewed["devices"][0]["id"], "192.0.2.10:47593");
        assert_eq!(
            snapshot_json_at(&shared, renewal + LAN_PEER_TTL - Duration::from_nanos(1))
                .await["devices"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        shared.expire_devices_at(renewal + LAN_PEER_TTL).await;
        let expired = snapshot_json_at(&shared, renewal + LAN_PEER_TTL).await;
        assert!(expired["devices"].as_array().unwrap().is_empty());
        assert_eq!(expired["visible"], false);
    }

    #[tokio::test]
    async fn snapshot_expires_lan_at_the_boundary_without_changing_active_transfers() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        handle_device_at(
            &shared,
            peer("pixel._tcp.local.", "192.0.2.10:47593", None),
            at,
        )
        .await;
        handle_message(
            &shared,
            ChannelMessage {
                id: "active-session".into(),
                direction: ChannelDirection::LibToFront,
                state: Some(rqs_lib::State::WaitingForUserConsent),
                transport: Some("lan".into()),
                ..Default::default()
            },
        )
        .await;
        let cancellation = CancellationToken::new();
        {
            let mut state = shared.state.write().await;
            state.network_consent = true;
            state.transfers[0].cancellation = Some(cancellation.clone());
        }
        let before =
            snapshot_json_at(&shared, at + LAN_PEER_TTL - Duration::from_nanos(1)).await;
        assert_eq!(before["devices"].as_array().unwrap().len(), 1);
        let after = snapshot_json_at(&shared, at + LAN_PEER_TTL).await;
        assert!(after["devices"].as_array().unwrap().is_empty());
        assert_eq!(after["visible"], false);
        assert_eq!(after["visibility_expires_at"], serde_json::Value::Null);
        assert_eq!(after["network_consent"], true);
        assert_eq!(after["transfers"], before["transfers"]);
        assert!(!cancellation.is_cancelled());
        assert!(after["revision"].as_u64().unwrap() > before["revision"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn late_old_owner_withdrawals_cannot_remove_the_current_route_owner() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        let id = "192.0.2.10:47593";
        handle_device_at(&shared, peer("old._tcp.local.", id, None), at).await;
        let mut current = peer("new._tcp.local.", id, None);
        current.name = Some("Current owner".into());
        handle_device_at(&shared, current.clone(), at).await;
        let mut old = peer("old._tcp.local.", id, None);
        old.present = Some(false);
        handle_device_at(&shared, old, at).await;
        let mut wrong_transport = current.clone();
        wrong_transport.present = Some(false);
        wrong_transport.transport = Some("bluetooth".into());
        handle_device_at(&shared, wrong_transport, at).await;
        let snapshot = snapshot_json_at(&shared, at).await;
        assert_eq!(
            snapshot["devices"],
            serde_json::json!([{ "id": id, "name": "Current owner", "transport": "lan" }])
        );
        current.present = Some(false);
        handle_device_at(&shared, current, at).await;
        assert!(snapshot_json_at(&shared, at).await["devices"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn bluetooth_change_only_discovery_does_not_expire_on_the_lan_clock() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        handle_device_at(
            &shared,
            peer("", "bt:AA:BB:CC:DD:EE:FF", None),
            at,
        )
        .await;
        handle_device_at(
            &shared,
            peer("pixel._tcp.local.", "192.0.2.10:47593", None),
            at,
        )
        .await;
        let snapshot = snapshot_json_at(&shared, at + LAN_PEER_TTL * 4).await;
        assert_eq!(
            snapshot["devices"],
            serde_json::json!([{
                "id": "bt:AA:BB:CC:DD:EE:FF",
                "name": "Pixel",
                "transport": "bluetooth"
            }])
        );
        assert_eq!(
            snapshot_json_at(&shared, at + LAN_PEER_TTL * 8).await,
            snapshot
        );
        let mut withdrawal = peer("", "bt:AA:BB:CC:DD:EE:FF", None);
        withdrawal.present = Some(false);
        handle_device_at(&shared, withdrawal, at + LAN_PEER_TTL * 8).await;
        assert!(snapshot_json_at(&shared, at + LAN_PEER_TTL * 8).await["devices"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn send_rejects_retired_and_expired_peers_before_enqueuing_a_transfer() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        let old_id = "192.0.2.10:47593";
        let new_id = "192.0.2.11:47594";
        handle_device_at(&shared, peer("pixel._tcp.local.", old_id, None), at).await;
        let renewed = at + Duration::from_secs(1);
        handle_device_at(&shared, peer("pixel._tcp.local.", new_id, None), renewed).await;
        let mut engine = RQS::with_options(
            Visibility::Invisible,
            None,
            Some(PathBuf::from("/tmp")),
            EngineOptions {
                bluetooth: false,
                lan: false,
                hotspot: false,
            },
        );
        let (sender, mut requests) = mpsc::channel(1);
        for (id, send_at) in [(old_id, renewed), (new_id, renewed + LAN_PEER_TTL)] {
            apply_operation_at(
                &shared,
                &mut engine,
                &sender,
                Action::Send(id.into(), vec!["/not-opened-for-a-stale-peer".into()]),
                send_at,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                requests.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            let snapshot = snapshot_json_at(&shared, send_at).await;
            assert!(snapshot["transfers"].as_array().unwrap().is_empty());
        }
        assert!(snapshot_json_at(&shared, renewed + LAN_PEER_TTL).await["devices"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn discovery_channel_lag_clears_the_public_device_model_and_advances_revision() {
        let (shared, _commands) = shared_for_test();
        let at = Instant::now();
        handle_device_at(
            &shared,
            peer("pixel._tcp.local.", "192.0.2.10:47593", None),
            at,
        )
        .await;
        handle_device_at(
            &shared,
            peer("", "bt:AA:BB:CC:DD:EE:FF", None),
            at,
        )
        .await;
        let before = snapshot_json_at(&shared, at).await;
        handle_device_lagged(&shared, 65).await;
        let after = snapshot_json_at(&shared, at).await;
        assert!(after["devices"].as_array().unwrap().is_empty());
        assert_eq!(
            after["revision"].as_u64().unwrap(),
            before["revision"].as_u64().unwrap() + 1
        );
        assert_eq!(after["visible"], before["visible"]);
        assert_eq!(after["network_consent"], before["network_consent"]);
        assert_eq!(after["transfers"], before["transfers"]);
        assert_eq!(after["errors"].as_array().unwrap().len(), 1);
        for _ in 0..150 {
            handle_device_lagged(&shared, 65).await;
        }
        assert_eq!(snapshot_json_at(&shared, at).await["errors"].as_array().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn prepared_selections_do_not_change_snapshot_or_enqueue_engine_actions() {
        let (shared, mut commands) = shared_for_test();
        let before = shared.snapshot().await.unwrap();
        let interface = Interface(shared.clone());
        let files = vec!["/private/selected file".to_owned()];
        let token = interface.prepare_share(files.clone()).await.unwrap();
        assert_eq!(shared.snapshot().await.unwrap(), before);
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(interface.take_share(token.clone()).await.unwrap(), files);
        assert!(interface.take_share(token).await.is_err());
        assert_eq!(shared.snapshot().await.unwrap(), before);
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn socket_close_cannot_turn_completed_transfer_into_failure() {
        let (shared, _commands) = shared_for_test();
        handle_message(
            &shared,
            ChannelMessage {
                id: "session".into(),
                direction: ChannelDirection::LibToFront,
                state: Some(rqs_lib::State::WaitingForUserConsent),
                transport: Some("bluetooth".into()),
                ..Default::default()
            },
        )
        .await;
        handle_message(
            &shared,
            ChannelMessage {
                id: "session".into(),
                direction: ChannelDirection::LibToFront,
                state: Some(rqs_lib::State::Finished),
                ..Default::default()
            },
        )
        .await;
        handle_message(
            &shared,
            ChannelMessage {
                id: "session".into(),
                direction: ChannelDirection::LibToFront,
                state: Some(rqs_lib::State::Disconnected),
                error: Some("Connection closed after completion".into()),
                ..Default::default()
            },
        )
        .await;
        let state = shared.state.read().await;
        assert_eq!(state.transfers[0].state, "completed");
        assert_eq!(state.transfers[0].transport.as_deref(), Some("bluetooth"));
        assert_eq!(state.transfers[0].error, None);
    }
}
