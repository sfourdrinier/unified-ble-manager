//! Radio boundary: one trait, one backend per platform.
//!
//! [`PeripheralRadio`] is backend-neutral: the simulator logic in `sim.rs`
//! only sees service declarations built from [`CharProperty`] /
//! [`CharPermission`] and the [`RadioEvent`] stream. The [`CharProperty`]
//! matches are exhaustive on purpose — adding a variant fails to compile
//! every backend until its mapping is updated.
//!
//! * macOS / Windows: [`CrateRadio`], implemented with `ble-peripheral-rust`
//!   0.2.0 (CoreBluetooth via `objc2-core-bluetooth`, WinRT via `windows`).
//!   The crate hardcodes its BlueZ advertisement object (name and service
//!   UUIDs only, no control over Includes/Appearance/TxPower/duration) and
//!   registers the advertisement before the GATT application.
//! * Linux: [`BluerRadio`](crate::bluer_radio::BluerRadio) in
//!   `src/bluer_radio.rs`, implemented with `bluer` 0.17.4 directly so every
//!   `LEAdvertisement1` property stays under the sim's control and the GATT
//!   application is registered before the advertisement. With
//!   `--linux-advertising mgmt-legacy` the advertisement is instead the
//!   sim's own kernel MGMT instance (`src/mgmt_socket.rs`); GATT stays on
//!   bluetoothd.
//!
//! Deliberately not used anywhere: `btleplug` (central role only), `bluest`
//! (no GATT server on macOS), raw `objc2-core-bluetooth` (would need a
//! hand-written delegate, run-loop pump and D-Bus GATT app for the same
//! surface).
//!
//! The advertised set (180D + FEEE) and the 31-byte payload budget live in
//! [`crate::advertisement`]; both backends advertise exactly that set.

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[cfg(not(target_os = "linux"))]
use ble_peripheral_rust::{
    gatt::{
        characteristic::Characteristic,
        peripheral_event::{
            PeripheralEvent, ReadRequestResponse, RequestResponse, WriteRequestResponse,
        },
        properties::{AttributePermission, CharacteristicProperty},
        service::Service,
    },
    Peripheral, PeripheralImpl,
};

#[cfg(target_os = "linux")]
#[path = "bluer_radio.rs"]
mod bluer_radio;
#[cfg(target_os = "linux")]
#[path = "mgmt_socket.rs"]
mod mgmt_socket;
#[cfg(target_os = "linux")]
pub use self::bluer_radio::BluerRadio as PlatformRadio;
#[cfg(target_os = "linux")]
pub use self::mgmt_socket::require_net_admin;
#[cfg(not(target_os = "linux"))]
pub use self::CrateRadio as PlatformRadio;

use crate::{advertisement, gatt_spec, sim::SimConfig};

/// Transport failure. Carries the backend message; never swallowed.
#[derive(Debug, Clone)]
pub struct RadioError(pub String);

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "radio error: {}", self.0)
    }
}

impl std::error::Error for RadioError {}

/// Answer to a read request: bytes from `offset`, or an ATT-style refusal.
#[derive(Debug)]
pub struct RadioReadAnswer {
    pub value: Vec<u8>,
    pub ok: bool,
}

/// Backend-agnostic peripheral events delivered to the simulator loop.
#[derive(Debug)]
pub enum RadioEvent {
    Powered(bool),
    Subscription {
        service: String,
        characteristic: String,
        subscribed: bool,
    },
    /// A central confirmed an indication: the session stays up (BlueZ
    /// reports confirmations on the notify fd; they are telemetry, not an
    /// unsubscribe). Constructed only by the Linux backend.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    IndicationConfirmed {
        service: String,
        characteristic: String,
    },
    Read {
        service: String,
        characteristic: String,
        offset: u64,
        reply: oneshot::Sender<RadioReadAnswer>,
    },
    Write {
        service: String,
        characteristic: String,
        value: Vec<u8>,
        reply: oneshot::Sender<bool>,
    },
    /// A queued send settled in the pump: delivery accepted by the OS or
    /// failed loudly with the reason. Constructed only by backends with an
    /// asynchronous send pump (Linux); direct backends answer inline.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NotifySettled {
        service: String,
        characteristic: String,
        outcome: SendOutcome,
    },
}

/// What one `notify` call did — explicit, never inferred from what was asked.
/// A signal requests; the result reports what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// No live subscription session: the normal stream-tick case, never an
    /// error — and never reported as a delivery either.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NotSubscribed,
    /// Accepted into the bounded ordered pump; a `NotifySettled` event
    /// follows with the delivery answer.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Queued,
    /// The OS took the value (direct backends answer inline).
    OsAccepted,
    /// The value was dropped, with the reason. Never silent.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Failed(String),
}

/// One entry in the bounded ordered send pump.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct QueuedSend {
    pub service: String,
    pub characteristic: Uuid,
    pub generation: u64,
    pub value: Vec<u8>,
}

/// How many sends the pump holds before `notify` fails loudly instead of
/// growing memory. Stream ticks at ECG rates drain in milliseconds; a full
/// queue means the link is dead, and saying so beats buffering forever.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SEND_QUEUE_CAPACITY: usize = 128;

/// Bounded FIFO of [`QueuedSend`]: push fails with the returned send when
/// full (the caller reports it loudly), pop delivers in arrival order. Pure
/// so it is unit-testable without a radio.
#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SendQueue {
    queue: std::collections::VecDeque<QueuedSend>,
    capacity: usize,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl SendQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// Enqueues a send, or hands it back when the queue is full.
    pub fn push(&mut self, send: QueuedSend) -> Result<(), QueuedSend> {
        if self.queue.len() >= self.capacity {
            return Err(send);
        }
        self.queue.push_back(send);
        Ok(())
    }

    /// Dequeues the oldest send, in arrival order.
    pub fn pop(&mut self) -> Option<QueuedSend> {
        self.queue.pop_front()
    }

    /// Queue depth (test introspection; the pump drains via [`Self::pop`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Generation-aware subscription registry: every subscribe bumps an
/// ever-increasing generation, so a stale session ending late can never
/// remove a newer session's writer. Pure so it is unit-testable.
#[derive(Debug, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SubscriptionLedger {
    generations: std::collections::HashMap<Uuid, u64>,
    live: std::collections::HashMap<Uuid, u64>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl SubscriptionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a new subscription session, returning its generation. The
    /// writer must be installed before readiness is announced.
    pub fn subscribe(&mut self, characteristic: Uuid) -> u64 {
        let generation = self.generations.get(&characteristic).copied().unwrap_or(0);
        self.generations
            .insert(characteristic, generation.wrapping_add(1));
        self.live.insert(characteristic, generation);
        generation
    }

    /// Whether the characteristic is currently subscribed.
    pub fn is_subscribed(&self, characteristic: Uuid) -> bool {
        self.live.contains_key(&characteristic)
    }

    /// The live generation, if subscribed.
    pub fn current(&self, characteristic: Uuid) -> Option<u64> {
        self.live.get(&characteristic).copied()
    }

    /// Whether `generation` is still the live session.
    pub fn is_current(&self, characteristic: Uuid, generation: u64) -> bool {
        self.live.get(&characteristic).copied() == Some(generation)
    }

    /// Ends the session only when `generation` is still live: returns whether
    /// anything was removed, so a stale cleanup removes nothing.
    pub fn unsubscribe(&mut self, characteristic: Uuid, generation: u64) -> bool {
        if self.is_current(characteristic, generation) {
            self.live.remove(&characteristic);
            true
        } else {
            false
        }
    }
}

/// Parses a Bluetooth address (`AA:BB:CC:DD:EE:FF`, case-insensitive) into
/// octets. Fail-closed: anything else is a loud error, never a guess.
pub fn parse_bt_address(text: &str) -> Result<[u8; 6], String> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 6 {
        return Err(format!(
            "{text:?} is not a Bluetooth address (want AA:BB:CC:DD:EE:FF)"
        ));
    }
    let mut octets = [0u8; 6];
    for (index, part) in parts.iter().enumerate() {
        if part.len() != 2 {
            return Err(format!(
                "{text:?} is not a Bluetooth address (want AA:BB:CC:DD:EE:FF)"
            ));
        }
        octets[index] = u8::from_str_radix(part, 16)
            .map_err(|_| format!("{text:?} is not a Bluetooth address (want AA:BB:CC:DD:EE:FF)"))?;
    }
    Ok(octets)
}

/// Canonical Bluetooth address text (`AA:BB:CC:DD:EE:FF`, uppercase).
/// Fail-closed exactly like [`parse_bt_address`].
pub fn normalize_bt_address(text: &str) -> Result<String, String> {
    let octets = parse_bt_address(text)?;
    Ok(format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        octets[0], octets[1], octets[2], octets[3], octets[4], octets[5]
    ))
}

/// Addresses that interacted with this peripheral's GATT application during
/// the current connection: every read, write and notify-subscribe carries
/// the central's address, so drop-link can name exactly the sim's own
/// clients instead of guessing from adapter connections. Entries are
/// removed when the address is observed disconnected — never immortal.
/// Pure so it is unit-testable without a radio. Populated only by the
/// Linux backend (the only one whose GATT requests carry addresses), so
/// other platforms allow the dead code rather than tracking nothing.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
#[derive(Debug, Default)]
pub struct GattClientSet {
    addresses: std::collections::BTreeSet<String>,
}

#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
impl GattClientSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an interaction; false when the address is malformed (the
    /// caller reports it loudly — never a silent skip).
    pub fn insert(&mut self, address: &str) -> bool {
        match normalize_bt_address(address) {
            Ok(canonical) => {
                self.addresses.insert(canonical);
                true
            }
            Err(_) => false,
        }
    }

    /// Removes an address observed disconnected; whether anything was there.
    pub fn remove(&mut self, address: &str) -> bool {
        match normalize_bt_address(address) {
            Ok(canonical) => self.addresses.remove(&canonical),
            Err(_) => false,
        }
    }

    /// Set state (test introspection; the drop path prunes via
    /// [`Self::remove`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Sorted snapshot for reports and drop targets.
    pub fn snapshot(&self) -> Vec<String> {
        self.addresses.iter().cloned().collect()
    }
}

/// Drop targets: tracked simulator clients plus the explicit allowlist,
/// normalized, deduplicated and sorted. Malformed entries are left out —
/// the caller reports them as skips, never disconnects them, never silent.
pub fn drop_targets(clients: &[String], allowlist: &[String]) -> Vec<String> {
    let mut targets = std::collections::BTreeSet::new();
    for address in clients.iter().chain(allowlist.iter()) {
        if let Ok(canonical) = normalize_bt_address(address) {
            targets.insert(canonical);
        }
    }
    targets.into_iter().collect()
}

/// Connected devices that are neither tracked clients nor allowlisted: they
/// stay connected, and the drop report names each one with its reason.
pub fn non_client_skips(connected: &[String], targets: &[String]) -> Vec<String> {
    let targets: std::collections::BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    connected
        .iter()
        .filter(|address| !targets.contains(address.as_str()))
        .cloned()
        .collect()
}

/// Why a connected device was skipped by drop-link — one vocabulary on
/// every backend. Emitted only where disconnects happen (Linux today).
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub const SKIP_NOT_CONNECTED: &str = "not connected";
pub const SKIP_NOT_CLIENT: &str = "not a simulator client";

/// One address drop-link did not disconnect, with the reason. Never silent.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DisconnectSkip {
    pub address: String,
    pub reason: String,
}

/// What one drop-link did, per address: dropped, or skipped with the
/// reason. A signal requests; this result reports what happened.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DisconnectReport {
    pub dropped: Vec<String>,
    pub skipped: Vec<DisconnectSkip>,
}

impl DisconnectReport {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    pub fn add_dropped(&mut self, address: String) {
        self.dropped.push(address);
    }

    pub fn skip(&mut self, address: String, reason: String) {
        self.skipped.push(DisconnectSkip { address, reason });
    }

    /// A failed disconnect lands in the report (callers also log it, as
    /// before) — never a silent drop of the failure. Linux-only today (the
    /// only backend with a disconnect API), like the pump types below.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn skip_failed(&mut self, address: String, message: String) {
        self.skip(address, format!("disconnect failed: {message}"));
    }
}

/// Characteristic properties the simulator needs. Every backend maps these
/// exhaustively onto its own GATT types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharProperty {
    Read,
    Write,
    WriteWithoutResponse,
    Notify,
    Indicate,
}

/// GATT permissions the simulator needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharPermission {
    Readable,
    Writeable,
}

/// Characteristic declaration, mapped onto the backend's GATT types.
#[derive(Debug, Clone)]
pub struct CharSpec {
    pub uuid: Uuid,
    pub properties: Vec<CharProperty>,
    pub permissions: Vec<CharPermission>,
    pub initial_value: Option<Vec<u8>>,
}

/// Service declaration.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub uuid: Uuid,
    pub characteristics: Vec<CharSpec>,
}

/// Radio backend surface the simulator needs. The three defaulted methods let
/// Linux-only capabilities grow without touching the macOS/Windows backend:
/// backends without the OS API inherit a counted no-op.
#[async_trait]
pub trait PeripheralRadio: Send {
    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError>
    where
        Self: Sized;
    async fn is_powered(&mut self) -> Result<bool, RadioError>;
    async fn is_advertising(&mut self) -> Result<bool, RadioError>;
    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError>;
    async fn stop_advertising(&mut self) -> Result<(), RadioError>;
    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError>;
    /// Sends a notification/indication to subscribed centrals, reporting
    /// exactly what happened: `NotSubscribed` is the normal stream-tick case
    /// (never an error, never reported as a delivery), `Queued` means the
    /// bounded ordered pump holds the value (a `NotifySettled` event follows),
    /// `OsAccepted` means the OS took it inline, and `Failed` carries the
    /// reason. A dead session is a `Failed` — never a silent drop.
    async fn notify(
        &mut self,
        characteristic: Uuid,
        value: Vec<u8>,
    ) -> Result<SendOutcome, RadioError>;
    /// Stages manufacturer data for the next advertisement. Default: no-op
    /// (Apple exposes no manufacturer-data peripheral API).
    async fn set_adv_manufacturer_data(
        &mut self,
        _company: u16,
        _data: Vec<u8>,
    ) -> Result<(), RadioError> {
        Ok(())
    }
    /// Whether staged manufacturer data actually goes over the air.
    fn supports_manufacturer_data(&self) -> bool {
        false
    }
    /// Addresses that interacted with this peripheral's GATT application
    /// during the current connection (reads, writes, notify-subscribes all
    /// carry the central's address). Default: none — a backend whose events
    /// carry no central address cannot attribute, and says so by answering
    /// empty rather than guessing.
    fn simulator_clients(&self) -> Vec<String> {
        Vec::new()
    }
    /// Currently-connected adapter devices as canonical address text.
    /// Default: unknown — this backend cannot enumerate, so the drop report
    /// names no strangers rather than guessing. Never a fabricated list.
    async fn connected_devices(&mut self) -> Result<Vec<String>, RadioError> {
        Ok(Vec::new())
    }
    /// Disconnects exactly `targets` — tracked simulator clients plus the
    /// explicit allowlist, unioned by the caller — and reports per address
    /// what happened: dropped, or skipped with the reason (not connected,
    /// disconnect failed). Only addresses in `targets` are ever touched.
    /// Default: no disconnect API on that backend — an empty report, never a
    /// fabricated drop.
    async fn disconnect_centrals(
        &mut self,
        _targets: &[String],
    ) -> Result<DisconnectReport, RadioError> {
        Ok(DisconnectReport::new())
    }
    /// Tears down the live subscription session for one characteristic, so
    /// the next send on it fails loudly instead of going out (adversarial
    /// interrupted-setup). True when a live session was torn down. Default:
    /// unsupported — false, never a fabricated teardown.
    async fn drop_subscription(&mut self, _characteristic: Uuid) -> Result<bool, RadioError> {
        Ok(false)
    }
    /// Which path carries the advertisement and what it holds, for the log.
    /// Default: nothing beyond the platform backend itself.
    fn advertising_detail(&self) -> Option<serde_json::Value> {
        None
    }
    /// True after an advertising failure that restarting cannot fix (the
    /// process exits with `EXIT_ADVERTISING_UNAVAILABLE` instead of 1, so a
    /// supervisor does not loop on it). Default: never.
    fn advertising_unavailable(&self) -> bool {
        false
    }
}

/// Declarations for the whole H10 surface: services, order, characteristic
/// order, properties and counts match the h10-capture fingerprints in
/// `fixtures/h10-fingerprints/` exactly (180D, 180A, 180F, the `6217ff4b`
/// vendor service, PMD, FEEE). Initial values mirror the simulator config so
/// backends that serve reads from the declaration agree with the
/// event-driven answers in the main loop.
pub fn h10_services(config: &SimConfig) -> Result<Vec<ServiceSpec>, RadioError> {
    let read = || vec![CharPermission::Readable];
    let write = || vec![CharPermission::Writeable];
    Ok(vec![
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_SERVICE),
            characteristics: vec![
                CharSpec {
                    uuid: advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_MEASUREMENT),
                    properties: vec![CharProperty::Notify],
                    permissions: Vec::new(),
                    initial_value: None,
                },
                CharSpec {
                    uuid: advertisement::short_uuid(gatt_spec::uuid16::BODY_SENSOR_LOCATION),
                    properties: vec![CharProperty::Read],
                    permissions: read(),
                    initial_value: Some(gatt_spec::encode_body_sensor_location()),
                },
            ],
        },
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::DEVICE_INFORMATION_SERVICE),
            // The strap lists hardware revision (0x2A27) before firmware
            // revision (0x2A26) — pinned by the fingerprints.
            characteristics: vec![
                (gatt_spec::uuid16::MANUFACTURER_NAME, "Polar Electro Oy"),
                (gatt_spec::uuid16::MODEL_NUMBER, "H10"),
                (gatt_spec::uuid16::SERIAL_NUMBER, "SIM000001"),
                (gatt_spec::uuid16::HARDWARE_REVISION, "9"),
                (gatt_spec::uuid16::FIRMWARE_REVISION, "3.2.1"),
                (gatt_spec::uuid16::SOFTWARE_REVISION, "3.2.1"),
            ]
            .into_iter()
            .map(|(short, text): (u16, &str)| CharSpec {
                uuid: advertisement::short_uuid(short),
                properties: vec![CharProperty::Read],
                permissions: read(),
                // DIS strings carry the strap's trailing NUL, like the
                // event-driven answers in `SimState::static_read`.
                initial_value: Some(match short {
                    x if x == gatt_spec::uuid16::MANUFACTURER_NAME => {
                        gatt_spec::encode_dis_string(&config.manufacturer)
                    }
                    x if x == gatt_spec::uuid16::MODEL_NUMBER => {
                        gatt_spec::encode_dis_string(&config.model)
                    }
                    x if x == gatt_spec::uuid16::SERIAL_NUMBER => {
                        gatt_spec::encode_dis_string(&config.serial)
                    }
                    x if x == gatt_spec::uuid16::FIRMWARE_REVISION => {
                        gatt_spec::encode_dis_string(&config.firmware)
                    }
                    x if x == gatt_spec::uuid16::HARDWARE_REVISION => {
                        gatt_spec::encode_dis_string(&config.hardware)
                    }
                    x if x == gatt_spec::uuid16::SOFTWARE_REVISION => {
                        gatt_spec::encode_dis_string(&config.software)
                    }
                    _ => gatt_spec::encode_dis_string(text),
                }),
            })
            .chain(std::iter::once(CharSpec {
                uuid: advertisement::short_uuid(gatt_spec::uuid16::SYSTEM_ID),
                properties: vec![CharProperty::Read],
                permissions: read(),
                initial_value: Some(gatt_spec::encode_system_id(
                    config.system_id_manufacturer,
                    config.system_id_oui,
                )),
            }))
            .collect(),
        },
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::BATTERY_SERVICE),
            characteristics: vec![CharSpec {
                uuid: advertisement::short_uuid(gatt_spec::uuid16::BATTERY_LEVEL),
                properties: vec![CharProperty::Read, CharProperty::Notify],
                permissions: read(),
                initial_value: Some(gatt_spec::encode_battery_level(config.battery_percent)),
            }],
        },
        ServiceSpec {
            uuid: parse_uuid(gatt_spec::vendor::SERVICE)?,
            characteristics: vec![
                CharSpec {
                    uuid: parse_uuid(gatt_spec::vendor::READ)?,
                    properties: vec![CharProperty::Read],
                    permissions: read(),
                    // The value is UNCONFIRMED (no capture reads it): an
                    // explicit empty placeholder, never a guessed payload.
                    initial_value: Some(Vec::new()),
                },
                CharSpec {
                    uuid: parse_uuid(gatt_spec::vendor::WRITE_INDICATE)?,
                    properties: vec![CharProperty::WriteWithoutResponse, CharProperty::Indicate],
                    permissions: write(),
                    initial_value: None,
                },
            ],
        },
        ServiceSpec {
            uuid: parse_uuid(gatt_spec::pmd::SERVICE)?,
            characteristics: vec![
                CharSpec {
                    uuid: parse_uuid(gatt_spec::pmd::CONTROL_POINT)?,
                    properties: vec![
                        CharProperty::Read,
                        CharProperty::Write,
                        CharProperty::Indicate,
                    ],
                    permissions: {
                        let mut permissions = read();
                        permissions.extend(write());
                        permissions
                    },
                    initial_value: Some(gatt_spec::encode_pmd_features()),
                },
                CharSpec {
                    uuid: parse_uuid(gatt_spec::pmd::DATA)?,
                    properties: vec![CharProperty::Notify],
                    permissions: Vec::new(),
                    initial_value: None,
                },
            ],
        },
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::POLAR_ADV_SERVICE),
            characteristics: vec![
                CharSpec {
                    uuid: parse_uuid(gatt_spec::feee::CHAR_51)?,
                    properties: vec![
                        CharProperty::Write,
                        CharProperty::WriteWithoutResponse,
                        CharProperty::Notify,
                    ],
                    permissions: write(),
                    initial_value: None,
                },
                CharSpec {
                    uuid: parse_uuid(gatt_spec::feee::CHAR_52)?,
                    properties: vec![CharProperty::Notify],
                    permissions: Vec::new(),
                    initial_value: None,
                },
                CharSpec {
                    uuid: parse_uuid(gatt_spec::feee::CHAR_53)?,
                    properties: vec![CharProperty::Write, CharProperty::WriteWithoutResponse],
                    permissions: write(),
                    initial_value: None,
                },
            ],
        },
    ])
}

fn parse_uuid(text: &str) -> Result<Uuid, RadioError> {
    Uuid::parse_str(text).map_err(|error| RadioError(format!("invalid UUID {text}: {error}")))
}

/// Service UUIDs carried in the advertisement: Heart Rate + Polar (FEEE).
/// Pinned to [`advertisement::ADVERTISED_SERVICES`]; the 128-bit PMD service
/// UUID is served over GATT, never advertised.
pub fn adv_service_uuids() -> Vec<Uuid> {
    advertisement::ADVERTISED_SERVICES
        .iter()
        .map(|short| advertisement::short_uuid(*short))
        .collect()
}

/// 16-bit short UUID of `uuid`, for routing read/write events to `SimState`.
pub fn short_of(uuid: &Uuid) -> Option<u16> {
    let fields = uuid.as_fields();
    if fields.1 == 0 && fields.2 == 0x1000 && fields.3 == b"\x80\x00\x00\x80\x5F\x9B\x34\xFB" {
        Some((fields.0 & 0xFFFF) as u16)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn map_property(property: &CharProperty) -> CharacteristicProperty {
    match property {
        CharProperty::Read => CharacteristicProperty::Read,
        CharProperty::Write => CharacteristicProperty::Write,
        CharProperty::WriteWithoutResponse => CharacteristicProperty::WriteWithoutResponse,
        CharProperty::Notify => CharacteristicProperty::Notify,
        CharProperty::Indicate => CharacteristicProperty::Indicate,
    }
}

#[cfg(not(target_os = "linux"))]
fn map_permission(permission: &CharPermission) -> AttributePermission {
    match permission {
        CharPermission::Readable => AttributePermission::Readable,
        CharPermission::Writeable => AttributePermission::Writeable,
    }
}

/// [`PeripheralRadio`] implemented with `ble-peripheral-rust` (macOS/Windows).
#[cfg(not(target_os = "linux"))]
pub struct CrateRadio {
    inner: Peripheral,
}

#[cfg(not(target_os = "linux"))]
#[async_trait]
impl PeripheralRadio for CrateRadio {
    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
        let (backend_tx, mut backend_rx) = mpsc::channel::<PeripheralEvent>(256);
        let inner = Peripheral::new(backend_tx)
            .await
            .map_err(|error| RadioError(error.to_string()))?;
        tokio::spawn(async move {
            while let Some(event) = backend_rx.recv().await {
                if translate(event, &events).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self { inner })
    }

    async fn is_powered(&mut self) -> Result<bool, RadioError> {
        self.inner
            .is_powered()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn is_advertising(&mut self) -> Result<bool, RadioError> {
        self.inner
            .is_advertising()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError> {
        self.inner
            .start_advertising(name, uuids)
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn stop_advertising(&mut self) -> Result<(), RadioError> {
        self.inner
            .stop_advertising()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError> {
        let backend = Service {
            uuid: service.uuid,
            primary: true,
            characteristics: service
                .characteristics
                .iter()
                .map(|characteristic| Characteristic {
                    uuid: characteristic.uuid,
                    properties: characteristic.properties.iter().map(map_property).collect(),
                    permissions: characteristic
                        .permissions
                        .iter()
                        .map(map_permission)
                        .collect(),
                    value: characteristic.initial_value.clone(),
                    descriptors: Vec::new(),
                })
                .collect(),
        };
        self.inner
            .add_service(&backend)
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn notify(
        &mut self,
        characteristic: Uuid,
        value: Vec<u8>,
    ) -> Result<SendOutcome, RadioError> {
        // CoreBluetooth stages the value in the backend even with no live
        // subscriber, so delivery is always "accepted" here.
        self.inner
            .update_characteristic(characteristic, value)
            .await
            .map(|()| SendOutcome::OsAccepted)
            .map_err(|error| RadioError(error.to_string()))
    }
}

#[cfg(not(target_os = "linux"))]
async fn translate(event: PeripheralEvent, events: &mpsc::Sender<RadioEvent>) -> Result<(), ()> {
    match event {
        PeripheralEvent::StateUpdate { is_powered } => {
            events
                .send(RadioEvent::Powered(is_powered))
                .await
                .map_err(|_| ())?;
        }
        PeripheralEvent::CharacteristicSubscriptionUpdate {
            request,
            subscribed,
        } => {
            events
                .send(RadioEvent::Subscription {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    subscribed,
                })
                .await
                .map_err(|_| ())?;
        }
        PeripheralEvent::ReadRequest {
            request,
            offset,
            responder,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel::<RadioReadAnswer>();
            events
                .send(RadioEvent::Read {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    offset,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ())?;
            let answer = reply_rx.await.unwrap_or(RadioReadAnswer {
                value: Vec::new(),
                ok: false,
            });
            let _ = responder.send(ReadRequestResponse {
                value: answer.value,
                response: if answer.ok {
                    RequestResponse::Success
                } else {
                    RequestResponse::RequestNotSupported
                },
            });
        }
        PeripheralEvent::WriteRequest {
            request,
            value,
            offset: _,
            responder,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel::<bool>();
            events
                .send(RadioEvent::Write {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    value,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ())?;
            let accepted = reply_rx.await.unwrap_or(false);
            let _ = responder.send(WriteRequestResponse {
                response: if accepted {
                    RequestResponse::Success
                } else {
                    RequestResponse::RequestNotSupported
                },
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopRadio;

    #[async_trait]
    impl PeripheralRadio for NoopRadio {
        async fn open(_events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
            Ok(Self)
        }
        async fn is_powered(&mut self) -> Result<bool, RadioError> {
            Ok(true)
        }
        async fn is_advertising(&mut self) -> Result<bool, RadioError> {
            Ok(false)
        }
        async fn start_advertising(
            &mut self,
            _name: &str,
            _uuids: &[Uuid],
        ) -> Result<(), RadioError> {
            Ok(())
        }
        async fn stop_advertising(&mut self) -> Result<(), RadioError> {
            Ok(())
        }
        async fn add_service(&mut self, _service: &ServiceSpec) -> Result<(), RadioError> {
            Ok(())
        }
        async fn notify(
            &mut self,
            _characteristic: Uuid,
            _value: Vec<u8>,
        ) -> Result<SendOutcome, RadioError> {
            Ok(SendOutcome::OsAccepted)
        }
    }

    #[test]
    fn h10_surface_matches_the_real_fingerprint_layout() {
        use crate::advertisement::short_uuid;
        use crate::gatt_spec::{feee, pmd, uuid16, vendor};
        let services = h10_services(&SimConfig::default()).expect("H10 surface builds");
        let uuids: Vec<String> = services
            .iter()
            .map(|service| service.uuid.to_string().to_lowercase())
            .collect();
        assert_eq!(
            uuids,
            vec![
                short_uuid(uuid16::HEART_RATE_SERVICE).to_string(),
                short_uuid(uuid16::DEVICE_INFORMATION_SERVICE).to_string(),
                short_uuid(uuid16::BATTERY_SERVICE).to_string(),
                vendor::SERVICE.to_lowercase(),
                pmd::SERVICE.to_lowercase(),
                short_uuid(uuid16::POLAR_ADV_SERVICE).to_string(),
            ],
            "service order and set match fixtures/h10-fingerprints (180D, 180A, 180F, 6217ff4b, PMD, FEEE)",
        );
        let counts: Vec<usize> = services
            .iter()
            .map(|service| service.characteristics.len())
            .collect();
        assert_eq!(counts, vec![2, 7, 1, 2, 2, 3]);
        // Device Information lists hardware (0x2A27) before firmware (0x2A26).
        let dis_shorts: Vec<u16> = services[1]
            .characteristics
            .iter()
            .map(|characteristic| super::short_of(&characteristic.uuid).unwrap())
            .collect();
        assert_eq!(
            dis_shorts,
            vec![
                uuid16::MANUFACTURER_NAME,
                uuid16::MODEL_NUMBER,
                uuid16::SERIAL_NUMBER,
                uuid16::HARDWARE_REVISION,
                uuid16::FIRMWARE_REVISION,
                uuid16::SOFTWARE_REVISION,
                uuid16::SYSTEM_ID,
            ]
        );
        // The vendor service is read plus write-without-response/indicate.
        assert_eq!(
            services[3].characteristics[1].properties,
            vec![CharProperty::WriteWithoutResponse, CharProperty::Indicate]
        );
        // The FEEE characteristics follow the PMD base UUID with the
        // fingerprint's write/notify mix.
        let feee_uuids: Vec<String> = services[5]
            .characteristics
            .iter()
            .map(|characteristic| characteristic.uuid.to_string().to_lowercase())
            .collect();
        assert_eq!(
            feee_uuids,
            vec![
                feee::CHAR_51.to_lowercase(),
                feee::CHAR_52.to_lowercase(),
                feee::CHAR_53.to_lowercase(),
            ]
        );
        assert!(services[5].characteristics[0]
            .properties
            .contains(&CharProperty::WriteWithoutResponse));
        assert!(services[5].characteristics[0]
            .properties
            .contains(&CharProperty::Notify));
        assert_eq!(
            services[5].characteristics[1].properties,
            vec![CharProperty::Notify]
        );
        // Seven notify/indicate characteristics carry the seven CCCDs.
        let cccd = services
            .iter()
            .flat_map(|service| &service.characteristics)
            .filter(|characteristic| {
                characteristic.properties.contains(&CharProperty::Notify)
                    || characteristic.properties.contains(&CharProperty::Indicate)
            })
            .count();
        assert_eq!(cccd, 7);
    }

    #[tokio::test]
    async fn defaulted_backend_methods_are_counted_noops() {
        let mut radio = NoopRadio;
        assert!(radio
            .set_adv_manufacturer_data(0x006B, vec![1])
            .await
            .is_ok());
        assert!(
            !radio.supports_manufacturer_data(),
            "a backend without the OS API must say so"
        );
        let report = radio.disconnect_centrals(&[]).await.unwrap();
        assert!(
            report.dropped.is_empty() && report.skipped.is_empty(),
            "a backend without the OS API reports nothing dropped, never a fabricated drop"
        );
        assert!(
            radio.simulator_clients().is_empty(),
            "no attribution without central addresses"
        );
        assert_eq!(
            radio.connected_devices().await.unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(radio.advertising_detail(), None);
        assert!(!radio.advertising_unavailable());
    }

    #[test]
    fn bt_addresses_normalize_to_uppercase_canonical() {
        assert_eq!(
            super::normalize_bt_address("aa:bb:cc:dd:ee:ff"),
            Ok("AA:BB:CC:DD:EE:FF".to_string())
        );
        assert_eq!(
            super::normalize_bt_address("AA:BB:CC:DD:EE:FF"),
            Ok("AA:BB:CC:DD:EE:FF".to_string())
        );
        assert!(super::normalize_bt_address("not-an-address").is_err());
        assert!(super::normalize_bt_address("AA:BB:CC:DD:EE").is_err());
    }

    #[test]
    fn gatt_client_set_dedupes_and_prunes() {
        use super::GattClientSet;
        let mut set = GattClientSet::new();
        assert!(set.is_empty());
        assert!(set.insert("aa:bb:cc:dd:ee:ff"));
        assert!(set.insert("AA:BB:CC:DD:EE:FF"));
        assert!(!set.insert("bogus"));
        assert_eq!(set.snapshot(), vec!["AA:BB:CC:DD:EE:FF".to_string()]);
        assert!(set.remove("AA:bb:CC:dd:EE:ff"));
        assert!(set.is_empty());
        assert!(!set.remove("11:22:33:44:55:66"));
    }

    #[test]
    fn drop_targets_union_clients_and_allowlist_sorted() {
        use super::drop_targets;
        let targets = drop_targets(
            &["BB:BB:BB:BB:BB:BB".to_string()],
            &[
                "aa:aa:aa:aa:aa:aa".to_string(),
                "BB:BB:BB:BB:BB:BB".to_string(),
                "bogus".to_string(),
            ],
        );
        assert_eq!(
            targets,
            vec![
                "AA:AA:AA:AA:AA:AA".to_string(),
                "BB:BB:BB:BB:BB:BB".to_string()
            ]
        );
    }

    #[test]
    fn non_client_skips_name_connected_strangers_only() {
        use super::non_client_skips;
        let skips = non_client_skips(
            &[
                "AA:AA:AA:AA:AA:AA".to_string(),
                "CC:CC:CC:CC:CC:CC".to_string(),
            ],
            &["AA:AA:AA:AA:AA:AA".to_string()],
        );
        assert_eq!(skips, vec!["CC:CC:CC:CC:CC:CC".to_string()]);
    }

    #[test]
    fn disconnect_report_carries_dropped_and_skip_reasons() {
        use super::{DisconnectReport, SKIP_NOT_CLIENT, SKIP_NOT_CONNECTED};
        let mut report = DisconnectReport::new();
        assert!(report.dropped.is_empty() && report.skipped.is_empty());
        report.add_dropped("AA:AA:AA:AA:AA:AA".to_string());
        report.skip(
            "BB:BB:BB:BB:BB:BB".to_string(),
            SKIP_NOT_CONNECTED.to_string(),
        );
        report.skip("CC:CC:CC:CC:CC:CC".to_string(), SKIP_NOT_CLIENT.to_string());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["dropped"], serde_json::json!(["AA:AA:AA:AA:AA:AA"]));
        assert_eq!(
            json["skipped"][0]["address"],
            serde_json::json!("BB:BB:BB:BB:BB:BB")
        );
        assert_eq!(
            json["skipped"][0]["reason"],
            serde_json::json!(SKIP_NOT_CONNECTED)
        );
    }

    #[test]
    fn bt_addresses_parse_strictly() {
        assert_eq!(
            super::parse_bt_address("AA:BB:CC:DD:EE:FF"),
            Ok([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert_eq!(
            super::parse_bt_address("aa:bb:cc:dd:ee:ff"),
            Ok([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert!(super::parse_bt_address("not-an-address").is_err());
        assert!(super::parse_bt_address("AA:BB:CC:DD:EE").is_err());
        assert!(super::parse_bt_address("AA:BB:CC:DD:EE:FG").is_err());
        assert!(super::parse_bt_address("AA:BB:CC:DD:EE:FFF").is_err());
        assert!(super::parse_bt_address("").is_err());
    }

    #[test]
    fn ledger_replaces_stale_generations() {
        use super::SubscriptionLedger;
        let uuid = Uuid::nil();
        let mut ledger = SubscriptionLedger::new();
        assert!(!ledger.is_subscribed(uuid));
        let first = ledger.subscribe(uuid);
        assert!(ledger.is_subscribed(uuid));
        assert_eq!(ledger.current(uuid), Some(first));
        // A resubscribe supersedes the old session: the old generation is
        // stale, and its late cleanup must remove nothing.
        let second = ledger.subscribe(uuid);
        assert_ne!(first, second, "generations never repeat");
        assert!(!ledger.is_current(uuid, first));
        assert!(ledger.is_current(uuid, second));
        assert!(
            !ledger.unsubscribe(uuid, first),
            "stale cleanup removes nothing"
        );
        assert!(
            ledger.is_subscribed(uuid),
            "live session survives stale cleanup"
        );
        assert!(ledger.unsubscribe(uuid, second));
        assert!(!ledger.is_subscribed(uuid));
        // Generations keep increasing even across unsubscribe gaps.
        let third = ledger.subscribe(uuid);
        assert_ne!(second, third);
    }

    #[test]
    fn send_queue_is_a_bounded_fifo() {
        use super::{QueuedSend, SendQueue};
        let send = |n: u8| QueuedSend {
            service: "svc".to_string(),
            characteristic: Uuid::nil(),
            generation: u64::from(n),
            value: vec![n],
        };
        let mut queue = SendQueue::new(2);
        assert!(queue.is_empty());
        assert!(queue.push(send(1)).is_ok());
        assert!(queue.push(send(2)).is_ok());
        assert_eq!(queue.len(), 2);
        let dropped = queue
            .push(send(3))
            .expect_err("full queue hands the send back");
        assert_eq!(dropped.generation, 3, "nothing is silently dropped");
        assert_eq!(queue.pop().expect("fifo").generation, 1);
        assert_eq!(queue.pop().expect("fifo").generation, 2);
        assert!(queue.pop().is_none());
    }
}
