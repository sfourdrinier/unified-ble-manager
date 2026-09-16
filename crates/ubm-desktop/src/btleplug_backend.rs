//! Production btleplug backend for the desktop radio boundary.
//!
//! [`BtleplugRadio`] implements [`RadioBoundary`](crate::boundary::RadioBoundary)
//! over `btleplug::platform` (CoreBluetooth on macOS, WinRT on Windows,
//! BlueZ on Linux). It owns no scheduler and no operation map: it translates
//! calls and events, and the real [`ubm_core::central::Central`] keeps every
//! decision. Notification streams are fanned into one event channel by
//! per-subscription forwarder tasks spawned on the shared desktop executor
//! handle given to [`BtleplugRadio::open`] (never one runtime per manager).
//!
//! Radio facts btleplug cannot provide are reported honestly, never
//! synthesized: RSSI/addresses stay `None` when the OS withholds them, and
//! indications are indistinguishable from notifications on this stream (see
//! `PARITY_GAPS.md`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};

use btleplug::api::{
    Central as _, CentralEvent, CharPropFlags, Characteristic, Descriptor, Manager as _,
    Peripheral as _, ScanFilter, Service, ValueNotification,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};

use crate::boundary::{
    CharacteristicSnapshot, DescriptorSnapshot, InstanceKey, ManufacturerData, PeerSnapshot,
    PropertyFlags, RadioBoundary, RadioEvent, ScanFilterSpec, ServiceData, ServiceSnapshot,
};
use crate::errors::DesktopError;
use ubm_core::central::{
    GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
    GATT_PROP_WRITE_NO_RESPONSE,
};

type EventStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>;
type NotificationStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = ValueNotification> + Send>>;

/// One live notification forwarder: the task fanning one characteristic
/// instance into the shared event channel, plus the instance address for
/// best-effort OS unsubscribe at teardown.
struct ForwarderEntry {
    task: tokio::task::JoinHandle<()>,
    peer_id: String,
    service_uuid: String,
    service_occurrence: u64,
    characteristic_uuid: String,
    characteristic_occurrence: u64,
}

impl ForwarderEntry {
    /// Instance scope this forwarder owns, for ambiguity checks (F09).
    fn scope(&self) -> InstanceKey {
        (
            self.peer_id.clone(),
            self.service_uuid.clone(),
            self.service_occurrence,
            self.characteristic_uuid.clone(),
            self.characteristic_occurrence,
        )
    }
}

/// Bounded notification ingress (F07): 256 items / 256 KiB bytes at the first
/// owned handoff. Forwarders `try_send` (never block the runtime worker);
/// overload drops are counted, never silent, and adapter control (connect,
/// disconnect, service-change) travels a separate stream so it never starves.
const NOTIFICATION_CAP: usize = 256;
const NOTIFICATION_BYTES: u64 = 262_144;

/// Production radio backend over one btleplug adapter.
pub struct BtleplugRadio {
    adapter: Adapter,
    adapter_label: String,
    spawn: tokio::runtime::Handle,
    events: Mutex<Option<EventStream>>,
    notifications: mpsc::Sender<RadioEvent>,
    notification_rx: Mutex<mpsc::Receiver<RadioEvent>>,
    ingress_bytes: Arc<AtomicU64>,
    ingress_dropped: Arc<AtomicU64>,
    forwarders: StdMutex<HashMap<String, ForwarderEntry>>,
    /// Cleanup debt (F13): scopes whose native CCCD may be live without
    /// an installed forwarder — a failed setup rollback or a failed
    /// consumer-less teardown. Retry and dispose keep attempting the
    /// native release until it succeeds; the ambiguity check (F09)
    /// treats debt as live because the CCCD may still emit.
    cleanup_debt: StdMutex<HashSet<InstanceKey>>,
}

impl BtleplugRadio {
    /// Open the default btleplug adapter. When `adapter_id` is `Some`, it
    /// must equal `Adapter::adapter_info()` exactly; otherwise
    /// `adapter.unavailable` fails the open without touching another
    /// adapter. `spawn` is the shared desktop executor handle (see
    /// [`crate::executor::desktop_runtime`]).
    pub async fn open(
        spawn: tokio::runtime::Handle,
        adapter_id: Option<String>,
    ) -> Result<Self, DesktopError> {
        let manager = Manager::new().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.open").with_detail(error.to_string())
        })?;
        let adapters = manager.adapters().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.enumerate").with_detail(error.to_string())
        })?;
        let mut chosen: Option<(Adapter, String)> = None;
        for adapter in adapters {
            // Never synthesize an adapter identity: when the OS withholds
            // the info, the adapter is skipped, not labelled "unknown".
            let info = match adapter.adapter_info().await {
                Ok(info) => info,
                Err(_) => continue,
            };
            let wanted = adapter_id.as_deref().unwrap_or(info.as_str());
            if info == wanted {
                chosen = Some((adapter, info));
                break;
            }
        }
        let Some((adapter, adapter_label)) = chosen else {
            return Err(DesktopError::adapter_unavailable("adapter.select")
                .with_detail("no matching btleplug adapter"));
        };
        let events = adapter.events().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.events").with_detail(error.to_string())
        })?;
        let (notifications, notification_rx) = mpsc::channel(NOTIFICATION_CAP);
        Ok(Self {
            adapter,
            adapter_label,
            spawn,
            events: Mutex::new(Some(events)),
            notifications,
            notification_rx: Mutex::new(notification_rx),
            ingress_bytes: Arc::new(AtomicU64::new(0)),
            ingress_dropped: Arc::new(AtomicU64::new(0)),
            forwarders: StdMutex::new(HashMap::new()),
            cleanup_debt: StdMutex::new(HashSet::new()),
        })
    }

    /// Bytes currently queued in the bounded notification ingress (F07).
    #[must_use]
    pub fn ingress_queued_bytes(&self) -> u64 {
        self.ingress_bytes.load(Ordering::Relaxed)
    }

    /// Notifications dropped by explicit ingress overload (F07).
    #[must_use]
    pub fn ingress_dropped(&self) -> u64 {
        self.ingress_dropped.load(Ordering::Relaxed)
    }

    async fn peripheral_by_id(&self, peer_id: &str) -> Result<Peripheral, DesktopError> {
        for peripheral in self.adapter.peripherals().await.map_err(map_radio(
            "peer.lookup",
            ubm_core::contracts::BleErrorCode::PeerNotFound,
            ubm_core::contracts::BleErrorDomain::Connection,
        ))? {
            if peripheral.id().to_string() == peer_id {
                return Ok(peripheral);
            }
        }
        Err(DesktopError::new(
            ubm_core::contracts::BleErrorCode::PeerNotFound,
            ubm_core::contracts::BleErrorDomain::Connection,
            "peer.lookup",
        )
        .with_detail(peer_id.to_owned()))
    }

    async fn snapshot(&self, peripheral: &Peripheral) -> PeerSnapshot {
        let properties = peripheral.properties().await.ok().flatten();
        let (service_uuids, rssi, local_name, manufacturer_data, service_data, tx_power_level) =
            properties
                .map(|facts| {
                    (
                        facts
                            .services
                            .into_iter()
                            .map(|uuid| uuid.to_string())
                            .collect(),
                        facts.rssi,
                        facts.local_name,
                        sorted_manufacturer_data(&facts.manufacturer_data),
                        sorted_service_data(&facts.service_data),
                        facts.tx_power_level,
                    )
                })
                .unwrap_or_default();
        // btleplug exposes the address type opaquely per platform; report the
        // string, never guess public-vs-random (see PARITY_GAPS.md).
        let address = {
            let raw = peripheral.address().to_string();
            if raw.is_empty() || raw == "00:00:00:00:00:00" {
                None
            } else {
                Some(raw)
            }
        };
        PeerSnapshot {
            id: peripheral.id().to_string(),
            address,
            service_uuids,
            rssi,
            local_name,
            manufacturer_data,
            service_data,
            tx_power_level,
        }
    }

    async fn advertisement_for(&self, id: &PeripheralId) -> Option<RadioEvent> {
        let wanted = id.to_string();
        for peripheral in self.adapter.peripherals().await.ok()? {
            if peripheral.id().to_string() == wanted {
                return Some(RadioEvent::Advertisement(self.snapshot(&peripheral).await));
            }
        }
        None
    }

    /// Occurrence-aware instance lookup over the cached GATT database.
    /// Services and characteristics iterate in btleplug's canonical order
    /// (UUID-first), the same order [`RadioBoundary::discover`] numbers
    /// occurrences in, so occurrence `n` selects the n-th same-UUID entry
    /// in both paths. UUID alone never identifies an instance.
    fn find_characteristic(
        peripheral: &Peripheral,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Option<Characteristic> {
        let services = peripheral.services();
        select_service(&services, service_uuid, service_occurrence).and_then(|service| {
            select_characteristic(service, characteristic_uuid, characteristic_occurrence).cloned()
        })
    }

    async fn recv_notification(&self) -> Option<RadioEvent> {
        let mut queue = self.notification_rx.lock().await;
        let event = queue.recv().await?;
        if let RadioEvent::Notification { ref value, .. } = event {
            self.ingress_bytes
                .fetch_sub(value.len() as u64, Ordering::Relaxed);
        }
        Some(event)
    }

    /// Abort every live forwarder for one peer (F10): after a disconnect
    /// or service change, the peer's old GATT handles are dead, so its
    /// forwarders stop emitting rather than draining stale values into
    /// the shared channel. Values already queued still carry the dead
    /// install-time epoch and fail the central's routing check. Task abort
    /// only: the link is gone (or the handles are), so no OS unsubscribe
    /// is attempted here — resubscribe reinstalls through the normal path.
    fn abort_peer_forwarders(&self, peer_id: &str) {
        let mut table = self.forwarders.lock().expect("forwarder table");
        let stale: Vec<String> = table
            .iter()
            .filter(|(_, entry)| entry.peer_id == peer_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &stale {
            if let Some(entry) = table.remove(key) {
                entry.task.abort();
            }
        }
        // The dead handles settle any cleanup debt for this peer too: a
        // lost link or a replaced GATT database releases the native
        // CCCDs, so no retry/dispose unsubscribe is owed for them.
        self.cleanup_debt
            .lock()
            .expect("cleanup debt")
            .retain(|scope| scope.0 != peer_id);
    }

    fn find_descriptor(
        peripheral: &Peripheral,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Option<Descriptor> {
        Self::find_characteristic(
            peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .and_then(|characteristic| {
            select_descriptor(&characteristic, descriptor_uuid, descriptor_occurrence).cloned()
        })
    }
}

/// Per-instance forwarder key: duplicate UUIDs never share a forwarder.
fn forwarder_key(
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
) -> String {
    format!(
        "{peer_id}#{service_uuid}:{service_occurrence}#{characteristic_uuid}:{characteristic_occurrence}"
    )
}

/// Identity one notification forwarder routes by (F09): the owning
/// service UUID plus the characteristic UUID — the full identity
/// btleplug 0.12 exposes on [`ValueNotification`]. Occurrence levels
/// are NOT on the wire: same-scope duplicate instances are
/// indistinguishable here and must be rejected at enable time (see
/// [`route_is_ambiguous`]), never fanned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotificationRoute {
    service_uuid: uuid::Uuid,
    characteristic_uuid: uuid::Uuid,
}

impl NotificationRoute {
    /// True when this notification belongs to the subscribed instance:
    /// both the service and the characteristic identity must match, or
    /// bytes for one service would misroute into another service's
    /// same-UUID subscription.
    fn matches(&self, note: &ValueNotification) -> bool {
        note.service_uuid == self.service_uuid && note.uuid == self.characteristic_uuid
    }
}

/// True when enabling one more subscription would create ambiguous
/// routing (F09): `live_scopes` already holds the same (peer, service,
/// characteristic) scope under a different instance. The native stream
/// carries no occurrence/handle identity, so the second enablement must
/// fail explicitly instead of receiving misattributed bytes. Re-enabling
/// the exact same instance is not ambiguous (idempotent replace).
fn route_is_ambiguous(
    live_scopes: &[InstanceKey],
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
) -> bool {
    live_scopes.iter().any(|scope| {
        scope.0 == peer_id
            && scope.1 == service_uuid
            && scope.3 == characteristic_uuid
            && (scope.2 != service_occurrence || scope.4 != characteristic_occurrence)
    })
}

/// Scopes with a possibly-live native CCCD (F13): installed forwarders
/// plus cleanup-debt entries (native enablements without a consumer
/// after a failed setup or teardown). The ambiguity check (F09) must see
/// both — a debt CCCD can still emit unattributable bytes.
fn live_scopes(
    forwarders: &HashMap<String, ForwarderEntry>,
    debt: &HashSet<InstanceKey>,
) -> Vec<InstanceKey> {
    let mut scopes: Vec<InstanceKey> = forwarders.values().map(ForwarderEntry::scope).collect();
    scopes.extend(debt.iter().cloned());
    scopes
}

/// Fold one native-unsubscribe outcome into the forwarder table (F13).
/// The forwarder is removed only when the native disable succeeds, so a
/// still-enabled CCCD keeps its consumer and values keep flowing until a
/// retry disables it (the central's L7 path relies on this). When no
/// consumer exists to preserve, a failed disable parks the scope as
/// cleanup debt for retry/dispose instead of vanishing.
fn apply_unsubscribe_outcome(
    forwarders: &mut HashMap<String, ForwarderEntry>,
    debt: &mut HashSet<InstanceKey>,
    key: &str,
    scope: &InstanceKey,
    unsubscribed: bool,
) {
    if unsubscribed {
        if let Some(entry) = forwarders.remove(key) {
            entry.task.abort();
        }
        debt.remove(scope);
    } else if !forwarders.contains_key(key) {
        // No consumer to preserve: retain the unresolved native
        // resource as cleanup debt for retry/dispose.
        debt.insert(scope.clone());
    }
    // Otherwise the forwarder stays installed: the CCCD is still live,
    // so values must keep flowing until a retry disables it.
}

/// Fold one setup-rollback outcome into the cleanup debt (F13): after a
/// successful native subscribe whose stream install failed, a failed
/// compensating unsubscribe leaves a possibly-live CCCD with no
/// consumer — parked as debt for retry/dispose. A successful rollback
/// (or a later full enable) clears it.
fn apply_enable_stream_failure(
    debt: &mut HashSet<InstanceKey>,
    scope: &InstanceKey,
    rollback_ok: bool,
) {
    if rollback_ok {
        debt.remove(scope);
    } else {
        debt.insert(scope.clone());
    }
}

/// Select the `occurrence`-th service with `uuid` in canonical
/// (UUID-first) order. `None` when the instance does not exist.
fn select_service<'s>(
    services: &'s BTreeSet<Service>,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Service> {
    let want = usize::try_from(occurrence).ok()?;
    services
        .iter()
        .filter(|service| service.uuid.to_string() == uuid)
        .nth(want)
}

/// Select the `occurrence`-th characteristic with `uuid` under one
/// service instance, in canonical order.
fn select_characteristic<'s>(
    service: &'s Service,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Characteristic> {
    let want = usize::try_from(occurrence).ok()?;
    service
        .characteristics
        .iter()
        .filter(|characteristic| characteristic.uuid.to_string() == uuid)
        .nth(want)
}

/// Select the `occurrence`-th descriptor with `uuid` under one
/// characteristic instance, in canonical order.
fn select_descriptor<'s>(
    characteristic: &'s Characteristic,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Descriptor> {
    let want = usize::try_from(occurrence).ok()?;
    characteristic
        .descriptors
        .iter()
        .filter(|descriptor| descriptor.uuid.to_string() == uuid)
        .nth(want)
}

fn map_radio(
    operation: &'static str,
    code: ubm_core::contracts::BleErrorCode,
    domain: ubm_core::contracts::BleErrorDomain,
) -> impl Fn(btleplug::Error) -> DesktopError {
    move |error| DesktopError::new(code, domain, operation).with_detail(error.to_string())
}

fn property_flags(flags: CharPropFlags) -> PropertyFlags {
    PropertyFlags {
        read: flags.contains(CharPropFlags::READ),
        write: flags.contains(CharPropFlags::WRITE),
        write_without_response: flags.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
        notify: flags.contains(CharPropFlags::NOTIFY),
        indicate: flags.contains(CharPropFlags::INDICATE),
    }
}

/// Translate one btleplug manufacturer-data map into snapshot sections
/// (F22): company IDs plus payload bytes verbatim, sorted by company ID
/// so the unordered OS map yields a deterministic snapshot. Empty payloads
/// are preserved (section present), never dropped.
fn sorted_manufacturer_data(sections: &HashMap<u16, Vec<u8>>) -> Vec<ManufacturerData> {
    let mut entries: Vec<ManufacturerData> = sections
        .iter()
        .map(|(company_id, payload)| ManufacturerData {
            company_id: *company_id,
            payload: payload.clone(),
        })
        .collect();
    entries.sort_by_key(|entry| entry.company_id);
    entries
}

/// Translate one btleplug service-data map into snapshot sections (F22):
/// UUIDs plus payload bytes verbatim, sorted by UUID so the unordered OS
/// map yields a deterministic snapshot. Empty payloads are preserved
/// (section present), never dropped.
fn sorted_service_data(sections: &HashMap<uuid::Uuid, Vec<u8>>) -> Vec<ServiceData> {
    let mut entries: Vec<ServiceData> = sections
        .iter()
        .map(|(uuid, payload)| ServiceData {
            uuid: uuid.to_string(),
            payload: payload.clone(),
        })
        .collect();
    entries.sort_by(|left, right| left.uuid.cmp(&right.uuid));
    entries
}

fn property_bits(flags: PropertyFlags) -> u8 {
    let mut bits = 0u8;
    if flags.read {
        bits |= GATT_PROP_READ;
    }
    if flags.write {
        bits |= GATT_PROP_WRITE;
    }
    if flags.write_without_response {
        bits |= GATT_PROP_WRITE_NO_RESPONSE;
    }
    if flags.notify {
        bits |= GATT_PROP_NOTIFY;
    }
    if flags.indicate {
        bits |= GATT_PROP_INDICATE;
    }
    bits
}

impl RadioBoundary for BtleplugRadio {
    async fn adapter_name(&self) -> Result<String, DesktopError> {
        Ok(self.adapter_label.clone())
    }

    async fn start_scan(&self, filter: ScanFilterSpec) -> Result<(), DesktopError> {
        let mut services = Vec::with_capacity(filter.service_uuids.len());
        for uuid in &filter.service_uuids {
            services.push(uuid::Uuid::parse_str(uuid).map_err(|_| {
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::ScanFilterInvalid,
                    ubm_core::contracts::BleErrorDomain::Scan,
                    "scan.filter",
                )
            })?);
        }
        self.adapter
            .start_scan(ScanFilter { services })
            .await
            .map_err(|error| DesktopError::scan_start_failed(error.to_string()))
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.adapter
            .stop_scan()
            .await
            .map_err(|error| DesktopError::scan_stop_failed(error.to_string()))
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        let mut out = Vec::new();
        let peripherals = self.adapter.peripherals().await.map_err(map_radio(
            "peer.list",
            ubm_core::contracts::BleErrorCode::AdapterUnavailable,
            ubm_core::contracts::BleErrorDomain::Adapter,
        ))?;
        for peripheral in &peripherals {
            out.push(self.snapshot(peripheral).await);
        }
        Ok(out)
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        peripheral
            .connect()
            .await
            .map_err(|error| DesktopError::connection_failed(error.to_string()))
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        // btleplug maps an already-released peripheral to success-or-error
        // per platform; treat "already gone" as released (core treats
        // `is_connected() == false` as released too).
        if !peripheral.is_connected().await.unwrap_or(true) {
            return Ok(());
        }
        peripheral.disconnect().await.map_err(|error| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::ConnectionLost,
                ubm_core::contracts::BleErrorDomain::Connection,
                "connection.disconnect",
            )
            .with_detail(error.to_string())
        })
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        peripheral.discover_services().await.map_err(map_radio(
            "discovery.complete",
            ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
            ubm_core::contracts::BleErrorDomain::Gatt,
        ))?;
        // The cached service set already iterates in canonical UUID-first
        // order; occurrences count per UUID in that order so instance
        // numbers agree with the occurrence-aware lookup path (H1).
        let services = peripheral.services();
        let mut out = Vec::with_capacity(services.len());
        let mut service_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
        for service in &services {
            let service_occurrence = service_counts.entry(service.uuid).or_insert(0);
            let service_occ = *service_occurrence;
            *service_occurrence += 1;
            let mut chars_out = Vec::with_capacity(service.characteristics.len());
            let mut char_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
            for characteristic in &service.characteristics {
                let char_occurrence = char_counts.entry(characteristic.uuid).or_insert(0);
                let char_occ = *char_occurrence;
                *char_occurrence += 1;
                let mut desc_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
                let mut descs_out = Vec::with_capacity(characteristic.descriptors.len());
                for descriptor in &characteristic.descriptors {
                    let desc_occurrence = desc_counts.entry(descriptor.uuid).or_insert(0);
                    let desc_occ = *desc_occurrence;
                    *desc_occurrence += 1;
                    descs_out.push(DescriptorSnapshot {
                        uuid: descriptor.uuid.to_string(),
                        occurrence: desc_occ,
                    });
                }
                chars_out.push(CharacteristicSnapshot {
                    uuid: characteristic.uuid.to_string(),
                    occurrence: char_occ,
                    properties: property_flags(characteristic.properties),
                    descriptors: descs_out,
                });
            }
            out.push(ServiceSnapshot {
                uuid: service.uuid.to_string(),
                occurrence: service_occ,
                characteristics: chars_out,
            });
        }
        Ok(out)
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.read",
            )
        })?;
        peripheral
            .read(&characteristic)
            .await
            .map_err(|error| DesktopError::read_failed(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.write",
            )
        })?;
        let mode = if with_response {
            btleplug::api::WriteType::WithResponse
        } else {
            btleplug::api::WriteType::WithoutResponse
        };
        peripheral
            .write(&characteristic, &value, mode)
            .await
            .map_err(|error| DesktopError::write_failed(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let descriptor = Self::find_descriptor(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.read-descriptor",
            )
        })?;
        peripheral
            .read_descriptor(&descriptor)
            .await
            .map_err(|error| DesktopError::read_failed(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
        value: Vec<u8>,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let descriptor = Self::find_descriptor(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.write-descriptor",
            )
        })?;
        peripheral
            .write_descriptor(&descriptor, &value)
            .await
            .map_err(|error| DesktopError::write_failed(error.to_string()))
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        // The peripheral lookup already failed closed upstream for unknown
        // peers; here an unknown peer is simply unmeasured, and the write
        // path fails closed as `capability.unavailable`.
        self.peripheral_by_id(peer_id).await.ok().map(|p| p.mtu())
    }

    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::subscribe_failed(format!(
                "unknown characteristic {characteristic_uuid} occurrence {characteristic_occurrence}"
            ))
        })?;
        let scope: InstanceKey = (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        );
        let key = forwarder_key(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        if enable {
            // F09: fail before touching the CCCD when a live subscription
            // already owns this (peer, service, characteristic) scope under
            // another instance — the native stream carries no occurrence
            // identity, so a second forwarder could only fan out bytes.
            // Debt counts as live (F13): the orphaned CCCD may still emit.
            let ambiguous = {
                let table = self.forwarders.lock().expect("forwarder table");
                let debt = self.cleanup_debt.lock().expect("cleanup debt");
                let live = live_scopes(&table, &debt);
                route_is_ambiguous(
                    &live,
                    peer_id,
                    service_uuid,
                    service_occurrence,
                    characteristic_uuid,
                    characteristic_occurrence,
                )
            };
            if ambiguous {
                return Err(DesktopError::subscribe_failed(format!(
                    "ambiguous notification routing: characteristic {characteristic_uuid} \
                     under service {service_uuid} already has a live subscription on \
                     another instance; the native stream carries no occurrence identity"
                )));
            }
            peripheral
                .subscribe(&characteristic)
                .await
                .map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
            let stream: NotificationStream = match peripheral.notifications().await {
                Ok(stream) => stream,
                Err(error) => {
                    // F13: the CCCD is already enabled with no forwarder to
                    // consume it — roll back the native enablement so no
                    // orphan subscription outlives this failure. A failed
                    // rollback parks cleanup debt for retry/dispose.
                    let rollback_ok = peripheral.unsubscribe(&characteristic).await.is_ok();
                    apply_enable_stream_failure(
                        &mut self.cleanup_debt.lock().expect("cleanup debt"),
                        &scope,
                        rollback_ok,
                    );
                    return Err(DesktopError::subscribe_failed(error.to_string()));
                }
            };
            let sender = self.notifications.clone();
            let queued_bytes = Arc::clone(&self.ingress_bytes);
            let dropped = Arc::clone(&self.ingress_dropped);
            let peer = peer_id.to_owned();
            let service = service_uuid.to_owned();
            let instance_characteristic = characteristic_uuid.to_owned();
            // The btleplug stream is peripheral-wide: filter on the full
            // (service, characteristic) identity so one subscription never
            // routes another scope's values. Same-scope duplicate
            // instances stay indistinguishable on this stream (no handles
            // exposed) and are rejected at enable time, never fanned
            // out; see PARITY_GAPS.md.
            let route = NotificationRoute {
                service_uuid: characteristic.service_uuid,
                characteristic_uuid: characteristic.uuid,
            };
            // The subscription epoch is captured at install, never minted
            // at dequeue: every value this forwarder emits is attributable
            // to exactly the enablement that installed it (F10).
            let installed_epoch = epoch;
            let forwarder = self.spawn.spawn(async move {
                let mut stream = stream;
                while let Some(note) = stream.next().await {
                    if !route.matches(&note) {
                        continue;
                    }
                    // F07: bounded ingress at the first owned handoff —
                    // `try_send` never blocks the runtime worker, overload
                    // drops are counted (never silent), and the CCCD stays
                    // enabled so later values still flow after the drain.
                    let len = note.value.len() as u64;
                    if queued_bytes.load(Ordering::Relaxed).saturating_add(len) > NOTIFICATION_BYTES
                    {
                        dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let event = RadioEvent::Notification {
                        peer_id: peer.clone(),
                        service_uuid: service.clone(),
                        service_occurrence,
                        characteristic_uuid: instance_characteristic.clone(),
                        characteristic_occurrence,
                        epoch: installed_epoch,
                        value: note.value,
                    };
                    match sender.try_send(event) {
                        Ok(()) => {
                            queued_bytes.fetch_add(len, Ordering::Relaxed);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            });
            // Defensive replace: a live entry under the same per-instance
            // key is aborted before overwrite so no forwarder ever leaks.
            let replaced = self.forwarders.lock().expect("forwarder table").insert(
                key,
                ForwarderEntry {
                    task: forwarder,
                    peer_id: peer_id.to_owned(),
                    service_uuid: service_uuid.to_owned(),
                    service_occurrence,
                    characteristic_uuid: characteristic_uuid.to_owned(),
                    characteristic_occurrence,
                },
            );
            if let Some(stale) = replaced {
                stale.task.abort();
            }
            // A full enable supersedes any parked setup debt for this
            // scope: the new forwarder owns the native CCCD now.
            self.cleanup_debt
                .lock()
                .expect("cleanup debt")
                .remove(&scope);
        } else {
            // F13: the native disable runs BEFORE the forwarder is
            // touched — only a successful unsubscribe removes the
            // consumer, so a still-enabled CCCD keeps forwarding until
            // a retry disables it (the central's L7 path relies on
            // values continuing to flow here).
            let outcome = peripheral.unsubscribe(&characteristic).await;
            apply_unsubscribe_outcome(
                &mut self.forwarders.lock().expect("forwarder table"),
                &mut self.cleanup_debt.lock().expect("cleanup debt"),
                &key,
                &scope,
                outcome.is_ok(),
            );
            outcome.map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
        }
        Ok(())
    }

    /// Teardown hook (M3): abort every live forwarder and best-effort
    /// release every OS-side CCCD, including parked cleanup debt (F13):
    /// an orphaned native enablement is still owed its unsubscribe.
    /// Failures are swallowed: teardown reports facts, never new failures.
    async fn close(&self) {
        let entries: Vec<ForwarderEntry> = self
            .forwarders
            .lock()
            .expect("forwarder table")
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in &entries {
            entry.task.abort();
        }
        let mut scopes: Vec<InstanceKey> = entries.iter().map(ForwarderEntry::scope).collect();
        scopes.extend(self.cleanup_debt.lock().expect("cleanup debt").drain());
        for scope in &scopes {
            let peripheral = match self.peripheral_by_id(&scope.0).await {
                Ok(peripheral) => peripheral,
                Err(_) => continue,
            };
            let characteristic = match Self::find_characteristic(
                &peripheral,
                &scope.1,
                scope.2,
                &scope.3,
                scope.4,
            ) {
                Some(characteristic) => characteristic,
                None => continue,
            };
            let _ = peripheral.unsubscribe(&characteristic).await;
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        enum Step {
            Notification(Option<RadioEvent>),
            Adapter(Option<CentralEvent>),
        }
        loop {
            // Both guards drop at the end of the block, before any
            // snapshot lookup awaits, so a slow OS lookup never wedges
            // the event source.
            let step = {
                let mut events = self.events.lock().await;
                let stream = events.as_mut()?;
                // Deliberately unbiased: a notification flood must never
                // starve adapter events (a delayed DeviceDisconnected is
                // a stale link, not a slow one).
                tokio::select! {
                    notified = self.recv_notification() => Step::Notification(notified),
                    event = stream.next() => Step::Adapter(event),
                }
            };
            match step {
                Step::Notification(notified) => return notified,
                Step::Adapter(None) => return None,
                Step::Adapter(Some(CentralEvent::DeviceDiscovered(id)))
                | Step::Adapter(Some(CentralEvent::DeviceUpdated(id))) => {
                    if let Some(event) = self.advertisement_for(&id).await {
                        return Some(event);
                    }
                }
                // A changed GATT database invalidates discovered paths:
                // surface it as its own event so the central invalidates
                // generations instead of re-reading stale handles. The
                // next discovery refreshes the snapshot.
                Step::Adapter(Some(CentralEvent::DeviceServicesModified(id))) => {
                    self.abort_peer_forwarders(&id.to_string());
                    return Some(RadioEvent::ServicesChanged(id.to_string()));
                }
                Step::Adapter(Some(CentralEvent::DeviceConnected(id))) => {
                    return Some(RadioEvent::Connected(id.to_string()));
                }
                Step::Adapter(Some(CentralEvent::DeviceDisconnected(id))) => {
                    self.abort_peer_forwarders(&id.to_string());
                    return Some(RadioEvent::Disconnected(id.to_string()));
                }
                Step::Adapter(Some(_)) => {}
            }
        }
    }
}

/// Translate one discovered service snapshot into core path registrations is
/// owned by [`crate::DesktopCentral`]; this helper exposes the property-bit
/// mapping for it.
#[must_use]
pub fn core_property_bits(flags: PropertyFlags) -> u8 {
    property_bits(flags)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use btleplug::api::{CharPropFlags, Characteristic, Descriptor, Service, ValueNotification};

    use super::{
        ForwarderEntry, NotificationRoute, apply_enable_stream_failure, apply_unsubscribe_outcome,
        core_property_bits, forwarder_key, live_scopes, route_is_ambiguous, select_characteristic,
        select_descriptor, select_service, sorted_manufacturer_data, sorted_service_data,
    };
    use crate::boundary::PropertyFlags;
    use ubm_core::central::{
        GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
    };

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).expect("fixture uuid")
    }

    fn characteristic(service: &str, char: &str, flags: CharPropFlags) -> Characteristic {
        Characteristic {
            uuid: uuid(char),
            service_uuid: uuid(service),
            properties: flags,
            descriptors: BTreeSet::new(),
        }
    }

    fn service_with(uuid_text: &str, chars: Vec<Characteristic>) -> Service {
        Service {
            uuid: uuid(uuid_text),
            primary: true,
            characteristics: chars.into_iter().collect(),
        }
    }

    #[test]
    fn occurrence_selects_among_duplicate_characteristics() {
        let service = service_with(
            HRM_SERVICE,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::NOTIFY),
            ],
        );
        let first = select_characteristic(&service, HRM_MEASUREMENT, 0).expect("instance 0");
        assert!(first.properties.contains(CharPropFlags::READ));
        let second = select_characteristic(&service, HRM_MEASUREMENT, 1).expect("instance 1");
        assert!(second.properties.contains(CharPropFlags::NOTIFY));
        assert!(
            select_characteristic(&service, HRM_MEASUREMENT, 2).is_none(),
            "missing instance selects nothing, never instance 0"
        );
    }

    #[test]
    fn manufacturer_sections_sort_by_company_with_verbatim_payloads() {
        let sections = HashMap::from([
            (0x02b2u16, vec![0xb2, 0x02, 0x01]),
            (0x006bu16, vec![]),
            (0xffffu16, vec![0x00]),
        ]);
        let sorted = sorted_manufacturer_data(&sections);
        let ids: Vec<u16> = sorted.iter().map(|entry| entry.company_id).collect();
        assert_eq!(ids, vec![0x006b, 0x02b2, 0xffff]);
        assert_eq!(sorted[0].payload, Vec::<u8>::new());
        assert_eq!(sorted[1].payload, vec![0xb2, 0x02, 0x01]);
        assert_eq!(sorted[2].payload, vec![0x00]);
        assert!(
            sorted_manufacturer_data(&HashMap::new()).is_empty(),
            "no sections is empty, never synthesized"
        );
    }

    #[test]
    fn service_sections_sort_by_uuid_with_verbatim_payloads() {
        let high = uuid::Uuid::parse_str("0000fef5-0000-1000-8000-00805f9b34fb").expect("uuid");
        let low = uuid::Uuid::parse_str("0000180d-0000-1000-8000-00805f9b34fb").expect("uuid");
        let sections = HashMap::from([(high, vec![0x01]), (low, vec![])]);
        let sorted = sorted_service_data(&sections);
        let uuids: Vec<&str> = sorted.iter().map(|entry| entry.uuid.as_str()).collect();
        assert_eq!(
            uuids,
            vec![
                "0000180d-0000-1000-8000-00805f9b34fb",
                "0000fef5-0000-1000-8000-00805f9b34fb",
            ]
        );
        assert_eq!(sorted[0].payload, Vec::<u8>::new());
        assert_eq!(sorted[1].payload, vec![0x01]);
        assert!(
            sorted_service_data(&HashMap::new()).is_empty(),
            "no sections is empty, never synthesized"
        );
    }

    #[test]
    fn occurrence_counts_per_uuid_not_flat_index() {
        // Flat indexing would number BATTERY_LEVEL as 2; per-UUID
        // counting (matching discover()) numbers it 0.
        let service = service_with(
            HRM_SERVICE,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::NOTIFY),
                characteristic(HRM_SERVICE, BATTERY_LEVEL, CharPropFlags::READ),
            ],
        );
        let battery = select_characteristic(&service, BATTERY_LEVEL, 0).expect("battery");
        assert_eq!(battery.uuid, uuid(BATTERY_LEVEL));
        assert!(
            select_characteristic(&service, BATTERY_LEVEL, 1).is_none(),
            "second battery instance does not exist"
        );
    }

    #[test]
    fn occurrence_selects_among_duplicate_services() {
        let services: BTreeSet<Service> = [
            service_with(
                HRM_SERVICE,
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
                    CharPropFlags::READ,
                )],
            ),
            service_with(
                HRM_SERVICE,
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
                    CharPropFlags::NOTIFY,
                )],
            ),
        ]
        .into_iter()
        .collect();
        // The two services share a UUID but differ in characteristics, so
        // the cache holds both: occurrence selects the instance.
        assert_eq!(services.len(), 2, "distinct duplicates both survive");
        let first = select_service(&services, HRM_SERVICE, 0).expect("instance 0");
        let second = select_service(&services, HRM_SERVICE, 1).expect("instance 1");
        assert_ne!(
            first.characteristics, second.characteristics,
            "occurrences address different instances"
        );
        assert!(
            select_service(&services, HRM_SERVICE, 2).is_none(),
            "missing instance selects nothing, never instance 0"
        );
    }

    #[test]
    fn occurrence_selects_among_duplicate_descriptors() {
        let with_desc = |flags: CharPropFlags| {
            let mut descriptors = BTreeSet::new();
            descriptors.insert(Descriptor {
                uuid: uuid(USER_DESCRIPTION),
                service_uuid: uuid(HRM_SERVICE),
                characteristic_uuid: uuid(HRM_MEASUREMENT),
            });
            Characteristic {
                uuid: uuid(HRM_MEASUREMENT),
                service_uuid: uuid(HRM_SERVICE),
                properties: flags,
                descriptors,
            }
        };
        // Descriptors differing only by UUID-collapsed identity stay one
        // entry; distinct-UUID descriptors number per UUID.
        let characteristic = with_desc(CharPropFlags::READ);
        assert!(
            select_descriptor(&characteristic, USER_DESCRIPTION, 0).is_some(),
            "descriptor instance 0"
        );
        assert!(
            select_descriptor(&characteristic, USER_DESCRIPTION, 1).is_none(),
            "no phantom descriptor instance"
        );
        assert!(
            select_descriptor(&characteristic, BATTERY_LEVEL, 0).is_none(),
            "unknown descriptor selects nothing"
        );
    }

    #[test]
    fn forwarder_keys_are_per_instance() {
        let first = forwarder_key("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let second = forwarder_key("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 1);
        assert_ne!(first, second, "duplicate UUIDs never share a forwarder");
    }

    #[test]
    fn property_bits_cover_read_write_notify_indicate() {
        let bits = core_property_bits(PropertyFlags {
            read: true,
            write: true,
            write_without_response: true,
            notify: true,
            indicate: true,
        });
        assert!(bits & GATT_PROP_READ != 0);
        assert!(bits & GATT_PROP_WRITE != 0);
        assert!(bits & GATT_PROP_NOTIFY != 0);
        assert!(bits & GATT_PROP_INDICATE != 0);
    }

    #[test]
    fn empty_properties_map_to_no_bits() {
        let bits = core_property_bits(PropertyFlags {
            read: false,
            write: false,
            write_without_response: false,
            notify: false,
            indicate: false,
        });
        assert_eq!(bits, 0);
    }

    #[test]
    fn f08_write_capabilities_map_to_independent_bits() {
        use ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE;

        let mapping = |write: bool, without_response: bool| {
            core_property_bits(PropertyFlags {
                read: false,
                write,
                write_without_response: without_response,
                notify: false,
                indicate: false,
            })
        };
        assert_eq!(mapping(false, false), 0, "neither capability sets no bit");
        assert_eq!(
            mapping(true, false),
            GATT_PROP_WRITE,
            "request-write sets only the write bit"
        );
        assert_eq!(
            mapping(false, true),
            GATT_PROP_WRITE_NO_RESPONSE,
            "command-write sets only the no-response bit, never the write bit"
        );
        assert_eq!(
            mapping(true, true),
            GATT_PROP_WRITE | GATT_PROP_WRITE_NO_RESPONSE,
            "both capabilities survive independently"
        );
    }

    fn notification(service: &str, char: &str, value: Vec<u8>) -> ValueNotification {
        ValueNotification {
            uuid: uuid(char),
            service_uuid: uuid(service),
            value,
        }
    }

    #[test]
    fn f09_same_characteristic_uuid_under_two_services_routes_only_to_owner() {
        // The same characteristic UUID appears under two services — the
        // peripheral-wide OS stream delivers both to every forwarder, so
        // each forwarder must filter on the service identity too. Bytes
        // for service A must never reach the service B subscription.
        let route_hrm = NotificationRoute {
            service_uuid: uuid(HRM_SERVICE),
            characteristic_uuid: uuid(HRM_MEASUREMENT),
        };
        let route_battery = NotificationRoute {
            service_uuid: uuid(BATTERY_SERVICE),
            characteristic_uuid: uuid(HRM_MEASUREMENT),
        };
        let note_hrm = notification(HRM_SERVICE, HRM_MEASUREMENT, vec![0x01]);
        let note_battery = notification(BATTERY_SERVICE, HRM_MEASUREMENT, vec![0x02]);
        assert!(
            route_hrm.matches(&note_hrm),
            "owner route accepts its own service bytes"
        );
        assert!(
            route_battery.matches(&note_battery),
            "owner route accepts its own service bytes"
        );
        assert!(
            !route_hrm.matches(&note_battery),
            "cross-service bytes must not route to the wrong subscription"
        );
        assert!(
            !route_battery.matches(&note_hrm),
            "cross-service bytes must not route to the wrong subscription"
        );
        let other_char = notification(HRM_SERVICE, BATTERY_LEVEL, vec![0x03]);
        assert!(
            !route_hrm.matches(&other_char),
            "another characteristic under the same service still filters out"
        );
    }

    #[test]
    fn f09_duplicate_occurrence_enable_rejected_explicitly() {
        // One live subscription owns (peer, service 0, char 0). The native
        // stream carries no occurrence identity, so enabling a second
        // instance of the same scope must report ambiguity explicitly —
        // never install a second forwarder that would fan out bytes.
        let live = [(
            "peer-1".to_owned(),
            HRM_SERVICE.to_owned(),
            0u64,
            HRM_MEASUREMENT.to_owned(),
            0u64,
        )];
        assert!(
            route_is_ambiguous(&live, "peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 1),
            "second characteristic occurrence of a live scope is ambiguous"
        );
        assert!(
            route_is_ambiguous(&live, "peer-1", HRM_SERVICE, 1, HRM_MEASUREMENT, 0),
            "same characteristic under a duplicate service occurrence is ambiguous"
        );
        assert!(
            !route_is_ambiguous(&live, "peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0),
            "re-enabling the exact same instance stays idempotent, not ambiguous"
        );
        assert!(
            !route_is_ambiguous(&live, "peer-1", BATTERY_SERVICE, 0, HRM_MEASUREMENT, 0),
            "same characteristic under a different service is a distinct scope"
        );
        assert!(
            !route_is_ambiguous(&live, "peer-1", HRM_SERVICE, 0, BATTERY_LEVEL, 0),
            "a different characteristic under the same service is a distinct scope"
        );
        assert!(
            !route_is_ambiguous(&live, "peer-2", HRM_SERVICE, 0, HRM_MEASUREMENT, 1),
            "another peer never collides with this peer's scopes"
        );
        assert!(
            !route_is_ambiguous(&[], "peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0),
            "first enablement of a scope is never ambiguous"
        );
    }

    fn scope(
        peer: &str,
        service: &str,
        service_occurrence: u64,
        char: &str,
        char_occurrence: u64,
    ) -> crate::boundary::InstanceKey {
        (
            peer.to_owned(),
            service.to_owned(),
            service_occurrence,
            char.to_owned(),
            char_occurrence,
        )
    }

    fn forwarder(
        task: tokio::task::JoinHandle<()>,
        scope: &crate::boundary::InstanceKey,
    ) -> ForwarderEntry {
        ForwarderEntry {
            task,
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
        }
    }

    fn scope_key(scope: &crate::boundary::InstanceKey) -> String {
        forwarder_key(&scope.0, &scope.1, scope.2, &scope.3, scope.4)
    }

    #[test]
    fn f13_enable_stream_failure_rolls_back_or_records_debt() {
        // Native subscribe succeeded but the stream install failed: a
        // successful compensating unsubscribe leaves nothing behind,
        // while a failed rollback parks the scope as cleanup debt for
        // retry/dispose — it must never silently vanish.
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let mut debt = std::collections::HashSet::new();
        apply_enable_stream_failure(&mut debt, &scope, true);
        assert!(
            debt.is_empty(),
            "successful rollback leaves no cleanup debt"
        );
        apply_enable_stream_failure(&mut debt, &scope, false);
        assert_eq!(
            debt,
            std::collections::HashSet::from([scope.clone()]),
            "failed rollback parks the orphaned native enablement as debt"
        );
        // A later successful rollback (retry/dispose) clears the debt.
        apply_enable_stream_failure(&mut debt, &scope, true);
        assert!(debt.is_empty(), "retry success clears the debt");
    }

    #[tokio::test]
    async fn f13_disable_failure_preserves_forwarder() {
        // The CCCD is still live when the native disable fails, so the
        // forwarder must stay: values keep flowing until a retry
        // disables it. Only a successful unsubscribe removes the
        // consumer — and a retry then completes the teardown.
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&scope);
        let mut forwarders = std::collections::HashMap::new();
        forwarders.insert(key.clone(), forwarder(tokio::spawn(async {}), &scope));
        let mut debt = std::collections::HashSet::new();
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, false);
        assert!(
            forwarders.contains_key(&key),
            "failed disable keeps the forwarder: the CCCD is still live"
        );
        assert!(
            debt.is_empty(),
            "no separate debt while the retained forwarder owns cleanup"
        );
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, true);
        assert!(
            !forwarders.contains_key(&key),
            "retry success removes the forwarder"
        );
        assert!(debt.is_empty(), "retry success holds no debt");
    }

    #[test]
    fn f13_unresolved_teardown_without_consumer_parks_debt() {
        // Disabling a scope with no installed forwarder (retry of a
        // half-torn-down enablement): success clears any debt, failure
        // parks it so dispose still attempts the native release.
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&scope);
        let mut forwarders = std::collections::HashMap::new();
        let mut debt = std::collections::HashSet::new();
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, false);
        assert_eq!(
            debt,
            std::collections::HashSet::from([scope.clone()]),
            "failed consumer-less disable retains the unresolved resource"
        );
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, true);
        assert!(debt.is_empty(), "retry success clears the debt");
    }

    #[tokio::test]
    async fn f13_debt_scope_counts_as_live_for_ambiguity() {
        // A debt CCCD may still emit bytes the native stream cannot
        // attribute, so it blocks an ambiguous sibling enablement
        // exactly like an installed forwarder does.
        let debt_scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let forwarders = std::collections::HashMap::new();
        let debt = std::collections::HashSet::from([debt_scope]);
        let live = live_scopes(&forwarders, &debt);
        assert_eq!(live.len(), 1, "debt scopes count as live");
        assert!(
            route_is_ambiguous(&live, "peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 1),
            "sibling occurrence of a debt scope is ambiguous"
        );
        assert!(
            !route_is_ambiguous(&live, "peer-1", BATTERY_SERVICE, 0, HRM_MEASUREMENT, 0),
            "unrelated scopes stay out of the debt's way"
        );
    }
}
