// Copyright 2020, 2022 Google LLC
// Copyright 2026 omarchy-android-quickshare contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     https://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Name/BLE encodings adapted from Google Nearby at
// d128b5fca30b3fa8d4642f8c75dd9e314e090e74:
// connections/implementation/{bluetooth_device_name,ble_advertisement}.cc,
// connections/implementation/mediums/ble/{ble_advertisement,
// ble_advertisement_header,bloom_filter,ble_utils}.cc,
// connections/implementation/mediums/ble.cc, sharing/advertisement.cc.
// Linux transport implementation is original.

//! BlueZ SDP/RFCOMM transport; BLE is discovery, never a GATT data socket.
//! One engine-lifetime server profile owns the SDP listener. Outgoing sessions
//! resolve the remote RFCOMM channel with SDP and own their native sockets;
//! they never consume Profile1 callbacks. This separates simultaneous incoming
//! and outgoing connections to the same MAC and makes cancellation close only
//! the attempted socket, not another transfer's Device1 profile.
//!
//! Visible/Temporarily visibility explicitly requests temporary advertising,
//! including an encoded adapter Alias. Each owned Alias/discoverability property
//! is restored independently when visibility ends and on shutdown; intervening
//! user changes to that property are preserved. Powered is never changed. Callers
//! must cancel and await `run`, not abort its task, for confirmed restoration
//! (Drop additionally schedules one best-effort attempt). A private, durable
//! per-adapter journal recovers owned settings after SIGKILL/restart, including
//! when the next startup is invisible. It never stores session keys.
//!
//! Visible Sharing endpoint info cannot use Nearby fast advertisements: even
//! a one-byte name needs 19 bytes, whereas fast endpoint info allows only 17.
//! Publish Google's regular legacy discovery instead: a 17-byte FEF3 header
//! (24 advertising bytes including UUID/AD framing and Flags) points to a
//! read-only GATT advertisement slot containing the full regular frame.
//! GATT exposes discovery metadata only; sessions still use SDP/RFCOMM. BLE
//! setup failure is logged with its full chain and leaves Classic operational.
//! Remote regular headers are bloom-filtered before a bounded GATT slot fetch.
//! Discovery metadata shares a 128-address/45-second lease. A bounded global
//! signal stream avoids retaining BlueR subscriptions for rotating devices.
//!
//! A persistent D-Bus owner explicitly starts/stops only our discovery session.
//! Per-controller serialization pauses scanning and awaits Discovering=false
//! before outgoing GATT connection work or advertising transitions; normal,
//! canceled and timed-out outgoing reads confirm their owned bearer disconnect.
//! Discovery owned by other clients causes a reported fail-closed state, not
//! foreign StopDiscovery, radio power cycling or assumed firmware recovery.
//! Visible mode alternates confirmed advertising (8s) and discovery (4s).
//! All live LE bearers defer role transitions, even without a metadata read.
//! Read-only kernel snapshots distinguish LE from unrelated Classic links.
//! Cancellation is not a fabricated disconnect and never disconnects a peer.
//! Hidden mode otherwise scans continuously. There is no stable own scan+AD
//! concurrency. Scheduled withdrawal retains GATT, Classic discoverability,
//! the server profile and every established file-data socket. An expired scan
//! window cannot restart after a delayed GATT cleanup. The healthy policy cycle
//! is 12s (<45s peer lease); confirmation/readers/cleanup may defer it. Failures
//! stop the scheduler rather than retry/reset.
//! A separate, awaited lifetime publisher task keeps Profile1 admission
//! responsive while serialized radio work waits; timers are policy deadlines,
//! never sleeps guessed to let firmware settle.
//! D-Bus advertising unregister replies confirm daemon ownership removal;
//! BlueZ does not expose a separate per-advertisement firmware completion ack.
//! Other clients can race a D-Bus state snapshot; the lock serializes our
//! commands, not theirs, and does not establish a controller firmware fix.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use bluer::rfcomm::{Profile, ProfileHandle, ReqError, Role};
use bluer::{l2cap, rfcomm};
use bluer::{Adapter, Address, AddressType, Session};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio::time::{timeout, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::channel::{ChannelDirection, ChannelMessage};
use crate::transport::TransportStream;
use crate::{DeviceType, EndpointInfo, Visibility};

#[path = "bluetooth_discovery.rs"]
mod discovery;
use discovery::{DiscoveryRecords, Observation};

pub const SERVICE_UUID: Uuid = Uuid::from_u128(0xa82efa21ae5c3dde9bbcf16da7b16c5a);
const BLE_UUID: Uuid = Uuid::from_u128(0x0000fef300001000800000805f9b34fb);
const SERVICE_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];
const BLE_ADVERTISEMENT_SLOT: Uuid = Uuid::from_u128(0x00000000000030008000000000000000);
const BLE_LEGACY_HEADER_LEN: usize = 17;
const BLE_LEGACY_AD_LEN: usize = 3 + 4 + BLE_LEGACY_HEADER_LEN;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const PEER_LIFETIME: Duration = Duration::from_secs(45);
const BLE_GATT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BLE_ADVERTISING_WINDOW: Duration = Duration::from_secs(8);
const BLE_SCANNING_WINDOW: Duration = Duration::from_secs(4);
const MAX_ENDPOINT_INFO: usize = 131;
const SDP_PSM: u16 = 1;
const SDP_MTU: u16 = 1024;
const MAX_SDP_ATTRIBUTES: usize = 65_535;
const MAX_SDP_CONTINUATION: usize = 16;


const HCI_GET_CONNECTION_LIST: libc::c_ulong = 0x800448d4;
const LE_LINK: u8 = 0x80;
const BT_CLOSED: u16 = 9;
const MAX_CONTROLLER_CONNECTIONS: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HciConnection {
    handle: u16,
    address: [u8; 6],
    link_type: u8,
    outgoing: u8,
    state: u16,
    link_mode: u32,
}

#[repr(C)]
struct HciConnectionList {
    index: u16,
    count: u16,
    connections: [HciConnection; MAX_CONTROLLER_CONNECTIONS],
}

impl HciConnectionList {
    fn links(&self) -> Result<&[HciConnection]> {
        ensure!(
            usize::from(self.count) < self.connections.len(),
            "Bluetooth connection snapshot reached its capacity"
        );
        Ok(&self.connections[..usize::from(self.count)])
    }

    fn has_le_bearer(&self) -> Result<bool> {
        Ok(self.links()?.iter().any(|link| link.link_type == LE_LINK && link.state != BT_CLOSED))
    }

    fn has_classic_bearer(&self, mut address: Address) -> Result<bool> {
        address.0.reverse();
        Ok(self.links()?.iter().any(|link| link.link_type == 1 && link.state != BT_CLOSED && link.address == address.0))
    }
}

struct HciConnections {
    fd: OwnedFd,
    index: u16,
}

impl HciConnections {
    fn new(adapter: &str) -> Result<Self> {
        let index = adapter
            .strip_prefix("hci")
            .context("invalid Bluetooth adapter name")?
            .parse()?;
        // An unbound HCI raw socket permits this read-only ioctl as an
        // ordinary user. Do not bind a monitor, send commands, or add caps.
        let fd = unsafe {
            libc::socket(libc::AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 1)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error())
                .context("opening read-only Bluetooth connection query");
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            index,
        })
    }

    fn snapshot(&self) -> Result<HciConnectionList> {
        let mut list = HciConnectionList {
            index: self.index,
            count: MAX_CONTROLLER_CONNECTIONS as u16,
            connections: [HciConnection::default(); MAX_CONTROLLER_CONNECTIONS],
        };
        // Linux HCIGETCONNLIST writes the native C request and connection
        // array; all storage is initialized, aligned and bounded on the stack.
        if unsafe {
            libc::ioctl(self.fd.as_raw_fd(), HCI_GET_CONNECTION_LIST, &mut list)
        } < 0 {
            return Err(io::Error::last_os_error())
                .context("reading Bluetooth controller connections");
        }
        Ok(list)
    }

    fn has_le_bearer(&self) -> Result<bool> { self.snapshot()?.has_le_bearer() }
    fn has_classic_bearer(&self, address: Address) -> Result<bool> { self.snapshot()?.has_classic_bearer(address) }
}
// All scanner ownership and HCI-changing transitions for this controller go
// through one lock. D-Bus StopDiscovery is issued by the same persistent owner
// that started it, and Discovering=false is awaited before GATT/AD changes.
// Another client's discovery session is never stopped on our behalf.
struct RadioCoordinator {
    adapter: Adapter,
    bus: zbus::Connection,
    owner: String,
    path: String,
    state: Mutex<RadioState>,
    paused: watch::Sender<bool>,
    schedule: watch::Sender<Option<BleRoleWindow>>,
    connections: HciConnections,
}

struct RadioState {
    scan_requested: bool,
    scan_active: bool,
    failed: bool,
    advertisement_active: bool,
    window: Option<BleRoleWindow>,
    le_connected: bool,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BleRole {
    Advertising,
    Scanning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BleRoleWindow {
    role: BleRole,
    until: Instant,
}

impl BleRoleWindow {
    fn new(role: BleRole, now: Instant) -> Self {
        let duration = match role {
            BleRole::Advertising => BLE_ADVERTISING_WINDOW,
            BleRole::Scanning => BLE_SCANNING_WINDOW,
        };
        Self {
            role,
            until: now + duration,
        }
    }

    fn next(self, now: Instant) -> Self {
        let role = match self.role {
            BleRole::Advertising => BleRole::Scanning,
            BleRole::Scanning => BleRole::Advertising,
        };
        // A delayed transition grants the next full policy window, never a
        // burst of catch-up Start/Stop commands for missed timer ticks.
        Self::new(role, now)
    }
}

impl RadioState {
    fn permits_scan(&self, now: Instant) -> bool {
        self.scan_requested
            && !self.failed
            && !self.advertisement_active
            && !self.le_connected
            && self
                .window
                .is_none_or(|window| window.role == BleRole::Scanning && now < window.until)
    }

    fn role_transition_due(&self, now: Instant) -> bool {
        !self.failed
            && !self.le_connected
            && self.window.is_some_and(|window| now >= window.until)
    }

    fn begin_ble_header_read(
        &self,
        read: &mut Option<BleHeaderRead>,
        header: BleLegacyHeader,
        now: Instant,
    ) -> bool {
        if !self.permits_scan(now) {
            return false;
        }
        if read.as_ref().is_some_and(|previous| previous.header == header) {
            return false;
        }
        *read = Some(BleHeaderRead { header, frame: None });
        true
    }
}

async fn bluez_call<T: serde::Serialize + zbus::zvariant::Type>(
    bus: &zbus::Connection,
    owner: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &T,
) -> Result<()> {
    let proxy = zbus::Proxy::new(bus, owner, path, interface).await?;
    timeout(OPERATION_TIMEOUT, proxy.call::<_, _, ()>(method, body))
        .await
        .with_context(|| format!("BlueZ {interface}.{method} timed out"))?
        .with_context(|| format!("BlueZ {interface}.{method}"))
}

impl RadioCoordinator {
    async fn new(adapter: Adapter) -> Result<Arc<Self>> {
        let bus = timeout(OPERATION_TIMEOUT, zbus::Connection::system())
            .await
            .context("opening owned Bluetooth coordination bus timed out")??;
        let owner = discovery::bluez_owner(&bus).await?;
        let (paused, _) = watch::channel(true);
        let (schedule, _) = watch::channel(None);
        let connections = HciConnections::new(adapter.name())?;
        Ok(Arc::new(Self {
            path: format!("/org/bluez/{}", adapter.name()),
            adapter,
            bus,
            owner,
            state: Mutex::new(RadioState {
                scan_requested: false,
                scan_active: false,
                failed: false,
                advertisement_active: false,
                window: None,
                le_connected: false,
            }),
            paused,
            schedule,
            connections,
        }))
    }

    fn refresh_connections(&self, state: &mut RadioState) -> Result<()> {
        let connected = self.connections.has_le_bearer()?;
        if connected != state.le_connected {
            debug!("Nearby LE bearer gate: connected={connected}; Classic links do not gate roles");
        }
        state.le_connected = connected;
        Ok(())
    }

    fn set_window(&self, state: &mut RadioState, window: Option<BleRoleWindow>) {
        state.window = window;
        self.schedule.send_replace(window);
    }

    async fn pause_gatt(&self) -> Result<Option<tokio::sync::MutexGuard<'_, RadioState>>> {
        let mut state = self.state.lock().await;
        self.refresh_connections(&mut state)?;
        ensure!(
            !state.failed,
            "Bluetooth radio coordination is failed closed"
        );
        // A radio packet can be queued just as the scheduler switches to AD.
        // Defer that fetch, without consuming its one-read header allowance.
        if !state.permits_scan(Instant::now()) {
            return Ok(None);
        }
        if let Err(error) = self.stop(&mut state).await {
            state.failed = true;
            return Err(error);
        }
        let instances = match self.adapter.active_advertising_instances().await {
            Ok(instances) => instances,
            Err(error) => {
                state.failed = true;
                return Err(error.into());
            }
        };
        if instances != 0 {
            state.failed = true;
            bail!("Another client's advertisement prevents a safe central GATT role; it will not be withdrawn");
        }
        if !state.permits_scan(Instant::now()) {
            return Ok(None);
        }
        Ok(Some(state))
    }


    async fn wait_discovering(&self, wanted: bool) -> Result<()> {
        let mut events = discovery::signals(&self.bus, &self.owner, Some(&self.path)).await?;
        if self.adapter.is_discovering().await? == wanted {
            return Ok(());
        }
        timeout(OPERATION_TIMEOUT, async {
            while let Some(event) = events.next().await {
                let event = event?;
                if let Some(properties) = discovery::changed_properties(&event, "org.bluez.Adapter1")? {
                    if properties.get("Discovering").is_some_and(|value| bool::try_from(value).ok() == Some(wanted)) {
                        return Ok(());
                    }
                    if properties.get("Powered").is_some_and(|value| bool::try_from(value).ok() == Some(false)) {
                        bail!("Bluetooth powered off while awaiting discovery transition");
                    }
                }
            }
            bail!("Bluetooth adapter removed while awaiting discovery transition")
        }).await.with_context(|| format!(
            "Bluetooth Discovering={wanted} was not confirmed; another client's discovery or an unresponsive controller prevents safe coordination"
        ))?
    }

    async fn stop(&self, state: &mut RadioState) -> Result<()> {
        self.paused.send_replace(true);
        if state.scan_active {
            bluez_call(
                &self.bus,
                &self.owner,
                &self.path,
                "org.bluez.Adapter1",
                "StopDiscovery",
                &(),
            )
            .await?;
            state.scan_active = false;
        } else {
            ensure!(!self.adapter.is_discovering().await?,
                "Bluetooth controller is busy discovering for another client; no foreign StopDiscovery will be issued");
        }
        self.wait_discovering(false).await
    }

    async fn start(&self, state: &mut RadioState) -> Result<()> {
        self.refresh_connections(state)?;
        if state.le_connected {
            return Ok(());
        }
        ensure!(
            !state.failed,
            "Bluetooth radio coordination is failed closed"
        );
        ensure!(
            !state.advertisement_active,
            "Bluetooth scanner cannot start while our advertisement is active"
        );
        ensure!(self.adapter.active_advertising_instances().await? == 0,
            "Bluetooth controller is advertising for another client; scanner start refused without withdrawing its advertisement");
        ensure!(
            self.adapter.is_powered().await?,
            "Bluetooth controller powered off; scanner will not auto-reset or repower it"
        );
        ensure!(!self.adapter.is_discovering().await?,
            "Bluetooth controller is busy discovering for another client; its session will not be stopped");
        // Arm ownership before the reply: a timeout does not prove the
        // daemon failed to acquire our session.
        state.scan_active = true;
        bluez_call(
            &self.bus,
            &self.owner,
            &self.path,
            "org.bluez.Adapter1",
            "StartDiscovery",
            &(),
        )
        .await?;
        self.wait_discovering(true).await?;
        self.paused.send_replace(false);
        Ok(())
    }

    async fn pause(&self) -> Result<tokio::sync::MutexGuard<'_, RadioState>> {
        let mut state = self.state.lock().await;
        ensure!(
            !state.failed,
            "Bluetooth radio coordination is failed closed"
        );
        if let Err(error) = self.stop(&mut state).await {
            state.failed = true;
            return Err(error);
        }
        Ok(state)
    }

    async fn resume(&self, state: &mut RadioState) -> Result<()> {
        self.refresh_connections(state)?;
        if state.permits_scan(Instant::now()) && !state.scan_active {
            if let Err(error) = self.start(state).await {
                state.failed = true;
                return Err(error);
            }
        }
        Ok(())
    }

    async fn set_scanning(&self, requested: bool) -> Result<()> {
        let mut state = self.state.lock().await;
        state.scan_requested = requested;
        let result: Result<()> = async {
            if requested {
                ensure!(
                    !state.failed,
                    "Bluetooth radio coordination is failed closed"
                );
                let filter: HashMap<&str, zbus::zvariant::Value<'_>> = [
                    ("Transport", zbus::zvariant::Value::from("auto")),
                    ("DuplicateData", zbus::zvariant::Value::from(true)),
                    ("Discoverable", zbus::zvariant::Value::from(false)),
                ]
                .into();
                bluez_call(
                    &self.bus,
                    &self.owner,
                    &self.path,
                    "org.bluez.Adapter1",
                    "SetDiscoveryFilter",
                    &(filter,),
                )
                .await?;
                self.resume(&mut state).await
            } else {
                self.stop(&mut state).await?;
                let empty: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
                bluez_call(
                    &self.bus,
                    &self.owner,
                    &self.path,
                    "org.bluez.Adapter1",
                    "SetDiscoveryFilter",
                    &(empty,),
                )
                .await
            }
        }
        .await;
        if result.is_err() {
            state.failed = true;
        }
        result
    }
}

pub struct Bluetooth {
    local_address: Address,
    adapter: Adapter,
    radio: Arc<RadioCoordinator>,
    endpoint_id: [u8; 4],
    endpoint_info: Vec<u8>,
    encoded_name: String,
    visibility: watch::Receiver<Visibility>,
    // Backend lifecycle only; authenticated sessions own transfer events.
    events: broadcast::Sender<ChannelMessage>,
    running: Mutex<()>,
    scanning: Mutex<()>,
    server: Mutex<ProfileHandle>,
    journal: Arc<StateJournal>,
}

impl std::fmt::Debug for Bluetooth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Bluetooth")
            .field("adapter", &self.adapter.name())
            .finish_non_exhaustive()
    }
}

impl Bluetooth {
    pub async fn new(
        endpoint_id: [u8; 4],
        name: String,
        visibility: watch::Receiver<Visibility>,
        events: broadcast::Sender<ChannelMessage>,
    ) -> Result<Self> {
        ensure!(
            endpoint_id.iter().all(u8::is_ascii_alphanumeric),
            "invalid endpoint id"
        );
        let endpoint_info = encode_endpoint_info(&name)?;
        let encoded_name = encode_classic_name(endpoint_id, &endpoint_info)?;
        let session = timeout(OPERATION_TIMEOUT, Session::new())
            .await
            .context("opening BlueZ session timed out")??;
        let adapter = timeout(OPERATION_TIMEOUT, session.default_adapter())
            .await
            .context("finding Bluetooth adapter timed out")??;
        let local_address = adapter.address().await?;
        let journal = Arc::new(StateJournal::open(local_address)?);
        timeout(OPERATION_TIMEOUT, journal.recover(&adapter))
            .await
            .context("recovering Bluetooth adapter settings timed out")??;
        ensure!(
            adapter.is_powered().await?,
            "Bluetooth adapter is powered off; enable it explicitly"
        );
        let radio = RadioCoordinator::new(adapter.clone()).await?;
        // BlueZ rejects registering the same UUID twice, even for opposite
        // roles. Keep exactly one server registration for the engine lifetime.
        let server = timeout(OPERATION_TIMEOUT, session.register_profile(profile()))
            .await
            .context("registering NearbySharing RFCOMM profile timed out")??;
        Ok(Self {
            local_address,
            adapter,
            radio,
            endpoint_id,
            endpoint_info,
            encoded_name,
            visibility,
            events,
            server: Mutex::new(server),
            running: Mutex::new(()),
            scanning: Mutex::new(()),
            journal,
        })
    }

    fn availability(&self, available: bool) {
        let _ = self.events.send(ChannelMessage {
            direction: ChannelDirection::LibToFront,
            bluetooth_available: Some(available),
            ..Default::default()
        });
    }

    pub async fn run(
        self: Arc<Self>,
        incoming: mpsc::Sender<TransportStream>,
        ctk: CancellationToken,
    ) -> Result<()> {
        let _running = self
            .running
            .try_lock()
            .context("Bluetooth server already running")?;
        let mut visibility = self.visibility.clone();
        // Hiding changes only announcements and callback admission. The
        // engine-owned profile and established RFCOMM sockets remain alive.
        let mut server = self
            .server
            .try_lock()
            .context("Bluetooth RFCOMM callback dispatcher already running")?;
        let mut adapter_events = discovery::signals(&self.radio.bus, &self.radio.owner, Some(&self.radio.path)).await?;
        // Radio transitions can wait for another peer's GATT cleanup. A
        // single lifetime task owns announcements so those waits never stall
        // the persistent Profile1 callback dispatcher or established sockets.
        let announcements_ctk = ctk.child_token();
        let mut announcements_task =
            tokio::spawn(self.clone().run_announcements(announcements_ctk.clone()));
        let mut announcements_finished = false;
        self.availability(true);
        let mut owner_health = tokio::time::interval(Duration::from_secs(5));
        owner_health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let outcome: Result<()> = async {
            loop {
                if ctk.is_cancelled() { break; }
                tokio::select! {
                    biased;
                    _ = ctk.cancelled() => break,
                    result = &mut announcements_task => {
                        announcements_finished = true;
                        result.context("Bluetooth announcements task failed")??;
                        break;
                    }
                    changed = visibility.changed() => {
                        changed.context("Bluetooth visibility owner closed")?;
                    }
                    _ = owner_health.tick() => {
                        ensure!(discovery::bluez_owner(&self.radio.bus).await? == self.radio.owner,
                            "BlueZ owner changed; Bluetooth backend must be restarted");
                        ensure!(self.adapter.address().await? == self.local_address,
                            "Bluetooth adapter identity changed");
                    }
                    event = adapter_events.next() => {
                        let event = event.context("Bluetooth adapter was removed")??;
                        if matches!(discovery::observation(&event, &self.radio.path)?, Some(Observation::PoweredOff)) {
                            bail!("Bluetooth powered off while backend was running");
                        }
                    }
                    request = server.next() => {
                        let request = request.context("BlueZ server profile was released")?;
                        // Recheck after wakeup: hidden mode must not accept queued callbacks.
                        if *visibility.borrow() == Visibility::Invisible || ctk.is_cancelled() {
                            request.reject(ReqError::Rejected);
                            continue;
                        }
                        match incoming.try_reserve() {
                            Ok(permit) => {
                                let stream = request.accept().context("accepting RFCOMM descriptor")?;
                                permit.send(Box::new(stream));
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => request.reject(ReqError::Rejected),
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                request.reject(ReqError::Canceled);
                                bail!("Bluetooth incoming session receiver closed");
                            }
                        }
                    }
                }
            }
            Ok(())
        }.await;
        self.availability(false);
        announcements_ctk.cancel();
        if !announcements_finished {
            if let Err(error) = announcements_task
                .await
                .context("Bluetooth announcements task failed")?
            {
                return Err(error.context(format!(
                    "Bluetooth announcements cleanup failed; server result: {outcome:?}"
                )));
            }
        }
        outcome
    }

    async fn run_announcements(self: Arc<Self>, ctk: CancellationToken) -> Result<()> {
        let mut visibility = self.visibility.clone();
        let mut schedule = self.radio.schedule.subscribe();
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.bluez")?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace(self.radio.path.as_str())?
            .arg(0, "org.bluez.Device1")?
            .build();
        let mut connections =
            zbus::MessageStream::for_match_rule(rule, &self.radio.bus, None).await?;
        let mut visible: Option<Announcements> = None;
        let outcome: Result<()> = async {
            loop {
                if ctk.is_cancelled() { break; }
                let requested = *visibility.borrow_and_update() != Visibility::Invisible;
                let reading = {
                    let mut state = self.radio.state.lock().await;
                    self.radio.refresh_connections(&mut state)?;
                    if !requested && visible.is_none() {
                        self.radio.resume(&mut state).await?;
                    }
                    state.le_connected
                };
                if requested && visible.is_none() && !reading {
                    visible = Some(self.announce(&ctk).await?);
                } else if !requested {
                    if let Some(mut announcements) = visible.take() {
                        announcements.restore().await?;
                    }
                }
                let window = *schedule.borrow_and_update();
                tokio::select! {
                    biased;
                    _ = ctk.cancelled() => break,
                    changed = visibility.changed() => {
                        changed.context("Bluetooth visibility owner closed")?;
                    }
                    changed = schedule.changed() => {
                        changed.context("Bluetooth role scheduler owner closed")?;
                    }
                    changed = async {
                        loop {
                            let message = connections.next().await
                                .context("Bluetooth connection observation ended")??;
                            let body = message.body();
                            let (_, properties, invalidated) = body.deserialize::<(
                                &str,
                                HashMap<&str, zbus::zvariant::Value<'_>>,
                                Vec<&str>,
                            )>()?;
                            if properties.contains_key("Connected")
                                || invalidated.contains(&"Connected")
                            {
                                break Ok::<(), anyhow::Error>(());
                            }
                        }
                    } => {
                        changed?;
                    }
                    _ = async {
                        if reading {
                            // BlueZ Connected combines Classic and LE, so a
                            // second bearer may change without a signal. This
                            // passive policy tick releases an LE-only gate
                            // even while a Classic headset remains connected.
                            tokio::time::sleep(BLE_SCANNING_WINDOW).await;
                        } else {
                            match window {
                                Some(window) => tokio::time::sleep_until(window.until).await,
                                None => std::future::pending::<()>().await,
                            }
                        }
                    }, if reading || visible.as_ref().is_some_and(|announcements| announcements.ble.is_some()) => {
                        if reading { continue; }
                        if *visibility.borrow() == Visibility::Invisible { continue; }
                        if let Some(announcements) = visible.as_mut() {
                            announcements.advance_role(&ctk, &visibility).await?;
                        }
                    }
                }
            }
            Ok(())
        }.await;
        // No operation is aborted mid-D-Bus call. Cancellation reaches this
        // awaited restoration, with discovery restart disabled on shutdown.
        if let Some(mut announcements) = visible {
            if ctk.is_cancelled() {
                self.radio.state.lock().await.scan_requested = false;
            }
            if let Err(error) = announcements.restore().await {
                return Err(error.context(format!(
                    "Bluetooth restoration failed; announcement result: {outcome:?}"
                )));
            }
        }
        outcome
    }

    pub async fn connect(&self, route: &str) -> Result<TransportStream> {
        let address = parse_route(route)?;
        timeout(OPERATION_TIMEOUT, async {
            let channel = resolve_rfcomm_channel(self.local_address, address).await?;
            let socket = rfcomm::Socket::new().context("opening outgoing RFCOMM socket")?;
            socket
                .bind(rfcomm::SocketAddr::new(self.local_address, 0))
                .context("binding outgoing RFCOMM socket to Bluetooth adapter")?;
            socket
                .set_security(rfcomm::Security {
                    level: rfcomm::SecurityLevel::Low,
                    key_size: 0,
                })
                .context("setting outgoing RFCOMM socket security")?;
            // Deregister the bound fd before connect; BlueR's cached writable
            // event can otherwise report completion while the kernel is pending.
            let address = NativeRfcommAddress::new(address, channel);
            let fd = unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) };
            let fd = unsafe { connect_owned_socket(fd, &address).await }
                .context("connecting NearbySharing RFCOMM channel discovered with SDP")?;
            let stream = unsafe { rfcomm::Stream::from_raw_fd(fd.into_raw_fd()) }
                .context("registering connected NearbySharing RFCOMM stream")?;
            Ok(Box::new(stream) as TransportStream)
        })
        .await
        .context("NearbySharing SDP/RFCOMM connection timed out")?
        // Dropping this future, including the timeout path, owns and closes
        // only its SDP/RFCOMM descriptors. There is no pending BlueZ callback
        // that could be mistaken for the next transfer to this MAC.
    }

    pub async fn discover(
        self: Arc<Self>,
        sender: broadcast::Sender<EndpointInfo>,
        ctk: CancellationToken,
    ) -> Result<()> {
        let _scanning = self
            .scanning
            .try_lock()
            .context("Bluetooth discovery already running")?;
        let mut records = DiscoveryRecords::default();
        let outcome: Result<()> = async {
            // One bounded subscription, not one BlueR receiver per MAC. BlueR
            // retains closed per-device subscriptions until another event.
            let mut events = discovery::signals(&self.radio.bus, &self.radio.owner, None).await?;
            self.radio.set_scanning(true).await?;
            let mut paused = self.radio.paused.subscribe();
            let mut expiry = tokio::time::interval(Duration::from_secs(5));
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let observation = tokio::select! {
                    _ = ctk.cancelled() => break,
                    changed = paused.changed() => {
                        changed.context("Bluetooth radio pause owner closed")?;
                        continue;
                    }
                    _ = expiry.tick() => {
                        ensure!(discovery::bluez_owner(&self.radio.bus).await? == self.radio.owner,
                            "BlueZ owner changed during Bluetooth discovery");
                        ensure!(self.adapter.address().await? == self.local_address,
                            "Bluetooth adapter identity changed during discovery");
                        records.expire(Instant::now(), &sender);
                        continue;
                    }
                    message = events.next() => {
                        let message = message.context("Bluetooth discovery ended unexpectedly")??;
                        discovery::observation(&message, &self.radio.path)?
                    }
                };
                let (address, evidence, advertised) = match observation {
                    Some(Observation::Added(address)) => (address, DiscoveryEvidence::Added, None),
                    Some(Observation::Radio(address, evidence, data)) => (address, evidence, data),
                    Some(Observation::Removed(address)) => {
                        records.remove(address, &sender);
                        continue;
                    }
                    Some(Observation::PoweredOff) => bail!("Bluetooth powered off during discovery"),
                    None => continue,
                };
                if *paused.borrow() { continue; }
                let observed = Instant::now();
                let index = records.admit(address, observed, &sender);
                let device = self.adapter.device(address)?;
                let observation = match self.read_peer(&device, evidence, advertised.as_ref(), &mut records.entries[index].header, &ctk).await {
                    Ok(value) => value,
                    Err(error) => {
                        if self.radio.state.lock().await.failed { return Err(error); }
                        warn!("Reading Bluetooth discovery for {address} failed: {error:#}");
                        continue;
                    }
                };
                records.update(index, observation, evidence, observed, &sender);
            }
            Ok(())
        }.await;
        records.clear(&sender);
        let cleanup = self.radio.set_scanning(false).await;
        match cleanup {
            Ok(()) => outcome,
            Err(error) => Err(error.context(format!(
                "Bluetooth discovery cleanup failed; scanner result: {outcome:?}"
            ))),
        }
    }

    async fn read_peer(
        &self,
        device: &bluer::Device,
        evidence: DiscoveryEvidence,
        advertised: Option<&BleServiceData>,
        ble_header: &mut Option<BleHeaderRead>,
        ctk: &CancellationToken,
    ) -> Result<Option<(EndpointInfo, DiscoverySource)>> {
        if device.rssi().await?.is_none() {
            return Ok(None);
        }
        let mut parsed = None;
        if evidence.observes(DiscoverySource::Classic)
            && device.address_type().await? != AddressType::LeRandom
            && device.class().await?.is_some()
        {
            if let Some(name) = device.name().await? {
                if let Ok(info) = parse_classic_name(&name) {
                    parsed = Some((device.address(), info, DiscoverySource::Classic));
                }
            }
        }
        if evidence.observes(DiscoverySource::Ble) {
            // Keep the radio event's actual snapshot. A later Get can return
            // an unrelated advertisement that arrived while GATT was running.
            let queried = if advertised.is_none() {
                device.service_data().await?
            } else {
                None
            };
            if let Some(data) = advertised.or(queried.as_ref()) {
                if let Some(bytes) = data.get(&BLE_UUID) {
                    let frame = if let Ok(frame) = parse_ble_service_data(bytes) {
                        Some(frame)
                    } else if let Ok(header) = parse_ble_legacy_header(bytes) {
                        if ble_header.as_ref().is_none_or(|read| read.header != header) {
                            match self
                                .read_remote_ble_slots(device, header, ble_header, ctk)
                                .await
                            {
                                Ok(frame) => {
                                    if let Some(read) = ble_header.as_mut()
                                        .filter(|read| read.header == header)
                                    {
                                        read.frame = frame;
                                    }
                                }
                                Err(error) => {
                                    if self.radio.state.lock().await.failed {
                                        return Err(error);
                                    }
                                    warn!("Nearby GATT discovery for {} unavailable: {error:#}", device.address());
                                }
                            }
                        }
                        ble_header.as_ref()
                            .filter(|read| read.header == header)
                            .and_then(|read| read.frame.clone())
                    } else {
                        None
                    };
                    if let Some((address, info)) = frame {
                        // Route only with the regular frame's explicit Classic
                        // MAC; the LE scan address may be random. Native SDP
                        // sockets do not require a preexisting Classic Device1.
                        parsed = Some((address, info, DiscoverySource::Ble));
                    }
                }
            }
        }
        let Some((address, info, source)) = parsed else {
            return Ok(None);
        };
        if info.endpoint_id == self.endpoint_id {
            return Ok(None);
        }
        let route = format!("bt:{address}/{SERVICE_UUID}");
        Ok(Some((
            EndpointInfo {
                fullname: format!("endpoint:{}", std::str::from_utf8(&info.endpoint_id)?),
                id: route,
                name: Some(info.name),
                rtype: Some(info.device_type),
                present: Some(true),
                transport: Some("bluetooth".into()),
                ..Default::default()
            },
            source,
        )))
    }

    async fn read_remote_ble_slots(
        &self,
        device: &bluer::Device,
        header: BleLegacyHeader,
        ble_header: &mut Option<BleHeaderRead>,
        ctk: &CancellationToken,
    ) -> Result<Option<(Address, ParsedEndpoint)>> {
        let Some(mut radio) = self.radio.pause_gatt().await? else {
            return Ok(None);
        };
        // The allowance is consumed only after an exclusive central-role
        // permit. A packet queued across an AD transition is deferred, not
        // recorded as a failed read and not retried after an actual attempt.
        if !radio.begin_ble_header_read(ble_header, header, Instant::now()) {
            self.radio.resume(&mut radio).await?;
            return Ok(None);
        }
        let slots = header.slots;
        let mut connection = GattDiscoveryConnection {
            device: None,
            radio: self.radio.clone(),
        };
        let fetch = async {
            let path = discovery::device_path(&self.radio.path, device.address());
            let policy = discovery::device_policy(&self.radio.bus, &self.radio.owner, &path).await?;
            if policy.blocked { return Ok(None); }
            if !policy.connected {
                let count = discovery::device_count(&self.radio.bus, &self.radio.owner, &self.radio.path).await?;
                if !policy.may_connect(count) || self.radio.connections.has_classic_bearer(device.address())? {
                    return Ok(None);
                }
                // Live adapter-wide census, not a resettable process quota:
                // public/static Connect promotions remain in BlueZ after
                // Disconnect and across our restarts. Never RemoveDevice.
                connection.device = Some(device.clone());
                if let Err(error) = bluez_call(&self.radio.bus, &self.radio.owner, &path, "org.bluez.Device1", "Connect", &()).await {
                    if matches!(error.downcast_ref::<zbus::Error>(),
                        Some(zbus::Error::MethodError(name, _, _)) if matches!(name.as_str(),
                            "org.bluez.Error.AlreadyConnected" | "org.bluez.Error.InProgress")) {
                        connection.device = None;
                    }
                    return Err(error.context("connecting advertisement GATT bearer"));
                }
            }
            discovery::read_slots(&self.radio.bus, &self.radio.owner, &path, slots).await
        };
        let result: Result<_> = tokio::select! {
            _ = ctk.cancelled() => Err(anyhow::anyhow!("Nearby GATT discovery canceled")),
            result = timeout(BLE_GATT_READ_TIMEOUT, fetch) =>
                result.context("Nearby GATT discovery timed out").and_then(|value| value),
        };
        // Neither cancellation nor timeout drops an armed connection guard:
        // Disconnect and its Connected=false confirmation finish under the
        // controller lock before any scanner resume or AD transition.
        if let Err(error) = connection.close().await {
            radio.failed = true;
            return Err(error.context(format!("GATT cleanup failed; read result: {result:?}")));
        }
        if ctk.is_cancelled() {
            radio.scan_requested = false;
        }
        self.radio.resume(&mut radio).await?;
        result
    }

    async fn announce(&self, ctk: &CancellationToken) -> Result<Announcements> {
        let mut radio = self.radio.pause().await?;
        let settings: Result<OwnedAdapterSettings> = async {
            Ok(OwnedAdapterSettings {
                alias: OwnedProperty {
                    previous: self.adapter.alias().await?,
                    applied: self.encoded_name.clone(),
                    pending: true,
                },
                discoverable: OwnedProperty {
                    previous: self.adapter.is_discoverable().await?,
                    applied: true,
                    pending: false,
                },
                timeout: OwnedProperty {
                    previous: self.adapter.discoverable_timeout().await?,
                    applied: 0,
                    pending: false,
                },
            })
        }
        .await;
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => {
                if ctk.is_cancelled() {
                    radio.scan_requested = false;
                }
                self.radio.resume(&mut radio).await?;
                return Err(error);
            }
        };
        let mut announcements = Announcements {
            adapter: self.adapter.clone(),
            radio: self.radio.clone(),
            settings: None,
            restore_on_drop: true,
            ble: None,
            journal: self.journal.clone(),
        };
        let result: Result<()> = timeout(OPERATION_TIMEOUT, async {
            // Publish ownership before each individual D-Bus call: cancellation
            // of its reply does not prove BlueZ left the property unchanged.
            self.journal.record(&settings)?;
            announcements.settings = Some(settings);
            info!("Quick Share visibility temporarily encodes the Bluetooth adapter Alias; it will be restored when hidden");
            self.adapter.set_alias(self.encoded_name.clone()).await?;
            let settings = announcements.settings.as_mut().context("missing Bluetooth ownership")?;
            settings.timeout.pending = true;
            self.journal.save(settings)?;
            self.adapter.set_discoverable_timeout(0).await?;
            settings.discoverable.pending = true;
            self.journal.save(settings)?;
            self.adapter.set_discoverable(true).await?;
            Ok(())
        }).await.context("Bluetooth Classic announcement setup timed out").and_then(|result| result);
        if let Err(error) = result {
            announcements.restore_on_drop = false;
            announcements
                .restore_paused(&mut radio)
                .await
                .context(format!("restoring after announcement error: {error:#}"))?;
            if ctk.is_cancelled() {
                radio.scan_requested = false;
            }
            self.radio.resume(&mut radio).await?;
            return Err(error);
        }
        // BLE is independent of the valid Classic name/profile. In particular,
        // neither a GATT registration error nor an advertising timeout hides
        // Classic or terminates the RFCOMM callback dispatcher.
        match self.announce_ble(ctk, &mut radio).await {
            Ok(ble) => {
                announcements.ble = Some(ble);
                self.radio.set_window(&mut radio, Some(BleRoleWindow::new(
                    BleRole::Advertising, Instant::now(),
                )));
                info!("Nearby BLE legacy discovery registered: {BLE_LEGACY_AD_LEN}/31 advertising bytes, read-only GATT slot; transfers use Classic RFCOMM");
            }
            Err(error) if radio.failed => {
                announcements.restore_on_drop = false;
                return Err(error.context("Bluetooth radio failed closed during BLE cleanup; owned adapter journal retained"));
            }
            Err(error) => warn!("Nearby BLE discovery unavailable (requires {BLE_LEGACY_AD_LEN} legacy advertising bytes and a read-only GATT server); Classic discovery/RFCOMM remain active: {error:#}"),
        }
        if ctk.is_cancelled() {
            radio.scan_requested = false;
        }
        if let Err(error) = self.radio.resume(&mut radio).await {
            announcements.restore_on_drop = false;
            return Err(error.context("Bluetooth radio failed closed while resuming scanner; owned adapter journal retained"));
        }
        Ok(announcements)
    }

    async fn announce_ble(
        &self,
        ctk: &CancellationToken,
        state: &mut RadioState,
    ) -> Result<BleAnnouncements> {
        let data =
            encode_ble_service_data(self.endpoint_id, &self.endpoint_info, self.local_address)?;
        let header = encode_ble_legacy_header(&data);
        let root = format!("/org/omarchy/QuickShare/ble/{}", Uuid::new_v4().as_simple());
        let mut announcements = BleAnnouncements {
            radio: self.radio.clone(),
            available: Arc::new(AtomicBool::new(true)),
            application: zbus::zvariant::OwnedObjectPath::try_from(format!("{root}/gatt"))?,
            advertisement: zbus::zvariant::OwnedObjectPath::try_from(format!(
                "{root}/advertisement"
            ))?,
            service: zbus::zvariant::OwnedObjectPath::try_from(format!("{root}/gatt/service0"))?,
            slot: zbus::zvariant::OwnedObjectPath::try_from(format!("{root}/gatt/service0/char0"))?,
            application_registered: false,
            advertisement_registered: false,
            application_exported: false,
            advertisement_exported: false,
            service_exported: false,
            slot_exported: false,
        };
        let result: Result<()> = async {
            let server = self.radio.bus.object_server();
            server
                .at(announcements.service.as_str(), NearbyGattService)
                .await?;
            announcements.service_exported = true;
            server
                .at(
                    announcements.slot.as_str(),
                    NearbyGattSlot {
                        service: announcements.service.clone(),
                        data,
                        available: announcements.available.clone(),
                        visibility: self.visibility.clone(),
                        ctk: ctk.clone(),
                    },
                )
                .await?;
            announcements.slot_exported = true;
            server
                .at(announcements.application.as_str(), zbus::fdo::ObjectManager)
                .await?;
            announcements.application_exported = true;
            server
                .at(
                    announcements.advertisement.as_str(),
                    NearbyLeAdvertisement {
                        header,
                        available: announcements.available.clone(),
                    },
                )
                .await?;
            announcements.advertisement_exported = true;
            let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            ensure!(!ctk.is_cancelled(), "Bluetooth announcement setup canceled");
            announcements.application_registered = true;
            bluez_call(
                &self.radio.bus,
                &self.radio.owner,
                &self.radio.path,
                "org.bluez.GattManager1",
                "RegisterApplication",
                &(&announcements.application, &options),
            )
            .await?;
            ensure!(!ctk.is_cancelled(), "Bluetooth announcement setup canceled");
            announcements.publish_advertisement(state).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if let Err(cleanup) = announcements.close(state).await {
                state.failed = true;
                return Err(cleanup.context(format!(
                    "BLE setup failed: {error:#}; unregister was not confirmed"
                )));
            }
            return Err(error);
        }
        Ok(announcements)
    }
}

// BlueZ's src/profile.c rejects any second registration of a UUID. Its default
// role enables both directions, but Profile1.NewConnection carries no direction
// or attempt id (BlueR 0.17.4 exposes only MAC, Version and Features). A shared
// callback queue therefore cannot safely distinguish same-MAC incoming/outgoing
// attempts. Register the server once and use independently owned SDP/RFCOMM
// sockets for outgoing sessions, instead of routing callbacks by MAC or guessing
// a channel. Channel 0 asks BlueZ to allocate and publish the local SDP channel.
// https://github.com/bluez/bluez/blob/master/src/profile.c
// https://docs.rs/bluer/0.17.4/bluer/rfcomm/struct.ConnectRequest.html
fn profile() -> Profile {
    Profile {
        uuid: SERVICE_UUID,
        name: Some("NearbySharing".into()),
        channel: Some(0),
        role: Some(Role::Server),
        require_authentication: Some(false),
        require_authorization: Some(false),
        auto_connect: Some(false),
        ..Default::default()
    }
}

// Linux Bluetooth UAPI layouts (bluetooth/l2cap.h and bluetooth/rfcomm.h).
// Explicit padding keeps every byte passed to connect initialized.
#[repr(C)]
struct NativeL2capAddress {
    family: libc::sa_family_t,
    psm: u16,
    address: [u8; 6],
    cid: u16,
    address_type: u8,
    padding: u8,
}

impl NativeL2capAddress {
    fn new(mut address: Address, psm: u16) -> Self {
        address.0.reverse();
        Self {
            family: libc::AF_BLUETOOTH as _,
            psm: psm.to_le(),
            address: address.0,
            cid: 0,
            address_type: AddressType::BrEdr as _,
            padding: 0,
        }
    }
}

#[repr(C)]
struct NativeRfcommAddress {
    family: libc::sa_family_t,
    address: [u8; 6],
    channel: u8,
    padding: u8,
}

impl NativeRfcommAddress {
    fn new(mut address: Address, channel: u8) -> Self {
        address.0.reverse();
        Self {
            family: libc::AF_BLUETOOTH as _,
            address: address.0,
            channel,
            padding: 0,
        }
    }
}

// Safety: address must be a fully initialized native sockaddr for this socket.
// Register only AFTER initiating connect: SO_ERROR=0 on an old writable event
// does not mean a Bluetooth connection has finished. No descriptor duplication,
// blocking thread, sleeps, data retries or foreign Device1.Disconnect are needed.
async unsafe fn connect_owned_socket<A: Sync>(fd: OwnedFd, address: &A) -> io::Result<OwnedFd> {
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::from_ref(address).cast(),
            std::mem::size_of::<A>() as libc::socklen_t,
        )
    };
    if result == 0 {
        return Ok(fd);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EINPROGRESS) {
        return Err(error);
    }
    let pending = AsyncFd::new(fd)?;
    let _ready = pending.writable().await?;
    let mut error = 0 as libc::c_int;
    let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            pending.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::from_mut(&mut error).cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    drop(_ready);
    Ok(pending.into_inner())
}

// SDP ServiceSearchAttribute (PDU 0x06/0x07), not an OBEX transaction.
// The wire layout and continuation semantics follow BlueZ's
// sdp_service_search_attr_req in lib/bluetooth/sdp.c. Only ServiceClassIDList
// (0x0001) and ProtocolDescriptorList (0x0004) are requested.
// https://github.com/bluez/bluez/blob/master/lib/bluetooth/sdp.c
async fn resolve_rfcomm_channel(local: Address, remote: Address) -> Result<u8> {
    let socket = l2cap::Socket::new_seq_packet().context("opening Bluetooth SDP socket")?;
    socket
        .bind(l2cap::SocketAddr::new(local, AddressType::BrEdr, 0))
        .context("binding Bluetooth SDP socket to adapter")?;
    socket
        .set_recv_mtu(SDP_MTU)
        .context("setting Bluetooth SDP receive MTU")?;
    let address = NativeL2capAddress::new(remote, SDP_PSM);
    let fd = unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) };
    let fd = unsafe { connect_owned_socket(fd, &address).await }
        .context("connecting remote Bluetooth SDP service")?;
    let socket = unsafe { l2cap::SeqPacket::from_raw_fd(fd.into_raw_fd()) }
        .context("registering connected Bluetooth SDP socket")?;
    let mut request = [0; 35 + MAX_SDP_CONTINUATION];
    let mut response = [0; SDP_MTU as usize];
    let mut continuation = [0; MAX_SDP_CONTINUATION];
    let mut continuation_len = 0;
    let mut attributes = Vec::new();
    let mut transaction = 1u16;
    // Leave room for the PDU header, byte count and maximal continuation state.
    let max_fragment = SDP_MTU - 8 - MAX_SDP_CONTINUATION as u16;
    loop {
        let request_len = encode_sdp_request(
            &mut request,
            transaction,
            max_fragment,
            &continuation[..continuation_len],
        )?;
        ensure!(
            socket.send(&request[..request_len]).await? == request_len,
            "Bluetooth SDP request was not sent in full"
        );
        let response_len = socket
            .recv(&mut response)
            .await
            .context("receiving Bluetooth SDP response")?;
        let fragment = decode_sdp_response(
            &response[..response_len],
            transaction,
            max_fragment as usize,
        )?;
        if attributes.is_empty() && fragment.continuation.is_empty() {
            // The usual one-PDU answer needs neither allocation nor copying.
            return parse_sdp_rfcomm_channel(fragment.attributes);
        }
        append_sdp_fragment(&mut attributes, &fragment)?;
        if fragment.continuation.is_empty() {
            return parse_sdp_rfcomm_channel(&attributes);
        }
        continuation_len = fragment.continuation.len();
        continuation[..continuation_len].copy_from_slice(fragment.continuation);
        transaction = transaction
            .checked_add(1)
            .context("too many Bluetooth SDP fragments")?;
    }
}

fn encode_sdp_request(
    output: &mut [u8; 35 + MAX_SDP_CONTINUATION],
    transaction: u16,
    max_fragment: u16,
    continuation: &[u8],
) -> Result<usize> {
    ensure!(
        continuation.len() <= MAX_SDP_CONTINUATION,
        "SDP continuation state too long"
    );
    ensure!(
        max_fragment >= 7,
        "SDP maximum attribute byte count too small"
    );
    let length = 35 + continuation.len();
    output[0] = 0x06;
    output[1..3].copy_from_slice(&transaction.to_be_bytes());
    output[3..5].copy_from_slice(&((length - 5) as u16).to_be_bytes());
    output[5..8].copy_from_slice(&[0x35, 17, 0x1c]);
    output[8..24].copy_from_slice(SERVICE_UUID.as_bytes());
    output[24..26].copy_from_slice(&max_fragment.to_be_bytes());
    output[26..34].copy_from_slice(&[0x35, 6, 0x09, 0, 1, 0x09, 0, 4]);
    output[34] = continuation.len() as u8;
    output[35..length].copy_from_slice(continuation);
    Ok(length)
}

struct SdpFragment<'a> {
    attributes: &'a [u8],
    continuation: &'a [u8],
}

fn decode_sdp_response(
    packet: &[u8],
    transaction: u16,
    max_fragment: usize,
) -> Result<SdpFragment<'_>> {
    ensure!(packet.len() >= 5, "truncated SDP response header");
    ensure!(
        u16::from_be_bytes(packet[1..3].try_into()?) == transaction,
        "SDP response transaction mismatch"
    );
    ensure!(
        u16::from_be_bytes(packet[3..5].try_into()?) as usize == packet.len() - 5,
        "SDP response parameter length mismatch"
    );
    if packet[0] == 0x01 {
        ensure!(packet.len() == 7, "malformed SDP error response");
        bail!(
            "remote SDP error 0x{:04x}",
            u16::from_be_bytes(packet[5..7].try_into()?)
        );
    }
    ensure!(packet[0] == 0x07, "unexpected SDP response PDU");
    ensure!(packet.len() >= 8, "truncated SDP attribute response");
    let attribute_len = u16::from_be_bytes(packet[5..7].try_into()?) as usize;
    ensure!(
        attribute_len <= max_fragment,
        "SDP response exceeds requested fragment size"
    );
    let continuation_offset = 7 + attribute_len;
    ensure!(
        continuation_offset < packet.len(),
        "truncated SDP attributes or continuation"
    );
    let continuation_len = packet[continuation_offset] as usize;
    ensure!(
        continuation_len <= MAX_SDP_CONTINUATION,
        "SDP continuation state too long"
    );
    ensure!(
        packet.len() == continuation_offset + 1 + continuation_len,
        "SDP continuation length mismatch"
    );
    ensure!(
        attribute_len != 0 || continuation_len == 0,
        "SDP continuation made no progress"
    );
    Ok(SdpFragment {
        attributes: &packet[7..continuation_offset],
        continuation: &packet[continuation_offset + 1..],
    })
}

fn append_sdp_fragment(attributes: &mut Vec<u8>, fragment: &SdpFragment<'_>) -> Result<()> {
    ensure!(
        attributes.len() <= MAX_SDP_ATTRIBUTES
            && fragment.attributes.len() <= MAX_SDP_ATTRIBUTES - attributes.len(),
        "Bluetooth SDP attributes exceed 65535-byte limit"
    );
    attributes.extend_from_slice(fragment.attributes);
    Ok(())
}

struct SdpElement<'a> {
    kind: u8,
    data: &'a [u8],
}

fn take_sdp_element<'a>(input: &mut &'a [u8]) -> Result<SdpElement<'a>> {
    let (&descriptor, mut rest) = input.split_first().context("missing SDP data element")?;
    let kind = descriptor >> 3;
    let size_index = descriptor & 7;
    let valid = match kind {
        0 => size_index == 0,
        1 | 2 => size_index <= 4,
        3 => matches!(size_index, 1 | 2 | 4),
        4 | 6 | 7 | 8 => size_index >= 5,
        5 => size_index == 0,
        _ => false,
    };
    ensure!(valid, "invalid SDP data element descriptor");
    let length = match size_index {
        _ if kind == 0 => 0,
        0..=4 => 1usize << size_index,
        5 => {
            let (&length, tail) = rest.split_first().context("truncated SDP 8-bit length")?;
            rest = tail;
            length as usize
        }
        6 => {
            ensure!(rest.len() >= 2, "truncated SDP 16-bit length");
            let length = u16::from_be_bytes(rest[..2].try_into()?) as usize;
            rest = &rest[2..];
            length
        }
        7 => {
            ensure!(rest.len() >= 4, "truncated SDP 32-bit length");
            let length = u32::from_be_bytes(rest[..4].try_into()?) as usize;
            rest = &rest[4..];
            length
        }
        _ => bail!("invalid SDP size descriptor"),
    };
    ensure!(length <= rest.len(), "truncated SDP data element payload");
    let (data, remaining) = rest.split_at(length);
    *input = remaining;
    Ok(SdpElement { kind, data })
}

fn take_sdp_sequence<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let element = take_sdp_element(input)?;
    ensure!(element.kind == 6, "expected SDP data element sequence");
    Ok(element.data)
}

fn sdp_uuid(element: &SdpElement<'_>) -> Result<Uuid> {
    ensure!(element.kind == 3, "expected SDP UUID");
    let short = match element.data.len() {
        2 => u16::from_be_bytes(element.data.try_into()?) as u32,
        4 => u32::from_be_bytes(element.data.try_into()?),
        16 => return Ok(Uuid::from_bytes(element.data.try_into()?)),
        _ => bail!("invalid SDP UUID length"),
    };
    Ok(Uuid::from_u128(
        ((short as u128) << 96) | 0x0000000000001000800000805f9b34fb,
    ))
}

fn sdp_protocol_channel(mut protocols: &[u8]) -> Result<Option<u8>> {
    let mut l2cap = false;
    let mut rfcomm_psm = true;
    let mut channel = None;
    let mut layer = 0;
    while !protocols.is_empty() {
        let mut descriptor = take_sdp_sequence(&mut protocols)?;
        let uuid = sdp_uuid(&take_sdp_element(&mut descriptor)?)?;
        if uuid == Uuid::from_u128(0x0000010000001000800000805f9b34fb) {
            ensure!(layer == 0, "SDP L2CAP is not the first protocol layer");
            if !descriptor.is_empty() {
                let psm = take_sdp_element(&mut descriptor)?;
                ensure!(
                    psm.kind == 1 && psm.data.len() == 2 && descriptor.is_empty(),
                    "SDP L2CAP PSM is not uint16"
                );
                rfcomm_psm = psm.data == [0, 3];
            }
            l2cap = true;
        } else if uuid == Uuid::from_u128(0x0000000300001000800000805f9b34fb) {
            ensure!(
                l2cap && layer == 1,
                "SDP RFCOMM does not follow the L2CAP layer"
            );
            ensure!(rfcomm_psm, "SDP L2CAP PSM is not RFCOMM");
            ensure!(channel.is_none(), "duplicate SDP RFCOMM descriptor");
            let parameter = take_sdp_element(&mut descriptor)?;
            ensure!(
                parameter.kind == 1 && parameter.data.len() == 1 && descriptor.is_empty(),
                "SDP RFCOMM channel is not a single uint8 parameter"
            );
            let value = parameter.data[0];
            ensure!(
                (1..=30).contains(&value),
                "SDP RFCOMM channel outside 1..30"
            );
            channel = Some(value);
        }
        while !descriptor.is_empty() {
            take_sdp_element(&mut descriptor)?;
        }
        layer += 1;
    }
    Ok(channel)
}

fn sdp_protocol_list_channel(value: SdpElement<'_>) -> Result<Option<u8>> {
    if value.kind == 6 {
        return sdp_protocol_channel(value.data);
    }
    ensure!(value.kind == 7, "invalid SDP ProtocolDescriptorList");
    // Core Vol. 3 Part B §5.1.5 permits an Alternative of protocol
    // stacks. Inspect each stack, not the alternative as a protocol layer.
    let mut alternatives = value.data;
    let mut channel = None;
    while !alternatives.is_empty() {
        let stack = take_sdp_sequence(&mut alternatives)?;
        if let Some(value) = sdp_protocol_channel(stack)? {
            ensure!(
                channel.is_none() || channel == Some(value),
                "ambiguous NearbySharing SDP RFCOMM alternatives"
            );
            channel = Some(value);
        }
    }
    Ok(channel)
}

fn parse_sdp_rfcomm_channel(attributes: &[u8]) -> Result<u8> {
    ensure!(
        attributes.len() <= MAX_SDP_ATTRIBUTES,
        "Bluetooth SDP attribute response too large"
    );
    let mut input = attributes;
    let mut records = take_sdp_sequence(&mut input)?;
    ensure!(input.is_empty(), "trailing SDP attribute response data");
    let mut channel = None;
    while !records.is_empty() {
        let mut record = take_sdp_sequence(&mut records)?;
        let mut service_matches = false;
        let mut saw_classes = false;
        let mut saw_protocols = false;
        let mut protocols = None;
        while !record.is_empty() {
            let id = take_sdp_element(&mut record)?;
            ensure!(
                id.kind == 1 && id.data.len() == 2,
                "SDP attribute id is not uint16"
            );
            let id = u16::from_be_bytes(id.data.try_into()?);
            let value = take_sdp_element(&mut record)?;
            match id {
                0x0001 => {
                    ensure!(
                        !saw_classes && value.kind == 6,
                        "invalid SDP ServiceClassIDList"
                    );
                    saw_classes = true;
                    let mut classes = value.data;
                    while !classes.is_empty() {
                        service_matches |=
                            sdp_uuid(&take_sdp_element(&mut classes)?)? == SERVICE_UUID;
                    }
                }
                0x0004 => {
                    ensure!(!saw_protocols, "duplicate SDP ProtocolDescriptorList");
                    saw_protocols = true;
                    // The service search matches UUIDs in any attribute, not
                    // just ServiceClassIDList. Do not interpret another
                    // service's valid non-RFCOMM protocols as our route.
                    protocols = Some(value);
                }
                _ => (),
            }
        }
        if service_matches {
            if let Some(value) = protocols
                .map(sdp_protocol_list_channel)
                .transpose()?
                .flatten()
            {
                ensure!(
                    channel.is_none() || channel == Some(value),
                    "ambiguous NearbySharing SDP RFCOMM channels"
                );
                channel = Some(value);
            }
        }
    }
    channel.context("remote SDP has no NearbySharing RFCOMM service")
}

#[derive(Debug, PartialEq, Eq)]
enum RestoreDecision {
    Resolved,
    Restore,
    Preserve,
}

struct OwnedProperty<T> {
    previous: T,
    applied: T,
    // Persisted before mutation, cleared only after restoration or observing
    // a different value. A checkpointed property cannot be reclaimed later.
    pending: bool,
}

impl<T: PartialEq> OwnedProperty<T> {
    fn decision(&self, current: &T) -> RestoreDecision {
        if !self.pending || current == &self.previous {
            RestoreDecision::Resolved
        } else if current == &self.applied {
            RestoreDecision::Restore
        } else {
            RestoreDecision::Preserve
        }
    }
}

struct OwnedAdapterSettings {
    alias: OwnedProperty<String>,
    discoverable: OwnedProperty<bool>,
    timeout: OwnedProperty<u32>,
}

impl OwnedAdapterSettings {
    fn encode(&self, address: Address) -> String {
        format!(
            "RQS-BT2\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            address,
            URL_SAFE_NO_PAD.encode(&self.alias.previous),
            URL_SAFE_NO_PAD.encode(&self.alias.applied),
            self.discoverable.previous,
            self.timeout.previous,
            self.discoverable.applied,
            self.timeout.applied,
            self.alias.pending,
            self.discoverable.pending,
            self.timeout.pending
        )
    }

    fn decode(text: &str, expected_address: Address) -> Result<Self> {
        let mut lines = text.lines();
        ensure!(
            lines.next() == Some("RQS-BT2"),
            "invalid Bluetooth state journal version"
        );
        let address: Address = lines.next().context("missing journal adapter")?.parse()?;
        ensure!(
            address == expected_address,
            "Bluetooth journal adapter mismatch"
        );
        let previous_alias = String::from_utf8(
            URL_SAFE_NO_PAD.decode(lines.next().context("missing prior Alias")?)?,
        )?;
        let applied_alias = String::from_utf8(
            URL_SAFE_NO_PAD.decode(lines.next().context("missing owned Alias")?)?,
        )?;
        parse_classic_name(&applied_alias)
            .context("journal Alias is not an owned NearbySharing announcement")?;
        let previous_discoverable = lines
            .next()
            .context("missing journal Discoverable")?
            .parse()?;
        let previous_timeout = lines
            .next()
            .context("missing journal DiscoverableTimeout")?
            .parse()?;
        let applied_discoverable = lines
            .next()
            .context("missing owned Discoverable")?
            .parse()?;
        let applied_timeout = lines
            .next()
            .context("missing owned DiscoverableTimeout")?
            .parse()?;
        let alias_pending = lines.next().context("missing Alias ownership")?.parse()?;
        let discoverable_pending = lines
            .next()
            .context("missing Discoverable ownership")?
            .parse()?;
        let timeout_pending = lines
            .next()
            .context("missing DiscoverableTimeout ownership")?
            .parse()?;
        ensure!(lines.next().is_none(), "trailing Bluetooth journal fields");
        Ok(Self {
            alias: OwnedProperty {
                previous: previous_alias,
                applied: applied_alias,
                pending: alias_pending,
            },
            discoverable: OwnedProperty {
                previous: previous_discoverable,
                applied: applied_discoverable,
                pending: discoverable_pending,
            },
            timeout: OwnedProperty {
                previous: previous_timeout,
                applied: applied_timeout,
                pending: timeout_pending,
            },
        })
    }
}

struct NearbyLeAdvertisement {
    header: [u8; BLE_LEGACY_HEADER_LEN],
    available: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl NearbyLeAdvertisement {
    fn release(&self) {
        self.available.store(false, Ordering::Release);
    }
    #[zbus(property, name = "Type")]
    fn kind(&self) -> &str {
        "peripheral"
    }
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, zbus::zvariant::OwnedValue> {
        let bytes = zbus::zvariant::Value::from(self.header.to_vec());
        [(
            BLE_UUID.to_string(),
            zbus::zvariant::OwnedValue::try_from(bytes)
                .expect("a byte array has an owned D-Bus representation"),
        )]
        .into()
    }
}

struct NearbyGattService;

#[zbus::interface(name = "org.bluez.GattService1")]
impl NearbyGattService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        "0000fef3-0000-1000-8000-00805f9b34fb"
    }
    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn includes(&self) -> Vec<zbus::zvariant::OwnedObjectPath> {
        Vec::new()
    }
}

struct NearbyGattSlot {
    service: zbus::zvariant::OwnedObjectPath,
    data: Vec<u8>,
    available: Arc<AtomicBool>,
    visibility: watch::Receiver<Visibility>,
    ctk: CancellationToken,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum NearbyGattError {
    NotPermitted(String),
    InvalidOffset(String),
    Failed(String),
    #[zbus(error)]
    ZBus(zbus::Error),
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl NearbyGattSlot {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        "00000000-0000-3000-8000-000000000000"
    }
    #[zbus(property)]
    fn service(&self) -> zbus::zvariant::OwnedObjectPath {
        self.service.clone()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<&str> {
        vec!["read"]
    }
    async fn read_value(
        &self,
        options: HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> std::result::Result<Vec<u8>, NearbyGattError> {
        let offset = options
            .get("offset")
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);
        let mtu = options
            .get("mtu")
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(23);
        let visible = self.available.load(Ordering::Acquire)
            && !self.ctk.is_cancelled()
            && *self.visibility.borrow() != Visibility::Invisible;
        let data = read_ble_advertisement(&self.data, offset as usize, mtu as usize, visible)
            .map_err(|error| match error {
                bluer::gatt::local::ReqError::NotPermitted => {
                    NearbyGattError::NotPermitted("Nearby visibility is inactive".into())
                }
                bluer::gatt::local::ReqError::InvalidOffset => NearbyGattError::InvalidOffset(
                    "Advertisement offset is outside the value".into(),
                ),
                _ => NearbyGattError::Failed(error.to_string()),
            })?;
        if !self.available.load(Ordering::Acquire)
            || self.ctk.is_cancelled()
            || *self.visibility.borrow() == Visibility::Invisible
        {
            return Err(NearbyGattError::NotPermitted(
                "Nearby visibility is inactive".into(),
            ));
        }
        Ok(data)
    }
}

struct BleAnnouncements {
    radio: Arc<RadioCoordinator>,
    available: Arc<AtomicBool>,
    application: zbus::zvariant::OwnedObjectPath,
    advertisement: zbus::zvariant::OwnedObjectPath,
    service: zbus::zvariant::OwnedObjectPath,
    slot: zbus::zvariant::OwnedObjectPath,
    application_registered: bool,
    advertisement_registered: bool,
    application_exported: bool,
    advertisement_exported: bool,
    service_exported: bool,
    slot_exported: bool,
}

impl BleAnnouncements {
    async fn unregister(
        &self,
        interface: &str,
        method: &str,
        path: &zbus::zvariant::OwnedObjectPath,
    ) -> Result<()> {
        match bluez_call(
            &self.radio.bus,
            &self.radio.owner,
            &self.radio.path,
            interface,
            method,
            &(path,),
        )
        .await
        {
            // Failed registration, or a missing GATT/advertising manager:
            // authoritative absence does not require unregistering a foreign
            // object and must not turn unsupported BLE into a Classic failure.
            Err(error)
                if matches!(error.downcast_ref::<zbus::Error>(),
                Some(zbus::Error::MethodError(name, _, _)) if matches!(name.as_str(),
                    "org.bluez.Error.DoesNotExist"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownMethod")) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    async fn publish_advertisement(&mut self, state: &mut RadioState) -> Result<()> {
        ensure!(
            !self.advertisement_registered && !state.advertisement_active,
            "Nearby advertisement already owned; duplicate registration refused"
        );
        ensure!(
            !state.scan_active && !self.radio.adapter.is_discovering().await?,
            "Discovery must be confirmed stopped before advertising role"
        );
        self.radio.refresh_connections(state)?;
        ensure!(
            !state.le_connected,
            "LE bearer must disconnect before advertising role"
        );
        ensure!(
            self.available.load(Ordering::Acquire),
            "Nearby advertisement was unexpectedly released; no automatic re-registration"
        );
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        self.advertisement_registered = true;
        state.advertisement_active = true;
        // The same exported object can be reused after confirmed Unregister:
        // BlueZ 5.87 advertising.c matches registrations by owner and path.
        bluez_call(
            &self.radio.bus,
            &self.radio.owner,
            &self.radio.path,
            "org.bluez.LEAdvertisingManager1",
            "RegisterAdvertisement",
            &(&self.advertisement, &options),
        )
        .await
    }

    async fn withdraw_advertisement(&mut self, state: &mut RadioState) -> Result<()> {
        ensure!(
            !state.scan_active,
            "Scanner must be paused before advertising withdrawal"
        );
        if self.advertisement_registered {
            ensure!(!self.radio.adapter.is_discovering().await?,
                "Another client's discovery prevents safe advertising withdrawal; radio must remain failed closed");
            self.unregister(
                "org.bluez.LEAdvertisingManager1",
                "UnregisterAdvertisement",
                &self.advertisement,
            )
            .await?;
            self.advertisement_registered = false;
            state.advertisement_active = false;
            ensure!(self.radio.adapter.active_advertising_instances().await? == 0,
                "Advertising withdrawal is not globally quiet; another client's advertisement will not be stopped");
        }
        // Intentionally retain the GATT application, exports, endpoint data
        // and read gate throughout the scan window. UnregisterAdvertisement
        // calls client_remove (not Release) in BlueZ 5.87 advertising.c.
        Ok(())
    }

    async fn close(&mut self, state: &mut RadioState) -> Result<()> {
        self.available.store(false, Ordering::Release);
        self.withdraw_advertisement(state).await?;
        if self.application_registered {
            self.unregister(
                "org.bluez.GattManager1",
                "UnregisterApplication",
                &self.application,
            )
            .await?;
            self.application_registered = false;
        }
        let server = self.radio.bus.object_server();
        if self.advertisement_exported {
            server
                .remove::<NearbyLeAdvertisement, _>(self.advertisement.as_str())
                .await?;
            self.advertisement_exported = false;
        }
        if self.slot_exported {
            server
                .remove::<NearbyGattSlot, _>(self.slot.as_str())
                .await?;
            self.slot_exported = false;
        }
        if self.service_exported {
            server
                .remove::<NearbyGattService, _>(self.service.as_str())
                .await?;
            self.service_exported = false;
        }
        if self.application_exported {
            server
                .remove::<zbus::fdo::ObjectManager, _>(self.application.as_str())
                .await?;
            self.application_exported = false;
        }
        Ok(())
    }
}

impl Drop for BleAnnouncements {
    fn drop(&mut self) {
        // Only an abnormal task abort reaches Drop with owned registrations.
        // Normal visibility, cancellation and timeout paths await close().
        self.available.store(false, Ordering::Release);
    }
}

struct Announcements {
    adapter: Adapter,
    radio: Arc<RadioCoordinator>,
    settings: Option<OwnedAdapterSettings>,
    restore_on_drop: bool,
    ble: Option<BleAnnouncements>,
    journal: Arc<StateJournal>,
}

impl Announcements {
    async fn restore(&mut self) -> Result<()> {
        self.restore_on_drop = false;
        if let Some(ble) = &self.ble {
            ble.available.store(false, Ordering::Release);
        }
        let radio = self.radio.clone();
        let mut state = radio.pause().await?;
        self.restore_paused(&mut state).await?;
        radio.resume(&mut state).await
    }

    async fn advance_role(
        &mut self,
        ctk: &CancellationToken,
        visibility: &watch::Receiver<Visibility>,
    ) -> Result<()> {
        let radio = self.radio.clone();
        let mut state = radio.state.lock().await;
        radio.refresh_connections(&mut state)?;
        if state.le_connected {
            return Ok(());
        }
        if let Err(error) = radio.stop(&mut state).await {
            state.failed = true;
            return Err(error);
        }
        if ctk.is_cancelled() {
            state.scan_requested = false;
            return Ok(());
        }
        // Visibility may have changed while waiting for another peer's GATT
        // cleanup. Do not publish a fresh AD for a now-hidden endpoint.
        if *visibility.borrow() == Visibility::Invisible {
            return Ok(());
        }
        let result: Result<()> = async {
            let Some(window) = state.window else { return Ok(()); };
            // A connection can precede or omit ReadValue. The kernel bearer
            // snapshot, not a metadata read or Device1's aggregate Connected
            // flag, decides when a radio handoff is safe.
            if !state.role_transition_due(Instant::now()) { return Ok(()); }
            let next = window.next(Instant::now());
            let ble = self.ble.as_mut().context("scheduled BLE advertiser is unavailable")?;
            ensure!(ble.available.load(Ordering::Acquire),
                "Nearby BLE advertisement was unexpectedly released; role scheduler stops without retry");
            match next.role {
                BleRole::Scanning => {
                    ble.withdraw_advertisement(&mut state).await?;
                    radio.set_window(&mut state, Some(next));
                    if ctk.is_cancelled() { state.scan_requested = false; }
                    radio.resume(&mut state).await?;
                }
                BleRole::Advertising => {
                    ble.publish_advertisement(&mut state).await?;
                }
            }
            // Window duration starts at confirmed role entry, not at a
            // timer tick before potentially slow D-Bus confirmation.
            radio.set_window(&mut state, Some(BleRoleWindow::new(next.role, Instant::now())));
            debug!("Nearby exclusive BLE role: {:?}, advertising={} scanning={}",
                next.role, state.advertisement_active, state.scan_active);
            Ok(())
        }.await;
        if result.is_err() {
            state.failed = true;
            if let Some(ble) = &self.ble {
                ble.available.store(false, Ordering::Release);
            }
        }
        result
    }

    async fn restore_paused(&mut self, state: &mut RadioState) -> Result<()> {
        if let Some(ble) = self.ble.as_mut() {
            if let Err(error) = ble.close(state).await {
                state.failed = true;
                return Err(error);
            }
            self.ble = None;
        }
        self.radio.set_window(state, None);
        if let Some(settings) = self.settings.as_mut() {
            self.journal.restore(&self.adapter, settings).await?;
            self.settings = None;
        }
        Ok(())
    }
}

impl Drop for Announcements {
    fn drop(&mut self) {
        if let Some(ble) = &self.ble {
            ble.available.store(false, Ordering::Release);
        }
        if !self.restore_on_drop || (self.settings.is_none() && self.ble.is_none()) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!("Bluetooth restoration requires the Tokio runtime to remain alive");
            return;
        };
        let mut restoration = Announcements {
            adapter: self.adapter.clone(),
            radio: self.radio.clone(),
            settings: self.settings.take(),
            // Disarm before spawning, including if the future is canceled or
            // BlueZ is gone. The durable journal remains for the next startup.
            restore_on_drop: false,
            ble: self.ble.take(),
            journal: self.journal.clone(),
        };
        runtime.spawn(async move {
            if let Err(error) = restoration.restore().await {
                error!("Bluetooth shutdown restoration failed: {error}");
            }
        });
    }
}

// The permanent lock has a different inode from the removable state record.
// It is held for the backend lifetime, including asynchronous Drop restoration.
// Otherwise removing a locked journal would allow a second process to acquire
// a different inode and race the adapter's global Alias.
struct StateJournal {
    path: PathBuf,
    address: Address,
    _lock: File,
}

impl StateJournal {
    fn open(address: Address) -> Result<Self> {
        let base = directories::BaseDirs::new().context("Bluetooth state directory unavailable")?;
        let directory = base
            .state_dir()
            .unwrap_or_else(|| base.data_local_dir())
            .join("rquickshare")
            .join("bluetooth");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let metadata = std::fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Bluetooth state directory must be private and owned by this user"
        );
        let stem = address.to_string().replace(':', "");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(format!("{stem}.lock")))?;
        Self::validate_file(&lock)?;
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        ensure!(
            locked == 0,
            "another Quick Share Bluetooth backend owns this adapter: {}",
            std::io::Error::last_os_error()
        );
        let journal = Self {
            path: directory.join(format!("{stem}.state")),
            address,
            _lock: lock,
        };
        // Discard an unpublished intent/checkpoint. The atomically published
        // record either precedes the mutation or conservatively retains the
        // pending property; recovery checks its actual value before restoring.
        let pending = journal.path.with_extension("pending");
        match Self::open_private(&pending) {
            Ok(_) => std::fs::remove_file(pending)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(journal)
    }

    fn validate_file(file: &File) -> Result<()> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "Bluetooth state file must be a single-link mode-0600 file owned by this user"
        );
        Ok(())
    }

    fn open_private(path: &Path) -> std::io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Self::validate_file(&file).map_err(std::io::Error::other)?;
        Ok(file)
    }

    fn sync_directory(&self) -> Result<()> {
        File::open(
            self.path
                .parent()
                .context("Bluetooth journal has no directory")?,
        )?
        .sync_all()?;
        Ok(())
    }

    fn record(&self, settings: &OwnedAdapterSettings) -> Result<()> {
        ensure!(
            !self.path.try_exists()?,
            "unrecovered Bluetooth state journal exists"
        );
        self.save(settings)
    }

    fn save(&self, settings: &OwnedAdapterSettings) -> Result<()> {
        let record = settings.encode(self.address);
        let pending = self.path.with_extension("pending");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&pending)?;
        Self::validate_file(&file)?;
        file.write_all(record.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(pending, &self.path)?;
        self.sync_directory()?;
        Ok(())
    }

    async fn recover(&self, adapter: &Adapter) -> Result<()> {
        let file = match Self::open_private(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut text = String::new();
        file.take(8193).read_to_string(&mut text)?;
        ensure!(text.len() <= 8192, "oversize Bluetooth state journal");
        ensure!(
            self.address == adapter.address().await?,
            "Bluetooth journal adapter mismatch"
        );
        let mut settings = OwnedAdapterSettings::decode(&text, self.address)?;
        self.restore(adapter, &mut settings).await
    }

    async fn restore(&self, adapter: &Adapter, settings: &mut OwnedAdapterSettings) -> Result<()> {
        let mut errors = Vec::new();
        // Exposure and name are independent. A rename must never prevent
        // recovery of service-owned Discoverable=true / Timeout=0.
        if settings.discoverable.pending {
            let result: Result<()> = async {
                let current = adapter.is_discoverable().await?;
                if settings.discoverable.decision(&current) == RestoreDecision::Restore {
                    adapter
                        .set_discoverable(settings.discoverable.previous)
                        .await?;
                }
                settings.discoverable.pending = false;
                if let Err(error) = self.save(settings) {
                    settings.discoverable.pending = true;
                    return Err(error);
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                errors.push(format!("Discoverable: {error:#}"));
            }
        }
        if settings.timeout.pending {
            let result: Result<()> = async {
                let current = adapter.discoverable_timeout().await?;
                if settings.timeout.decision(&current) == RestoreDecision::Restore {
                    adapter
                        .set_discoverable_timeout(settings.timeout.previous)
                        .await?;
                }
                settings.timeout.pending = false;
                if let Err(error) = self.save(settings) {
                    settings.timeout.pending = true;
                    return Err(error);
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                errors.push(format!("DiscoverableTimeout: {error:#}"));
            }
        }
        if settings.alias.pending {
            let result: Result<()> = async {
                let current = adapter.alias().await?;
                if settings.alias.decision(&current) == RestoreDecision::Restore {
                    adapter.set_alias(settings.alias.previous.clone()).await?;
                }
                settings.alias.pending = false;
                if let Err(error) = self.save(settings) {
                    settings.alias.pending = true;
                    return Err(error);
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                errors.push(format!("Alias: {error:#}"));
            }
        }
        ensure!(
            errors.is_empty(),
            "Bluetooth restoration: {}",
            errors.join("; ")
        );
        self.clear()
            .context("clearing restored Bluetooth state journal")?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => self.sync_directory()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let pending = self.path.with_extension("pending");
        match Self::open_private(&pending) {
            Ok(_) => {
                std::fs::remove_file(pending)?;
                self.sync_directory()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum DiscoverySource {
    Classic,
    Ble,
}

#[derive(Clone, Copy)]
enum DiscoveryEvidence {
    Added,
    Classic,
    Ble,
}

impl DiscoveryEvidence {
    fn observes(self, source: DiscoverySource) -> bool {
        matches!(
            (self, source),
            (Self::Added, _)
                | (Self::Classic, DiscoverySource::Classic)
                | (Self::Ble, DiscoverySource::Ble)
        )
    }
}

type BleServiceData = HashMap<Uuid, Vec<u8>>;

fn nearby_service_observation(current: &BleServiceData) -> bool {
    // BlueZ 5.87 src/device.c::device_set_service_data clears the old map
    // before each report when DuplicateData is enabled. FEF3 must therefore
    // be present in this radio report, not merely in a cached Device1 getter.
    current.contains_key(&BLE_UUID)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BleLegacyHeader {
    slots: u8,
    wire: [u8; BLE_LEGACY_HEADER_LEN],
}

struct BleHeaderRead {
    header: BleLegacyHeader,
    frame: Option<(Address, ParsedEndpoint)>,
}

struct GattDiscoveryConnection {
    device: Option<bluer::Device>,
    radio: Arc<RadioCoordinator>,
}

impl GattDiscoveryConnection {
    async fn close(&mut self) -> Result<()> {
        if let Some(device) = self.device.take() {
            let path = discovery::device_path(&self.radio.path, device.address());
            let mut events = discovery::signals(&self.radio.bus, &self.radio.owner, Some(&path)).await?;
            let result: Result<()> = async {
                let policy = discovery::device_policy(&self.radio.bus, &self.radio.owner, &path).await?;
                ensure!(policy.may_disconnect(), "GATT device acquired protected user state; refusing Disconnect");
                ensure!(!self.radio.connections.has_classic_bearer(device.address())?,
                    "Classic connection now shares this device; refusing aggregate Disconnect");
                bluez_call(&self.radio.bus, &self.radio.owner, &path, "org.bluez.Device1", "Disconnect", &()).await?;
                if !discovery::device_policy(&self.radio.bus, &self.radio.owner, &path).await?.connected {
                    return Ok(());
                }
                timeout(OPERATION_TIMEOUT, async {
                    while let Some(event) = events.next().await {
                        let event = event?;
                        if let Some(properties) = discovery::changed_properties(&event, "org.bluez.Device1")? {
                            if properties.get("Connected").is_some_and(|value| bool::try_from(value).ok() == Some(false)) {
                                return Ok(());
                            }
                        }
                    }
                    bail!("GATT device removed before disconnect confirmation")
                })
                .await
                .context("owned GATT Connected=false was not confirmed")?
            }
            .await;
            // A failed confirmation is surfaced and poisons the coordinator;
            // do not issue a duplicate asynchronous Disconnect from Drop.
            return result;
        }
        Ok(())
    }
}

impl Drop for GattDiscoveryConnection {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            // This is an abnormal task abort only, never normal cancellation
            // or a timeout. Do not let its cleanup race scanner/AD commands.
            let radio = self.radio.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let mut state = radio.state.lock().await;
                    state.failed = true;
                    radio.paused.send_replace(true);
                    if let Err(error) = radio.wait_discovering(false).await {
                        error!("Aborted GATT task cannot safely clean up; radio failed closed: {error:#}");
                        return;
                    }
                    let mut connection = GattDiscoveryConnection {
                        device: Some(device), radio: radio.clone(),
                    };
                    if let Err(error) = connection.close().await {
                        error!("Aborted GATT task cleanup failed; radio remains failed closed: {error:#}");
                    }
                });
            } else {
                error!("Aborted GATT cleanup requires the Tokio runtime to remain alive");
            }
        }
    }
}

struct Peer {
    endpoint: EndpointInfo,
    seen: Instant,
    source: DiscoverySource,
}


fn parse_route(route: &str) -> Result<Address> {
    let (address, uuid) = route
        .strip_prefix("bt:")
        .context("not a Bluetooth route")?
        .split_once('/')
        .context("Bluetooth route requires MAC/UUID")?;
    ensure!(
        Uuid::parse_str(uuid)? == SERVICE_UUID,
        "not a NearbySharing service UUID"
    );
    let address: Address = address.parse().context("invalid Bluetooth MAC")?;
    ensure!(
        address != Address::new([0; 6]) && address != Address::new([255; 6]),
        "invalid Bluetooth MAC"
    );
    Ok(address)
}

#[derive(Clone, Debug, PartialEq)]
struct ParsedEndpoint {
    endpoint_id: [u8; 4],
    name: String,
    device_type: DeviceType,
}

fn encode_endpoint_info(name: &str) -> Result<Vec<u8>> {
    ensure!(
        !name.is_empty() && !name.contains('\0'),
        "Bluetooth endpoint name must be nonempty UTF-8 without NUL"
    );
    let mut end = name.len().min(MAX_ENDPOINT_INFO - 18);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let name = &name[..end];
    let mut bytes = Vec::with_capacity(18 + name.len());
    bytes.push((DeviceType::Laptop as u8) << 1);
    bytes.extend_from_slice(&rand::random::<[u8; 16]>());
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name.as_bytes());
    Ok(bytes)
}

fn parse_endpoint_info(endpoint_id: [u8; 4], bytes: &[u8]) -> Result<ParsedEndpoint> {
    let (name, device_type) = endpoint_fields(endpoint_id, bytes)?;
    Ok(ParsedEndpoint {
        endpoint_id,
        name: name.to_owned(),
        device_type,
    })
}

fn endpoint_fields(endpoint_id: [u8; 4], bytes: &[u8]) -> Result<(&str, DeviceType)> {
    ensure!(
        endpoint_id.iter().all(u8::is_ascii_alphanumeric),
        "invalid endpoint id"
    );
    ensure!(
        bytes.len() >= 19 && bytes.len() <= MAX_ENDPOINT_INFO,
        "invalid endpoint info length"
    );
    ensure!(
        bytes[0] >> 5 <= 1 && bytes[0] & 0x10 == 0,
        "unsupported or anonymous endpoint info"
    );
    let len = bytes[17] as usize;
    ensure!(
        len > 0 && 18 + len <= bytes.len(),
        "invalid endpoint name length"
    );
    let name = std::str::from_utf8(&bytes[18..18 + len])?;
    ensure!(!name.contains('\0'), "NUL in endpoint name");
    let mut tail = &bytes[18 + len..];
    while !tail.is_empty() {
        ensure!(
            tail.len() >= 2 && tail.len() >= 2 + tail[1] as usize,
            "truncated endpoint TLV"
        );
        if tail[0] == 2 {
            ensure!(tail[1] == 1, "invalid vendor TLV");
        }
        tail = &tail[2 + tail[1] as usize..];
    }
    Ok((name, DeviceType::from_raw_value((bytes[0] >> 1) & 7)))
}

fn encode_classic_name(endpoint_id: [u8; 4], info: &[u8]) -> Result<String> {
    endpoint_fields(endpoint_id, info)?;
    let mut bytes = Vec::with_capacity(16 + info.len());
    bytes.push(0x23); // Version 1, P2P point-to-point PCP 3.
    bytes.extend_from_slice(&endpoint_id);
    bytes.extend_from_slice(&SERVICE_HASH);
    bytes.extend_from_slice(&[0; 7]); // WebRTC unavailable, six reserved bytes.
    bytes.push(info.len() as u8);
    bytes.extend_from_slice(info);
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_classic_name(name: &str) -> Result<ParsedEndpoint> {
    ensure!(name.len() <= 248, "oversize Bluetooth name");
    let bytes = URL_SAFE_NO_PAD
        .decode(name)
        .or_else(|_| URL_SAFE.decode(name))?;
    ensure!(
        bytes.len() >= 16 && bytes[0] == 0x23,
        "unsupported Classic version/PCP"
    );
    ensure!(bytes[5..8] == SERVICE_HASH, "not a NearbySharing name");
    let end = 16 + bytes[15] as usize;
    ensure!(end <= bytes.len(), "truncated Classic endpoint info");
    validate_uwb_tail(&bytes[end..], false)?;
    parse_endpoint_info(bytes[1..5].try_into()?, &bytes[16..end])
}

fn validate_uwb_tail(tail: &[u8], extra: bool) -> Result<()> {
    if tail.is_empty() {
        return Ok(());
    }
    let len = tail[0] as usize;
    ensure!(matches!(len, 0 | 2 | 8), "invalid UWB address length");
    ensure!(
        tail.len() == 1 + len || (extra && tail.len() == 2 + len),
        "truncated or trailing UWB field"
    );
    Ok(())
}

fn parse_ble_legacy_header(bytes: &[u8]) -> Result<BleLegacyHeader> {
    // Nearby BleAdvertisementHeader accepts either raw 15/17 bytes or
    // WebSafe Base64 (internal/platform/base64_utils.cc). Missing PSM is zero.
    let mut decoded = [0; 18];
    let raw = if matches!(bytes.len(), 15 | 17) {
        bytes
    } else {
        ensure!(
            matches!(bytes.len(), 20 | 23 | 24),
            "invalid BLE header length"
        );
        let len = URL_SAFE_NO_PAD
            .decode_slice(bytes, &mut decoded)
            .or_else(|_| URL_SAFE.decode_slice(bytes, &mut decoded))?;
        &decoded[..len]
    };
    ensure!(
        matches!(raw.len(), 15 | 17),
        "invalid decoded BLE header length"
    );
    ensure!(raw[0] >> 5 == 2, "unsupported BLE header version");
    let slots = raw[0] & 15;
    ensure!(slots != 0, "BLE header has no advertisement slots");
    let mut required = [0; 10];
    add_ble_bloom(&mut required, b"NearbySharing");
    ensure!(
        required
            .iter()
            .zip(&raw[1..11])
            .all(|(required, advertised)| required & advertised == *required),
        "BLE header does not advertise NearbySharing"
    );
    let mut wire = [0; BLE_LEGACY_HEADER_LEN];
    wire[..raw.len()].copy_from_slice(raw);
    Ok(BleLegacyHeader { slots, wire })
}

fn is_ble_advertisement_slot(uuid: Uuid, slots: u8) -> bool {
    let base = BLE_ADVERTISEMENT_SLOT.as_u128();
    let value = uuid.as_u128();
    // ble_utils::GenerateAdvertisementUuid ORs slot into the LSB, not the
    // UUID MSB or the Bluetooth base UUID. Header slot count is four bits.
    value >= base && value - base < slots as u128 && slots <= 15
}

// Google's Ble::CreateAdvertisementHeader: random dummy service + real
// service in an 80-bit Murmur3 bloom filter, chained truncated SHA-256 hash.
// One slot, no extended-advertisement flag, and PSM zero: no BLE data socket.
fn encode_ble_legacy_header(data: &[u8]) -> [u8; BLE_LEGACY_HEADER_LEN] {
    let dummy = rand::random::<[u8; 128]>();
    let mut header = [0; BLE_LEGACY_HEADER_LEN];
    header[0] = 0x41;
    let mut bloom = [0; 10];
    add_ble_bloom(&mut bloom, &dummy);
    add_ble_bloom(&mut bloom, b"NearbySharing");
    header[1..11].copy_from_slice(&bloom);
    let dummy_hash = Sha256::digest(dummy);
    let mut hash = Sha256::new();
    hash.update(&dummy_hash[..4]);
    hash.update(data);
    header[11..15].copy_from_slice(&hash.finalize()[..4]);
    header
}

fn add_ble_bloom(bloom: &mut [u8; 10], value: &[u8]) {
    let hash = murmur3_low64(value);
    let hash1 = hash as i32;
    let hash2 = (hash >> 32) as i32;
    for i in 1_i32..=5 {
        let mut combined = hash1.wrapping_add(i.wrapping_mul(hash2));
        if combined < 0 {
            combined = !combined;
        }
        let bit = combined as usize % 80;
        bloom[bit / 8] |= 1 << (bit % 8);
    }
}

// MurmurHash3_x64_128, seed zero, low 64 bits (Nearby bloom_filter.cc).
// Explicit LE loads and wrapping arithmetic match the wire algorithm on any
// host, including negative 32-bit combined hashes in add_ble_bloom.
fn murmur3_low64(value: &[u8]) -> u64 {
    const C1: u64 = 0x87c37b91114253d5;
    const C2: u64 = 0x4cf5ad432745937f;
    fn mix1(k: u64) -> u64 {
        k.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2)
    }
    fn mix2(k: u64) -> u64 {
        k.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1)
    }
    fn finish(mut h: u64) -> u64 {
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
        h ^ (h >> 33)
    }
    let (mut h1, mut h2) = (0_u64, 0_u64);
    let mut blocks = value.chunks_exact(16);
    for block in &mut blocks {
        h1 ^= mix1(u64::from_le_bytes(block[..8].try_into().unwrap()));
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dce729);
        h2 ^= mix2(u64::from_le_bytes(block[8..].try_into().unwrap()));
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x38495ab5);
    }
    let remainder = blocks.remainder();
    let mut tail = [0; 16];
    tail[..remainder.len()].copy_from_slice(remainder);
    if remainder.len() > 8 {
        h2 ^= mix2(u64::from_le_bytes(tail[8..].try_into().unwrap()));
    }
    if !remainder.is_empty() {
        h1 ^= mix1(u64::from_le_bytes(tail[..8].try_into().unwrap()));
    }
    h1 ^= value.len() as u64;
    h2 ^= value.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    finish(h1).wrapping_add(finish(h2))
}

fn read_ble_advertisement(
    data: &[u8],
    offset: usize,
    mtu: usize,
    visible: bool,
) -> std::result::Result<Vec<u8>, bluer::gatt::local::ReqError> {
    use bluer::gatt::local::ReqError as GattError;
    if !visible {
        return Err(GattError::NotPermitted);
    }
    let remaining = data.get(offset..).ok_or(GattError::InvalidOffset)?;
    // ATT Read/Read Blob responses reserve one byte for the opcode.
    if mtu < 23 {
        return Err(GattError::Failed);
    }
    Ok(remaining[..remaining.len().min(mtu - 1)].to_vec())
}

fn encode_ble_service_data(endpoint_id: [u8; 4], info: &[u8], address: Address) -> Result<Vec<u8>> {
    endpoint_fields(endpoint_id, info)?;
    let mac = *address;
    ensure!(mac != [0; 6] && mac != [255; 6], "missing Classic MAC");
    let len = 9 + info.len() + 8;
    let mut bytes = Vec::with_capacity(8 + len + 2);
    bytes.push(0x48); // Medium advertisement V2, socket V2, not fast.
    bytes.extend_from_slice(&SERVICE_HASH);
    bytes.extend_from_slice(&(len as u32).to_be_bytes());
    bytes.push(0x23);
    bytes.extend_from_slice(&SERVICE_HASH);
    bytes.extend_from_slice(&endpoint_id);
    bytes.push(info.len() as u8);
    bytes.extend_from_slice(info);
    bytes.extend_from_slice(&mac);
    bytes.extend_from_slice(&[0, 0]); // UWB absent, WebRTC unavailable.
    bytes.extend_from_slice(&rand::random::<[u8; 2]>());
    Ok(bytes)
}

fn parse_ble_service_data(bytes: &[u8]) -> Result<(Address, ParsedEndpoint)> {
    ensure!(
        bytes.len() >= 8 && bytes.len() <= 512,
        "invalid BLE service data length"
    );
    ensure!(
        matches!(bytes[0] >> 5, 1 | 2) && matches!((bytes[0] >> 2) & 7, 1 | 2),
        "unsupported BLE version"
    );
    // Fast frames omit the service hash and Classic MAC: not verifiable routes.
    ensure!(
        bytes[0] & 3 == 0 && bytes[1..4] == SERVICE_HASH,
        "not a full NearbySharing BLE advertisement"
    );
    let len = u32::from_be_bytes(bytes[4..8].try_into()?) as usize;
    ensure!(len <= bytes.len() - 8 && len >= 15, "truncated BLE payload");
    let data = &bytes[8..8 + len];
    let mut tail = &bytes[8 + len..];
    if !tail.is_empty() {
        ensure!(tail.len() >= 2, "truncated BLE device token");
        tail = &tail[2..];
        if !tail.is_empty() {
            let mask = tail[0];
            ensure!(mask & !3 == 0, "unsupported BLE extra fields");
            tail = &tail[1..];
            if mask & 1 != 0 {
                ensure!(tail.len() >= 2, "truncated PSM");
                tail = &tail[2..];
            }
            if mask & 2 != 0 {
                ensure!(
                    !tail.is_empty() && tail.len() > tail[0] as usize,
                    "truncated instant connection field"
                );
                tail = &tail[1 + tail[0] as usize..];
            }
            ensure!(tail.is_empty(), "trailing BLE extra fields");
        }
    }
    ensure!(
        data[0] == 0x23 && data[1..4] == SERVICE_HASH,
        "invalid BLE connection advertisement"
    );
    let end = 9 + data[8] as usize;
    ensure!(end + 6 <= data.len(), "truncated Classic MAC");
    let mac: [u8; 6] = data[end..end + 6].try_into()?;
    ensure!(mac != [0; 6] && mac != [255; 6], "missing Classic MAC");
    validate_uwb_tail(&data[end + 6..], true)?;
    Ok((
        Address::new(mac),
        parse_endpoint_info(data[4..8].try_into()?, &data[9..end])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hardware regression: an unconnected BlueR socket can leave a writable
    // event cached before connect. A real Nearby peer must receive our first
    // SDP packet only after the kernel connection is complete.
    #[tokio::test]
    #[ignore = "requires a Bluetooth adapter and a consenting Nearby peer in Receive"]
    async fn sdp_connect_waits_for_kernel_after_cached_writable() -> Result<()> {
        let local = std::env::var("QUICKSHARE_TEST_BT_LOCAL")?.parse::<Address>()?;
        let remote = std::env::var("QUICKSHARE_TEST_BT_REMOTE")?.parse::<Address>()?;
        timeout(OPERATION_TIMEOUT, async {
            let socket = l2cap::Socket::new_seq_packet()?;
            socket.bind(l2cap::SocketAddr::new(local, AddressType::BrEdr, 0))?;
            socket.set_recv_mtu(SDP_MTU)?;
            // Reproduce the reactor interleaving, not a production delay.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let address = NativeL2capAddress::new(remote, SDP_PSM);
            let fd = unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) };
            let fd = unsafe { connect_owned_socket(fd, &address).await }?;
            let socket = unsafe { l2cap::SeqPacket::from_raw_fd(fd.into_raw_fd()) }?;
            let mut readiness = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 1);
            assert_ne!(readiness.revents & libc::POLLOUT, 0);
            assert_eq!(readiness.revents & (libc::POLLERR | libc::POLLHUP), 0);
            let max_fragment = SDP_MTU - 8 - MAX_SDP_CONTINUATION as u16;
            let mut request = [0; 35 + MAX_SDP_CONTINUATION];
            let size = encode_sdp_request(&mut request, 1, max_fragment, &[])?;
            assert_eq!(socket.send(&request[..size]).await?, size);
            let mut response = [0; SDP_MTU as usize];
            let size = socket.recv(&mut response).await?;
            let reply = decode_sdp_response(&response[..size], 1, max_fragment as usize)?;
            ensure!(
                reply.continuation.is_empty(),
                "probe requires a single SDP reply"
            );
            let channel = parse_sdp_rfcomm_channel(reply.attributes)?;
            println!("Kernel connection complete; native Nearby SDP channel={channel}");
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    #[test]
    fn visible_role_deadlines_do_not_overlap_or_catch_up_after_a_stall() {
        let start = Instant::now();
        let ad = BleRoleWindow::new(BleRole::Advertising, start);
        let scan = ad.next(ad.until);
        let next_ad = scan.next(scan.until);
        assert!(scan.until - start < PEER_LIFETIME);
        let mut state = RadioState {
            scan_requested: true,
            scan_active: false,
            failed: false,
            advertisement_active: true,
            window: Some(ad),
            le_connected: false,
        };
        assert!(!state.permits_scan(start));
        state.advertisement_active = false;
        assert!(!state.permits_scan(ad.until)); // AD must actually hand off.
        state.window = Some(scan);
        assert!(state.permits_scan(scan.until - Duration::from_nanos(1)));
        assert!(!state.permits_scan(scan.until)); // No late GATT-triggered restart.
        state.window = Some(next_ad);
        assert!(!state.permits_scan(scan.until));

        let resumed = ad.next(start + Duration::from_secs(100));
        assert!(resumed.until > start + Duration::from_secs(100));
        state.window = Some(resumed);
        state.advertisement_active = true;
        assert!(!state.permits_scan(resumed.until - Duration::from_nanos(1)));
        state.advertisement_active = false;
        assert!(state.permits_scan(resumed.until - Duration::from_nanos(1)));
        state.window = None; // Hidden mode no longer has duty-cycle expiration.
        assert!(state.permits_scan(start + Duration::from_secs(1000)));
        state.failed = true;
        assert!(!state.permits_scan(start));
    }

    #[test]
    fn le_bearers_without_metadata_reads_block_roles_but_classic_audio_does_not() {
        let now = Instant::now();
        let ad = BleRoleWindow::new(BleRole::Advertising, now);
        let mut links = HciConnectionList {
            index: 0,
            count: 3,
            connections: [HciConnection::default(); MAX_CONTROLLER_CONNECTIONS],
        };
        links.connections[0].link_type = 1; // Established Classic audio ACL.
        links.connections[0].state = 1;
        let mut state = RadioState {
            scan_requested: true,
            scan_active: false,
            failed: false,
            advertisement_active: false,
            window: Some(ad),
            le_connected: links.has_le_bearer().unwrap(),
        };
        assert!(state.role_transition_due(ad.until));
        // Public-address LE requires no ReadValue or discovery metadata.
        links.connections[1].link_type = LE_LINK;
        links.connections[1].state = 1;
        links.connections[2].link_type = LE_LINK;
        links.connections[2].state = 8; // Disconnect is not yet confirmed.
        state.le_connected = links.has_le_bearer().unwrap();
        assert!(!state.role_transition_due(ad.until + Duration::from_secs(60)));
        state.window = None; // Hide can withdraw AD, not resume an LE scan.
        assert!(!state.permits_scan(now));
        links.connections[1].state = BT_CLOSED;
        state.le_connected = links.has_le_bearer().unwrap();
        assert!(!state.permits_scan(now));
        links.connections[2].state = BT_CLOSED;
        state.le_connected = links.has_le_bearer().unwrap();
        // Classic remains connected: aggregate Device1 Connected=true must
        // not keep an LE gate closed after its last actual bearer disappears.
        assert!(state.permits_scan(now));
        state.window = Some(ad);
        assert!(state.role_transition_due(ad.until));
    }

    #[test]
    fn incomplete_controller_snapshot_cannot_authorize_radio_handoff() {
        let mut links = HciConnectionList {
            index: 0,
            count: MAX_CONTROLLER_CONNECTIONS as u16,
            connections: [HciConnection::default(); MAX_CONTROLLER_CONNECTIONS],
        };
        links.connections.fill(HciConnection {
            link_type: 1,
            state: 1,
            ..Default::default()
        });
        assert!(links.has_le_bearer().is_err());
        links.count -= 1;
        assert!(!links.has_le_bearer().unwrap());
        links.count = u16::MAX;
        assert!(links.has_le_bearer().is_err());
    }

    #[test]
    fn duplicate_header_never_retries_failed_read_and_changed_header_invalidates_route() {
        let address = Address::new([1, 2, 3, 4, 5, 6]);
        let body = encode_ble_service_data(*b"AB12", &fixture_info(), address).unwrap();
        let header = parse_ble_legacy_header(&encode_ble_legacy_header(&body)).unwrap();
        let mut reads = None;
        let now = Instant::now();
        let radio = RadioState {
            scan_requested: true,
            scan_active: false,
            failed: false,
            advertisement_active: false,
            window: None,
            le_connected: false,
        };
        assert!(radio.begin_ble_header_read(&mut reads, header, now));
        // No frame was installed: this is the timeout/failed-read state.
        for _ in 0..10 {
            assert!(!radio.begin_ble_header_read(&mut reads, header, now));
        }
        reads.as_mut().unwrap().frame = Some(parse_ble_service_data(&body).unwrap());
        assert!(!radio.begin_ble_header_read(&mut reads, header, now));
        assert!(reads.as_ref().unwrap().frame.is_some());
        let mut changed = header;
        changed.wire[11] ^= 1;
        assert!(radio.begin_ble_header_read(&mut reads, changed, now));
        assert!(reads.as_ref().unwrap().frame.is_none());
        assert!(!radio.begin_ble_header_read(&mut reads, changed, now));
    }

    #[test]
    fn queued_header_does_not_consume_allowance_after_scan_deadline_or_during_ad() {
        let address = Address::new([1, 2, 3, 4, 5, 6]);
        let body = encode_ble_service_data(*b"AB12", &fixture_info(), address).unwrap();
        let header = parse_ble_legacy_header(&encode_ble_legacy_header(&body)).unwrap();
        let now = Instant::now();
        let scan = BleRoleWindow::new(BleRole::Scanning, now);
        let mut radio = RadioState {
            scan_requested: true,
            scan_active: false,
            failed: false,
            advertisement_active: false,
            window: Some(scan),
            le_connected: false,
        };
        let mut reads = None;
        assert!(radio.begin_ble_header_read(&mut reads, header, now));
        reads.as_mut().unwrap().frame = Some(parse_ble_service_data(&body).unwrap());
        let mut changed = header;
        changed.wire[11] ^= 1;
        assert!(!radio.begin_ble_header_read(&mut reads, changed, scan.until));
        assert_eq!(reads.as_ref().unwrap().header, header);
        assert!(reads.as_ref().unwrap().frame.is_some());
        let ad = scan.next(scan.until);
        radio.window = Some(ad);
        radio.advertisement_active = true;
        assert!(!radio.begin_ble_header_read(&mut reads, changed, scan.until));
        assert_eq!(reads.as_ref().unwrap().header, header);
        radio.window = Some(ad.next(ad.until));
        radio.advertisement_active = false;
        assert!(radio.begin_ble_header_read(&mut reads, changed, ad.until));
        assert_eq!(reads.as_ref().unwrap().header, changed);
        assert!(reads.as_ref().unwrap().frame.is_none());
        assert!(!radio.begin_ble_header_read(&mut reads, changed, ad.until));
    }

    #[test]
    fn regular_header_accepts_raw_and_websafe_forms_but_bounds_slots_and_bloom() {
        let body =
            encode_ble_service_data(*b"AB12", &fixture_info(), Address::new([1, 2, 3, 4, 5, 6]))
                .unwrap();
        let header = encode_ble_legacy_header(&body);
        let parsed = parse_ble_legacy_header(&header).unwrap();
        for encoded in [URL_SAFE.encode(header), URL_SAFE_NO_PAD.encode(header)] {
            assert_eq!(parse_ble_legacy_header(encoded.as_bytes()).unwrap(), parsed);
        }
        assert_eq!(parse_ble_legacy_header(&header[..15]).unwrap(), parsed);
        for len in 0..15 {
            assert!(parse_ble_legacy_header(&header[..len]).is_err());
        }
        assert!(parse_ble_legacy_header(&header[..16]).is_err());
        let mut broken = header;
        broken[0] &= 0xf0;
        assert!(parse_ble_legacy_header(&broken).is_err());
        broken = header;
        broken[0] = 0x21;
        assert!(parse_ble_legacy_header(&broken).is_err());
        broken = header;
        broken[1..11].fill(0);
        assert!(parse_ble_legacy_header(&broken).is_err());
        // The hash and PSM are part of cache identity, even with stable slots.
        for offset in [11, 14, 15, 16] {
            let mut changed = header;
            changed[offset] ^= 1;
            assert_ne!(parse_ble_legacy_header(&changed).unwrap(), parsed);
        }
        for slots in 1..=15 {
            let mut changed = header;
            changed[0] = 0x50 | slots; // Extended support does not disable GATT.
            let parsed = parse_ble_legacy_header(&changed).unwrap();
            for slot in 0..16 {
                let uuid = Uuid::from_u128(BLE_ADVERTISEMENT_SLOT.as_u128() | slot);
                assert_eq!(
                    is_ble_advertisement_slot(uuid, parsed.slots),
                    slot < slots as u128
                );
            }
        }
        assert!(!is_ble_advertisement_slot(BLE_UUID, 15));
    }

    #[test]
    fn unrelated_service_updates_cannot_renew_cached_nearby_radio_evidence() {
        let foreign = Uuid::from_u128(0x0000180f00001000800000805f9b34fb);
        let original: BleServiceData = [(BLE_UUID, vec![0x41]), (foreign, vec![1])].into();
        assert!(nearby_service_observation(&original));
        let mut changed = original.clone();
        changed.insert(foreign, vec![2]);
        assert!(nearby_service_observation(&changed)); // FEF3 really accompanies it.
        changed.remove(&BLE_UUID);
        assert!(!nearby_service_observation(&changed));
        assert!(!nearby_service_observation(&BleServiceData::new()));
        assert!(!DiscoveryEvidence::Classic.observes(DiscoverySource::Ble));
        assert!(!DiscoveryEvidence::Ble.observes(DiscoverySource::Classic));
    }

    fn fixture_sdp_sequence(payload: &[u8]) -> Vec<u8> {
        let mut bytes = if payload.len() <= u8::MAX as usize {
            vec![0x35, payload.len() as u8]
        } else {
            let mut header = vec![0x36];
            header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            header
        };
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fixture_sdp_record(service: Uuid, channel: u8) -> Vec<u8> {
        let mut record = vec![0x09, 0, 1, 0x35, 17, 0x1c];
        record.extend_from_slice(service.as_bytes());
        record.extend_from_slice(&[
            0x09, 0, 4, 0x35, 12, 0x35, 3, 0x19, 1, 0, // L2CAP.
            0x35, 5, 0x19, 0, 3, 0x08, channel, // RFCOMM + uint8 channel.
        ]);
        fixture_sdp_sequence(&record)
    }

    fn fixture_sdp_response(transaction: u16, attributes: &[u8], continuation: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x07];
        packet.extend_from_slice(&transaction.to_be_bytes());
        packet
            .extend_from_slice(&((3 + attributes.len() + continuation.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&(attributes.len() as u16).to_be_bytes());
        packet.extend_from_slice(attributes);
        packet.push(continuation.len() as u8);
        packet.extend_from_slice(continuation);
        packet
    }

    #[test]
    fn sdp_request_rejects_unusable_sizes_and_continuations() {
        let mut request = [0; 35 + MAX_SDP_CONTINUATION];
        assert!(encode_sdp_request(&mut request, 8, 6, &[]).is_err());
        assert!(encode_sdp_request(&mut request, 8, 1000, &[0; 17]).is_err());
    }

    #[test]
    fn sdp_continuations_reassemble_every_descriptor_boundary() {
        let attributes = fixture_sdp_sequence(&fixture_sdp_record(SERVICE_UUID, 7));
        for split in 1..attributes.len() {
            let first = fixture_sdp_response(11, &attributes[..split], &[0, 1, 2]);
            let first = decode_sdp_response(&first, 11, 1000).unwrap();
            let mut assembled = Vec::new();
            append_sdp_fragment(&mut assembled, &first).unwrap();
            let mut request = [0; 35 + MAX_SDP_CONTINUATION];
            let length = encode_sdp_request(&mut request, 12, 1000, first.continuation).unwrap();
            assert_eq!(&request[34..length], &[3, 0, 1, 2]);
            let second = fixture_sdp_response(12, &attributes[split..], &[]);
            // A previous transaction's late response cannot complete this query.
            assert!(decode_sdp_response(&second, 11, 1000).is_err());
            let second = decode_sdp_response(&second, 12, 1000).unwrap();
            append_sdp_fragment(&mut assembled, &second).unwrap();
            assert!(second.continuation.is_empty());
            assert_eq!(parse_sdp_rfcomm_channel(&assembled).unwrap(), 7);
        }
    }

    #[test]
    fn sdp_response_rejects_wrong_pdu_lengths_transactions_and_continuations() {
        let attributes = fixture_sdp_sequence(&fixture_sdp_record(SERVICE_UUID, 7));
        let valid = fixture_sdp_response(12, &attributes, &[]);
        for length in 0..valid.len() {
            assert!(decode_sdp_response(&valid[..length], 12, 1000).is_err());
        }
        assert!(decode_sdp_response(&valid, 13, 1000).is_err());
        assert!(decode_sdp_response(&valid, 12, attributes.len() - 1).is_err());
        for (offset, value) in [
            (0, 0x05),            // AttributeResponse is not SearchAttributeResponse.
            (3, 1),               // Wrong PDU parameter length.
            (5, 1),               // Attribute bytes overrun the datagram.
            (valid.len() - 1, 1), // Missing continuation byte.
        ] {
            let mut malformed = valid.clone();
            malformed[offset] = value;
            assert!(decode_sdp_response(&malformed, 12, 1000).is_err());
        }
        assert!(decode_sdp_response(&fixture_sdp_response(12, &[], &[1]), 12, 1000).is_err());
        assert!(decode_sdp_response(&fixture_sdp_response(12, &[1], &[0; 17]), 12, 1000).is_err());
        let error = decode_sdp_response(&[0x01, 0, 12, 0, 2, 0, 2], 12, 1000);
        assert!(error.err().unwrap().to_string().contains("0x0002"));
        assert!(decode_sdp_response(&[0x01, 0, 12, 0, 1, 2], 12, 1000).is_err());
    }

    #[test]
    fn sdp_attribute_aggregation_is_bounded_before_copying() {
        let mut attributes = vec![0; MAX_SDP_ATTRIBUTES - 1];
        let overflow = SdpFragment {
            attributes: &[1, 2],
            continuation: &[3],
        };
        assert!(append_sdp_fragment(&mut attributes, &overflow).is_err());
        assert_eq!(attributes.len(), MAX_SDP_ATTRIBUTES - 1);
        let last = SdpFragment {
            attributes: &[1],
            continuation: &[],
        };
        append_sdp_fragment(&mut attributes, &last).unwrap();
        assert_eq!(attributes.len(), MAX_SDP_ATTRIBUTES);
        assert!(append_sdp_fragment(&mut attributes, &last).is_err());
        attributes.push(0);
        assert!(parse_sdp_rfcomm_channel(&attributes).is_err());
        assert!(append_sdp_fragment(&mut attributes, &last).is_err());
    }

    #[test]
    fn sdp_sequence_and_protocol_uuid_length_variants_keep_the_channel() {
        let record = fixture_sdp_record(SERVICE_UUID, 7);
        for mut header in [
            vec![0x35, record.len() as u8],
            vec![0x36, 0, record.len() as u8],
            vec![0x37, 0, 0, 0, record.len() as u8],
        ] {
            header.extend_from_slice(&record);
            assert_eq!(parse_sdp_rfcomm_channel(&header).unwrap(), 7);
        }
        let l2cap_uuid = Uuid::from_u128(0x0000010000001000800000805f9b34fb);
        let rfcomm_uuid = Uuid::from_u128(0x0000000300001000800000805f9b34fb);
        let mut l2cap128 = vec![0x1c];
        l2cap128.extend_from_slice(l2cap_uuid.as_bytes());
        let mut rfcomm128 = vec![0x1c];
        rfcomm128.extend_from_slice(rfcomm_uuid.as_bytes());
        for (l2cap, mut rfcomm) in [
            (vec![0x19, 1, 0], vec![0x19, 0, 3]),
            (vec![0x1a, 0, 0, 1, 0], vec![0x1a, 0, 0, 0, 3]),
            (l2cap128, rfcomm128),
        ] {
            rfcomm.extend_from_slice(&[0x08, 7]);
            let mut protocols = fixture_sdp_sequence(&l2cap);
            protocols.extend_from_slice(&fixture_sdp_sequence(&rfcomm));
            assert_eq!(sdp_protocol_channel(&protocols).unwrap(), Some(7));
        }
    }

    #[test]
    fn sdp_route_requires_the_matching_service_and_unambiguous_channel() {
        for channel in [1, 7, 30] {
            let response = fixture_sdp_sequence(&fixture_sdp_record(SERVICE_UUID, channel));
            assert_eq!(parse_sdp_rfcomm_channel(&response).unwrap(), channel);
        }
        let other = Uuid::from_u128(0x0000000300001000800000805f9b34fb);
        assert!(
            parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&fixture_sdp_record(other, 4))).is_err()
        );
        let mut records = fixture_sdp_record(other, 4);
        records.extend_from_slice(&fixture_sdp_record(SERVICE_UUID, 7));
        assert_eq!(
            parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&records)).unwrap(),
            7
        );
        records.extend_from_slice(&fixture_sdp_record(SERVICE_UUID, 7));
        assert_eq!(
            parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&records)).unwrap(),
            7
        );
        records.extend_from_slice(&fixture_sdp_record(SERVICE_UUID, 9));
        assert!(parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&records)).is_err());
        assert!(parse_sdp_rfcomm_channel(&[0x35, 0]).is_err());
    }

    #[test]
    fn sdp_protocol_alternatives_skip_other_stacks_but_reject_ambiguous_rfcomm() {
        let record = fixture_sdp_record(SERVICE_UUID, 7);
        let rfcomm = &record[27..];
        let l2cap = fixture_sdp_sequence(&[0x35, 6, 0x19, 1, 0, 0x09, 0x10, 1]);
        let mut alternatives = l2cap;
        alternatives.extend_from_slice(rfcomm);
        let mut alternative = fixture_sdp_sequence(&alternatives);
        alternative[0] = 0x3d;
        let mut attributes = record[2..24].to_vec();
        attributes.extend_from_slice(&[0x09, 0, 4]);
        attributes.extend_from_slice(&alternative);
        let response = fixture_sdp_sequence(&fixture_sdp_sequence(&attributes));
        assert_eq!(parse_sdp_rfcomm_channel(&response).unwrap(), 7);

        let another = fixture_sdp_record(SERVICE_UUID, 9);
        alternatives.extend_from_slice(&another[27..]);
        let mut alternative = fixture_sdp_sequence(&alternatives);
        alternative[0] = 0x3d;
        attributes.truncate(25);
        attributes.extend_from_slice(&alternative);
        let response = fixture_sdp_sequence(&fixture_sdp_sequence(&attributes));
        assert!(parse_sdp_rfcomm_channel(&response).is_err());
    }

    #[test]
    fn sdp_search_match_outside_service_class_does_not_abort_valid_record() {
        let unrelated = fixture_sdp_record(Uuid::from_u128(0x0000110100001000800000805f9b34fb), 7);
        let mut record = unrelated[2..24].to_vec();
        // A legal non-RFCOMM L2CAP PSM and the searched UUID as ServiceID.
        record.extend_from_slice(&[0x09, 0, 4, 0x35, 8, 0x35, 6, 0x19, 1, 0, 0x09, 0x10, 1]);
        record.extend_from_slice(&[0x09, 0, 3, 0x1c]);
        record.extend_from_slice(SERVICE_UUID.as_bytes());
        let mut records = fixture_sdp_sequence(&record);
        records.extend_from_slice(&fixture_sdp_record(SERVICE_UUID, 7));
        assert_eq!(
            parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&records)).unwrap(),
            7
        );
    }

    #[test]
    fn malformed_sdp_elements_and_protocol_parameters_cannot_select_a_route() {
        let valid = fixture_sdp_sequence(&fixture_sdp_record(SERVICE_UUID, 7));
        for length in 0..valid.len() {
            assert!(parse_sdp_rfcomm_channel(&valid[..length]).is_err());
        }
        for bytes in [
            vec![0x34],          // Sequence with a fixed-width size descriptor.
            vec![0x1d, 0],       // UUID with a variable-width size descriptor.
            vec![0x19, 0],       // Truncated UUID16.
            vec![0x36, 0],       // Truncated 16-bit length.
            vec![0x37, 0, 0, 0], // Truncated 32-bit length.
            vec![0x35, 1],       // Truncated sequence payload.
            vec![0x35, 255, 0],
        ] {
            assert!(take_sdp_element(&mut bytes.as_slice()).is_err());
        }
        for channel in [0, 31, 255] {
            assert!(
                parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&fixture_sdp_record(
                    SERVICE_UUID,
                    channel
                )))
                .is_err()
            );
        }
        for protocols in [
            vec![0x35, 5, 0x19, 0, 3, 0x08, 7], // Missing L2CAP layer.
            vec![
                0x35, 6, 0x19, 1, 0, 0x09, 0, 1, 0x35, 5, 0x19, 0, 3, 0x08, 7,
            ], // Wrong PSM.
            vec![0x35, 3, 0x19, 1, 0, 0x35, 6, 0x19, 0, 3, 0x09, 0, 7], // uint16 channel.
            vec![
                0x35, 3, 0x19, 1, 0, 0x35, 5, 0x19, 0, 3, 0x08, 7, 0x35, 5, 0x19, 0, 3, 0x08, 9,
            ], // Duplicate RFCOMM.
        ] {
            assert!(sdp_protocol_channel(&protocols).is_err());
        }
        let record = fixture_sdp_record(SERVICE_UUID, 7);
        let mut duplicate_classes = record[2..].to_vec();
        duplicate_classes.extend_from_slice(&record[2..24]);
        assert!(
            parse_sdp_rfcomm_channel(&fixture_sdp_sequence(&fixture_sdp_sequence(
                &duplicate_classes
            )))
            .is_err()
        );
        let mut trailing = valid;
        trailing.push(0);
        assert!(parse_sdp_rfcomm_channel(&trailing).is_err());
    }

    fn fixture_info() -> Vec<u8> {
        let mut info = vec![2]; // Phone, public name.
        info.extend_from_slice(&[0; 16]);
        info.extend_from_slice(&[5, b'P', b'i', b'x', b'e', b'l']);
        info
    }

    fn fixture_owned_settings() -> OwnedAdapterSettings {
        OwnedAdapterSettings {
            alias: OwnedProperty {
                previous: "Laptop".into(),
                applied: encode_classic_name(*b"AB12", &fixture_info()).unwrap(),
                pending: true,
            },
            discoverable: OwnedProperty {
                previous: false,
                applied: true,
                pending: true,
            },
            timeout: OwnedProperty {
                previous: 120,
                applied: 0,
                pending: true,
            },
        }
    }

    #[test]
    fn recovery_preserves_rename_but_restores_temporary_discoverability() {
        let address = Address::new([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        let settings = fixture_owned_settings();
        // Exercise the actual decoder and restoration decisions used by both
        // startup recovery and live cleanup.
        let recovered = OwnedAdapterSettings::decode(&settings.encode(address), address).unwrap();
        assert_eq!(
            recovered.alias.decision(&"User renamed laptop".into()),
            RestoreDecision::Preserve
        );
        assert_eq!(
            recovered.discoverable.decision(&true),
            RestoreDecision::Restore
        );
        assert_eq!(recovered.timeout.decision(&0), RestoreDecision::Restore);
        assert_eq!(recovered.discoverable.previous, false);
        assert_eq!(recovered.timeout.previous, 120);
    }

    #[test]
    fn restoration_preserves_independent_user_changes() {
        let mut settings = fixture_owned_settings();
        // A user-set timeout does not relinquish the service-owned name or
        // Discoverable value, and must not itself be overwritten.
        assert_eq!(settings.timeout.decision(&90), RestoreDecision::Preserve);
        assert_eq!(
            settings.discoverable.decision(&true),
            RestoreDecision::Restore
        );
        assert_eq!(
            settings.alias.decision(&settings.alias.applied),
            RestoreDecision::Restore
        );
        // With an initially discoverable adapter, an explicit user disable is
        // foreign even though the timeout remains service-owned.
        settings.discoverable.previous = true;
        assert_eq!(
            settings.discoverable.decision(&false),
            RestoreDecision::Preserve
        );
        assert_eq!(settings.timeout.decision(&0), RestoreDecision::Restore);
        assert_eq!(
            settings.alias.decision(&settings.alias.previous),
            RestoreDecision::Resolved
        );
        assert_eq!(settings.timeout.decision(&120), RestoreDecision::Resolved);
    }

    #[test]
    fn journal_checkpoints_each_property_without_reclaiming_it() {
        let address = Address::new([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        let mut settings = fixture_owned_settings();
        settings.discoverable.pending = false;
        settings.timeout.pending = false;
        // A crash before those two mutations must not claim coincidentally
        // matching values subsequently set by somebody else.
        let recovered = OwnedAdapterSettings::decode(&settings.encode(address), address).unwrap();
        assert_eq!(
            recovered.alias.decision(&recovered.alias.applied),
            RestoreDecision::Restore
        );
        assert_eq!(
            recovered.discoverable.decision(&true),
            RestoreDecision::Resolved
        );
        assert_eq!(recovered.timeout.decision(&0), RestoreDecision::Resolved);
        // A checkpointed rename stays relinquished if the user later chooses
        // the encoded name again while timeout restoration is still pending.
        settings.alias.pending = false;
        settings.timeout.pending = true;
        let recovered = OwnedAdapterSettings::decode(&settings.encode(address), address).unwrap();
        assert_eq!(
            recovered.alias.decision(&recovered.alias.applied),
            RestoreDecision::Resolved
        );
        assert_eq!(recovered.timeout.decision(&0), RestoreDecision::Restore);
    }

    #[test]
    fn nearby_bloom_hash_uses_x64_not_x86_murmur_and_little_endian_tail() {
        // Independent mmh3 x64_128 test_hash64 vector:
        // https://github.com/hajimes/mmh3/blob/master/tests/test_mmh3.py
        assert_eq!(murmur3_low64(b"foo"), 16_316_970_633_193_145_697);
        assert_eq!(murmur3_low64(b""), 0);
    }

    #[test]
    fn endpoint_name_truncation_preserves_utf8_and_length_fields() {
        for prefix in 109..=114 {
            for suffix in ["é", "€", "🦀"] {
                let name = format!("{}{suffix}{suffix}", "a".repeat(prefix));
                let info = encode_endpoint_info(&name).unwrap();
                let (parsed, _) = endpoint_fields(*b"AB12", &info).unwrap();
                assert!(name.starts_with(parsed));
                assert_eq!(info.len(), 18 + parsed.len());
                assert_eq!(info[17] as usize, parsed.len());
                assert!(parsed.len() <= 113);
                // Truncation must be maximal, not drop a whole valid character.
                let next = name[parsed.len()..].chars().next();
                if let Some(next) = next {
                    assert!(parsed.len() + next.len_utf8() > 113);
                }
                assert_eq!(
                    parse_classic_name(&encode_classic_name(*b"AB12", &info).unwrap())
                        .unwrap()
                        .name,
                    parsed
                );
            }
        }
        assert!(encode_endpoint_info("").is_err());
        assert!(encode_endpoint_info(&format!("{}\0", "a".repeat(200))).is_err());
    }

    #[test]
    fn gatt_read_blob_reassembles_every_mtu_without_offset_wraparound() {
        let info = encode_endpoint_info(&"z".repeat(200)).unwrap();
        let mac = Address::new([1, 2, 3, 4, 5, 6]);
        let body = encode_ble_service_data(*b"AB12", &info, mac).unwrap();
        for mtu in [23, 24, 64, 185, 517] {
            let mut assembled = Vec::new();
            loop {
                let part = read_ble_advertisement(&body, assembled.len(), mtu, true).unwrap();
                assert!(part.len() < mtu);
                if part.is_empty() {
                    break;
                }
                assembled.extend(part);
            }
            let (address, endpoint) = parse_ble_service_data(&assembled).unwrap();
            assert_eq!(address, mac);
            assert_eq!(endpoint.name, "z".repeat(113));
        }
        assert_eq!(
            read_ble_advertisement(&body, body.len(), 23, true).unwrap(),
            []
        );
        for offset in [body.len() + 1, usize::MAX] {
            assert!(matches!(
                read_ble_advertisement(&body, offset, 23, true),
                Err(bluer::gatt::local::ReqError::InvalidOffset)
            ));
        }
        for offset in [0, body.len(), usize::MAX] {
            assert!(matches!(
                read_ble_advertisement(&body, offset, 23, false),
                Err(bluer::gatt::local::ReqError::NotPermitted)
            ));
        }
        assert!(read_ble_advertisement(&body, 0, 0, true).is_err());
    }

    #[test]
    fn legacy_discovery_header_cannot_be_misparsed_as_classic_route() {
        let mac = Address::new([1, 2, 3, 4, 5, 6]);
        let info = encode_endpoint_info(&"🦀".repeat(40)).unwrap();
        let body = encode_ble_service_data(*b"AB12", &info, mac).unwrap();
        let header = encode_ble_legacy_header(&body);
        // The regular body is fetched through GATT, not crammed into AD.
        assert!(body.len() + 7 > 31);
        assert!(header.len() + 7 <= 31);
        for length in 0..=header.len() {
            assert!(parse_ble_service_data(&header[..length]).is_err());
        }
        // A corrupted one-byte endpoint length must not consume the MAC.
        let mut broken = body;
        broken[16] = 255;
        assert!(parse_ble_service_data(&broken).is_err());
    }

    #[test]
    fn classic_google_layout_and_name() {
        let info = fixture_info();
        let name = encode_classic_name(*b"AB12", &info).unwrap();
        let raw = URL_SAFE_NO_PAD.decode(&name).unwrap();
        assert_eq!(
            &raw[..16],
            &[0x23, b'A', b'B', b'1', b'2', 0xfc, 0x9f, 0x5e, 0, 0, 0, 0, 0, 0, 0, 23]
        );
        let parsed = parse_classic_name(&name).unwrap();
        assert_eq!(parsed.name, "Pixel");
        assert_eq!(parsed.device_type, DeviceType::Phone);
        assert_eq!(parsed.endpoint_id, *b"AB12");
    }

    #[test]
    fn rejects_malformed_classic_names() {
        let info = fixture_info();
        let name = encode_classic_name(*b"AB12", &info).unwrap();
        let raw = URL_SAFE_NO_PAD.decode(name).unwrap();
        for length in 0..raw.len() {
            assert!(parse_classic_name(&URL_SAFE_NO_PAD.encode(&raw[..length])).is_err());
        }
        for (offset, value) in [
            (0, 0x43),
            (5, 0),
            (15, 255),
            (16, 0x10),
            (33, 255),
            (34, 255),
        ] {
            let mut broken = raw.clone();
            broken[offset] = value;
            assert!(parse_classic_name(&URL_SAFE_NO_PAD.encode(broken)).is_err());
        }
        assert!(parse_classic_name("not base64!").is_err());
    }

    #[test]
    fn ble_mac_is_explicit_not_scan_address() {
        let mac = Address::new([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        let bytes = encode_ble_service_data(*b"AB12", &fixture_info(), mac).unwrap();
        let (route, parsed) = parse_ble_service_data(&bytes).unwrap();
        assert_eq!(route, mac);
        assert_eq!(parsed.name, "Pixel");
        for length in 0..bytes.len() - 4 {
            assert!(parse_ble_service_data(&bytes[..length]).is_err());
        }
        let mut broken = bytes.clone();
        broken[0] |= 2; // Fast frame cannot prove Classic MAC.
        assert!(parse_ble_service_data(&broken).is_err());
        let mut broken = bytes.clone();
        broken[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_ble_service_data(&broken).is_err());
        let mut broken = bytes;
        let mac_offset = 8 + 9 + fixture_info().len();
        broken[mac_offset..mac_offset + 6].fill(0);
        assert!(parse_ble_service_data(&broken).is_err());
    }

    #[test]
    fn only_nearby_uuid_routes_are_accepted() {
        assert_eq!(
            SERVICE_UUID.to_string(),
            "a82efa21-ae5c-3dde-9bbc-f16da7b16c5a"
        );
        assert!(parse_route(&format!("bt:12:34:56:78:9A:BC/{SERVICE_UUID}")).is_ok());
        for route in [
            "bt:00:00:00:00:00:00/a82efa21-ae5c-3dde-9bbc-f16da7b16c5a",
            "bt:12:34:56:78:9A:BC/00001101-0000-1000-8000-00805f9b34fb",
            "bt:12:34:56:78:9A:BC",
            "bt:no-mac/a82efa21-ae5c-3dde-9bbc-f16da7b16c5a",
        ] {
            assert!(parse_route(route).is_err());
        }
    }
}

#[cfg(test)]
mod discovery_guard_tests {
    use super::*;

    #[test]
    fn classic_connections_protect_only_their_own_device_from_gatt_disconnect() {
        let address = Address::new([1, 2, 3, 4, 5, 6]);
        let mut native = address.0;
        native.reverse();
        let mut links = HciConnectionList { index: 0, count: 2, connections: [HciConnection::default(); MAX_CONTROLLER_CONNECTIONS] };
        links.connections[0] = HciConnection { address: native, link_type: LE_LINK, state: 1, ..Default::default() };
        links.connections[1] = HciConnection { address: [9; 6], link_type: 1, state: 1, ..Default::default() };
        assert!(!links.has_classic_bearer(address).unwrap());
        links.connections[1].address = native;
        assert!(links.has_classic_bearer(address).unwrap());
        links.connections[1].state = BT_CLOSED;
        assert!(!links.has_classic_bearer(address).unwrap());
        links.count = MAX_CONTROLLER_CONNECTIONS as u16;
        assert!(links.has_classic_bearer(address).is_err());
    }
}
