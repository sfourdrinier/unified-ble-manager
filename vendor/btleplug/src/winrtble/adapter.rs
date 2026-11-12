// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use super::{
    ble::watcher::{BLEWatcher, ScanWatcherRecovery},
    peripheral::{PeerRegistry, Peripheral, PeripheralId},
};
use crate::{
    Error, Result,
    api::{AddressType, BDAddr, Central, CentralEvent, CentralState, ScanFilter},
    common::adapter_manager::AdapterManager,
};
use async_trait::async_trait;
use futures::stream::Stream;
use log::warn;
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::fmt::{self, Debug, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use windows::{
    Devices::Radios::{Radio, RadioState},
    Foundation::TypedEventHandler,
};

#[path = "adapter_cache.rs"]
mod adapter_cache;

pub(super) fn resolved_peer_identity(
    address: BDAddr,
    kind: windows::Devices::Bluetooth::BluetoothAddressType,
) -> Result<PeripheralId> {
    let kind = match kind {
        windows::Devices::Bluetooth::BluetoothAddressType::Public => AddressType::Public,
        windows::Devices::Bluetooth::BluetoothAddressType::Random => AddressType::Random,
        _ => {
            return Err(Error::Other(
                "WinRT resolved an unknown Bluetooth address type".into(),
            ));
        }
    };
    Ok(PeripheralId::with_address_type(address, Some(kind)))
}

fn cache_resolved_peripheral(
    manager: &Arc<AdapterManager<Peripheral>>,
    peers: &PeerRegistry,
    authorities: &Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    identity: PeripheralId,
) -> Result<Peripheral> {
    use crate::api::Peripheral as _;
    let candidate = peers.get(Arc::downgrade(manager), identity, authorities.clone())?;
    Ok(adapter_cache::cache_resolved_peer(
        candidate,
        |peer| peer.id(),
        |key, candidate| manager.peripheral_or_insert_with(key, || candidate).0,
    ))
}

/// Implementation of [api::Central](crate::api::Central).
#[derive(Clone)]
pub struct Adapter {
    watcher: Arc<Mutex<BLEWatcher>>,
    manager: Arc<AdapterManager<Peripheral>>,
    reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
    peers: Arc<PeerRegistry>,
    /// UBM patch (`winrt-adapter-by-id`): the radio this adapter reports
    /// state from, rebindable when the adapter device comes back after a
    /// removal. `None` when the OS withholds the radio (adapter access not
    /// granted), answered by `withheld`.
    radio: Arc<Mutex<Option<RadioBinding>>>,
    withheld: CentralState,
    /// Ordered event-stream fence acknowledgements. A native callback may
    /// have returned while its scoped event is still buffered by the manager,
    /// so a physical callback drain alone cannot release the scan owner.
    scan_ingress_fences: Arc<Mutex<ScanIngressFences>>,
}

#[derive(Debug, Default)]
struct ScanIngressFences {
    source_closed: bool,
    next_id: u64,
    pending: BTreeMap<u64, ScanIngressFenceFlight>,
    by_epoch: BTreeMap<u64, u64>,
}

#[derive(Debug)]
struct ScanIngressFenceFlight {
    ingress_epoch: u64,
    reply: watch::Sender<ScanIngressFenceReply>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanIngressFenceReply {
    Pending,
    Acknowledged,
    SourceClosed,
}

impl ScanIngressFences {
    fn source_closed_error() -> Error {
        Error::Other(
            "WinRT adapter event stream closed before its scan-ingress fence was consumed".into(),
        )
    }

    /// Join the one marker flight for this epoch, or create it. A cancelled
    /// caller only drops its receiver; it must never manufacture another
    /// marker or leave an unbounded sequence of orphaned senders behind.
    fn join_or_register(
        &mut self,
        ingress_epoch: u64,
    ) -> Result<(u64, watch::Receiver<ScanIngressFenceReply>, bool)> {
        if self.source_closed {
            return Err(Self::source_closed_error());
        }
        if let Some(fence_id) = self.by_epoch.get(&ingress_epoch).copied() {
            let flight = self
                .pending
                .get(&fence_id)
                .expect("WinRT scan-ingress epoch index must name a pending fence");
            return Ok((fence_id, flight.reply.subscribe(), false));
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::Other("WinRT scan-ingress fence identifier overflow".into()))?;
        let (reply, receiver) = watch::channel(ScanIngressFenceReply::Pending);
        self.pending.insert(
            self.next_id,
            ScanIngressFenceFlight {
                ingress_epoch,
                reply,
            },
        );
        self.by_epoch.insert(ingress_epoch, self.next_id);
        Ok((self.next_id, receiver, true))
    }

    fn acknowledge(&mut self, fence_id: u64) -> bool {
        let Some(flight) = self.pending.remove(&fence_id) else {
            return false;
        };
        self.by_epoch.remove(&flight.ingress_epoch);
        let _ = flight.reply.send(ScanIngressFenceReply::Acknowledged);
        true
    }

    fn close(&mut self) {
        self.source_closed = true;
        self.by_epoch.clear();
        for (_, flight) in std::mem::take(&mut self.pending) {
            let _ = flight.reply.send(ScanIngressFenceReply::SourceClosed);
        }
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// One radio and its `StateChanged` registration, removed on drop so the
/// handler never outlives the adapter.
#[derive(Debug)]
struct RadioBinding {
    radio: Radio,
    token: i64,
}

/// Public result for a recovery attempt. The watcher's internal registration
/// generation deliberately stays private to this adapter implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StalledScanWatcher {
    Replaced,
    Aborted,
    StillStopping,
}

impl Drop for RadioBinding {
    fn drop(&mut self) {
        if let Err(error) = self.radio.RemoveStateChanged(self.token) {
            warn!("Drop: removing the radio StateChanged handler failed: {error:?}");
        }
    }
}

// https://github.com/microsoft/windows-rs/blob/master/crates/libs/windows/src/Windows/Devices/Radios/mod.rs
/// UBM patch (UBM_PATCHES.md #7): `Disabled` is off, as the legacy addon's
/// `RadioPower` (`addon.cpp`); every other state is `Unknown`.
fn central_state(state: RadioState) -> CentralState {
    match state {
        RadioState::On => CentralState::PoweredOn,
        RadioState::Off | RadioState::Disabled => CentralState::PoweredOff,
        _ => CentralState::Unknown,
    }
}

fn get_central_state(radio: &Radio) -> Result<CentralState> {
    Ok(central_state(radio.State()?))
}

fn bind_radio(radio: Radio, manager: &Arc<AdapterManager<Peripheral>>) -> Result<RadioBinding> {
    let manager = Arc::clone(manager);
    let handler = TypedEventHandler::new(move |sender: windows::core::Ref<Radio>, _args| {
        // A state the OS cannot read is reported as `Unknown`, and logged.
        let state = sender
            .ok()
            .map_err(Error::from)
            .and_then(get_central_state)
            .unwrap_or_else(|error| {
                warn!("radio StateChanged: reading the state failed: {error:?}");
                CentralState::Unknown
            });
        manager.emit(CentralEvent::StateUpdate(state));
        Ok(())
    });
    let token = radio.StateChanged(&handler)?;
    Ok(RadioBinding { radio, token })
}

impl Adapter {
    pub fn reclaim_authority_handle(&self) -> crate::ubm_reclaim_authority::ReclaimAuthorityHandle {
        crate::ubm_reclaim_authority::ReclaimAuthorityHandle::from_owner(
            self.reclaim_authorities.clone(),
        )
    }

    /// Register a natively resolved explicit target without losing its LE
    /// address type when the connection is subsequently opened.
    pub async fn add_peripheral_with_address_type(
        &self,
        address: &PeripheralId,
        kind: AddressType,
    ) -> Result<Peripheral> {
        let identity = PeripheralId::with_address_type(address.address(), Some(kind));
        let peripheral = self.add_peripheral(&identity).await?;
        peripheral.set_explicit_address_type(kind)?;
        Ok(peripheral)
    }

    /// UBM patch (`winrt-adapter-by-id`): the adapter for one chosen radio
    /// (`BluetoothAdapter::FromIdAsync(id).GetRadioAsync()`), so a caller
    /// selects the Bluetooth adapter by its device id instead of by radio
    /// order. Scanning and connections still run through the Windows
    /// Bluetooth LE stack (the advertisement watcher and
    /// `BluetoothLEDevice::FromBluetoothAddressAsync` take no adapter), as
    /// they did in the legacy addon; the radio decides the reported state.
    pub fn from_radio(radio: Radio) -> Result<Self> {
        let (peers, authorities) = super::peripheral::shared_peer_owners();
        Self::from_radio_with_authorities(radio, authorities, peers)
    }

    pub(crate) fn from_radio_with_authorities(
        radio: Radio,
        reclaim_authorities: Arc<Mutex<crate::ubm_reclaim_authority::ReclaimAuthorities>>,
        peers: Arc<PeerRegistry>,
    ) -> Result<Self> {
        let manager = Arc::new(AdapterManager::default());
        let binding = bind_radio(radio, &manager)?;
        Ok(Adapter {
            watcher: Arc::new(Mutex::new(BLEWatcher::new()?)),
            manager,
            reclaim_authorities,
            peers,
            radio: Arc::new(Mutex::new(Some(binding))),
            withheld: CentralState::Unknown,
            scan_ingress_fences: Arc::default(),
        })
    }

    /// UBM patch (`winrt-adapter-by-id`): an adapter whose radio the OS
    /// withholds (the legacy addon selected such an adapter without a
    /// radio when access was not granted). `adapter_state` answers `state`
    /// until [`Adapter::rebind_radio`] supplies a radio.
    pub fn without_radio(state: CentralState) -> Result<Self> {
        let (peers, reclaim_authorities) = super::peripheral::shared_peer_owners();
        Ok(Adapter {
            watcher: Arc::new(Mutex::new(BLEWatcher::new()?)),
            manager: Arc::new(AdapterManager::default()),
            reclaim_authorities,
            peers,
            radio: Arc::new(Mutex::new(None)),
            withheld: state,
            scan_ingress_fences: Arc::default(),
        })
    }

    /// UBM patch (`winrt-adapter-by-id`): report state from `radio` from now
    /// on (the adapter device came back after a removal, so the previous
    /// `Radio` object is stale), and emit its current state.
    pub fn rebind_radio(&self, radio: Radio) -> Result<()> {
        let state = get_central_state(&radio)?;
        let binding = bind_radio(radio, &self.manager)?;
        let previous = self
            .radio
            .lock()
            .map_err(Into::<Error>::into)?
            .replace(binding);
        drop(previous);
        self.manager.emit(CentralEvent::StateUpdate(state));
        Ok(())
    }
}

impl Adapter {
    /// UBM patch (UBM_PATCHES.md #5): every time this adapter's
    /// advertisement watcher stops, with the OS's `BluetoothError`.
    pub fn scan_stopped_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<super::ble::watcher::ScanStopped>> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        Ok(watcher.stopped_events())
    }

    /// Retire a watcher after WinRT has reached a terminal state but failed to
    /// deliver its `Stopped` callback.  The replacement has a distinct
    /// generation, so a callback already queued from the old watcher cannot
    /// terminate a later scan.
    pub fn recover_stalled_scan_watcher(
        &self,
        expected_generation: u64,
    ) -> Result<StalledScanWatcher> {
        let mut watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        Ok(match watcher.reset_after_terminal(expected_generation)? {
            ScanWatcherRecovery::Replaced { aborted: false, .. } => StalledScanWatcher::Replaced,
            ScanWatcherRecovery::Replaced { aborted: true, .. } => StalledScanWatcher::Aborted,
            ScanWatcherRecovery::StillStopping => StalledScanWatcher::StillStopping,
        })
    }

    pub fn scan_watcher_generation(&self) -> Result<u64> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        Ok(watcher.generation())
    }

    /// Arm the next logical scan before the watcher can receive a callback.
    /// The epoch comes from the shared central and is captured immutably by
    /// this scan's `Received` handler.
    pub fn arm_scan_ingress(&self, ingress_epoch: u64) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        watcher.arm_ingress_epoch(ingress_epoch)
    }

    /// Wait for every callback admitted for this scan before the central can
    /// release its slot. Queued manager events retain their old epoch.
    pub async fn fence_scan_ingress(&self, ingress_epoch: u64) -> Result<()> {
        let fence = self
            .watcher
            .lock()
            .map_err(Into::<Error>::into)?
            .fence_ingress_epoch(ingress_epoch);
        fence.await?;
        let (fence_id, mut reply, emit_marker) = self
            .scan_ingress_fences
            .lock()
            .map_err(Into::<Error>::into)?
            .join_or_register(ingress_epoch)?;
        if emit_marker {
            self.manager.emit(CentralEvent::ScanIngressFence {
                ingress_epoch,
                fence_id,
            });
        }
        loop {
            match *reply.borrow_and_update() {
                ScanIngressFenceReply::Acknowledged => return Ok(()),
                ScanIngressFenceReply::SourceClosed => {
                    return Err(ScanIngressFences::source_closed_error());
                }
                ScanIngressFenceReply::Pending => {}
            }
            if reply.changed().await.is_err() {
                return Err(Error::Other(
                    "WinRT scan-ingress fence acknowledgement channel closed".into(),
                ));
            }
        }
    }

    /// Release a fenced epoch so the next scan can arm a new handler.
    pub fn retire_scan_ingress(&self, ingress_epoch: u64) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        watcher.retire_ingress_epoch(ingress_epoch)
    }

    /// The manager event stream closed. Verify that WinRT has actually
    /// reached a terminal watcher state before closing callback admission;
    /// otherwise callers retain ownership and report the unresolved platform
    /// state instead of inventing a completed stop.
    pub async fn scan_ingress_source_closed(&self) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        watcher.ingress_source_closed()?;
        self.scan_ingress_fences
            .lock()
            .map_err(Into::<Error>::into)?
            .close();
        Ok(())
    }

    /// Complete an ordered scan-ingress fence after the desktop bridge has
    /// consumed its marker. Returning false means this marker no longer owns
    /// a pending flight (for example after source closure); cancellation of a
    /// waiter deliberately leaves its flight acknowledgeable for a retry.
    pub fn acknowledge_scan_ingress_fence(&self, fence_id: u64) -> bool {
        self.scan_ingress_fences
            .lock()
            .map(|mut fences| fences.acknowledge(fence_id))
            .unwrap_or(false)
    }
}

impl Debug for Adapter {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Adapter")
            .field("manager", &self.manager)
            .finish()
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        Ok(self.manager.event_stream())
    }

    async fn start_scan(&self, filter: ScanFilter) -> Result<()> {
        if filter
            .windows
            .is_some_and(|options| options.allow_extended_advertisements)
        {
            use windows::{
                Devices::Bluetooth::BluetoothAdapter, Foundation::Metadata::ApiInformation,
                core::HSTRING,
            };
            if !ApiInformation::IsPropertyPresent(
                &HSTRING::from("Windows.Devices.Bluetooth.BluetoothAdapter"),
                &HSTRING::from("IsExtendedAdvertisingSupported"),
            )? {
                return Err(Error::NotSupported(
                    "WinRT cannot establish extended-advertising adapter support on this runtime"
                        .to_owned(),
                ));
            }
            // The advertisement watcher is system-wide and takes no selected
            // adapter, so query the OS default adapter that owns that route.
            let adapter = BluetoothAdapter::GetDefaultAsync()?.await?;
            filter.windows.unwrap_or_default().validate_runtime(
                true,
                true,
                adapter.IsExtendedAdvertisingSupported()?,
            )?;
        }
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        let ingress_epoch = watcher.armed_ingress_epoch()?;
        let manager = self.manager.clone();
        let reclaim_authorities = self.reclaim_authorities.clone();
        let peers = self.peers.clone();
        watcher.start(
            filter,
            Box::new(move |args| {
                let bluetooth_address = args.BluetoothAddress()?;
                let address: BDAddr = bluetooth_address.try_into().unwrap();
                let kind = match args.BluetoothAddressType() {
                    Ok(windows::Devices::Bluetooth::BluetoothAddressType::Public) => {
                        Some(AddressType::Public)
                    }
                    Ok(windows::Devices::Bluetooth::BluetoothAddressType::Random) => {
                        Some(AddressType::Random)
                    }
                    result => {
                        manager.emit(CentralEvent::AdvertisementUnread {
                            id: PeripheralId::from(address),
                            detail: format!(
                                "WinRT advertisement address identity unavailable: {result:?}"
                            ),
                        });
                        return Ok(());
                    }
                };
                let identity = PeripheralId::with_address_type(address, kind);
                let candidate = match peers.get(
                    Arc::downgrade(&manager),
                    identity.clone(),
                    reclaim_authorities.clone(),
                ) {
                    Ok(peer) => peer,
                    Err(error) => {
                        manager.emit(CentralEvent::AdvertisementUnread {
                            id: identity,
                            detail: format!("WinRT peer owner unavailable: {error:?}"),
                        });
                        return Ok(());
                    }
                };
                let (peripheral, created) =
                    manager.peripheral_or_insert_with(identity.clone(), || candidate);
                peripheral.update_properties(args);
                manager.emit(if created {
                    CentralEvent::DeviceDiscovered(identity.clone())
                } else {
                    CentralEvent::DeviceUpdated(identity.clone())
                });
                // UBM patch (UBM_PATCHES.md #17): every received
                // advertisement, with its own data; an unreadable one is
                // reported, never dropped.
                manager.emit(match Peripheral::advertisement_report(args, kind) {
                    Ok(report) => CentralEvent::AdvertisementScoped {
                        id: identity.clone(),
                        report,
                        ingress_epoch,
                    },
                    Err(error) => CentralEvent::AdvertisementUnread {
                        id: identity,
                        detail: format!("{error:?}"),
                    },
                });
                Ok(())
            }),
        )
    }

    async fn stop_scan(&self) -> Result<()> {
        let watcher = self.watcher.lock().map_err(Into::<Error>::into)?;
        watcher.stop()?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.manager.peripherals())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
    }

    /// UBM patch (UBM_PATCHES.md #19, finding 127): the peripheral at this
    /// address, known or registered now. The legacy WinRT addon connected by
    /// address (`BluetoothLEDevice::FromBluetoothAddressAsync`,
    /// `winrt-boundary.inc:868`), which `connect` does here too, so no scan
    /// is needed first.
    async fn add_peripheral(&self, address: &PeripheralId) -> Result<Peripheral> {
        // An untyped address cannot be matched to an existing public or random
        // owner by its bits alone. Resolve it before admitting any owner.
        let identity = if address.address_type().is_some() {
            address.clone()
        } else {
            let raw = address.address();
            super::marker_executor::spawn_owned(async move {
                let operation =
                    windows::Devices::Bluetooth::BluetoothLEDevice::FromBluetoothAddressAsync(
                        raw.into(),
                    )?;
                let device = super::ble::terminal_result(operation).await?;
                resolved_peer_identity(raw, device.BluetoothAddressType()?)
            })
            .map_err(|error| Error::Other(error.into()))?
            .await
            .map_err(|error| {
                Error::Other(format!("WinRT address resolution task failed: {error}").into())
            })??
        };
        cache_resolved_peripheral(
            &self.manager,
            &self.peers,
            &self.reclaim_authorities,
            identity,
        )
    }

    async fn clear_peripherals(&self) -> Result<()> {
        self.manager.clear_peripherals();
        Ok(())
    }

    async fn adapter_info(&self) -> Result<String> {
        // TODO: Get information about the adapter.
        Ok("WinRT".to_string())
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let radio = self.radio.lock().map_err(Into::<Error>::into)?;
        match radio.as_ref() {
            Some(binding) => get_central_state(&binding.radio),
            None => Ok(self.withheld.clone()),
        }
    }
}

#[cfg(test)]
mod adapter_cache_tests {
    use super::*;
    use crate::api::Peripheral as _;
    use futures::StreamExt;

    #[test]
    fn resolved_admission_uses_the_same_keys_as_adapter_lookup_and_scanning() {
        let address = BDAddr::from([1, 2, 3, 4, 5, 6]);
        let unknown = PeripheralId::from(address);
        let public = PeripheralId::with_address_type(address, Some(AddressType::Public));
        let random = PeripheralId::with_address_type(address, Some(AddressType::Random));
        for (first, second) in [
            (public.clone(), random.clone()),
            (random.clone(), public.clone()),
        ] {
            for typed_first in [false, true] {
                let manager = Arc::new(AdapterManager::<Peripheral>::default());
                let peers = PeerRegistry::default();
                let authorities = Arc::new(Mutex::new(Default::default()));
                let admit = |_requested: &PeripheralId, resolved: PeripheralId| {
                    cache_resolved_peripheral(&manager, &peers, &authorities, resolved).unwrap()
                };
                if typed_first {
                    // Match the advertisement path: registry first, then the
                    // adapter manager's typed-key insertion.
                    let candidate = peers
                        .get(Arc::downgrade(&manager), first.clone(), authorities.clone())
                        .unwrap();
                    manager.peripheral_or_insert_with(first.clone(), || candidate);
                }
                let native_identity = |id: &PeripheralId| {
                    let kind = match id.address_type().unwrap() {
                        AddressType::Public => {
                            windows::Devices::Bluetooth::BluetoothAddressType::Public
                        }
                        AddressType::Random => {
                            windows::Devices::Bluetooth::BluetoothAddressType::Random
                        }
                    };
                    resolved_peer_identity(address, kind).unwrap()
                };
                let initial = admit(&unknown, native_identity(&first));
                assert_eq!(initial.id(), first);
                // This is the same lookup used by Central::peripheral.
                assert_eq!(
                    manager.peripheral(&initial.id()).unwrap().id(),
                    initial.id()
                );
                admit(&first, first.clone());
                assert_eq!(manager.peripherals().len(), 1);
                let changed = admit(&unknown, native_identity(&second));
                assert_eq!(changed.id(), second);
                assert_eq!(manager.peripheral(&changed.id()).unwrap().id(), second);
                assert_eq!(manager.peripheral(&first).unwrap().id(), first);
                assert_eq!(manager.peripherals().len(), 2);
                assert!(manager.peripheral(&unknown).is_none());
            }
        }
    }

    #[test]
    fn scan_ingress_fence_waits_for_its_matching_bridge_acknowledgement() {
        let mut fences = ScanIngressFences::default();
        let (fence_id, mut reply, marker) = fences.join_or_register(41).expect("register fence");
        assert!(marker, "the first caller emits the marker");
        assert!(
            !fences.acknowledge(fence_id + 1),
            "a later or foreign marker cannot acknowledge this fence"
        );
        assert!(fences.acknowledge(fence_id));
        assert_eq!(
            *reply.borrow_and_update(),
            ScanIngressFenceReply::Acknowledged
        );
    }

    #[test]
    fn source_closure_fails_an_unconsumed_scan_ingress_marker() {
        let mut fences = ScanIngressFences::default();
        let (_, mut reply, _) = fences.join_or_register(41).expect("register fence");
        fences.close();
        assert_eq!(
            *reply.borrow_and_update(),
            ScanIngressFenceReply::SourceClosed
        );
    }

    #[test]
    fn cancelled_fence_waiter_retries_the_one_pending_epoch_marker() {
        let mut fences = ScanIngressFences::default();
        let (fence_id, cancelled_waiter, marker) =
            fences.join_or_register(41).expect("first fence flight");
        assert!(marker);
        drop(cancelled_waiter);

        let (retry_id, mut retry, retry_emits_marker) = fences
            .join_or_register(41)
            .expect("retry joins first flight");
        assert_eq!(retry_id, fence_id);
        assert!(
            !retry_emits_marker,
            "a retry after cancellation cannot queue a duplicate marker"
        );
        assert_eq!(
            fences.pending_count(),
            1,
            "one epoch owns one marker flight"
        );
        assert!(fences.acknowledge(fence_id));
        assert_eq!(
            *retry.borrow_and_update(),
            ScanIngressFenceReply::Acknowledged
        );
        assert_eq!(
            fences.pending_count(),
            0,
            "acknowledgement retires the flight"
        );
    }

    #[tokio::test]
    async fn cancelled_fence_retry_reuses_the_marker_held_by_the_event_consumer() {
        let adapter = Adapter::without_radio(CentralState::Unknown).expect("adapter");
        let mut events = adapter.events().await.expect("event stream");
        adapter.arm_scan_ingress(41).expect("arm epoch");
        adapter.retire_scan_ingress(41).expect("retire epoch");

        let cancelled = {
            let adapter = adapter.clone();
            tokio::spawn(async move { adapter.fence_scan_ingress(41).await })
        };
        let Some(CentralEvent::ScanIngressFence {
            ingress_epoch: 41,
            fence_id,
        }) = events.next().await
        else {
            panic!("first fence must publish its ordered marker");
        };
        cancelled.abort();
        assert!(
            cancelled
                .await
                .expect_err("fence waiter was cancelled")
                .is_cancelled()
        );

        let retry = {
            let adapter = adapter.clone();
            tokio::spawn(async move { adapter.fence_scan_ingress(41).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), events.next())
                .await
                .is_err(),
            "a retry joins the held marker instead of publishing a duplicate"
        );
        assert!(adapter.acknowledge_scan_ingress_fence(fence_id));
        assert!(retry.await.expect("retry task").is_ok());
    }
}
