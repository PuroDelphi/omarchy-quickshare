//! Discovery records and BlueZ signals share one bounded, expiring lifetime.
use super::*;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const MAX_SIGNAL_BODY: usize = 16 * 1024;
const MAX_OBJECT_REPLY: usize = 1024 * 1024;
type Properties = HashMap<String, OwnedValue>;
type Interfaces = HashMap<String, Properties>;

pub(super) struct DiscoveryRecord {
    pub address: Address,
    seen: Instant,
    pub header: Option<BleHeaderRead>,
    pub peer: Option<Peer>,
}

pub(super) struct DiscoveryRecords {
    pub entries: Vec<DiscoveryRecord>,
}

impl Default for DiscoveryRecords {
    fn default() -> Self { Self { entries: Vec::with_capacity(crate::DISCOVERY_PEER_LIMIT) } }
}

impl DiscoveryRecords {
    fn withdraw(&self, old: Option<Peer>, sender: &broadcast::Sender<EndpointInfo>) {
        if let Some(mut peer) = old {
            if !self.entries.iter().any(|entry| entry.peer.as_ref().is_some_and(|other| other.endpoint.id == peer.endpoint.id)) {
                peer.endpoint.present = Some(false);
                let _ = sender.send(peer.endpoint);
            }
        }
    }

    pub fn remove(&mut self, address: Address, sender: &broadcast::Sender<EndpointInfo>) {
        if let Some(index) = self.entries.iter().position(|entry| entry.address == address) {
            let removed = self.entries.swap_remove(index);
            self.withdraw(removed.peer, sender);
        }
    }

    pub fn expire(&mut self, now: Instant, sender: &broadcast::Sender<EndpointInfo>) {
        let mut index = 0;
        while index < self.entries.len() {
            if now.duration_since(self.entries[index].seen) >= PEER_LIFETIME {
                let removed = self.entries.swap_remove(index);
                self.withdraw(removed.peer, sender);
                continue;
            }
            if self.entries[index].peer.as_ref().is_some_and(|peer| now.duration_since(peer.seen) >= PEER_LIFETIME) {
                let old = self.entries[index].peer.take();
                self.withdraw(old, sender);
            }
            index += 1;
        }
    }

    pub fn admit(&mut self, address: Address, now: Instant, sender: &broadcast::Sender<EndpointInfo>) -> usize {
        self.expire(now, sender);
        if let Some(index) = self.entries.iter().position(|entry| entry.address == address) {
            self.entries[index].seen = now;
            return index;
        }
        if self.entries.len() == crate::DISCOVERY_PEER_LIMIT {
            let index = self.entries.iter().enumerate().min_by_key(|(_, entry)| entry.seen).expect("full discovery cache").0;
            let removed = self.entries.swap_remove(index);
            self.withdraw(removed.peer, sender);
        }
        self.entries.push(DiscoveryRecord { address, seen: now, header: None, peer: None });
        self.entries.len() - 1
    }

    pub fn update(&mut self, index: usize, observation: Option<(EndpointInfo, DiscoverySource)>, evidence: DiscoveryEvidence,
        now: Instant, sender: &broadcast::Sender<EndpointInfo>) {
        if let Some((endpoint, source)) = observation {
            let changed = self.entries[index].peer.as_ref().is_none_or(|peer|
                peer.endpoint.id != endpoint.id || peer.endpoint.name != endpoint.name || peer.endpoint.rtype != endpoint.rtype);
            if changed {
                let old = self.entries[index].peer.take();
                self.withdraw(old, sender);
                let _ = sender.send(endpoint.clone());
            }
            self.entries[index].peer = Some(Peer { endpoint, seen: now, source });
        } else if self.entries[index].peer.as_ref().is_some_and(|peer| evidence.observes(peer.source)) {
            let old = self.entries[index].peer.take();
            self.withdraw(old, sender);
        }
    }

    pub fn clear(&mut self, sender: &broadcast::Sender<EndpointInfo>) {
        while let Some(entry) = self.entries.pop() { self.withdraw(entry.peer, sender); }
    }
}

pub(super) async fn bluez_owner(bus: &zbus::Connection) -> Result<String> {
    let proxy = zbus::fdo::DBusProxy::new(bus).await?;
    Ok(timeout(OPERATION_TIMEOUT, proxy.get_name_owner("org.bluez".try_into()?)).await??.to_string())
}

pub(super) async fn signals(_bus: &zbus::Connection, owner: &str, path: Option<&str>) -> Result<zbus::MessageStream> {
    // Signal backpressure must not block the connection carrying Get/Connect/
    // Disconnect replies while an advertisement read is in progress.
    let bus = timeout(OPERATION_TIMEOUT, zbus::Connection::system()).await??;
    let mut rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(owner)?;
    if let Some(path) = path { rule = rule.path(path)?.interface("org.freedesktop.DBus.Properties")?.member("PropertiesChanged")?; }
    Ok(timeout(OPERATION_TIMEOUT, zbus::MessageStream::for_match_rule(rule.build(), &bus, Some(crate::DISCOVERY_PEER_LIMIT))).await??)
}

pub(super) fn changed_properties(message: &zbus::Message, interface: &str) -> Result<Option<Properties>> {
    ensure!(message.body().len() <= MAX_SIGNAL_BODY, "BlueZ property signal exceeds discovery budget");
    let header = message.header();
    if header.interface().map(|value| value.as_str()) != Some("org.freedesktop.DBus.Properties")
        || header.member().map(|value| value.as_str()) != Some("PropertiesChanged") { return Ok(None); }
    let (changed_interface, properties, _): (String, Properties, Vec<String>) = message.body().deserialize()?;
    Ok((changed_interface == interface).then_some(properties))
}

pub(super) fn flag(properties: &Properties, key: &str) -> Result<bool> {
    Ok(bool::try_from(properties.get(key).with_context(|| format!("BlueZ omitted {key}"))?)?)
}

fn device_address(path: &str, adapter: &str) -> Option<Address> {
    let suffix = path.strip_prefix(adapter)?.strip_prefix("/dev_")?;
    if suffix.len() != 17 { return None; }
    suffix.replace('_', ":").parse().ok()
}

pub(super) enum Observation {
    Added(Address),
    Removed(Address),
    Radio(Address, DiscoveryEvidence, Option<BleServiceData>),
    PoweredOff,
}

pub(super) fn observation(message: &zbus::Message, adapter: &str) -> Result<Option<Observation>> {
    ensure!(message.body().len() <= MAX_SIGNAL_BODY, "BlueZ discovery signal exceeds budget");
    let header = message.header();
    if header.interface().map(|value| value.as_str()) == Some("org.freedesktop.DBus.ObjectManager") {
        match header.member().map(|value| value.as_str()) {
            Some("InterfacesAdded") => {
                let (path, interfaces): (OwnedObjectPath, Interfaces) = message.body().deserialize()?;
                return Ok(device_address(path.as_str(), adapter).filter(|_| interfaces.contains_key("org.bluez.Device1")).map(Observation::Added));
            }
            Some("InterfacesRemoved") => {
                let (path, interfaces): (OwnedObjectPath, Vec<String>) = message.body().deserialize()?;
                return Ok(device_address(path.as_str(), adapter).filter(|_| interfaces.iter().any(|value| value == "org.bluez.Device1")).map(Observation::Removed));
            }
            _ => return Ok(None),
        }
    }
    let Some(path) = header.path() else { return Ok(None); };
    if path.as_str() == adapter {
        return Ok(changed_properties(message, "org.bluez.Adapter1")?
            .filter(|properties| properties.get("Powered").is_some_and(|value| bool::try_from(value).ok() == Some(false)))
            .map(|_| Observation::PoweredOff));
    }
    let Some(address) = device_address(path.as_str(), adapter) else { return Ok(None); };
    let Some(mut properties) = changed_properties(message, "org.bluez.Device1")? else { return Ok(None); };
    if let Some(value) = properties.remove("ServiceData") {
        let values: HashMap<String, OwnedValue> = value.try_into()?;
        let mut data = HashMap::new();
        if let Some(value) = values.iter().find_map(|(uuid, value)| (uuid.parse::<Uuid>().ok() == Some(BLE_UUID)).then_some(value)) {
            let bytes: Vec<u8> = value.try_clone()?.try_into()?;
            if bytes.len() <= MAX_ENDPOINT_INFO + 32 { data.insert(BLE_UUID, bytes); }
        }
        if nearby_service_observation(&data) { return Ok(Some(Observation::Radio(address, DiscoveryEvidence::Ble, Some(data)))); }
    }
    Ok(properties.keys().any(|key| matches!(key.as_str(), "RSSI" | "Name" | "Class"))
        .then_some(Observation::Radio(address, DiscoveryEvidence::Classic, None)))
}

pub(super) struct DevicePolicy {
    pub connected: bool,
    pub paired: bool,
    pub trusted: bool,
    pub blocked: bool,
}

impl DevicePolicy {
    pub fn may_connect(&self, device_count: usize) -> bool {
        !self.connected && !self.paired && !self.trusted && !self.blocked && device_count < crate::DISCOVERY_PEER_LIMIT
    }
    pub fn may_disconnect(&self) -> bool { !self.paired && !self.trusted && !self.blocked }
}

pub(super) fn device_path(adapter: &str, address: Address) -> String {
    format!("{adapter}/dev_{}", address.to_string().replace(':', "_"))
}

async fn device_properties(bus: &zbus::Connection, owner: &str, path: &str) -> Result<Properties> {
    let proxy = zbus::Proxy::new(bus, owner, path, "org.freedesktop.DBus.Properties").await?;
    let reply = timeout(OPERATION_TIMEOUT, proxy.call_method("GetAll", &("org.bluez.Device1",))).await??;
    ensure!(reply.body().len() <= MAX_SIGNAL_BODY, "BlueZ device properties exceed discovery budget");
    Ok(reply.body().deserialize()?)
}

pub(super) async fn device_policy(bus: &zbus::Connection, owner: &str, path: &str) -> Result<DevicePolicy> {
    let properties = device_properties(bus, owner, path).await?;
    Ok(DevicePolicy { connected: flag(&properties, "Connected")?, paired: flag(&properties, "Paired")? || flag(&properties, "Bonded")?, trusted: flag(&properties, "Trusted")?, blocked: flag(&properties, "Blocked")? })
}

async fn objects(bus: &zbus::Connection, owner: &str) -> Result<HashMap<OwnedObjectPath, Interfaces>> {
    let proxy = zbus::Proxy::new(bus, owner, "/", "org.freedesktop.DBus.ObjectManager").await?;
    let reply = timeout(OPERATION_TIMEOUT, proxy.call_method("GetManagedObjects", &())).await??;
    ensure!(reply.body().len() <= MAX_OBJECT_REPLY, "BlueZ object reply exceeds automatic discovery budget");
    Ok(reply.body().deserialize()?)
}

pub(super) async fn device_count(bus: &zbus::Connection, owner: &str, adapter: &str) -> Result<usize> {
    Ok(objects(bus, owner).await?.iter().filter(|(path, interfaces)|
        device_address(path.as_str(), adapter).is_some() && interfaces.contains_key("org.bluez.Device1")).count())
}

fn property_uuid(properties: &Properties) -> Option<Uuid> {
    <&str>::try_from(properties.get("UUID")?).ok()?.parse().ok()
}

pub(super) async fn read_slots(bus: &zbus::Connection, owner: &str, path: &str, slots: u8) -> Result<Option<(Address, ParsedEndpoint)>> {
    // BlueR services() creates another retained per-device subscription.
    // Use an owned, bounded signal stream and pin all GATT calls to this
    // daemon owner instead of following a replacement org.bluez instance.
    let mut events = signals(bus, owner, Some(path)).await?;
    loop {
        let properties = device_properties(bus, owner, path).await?;
        if flag(&properties, "ServicesResolved")? { break; }
        ensure!(flag(&properties, "Connected")?, "GATT device disconnected before service resolution");
        events.next().await.context("BlueZ GATT property stream ended")??;
    }
    let snapshot = objects(bus, owner).await?;
    let mut read_slots = 0_u16;
    let device_prefix = format!("{path}/");
    for (service_path, interfaces) in &snapshot {
        let Some(service) = interfaces.get("org.bluez.GattService1") else { continue; };
        if property_uuid(service) != Some(BLE_UUID)
            || !service_path.as_str().starts_with(&device_prefix) { continue; }
        let service_prefix = format!("{service_path}/");
        for (characteristic_path, interfaces) in &snapshot {
            let Some(properties) = interfaces.get("org.bluez.GattCharacteristic1") else { continue; };
            if !characteristic_path.as_str().starts_with(&service_prefix) { continue; }
            let Some(uuid) = property_uuid(properties).filter(|uuid| is_ble_advertisement_slot(*uuid, slots)) else { continue; };
            let slot = (uuid.as_u128() - BLE_ADVERTISEMENT_SLOT.as_u128()) as u8;
            let mask = 1_u16 << slot;
            if read_slots & mask != 0 { continue; }
            read_slots |= mask;
            let proxy = zbus::Proxy::new(bus, owner, characteristic_path.as_str(), "org.bluez.GattCharacteristic1").await?;
            let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            let reply = proxy.call_method("ReadValue", &(options,)).await?;
            ensure!(reply.body().len() <= MAX_SIGNAL_BODY, "GATT slot reply exceeds discovery budget");
            let body: Vec<u8> = reply.body().deserialize()?;
            if body.len() <= MAX_ENDPOINT_INFO + 32 {
                if let Ok(frame) = parse_ble_service_data(&body) { return Ok(Some(frame)); }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(index: usize) -> Address { Address::new([2, 0, 0, 0, (index >> 8) as u8, index as u8]) }
    fn endpoint(id: usize) -> EndpointInfo {
        EndpointInfo { id: format!("bt:fixture-{id}"), present: Some(true), transport: Some("bluetooth".into()), ..Default::default() }
    }

    #[test]
    fn rotation_pressure_withdraws_evicted_routes_and_forgets_their_headers() {
        let (sender, mut events) = broadcast::channel(4096);
        let now = Instant::now();
        let mut records = DiscoveryRecords::default();
        let mut visible = std::collections::HashSet::new();
        for index in 0..512 {
            let slot = records.admit(address(index), now + Duration::from_millis(index as u64), &sender);
            assert!(records.entries[slot].header.is_none());
            records.entries[slot].header = Some(BleHeaderRead { header: BleLegacyHeader { slots: 1, wire: [0; BLE_LEGACY_HEADER_LEN] }, frame: None });
            records.update(slot, Some((endpoint(index), DiscoverySource::Ble)), DiscoveryEvidence::Ble, now, &sender);
            while let Ok(event) = events.try_recv() {
                if event.present == Some(true) { visible.insert(event.id); } else { visible.remove(&event.id); }
                assert!(visible.len() <= crate::DISCOVERY_PEER_LIMIT);
            }
        }
        assert_eq!(records.entries.len(), crate::DISCOVERY_PEER_LIMIT);
        assert!(!visible.contains(&endpoint(0).id));
        let slot = records.admit(address(0), now + Duration::from_secs(1), &sender);
        assert!(records.entries[slot].header.is_none(), "evicted failed header must not poison a later address lease");
        records.expire(now + PEER_LIFETIME + Duration::from_secs(2), &sender);
        assert!(records.entries.is_empty());
        while let Ok(event) = events.try_recv() { if event.present == Some(false) { visible.remove(&event.id); } }
        assert!(visible.is_empty());
    }

    #[test]
    fn shared_route_is_withdrawn_only_when_its_last_radio_source_expires() {
        let (sender, mut events) = broadcast::channel(32);
        let now = Instant::now();
        let mut records = DiscoveryRecords::default();
        for (index, elapsed) in [(0, 0), (1, 10)] {
            let at = now + Duration::from_secs(elapsed);
            let slot = records.admit(address(index), at, &sender);
            records.update(slot, Some((endpoint(7), DiscoverySource::Ble)), DiscoveryEvidence::Ble, at, &sender);
        }
        while events.try_recv().is_ok() {}
        records.expire(now + PEER_LIFETIME, &sender);
        assert!(events.try_recv().is_err(), "one expired source cannot withdraw another source's route");
        records.expire(now + PEER_LIFETIME + Duration::from_secs(10), &sender);
        let event = events.try_recv().unwrap();
        assert_eq!((event.id, event.present), (endpoint(7).id, Some(false)));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn unrelated_radio_updates_do_not_renew_a_ble_peer_lease() {
        let (sender, mut events) = broadcast::channel(16);
        let now = Instant::now();
        let mut records = DiscoveryRecords::default();
        let slot = records.admit(address(0), now, &sender);
        records.update(slot, Some((endpoint(1), DiscoverySource::Ble)), DiscoveryEvidence::Ble, now, &sender);
        let slot = records.admit(address(0), now + Duration::from_secs(44), &sender);
        records.update(slot, None, DiscoveryEvidence::Classic, now + Duration::from_secs(44), &sender);
        assert_eq!(events.try_recv().unwrap().present, Some(true));
        records.expire(now + PEER_LIFETIME, &sender);
        assert_eq!(events.try_recv().unwrap().present, Some(false));
        assert_eq!(records.entries.len(), 1);
        assert!(records.entries[0].peer.is_none());
    }

    #[test]
    fn persistent_adapter_pressure_and_user_state_deny_automatic_connect() {
        let mut policy = DevicePolicy { connected: false, paired: false, trusted: false, blocked: false };
        assert!(policy.may_connect(crate::DISCOVERY_PEER_LIMIT - 1));
        for count in [crate::DISCOVERY_PEER_LIMIT, crate::DISCOVERY_PEER_LIMIT + 1, usize::MAX] { assert!(!policy.may_connect(count)); }
        for state in 0..4 {
            policy = DevicePolicy { connected: state == 0, paired: state == 1, trusted: state == 2, blocked: state == 3 };
            assert!(!policy.may_connect(0));
            if state != 0 { assert!(!policy.may_disconnect()); }
        }
    }

    fn property_signal(properties: Properties, adapter: &str) -> zbus::Message {
        zbus::Message::signal(device_path(adapter, address(1)), "org.freedesktop.DBus.Properties", "PropertiesChanged").unwrap()
            .build(&("org.bluez.Device1", properties, Vec::<String>::new())).unwrap()
    }

    #[test]
    fn connection_properties_are_not_radio_observations_and_signal_payloads_are_bounded() {
        let adapter = "/org/bluez/hci0";
        let properties = HashMap::from([("Connected".into(), OwnedValue::from(true)), ("Trusted".into(), OwnedValue::from(true))]);
        assert!(observation(&property_signal(properties, adapter), adapter).unwrap().is_none());
        let properties = HashMap::from([("RSSI".into(), OwnedValue::from(-40_i16))]);
        assert!(matches!(observation(&property_signal(properties, adapter), adapter).unwrap(), Some(Observation::Radio(_, DiscoveryEvidence::Classic, None))));
        assert!(observation(&property_signal(HashMap::new(), "/org/bluez/hci1"), adapter).unwrap().is_none());
        let properties = HashMap::from([("Name".into(), zbus::zvariant::Value::from("x".repeat(MAX_SIGNAL_BODY)).try_into().unwrap())]);
        assert!(observation(&property_signal(properties, adapter), adapter).is_err());
    }
}
