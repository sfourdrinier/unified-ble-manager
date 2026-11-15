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

use crate::{
    Error, Result,
    api::{ScanFilter, WindowsScanOptions, WindowsScanningMode},
};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use windows::{Devices::Bluetooth::Advertisement::*, Foundation::TypedEventHandler, core::Ref};

pub type AdvertisementEventHandler =
    Box<dyn Fn(&BluetoothLEAdvertisementReceivedEventArgs) -> windows::core::Result<()> + Send>;

/// UBM patch (UBM_PATCHES.md #5): the watcher stopped. `error` is the raw
/// `BluetoothError` (0 = `Success`, e.g. an explicit stop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanStopped {
    pub error: i32,
    pub error_name: String,
    pub generation: u64,
}

/// Result of attempting to retire a watcher after its `Stopped` callback was
/// lost. A nonterminal watcher must remain owned by its existing lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanWatcherRecovery {
    Replaced { generation: u64, aborted: bool },
    StillStopping,
}

/// The central-owned epoch a WinRT `Received` handler was armed for. A
/// handler captures it when registered; it must never read a later scan's
/// mutable epoch at callback time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanIngressEpochState {
    Idle,
    Armed(u64),
    Retiring(u64),
}

impl Default for ScanIngressEpochState {
    fn default() -> Self {
        Self::Idle
    }
}

/// Serializes WinRT callback admission with the cross-platform scan fence.
/// Retirement closes admission without waiting; fencing later drains exactly
/// the callbacks already admitted under the old epoch. Already-emitted
/// manager events retain that epoch, so the shared central can reject them
/// after the next scan starts.
#[derive(Default)]
struct ScanIngressEpochGate {
    state: Mutex<ScanIngressEpochGateState>,
    drained: Notify,
    #[cfg(test)]
    after_fence_condition: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for ScanIngressEpochGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScanIngressEpochGate")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct ScanIngressEpochGateState {
    state: ScanIngressEpochState,
    admitted_callbacks: usize,
    retired_through: u64,
}

struct ScanIngressEpochPermit {
    gate: Arc<ScanIngressEpochGate>,
}

impl Drop for ScanIngressEpochPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.admitted_callbacks = state
            .admitted_callbacks
            .checked_sub(1)
            .expect("accepted WinRT scan callback count underflow");
        if state.admitted_callbacks == 0 {
            self.gate.drained.notify_waiters();
        }
    }
}

impl ScanIngressEpochGate {
    fn lock_error() -> Error {
        Error::Other("watcher scan ingress epoch lock poisoned".into())
    }

    fn arm(&self, epoch: u64) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Self::lock_error())?;
        match state.state {
            ScanIngressEpochState::Idle if epoch > state.retired_through => {
                state.state = ScanIngressEpochState::Armed(epoch);
                Ok(())
            }
            current => Err(Error::Other(
                format!(
                    "cannot arm WinRT scan ingress epoch {epoch} while {current:?} remains owned"
                )
                .into(),
            )),
        }
    }

    fn armed_epoch(&self) -> Result<u64> {
        match self.state.lock().map_err(|_| Self::lock_error())?.state {
            ScanIngressEpochState::Armed(epoch) => Ok(epoch),
            current => Err(Error::Other(
                format!("cannot start WinRT scan without an armed ingress epoch ({current:?})")
                    .into(),
            )),
        }
    }

    fn enter(self: &Arc<Self>, epoch: u64) -> Result<Option<ScanIngressEpochPermit>> {
        let mut state = self.state.lock().map_err(|_| Self::lock_error())?;
        if matches!(state.state, ScanIngressEpochState::Armed(active) if active == epoch) {
            state.admitted_callbacks =
                state.admitted_callbacks.checked_add(1).ok_or_else(|| {
                    Error::Other("accepted WinRT scan callback count overflow".into())
                })?;
            Ok(Some(ScanIngressEpochPermit {
                gate: Arc::clone(self),
            }))
        } else {
            Ok(None)
        }
    }

    async fn fence(&self, epoch: u64) -> Result<()> {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            // `Notify` only records a wake for a waiter once the waiter is
            // enabled. A callback can complete between the state check below
            // and `.await`; enabling first keeps that completion from being
            // lost and leaving the scan owner hung.
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().map_err(|_| Self::lock_error())?;
                if epoch <= state.retired_through {
                    return Ok(());
                }
                match state.state {
                    ScanIngressEpochState::Retiring(active)
                        if active == epoch && state.admitted_callbacks == 0 =>
                    {
                        state.state = ScanIngressEpochState::Idle;
                        state.retired_through = epoch;
                        return Ok(());
                    }
                    ScanIngressEpochState::Retiring(active) if active == epoch => {}
                    current => {
                        return Err(Error::Other(
                            format!(
                                "cannot fence WinRT scan ingress epoch {epoch} while {current:?} is active"
                            )
                            .into(),
                        ));
                    }
                }
            }
            #[cfg(test)]
            if let Some(after_condition) = self
                .after_fence_condition
                .lock()
                .map_err(|_| Self::lock_error())?
                .take()
            {
                after_condition();
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn run_after_next_fence_condition(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self
            .after_fence_condition
            .lock()
            .expect("fence test hook lock") = Some(callback);
    }

    fn retire(&self, epoch: u64) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Self::lock_error())?;
        if epoch <= state.retired_through {
            return Ok(());
        }
        match state.state {
            ScanIngressEpochState::Armed(active) if active == epoch => {
                state.state = ScanIngressEpochState::Retiring(epoch);
                Ok(())
            }
            ScanIngressEpochState::Retiring(active) if active == epoch => Ok(()),
            current => Err(Error::Other(
                format!(
                    "cannot retire WinRT scan ingress epoch {epoch} while {current:?} is active"
                )
                .into(),
            )),
        }
    }

    /// The adapter's event stream has ended.  It can no longer carry a
    /// callback into the shared boundary, so close admission for whichever
    /// scan was current.  The central still performs the normal
    /// retire-and-fence sequence: this method deliberately does not claim
    /// that the callbacks admitted before closure have drained.
    fn source_closed(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Self::lock_error())?;
        if let ScanIngressEpochState::Armed(epoch) = state.state {
            state.state = ScanIngressEpochState::Retiring(epoch);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct BLEWatcher {
    watcher: BluetoothLEAdvertisementWatcher,
    stopped_token: i64,
    generation: u64,
    // UBM patch (UBM_PATCHES.md #5): the `Received` registration of the
    // current scan, removed on stop and before the next start so handlers
    // never accumulate across scans (upstream re-registers on every start,
    // duplicating every advertisement after the first scan).
    received: std::sync::Mutex<Option<i64>>,
    ingress: Arc<ScanIngressEpochGate>,
    stopped: tokio::sync::broadcast::Sender<ScanStopped>,
}

impl From<windows::core::Error> for Error {
    /// UBM patch (UBM_PATCHES.md #15): the HRESULT as the platform's
    /// answer, the legacy addon's `{code:"hresult", hresult}` identity.
    fn from(err: windows::core::Error) -> Error {
        Error::Platform(
            crate::PlatformError::new("winrt", "hresult", err.message().to_string()).with(
                "hresult",
                crate::winrtble::gatt_model::hresult_code(err.code().0),
            ),
        )
    }
}

fn prepare_then_retire<T, E>(
    prepare: impl FnOnce() -> std::result::Result<T, E>,
    retire: impl FnOnce() -> std::result::Result<(), E>,
) -> std::result::Result<T, E> {
    let prepared = prepare()?;
    retire()?;
    Ok(prepared)
}

fn watcher_status_is_terminal(status: BluetoothLEAdvertisementWatcherStatus) -> bool {
    matches!(
        status,
        BluetoothLEAdvertisementWatcherStatus::Stopped
            | BluetoothLEAdvertisementWatcherStatus::Aborted
    )
}

impl BLEWatcher {
    fn watcher_with_stopped_handler(
        stopped: &tokio::sync::broadcast::Sender<ScanStopped>,
        generation: u64,
    ) -> Result<(BluetoothLEAdvertisementWatcher, i64)> {
        let ad = BluetoothLEAdvertisementFilter::new()?;
        let watcher = BluetoothLEAdvertisementWatcher::Create(&ad)?;
        let sender = stopped.clone();
        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementWatcherStoppedEventArgs>| {
                if let Ok(args) = args.ok() {
                    let error = args.Error()?;
                    let _ = sender.send(ScanStopped {
                        error: error.0,
                        error_name: format!("{error:?}"),
                        generation,
                    });
                }
                Ok(())
            },
        );
        let token = watcher.Stopped(&handler)?;
        Ok((watcher, token))
    }

    pub fn new() -> Result<Self> {
        let (stopped, _) = tokio::sync::broadcast::channel(16);
        let (watcher, stopped_token) = Self::watcher_with_stopped_handler(&stopped, 1)?;
        Ok(BLEWatcher {
            watcher,
            stopped_token,
            generation: 1,
            received: std::sync::Mutex::new(None),
            ingress: Arc::new(ScanIngressEpochGate::default()),
            stopped,
        })
    }

    /// UBM patch (UBM_PATCHES.md #5): every time the watcher stops.
    pub fn stopped_events(&self) -> tokio::sync::broadcast::Receiver<ScanStopped> {
        self.stopped.subscribe()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn arm_ingress_epoch(&self, epoch: u64) -> Result<()> {
        self.ingress.arm(epoch)
    }

    pub fn armed_ingress_epoch(&self) -> Result<u64> {
        self.ingress.armed_epoch()
    }

    pub fn fence_ingress_epoch(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let ingress = Arc::clone(&self.ingress);
        async move { ingress.fence(epoch).await }
    }

    pub fn retire_ingress_epoch(&self, epoch: u64) -> Result<()> {
        self.ingress.retire(epoch)
    }

    /// Close callback admission after the adapter event stream itself has
    /// ended. The stream is only authoritative once the native watcher also
    /// says it is terminal; an ended consumer stream must not release a
    /// watcher that WinRT is still starting, scanning, or stopping.
    pub fn ingress_source_closed(&self) -> Result<()> {
        let status = self.watcher.Status()?;
        if !watcher_status_is_terminal(status) {
            return Err(Error::Other(
                format!("WinRT advertisement event source ended while watcher remains {status:?}")
                    .into(),
            ));
        }
        self.ingress.source_closed()
    }

    /// Replace a terminal watcher whose Stopped callback could not be
    /// delivered. The replacement handler is registered before retiring the
    /// old one; tagging generations makes queued old callbacks
    /// distinguishable from the replacement.
    pub fn reset_after_terminal(
        &mut self,
        expected_generation: u64,
    ) -> Result<ScanWatcherRecovery> {
        if self.generation != expected_generation {
            return Ok(ScanWatcherRecovery::StillStopping);
        }
        let status = self.watcher.Status()?;
        if !watcher_status_is_terminal(status) {
            return Ok(ScanWatcherRecovery::StillStopping);
        }
        let aborted = status == BluetoothLEAdvertisementWatcherStatus::Aborted;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::Other("watcher generation overflow".into()))?;
        let (watcher, stopped_token) = prepare_then_retire(
            || Self::watcher_with_stopped_handler(&self.stopped, generation),
            || {
                self.remove_received()?;
                self.watcher.RemoveStopped(self.stopped_token)?;
                Ok(())
            },
        )?;
        self.watcher = watcher;
        self.stopped_token = stopped_token;
        self.generation = generation;
        Ok(ScanWatcherRecovery::Replaced {
            generation,
            aborted,
        })
    }

    fn remove_received(&self) -> Result<()> {
        let mut token = self
            .received
            .lock()
            .map_err(|_| Error::Other("watcher registration lock poisoned".into()))?;
        if let Some(value) = *token {
            self.watcher.RemoveReceived(value)?;
            *token = None;
        }
        Ok(())
    }

    /// Prepare the watcher for one scan.
    ///
    /// UBM patch (UBM_PATCHES.md #11): default active scanning, without extended
    /// advertisements. BlueZ and CoreBluetooth both return the complete
    /// local name from the scan response. A passive WinRT watcher never
    /// sends `SCAN_REQ`, so that name is absent and a name-selected peer
    /// cannot be chosen. Explicit Windows options select active, passive or
    /// versioned None reception and opt into extended advertisements; absent
    /// options restore the active/nonextended defaults for every scan.
    ///
    /// UBM patch (UBM_PATCHES.md #16): the OS service-UUID filter stays
    /// empty. A scan response carries the complete local name and does not
    /// repeat the service UUIDs. Windows delivers that packet to `Received`
    /// only when `ServiceUuids` is empty; a non-empty filter keeps the
    /// advertising packet (empty name) and drops the scan response. BlueZ
    /// and CoreBluetooth both surface the name, so the OS filter cannot be
    /// used here. The caller's services are matched in software instead.
    fn configure_for_scan(&self, options: WindowsScanOptions) -> Result<()> {
        use windows::{Foundation::Metadata::ApiInformation, core::HSTRING};
        let none_present = options.mode != WindowsScanningMode::None
            || ApiInformation::IsEnumNamedValuePresent(
                &HSTRING::from("Windows.Devices.Bluetooth.Advertisement.BluetoothLEScanningMode"),
                &HSTRING::from("None"),
            )?;
        let extended_present = ApiInformation::IsPropertyPresent(
            &HSTRING::from(
                "Windows.Devices.Bluetooth.Advertisement.BluetoothLEAdvertisementWatcher",
            ),
            &HSTRING::from("AllowExtendedAdvertisements"),
        )?;
        // Adapter support was admitted by Adapter::start_scan before this
        // synchronous watcher configuration; both APIs still precede effects.
        options.validate_runtime(none_present, extended_present, true)?;
        let ad = self.watcher.AdvertisementFilter()?.Advertisement()?;
        ad.ServiceUuids()?.Clear()?;
        self.watcher.SetScanningMode(match options.mode {
            WindowsScanningMode::Active => BluetoothLEScanningMode::Active,
            WindowsScanningMode::Passive => BluetoothLEScanningMode::Passive,
            WindowsScanningMode::None => BluetoothLEScanningMode::None,
        })?;
        if extended_present {
            self.watcher
                .SetAllowExtendedAdvertisements(options.allow_extended_advertisements)?;
        }
        Ok(())
    }

    pub fn start(&self, filter: ScanFilter, on_received: AdvertisementEventHandler) -> Result<()> {
        // Service/name conjunctions may span several packets in either order.
        // Native ingress cannot decide which partial packet will be needed by
        // the bounded, generation-scoped shared evidence matcher. Keep all
        // packets here and leave that decision to the canonical matcher.
        self.configure_for_scan(filter.windows.unwrap_or_default())?;
        let ingress_epoch = self.ingress.armed_epoch()?;
        let ingress = Arc::clone(&self.ingress);

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Ok(args) = args.ok() {
                    if let Some(_permit) = ingress.enter(ingress_epoch).map_err(|error| {
                        windows::core::Error::new(
                            windows::core::HRESULT(0x8000_4005_u32 as i32),
                            error.to_string(),
                        )
                    })? {
                        on_received(args)?;
                    }
                }
                Ok(())
            },
        );

        self.remove_received()?;
        let token = self.watcher.Received(&handler)?;
        *self
            .received
            .lock()
            .map_err(|_| Error::Other("watcher registration lock poisoned".into()))? = Some(token);
        self.watcher.Start()?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.watcher.Stop()?;
        self.remove_received()
    }
}

#[cfg(test)]
mod ubm_scan_mode_tests {
    use super::*;
    use crate::winrtble::utils;

    #[test]
    fn replacement_prepare_failure_never_retires_the_old_callback_route() {
        let retired = std::sync::atomic::AtomicBool::new(false);
        let result: std::result::Result<(), &str> = prepare_then_retire(
            || Err("replacement Stopped registration failed"),
            || {
                retired.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(result, Err("replacement Stopped registration failed"));
        assert!(
            !retired.load(std::sync::atomic::Ordering::SeqCst),
            "the current watcher remains subscribed when replacement creation fails"
        );
    }

    /// UBM patch #11: every scan requests scan responses, and extended
    /// advertisements stay off. The complete local name of a legacy
    /// advertiser arrives in the scan response.
    #[test]
    fn a_scan_is_active_without_extended_advertisements() {
        let watcher = BLEWatcher::new().expect("watcher");
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert_eq!(
            watcher.watcher.ScanningMode().expect("mode"),
            BluetoothLEScanningMode::Active
        );
        assert!(
            !watcher
                .watcher
                .AllowExtendedAdvertisements()
                .expect("extended advertisements")
        );
    }

    /// UBM patch #16: requested service UUIDs stay off the OS filter. A
    /// non-empty OS filter drops scan responses, and that is the packet
    /// that carries the local name.
    #[test]
    fn the_service_filter_stays_off_the_os_watcher() {
        let heart_rate = utils::to_guid(&uuid::Uuid::from_u128(
            0x0000180d_0000_1000_8000_00805f9b34fb,
        ));
        let battery = utils::to_guid(&uuid::Uuid::from_u128(
            0x0000180f_0000_1000_8000_00805f9b34fb,
        ));
        let watcher = BLEWatcher::new().expect("watcher");
        let read_back = |watcher: &BLEWatcher| -> Vec<windows::core::GUID> {
            let uuids = watcher
                .watcher
                .AdvertisementFilter()
                .and_then(|filter| filter.Advertisement())
                .and_then(|ad| ad.ServiceUuids())
                .expect("filter");
            (0..uuids.Size().expect("size"))
                .map(|index| uuids.GetAt(index).expect("uuid"))
                .collect()
        };
        // Seed a stale native filter to prove configuration clears it.
        let uuids = watcher
            .watcher
            .AdvertisementFilter()
            .unwrap()
            .Advertisement()
            .unwrap()
            .ServiceUuids()
            .unwrap();
        uuids.Append(heart_rate).unwrap();
        uuids.Append(battery).unwrap();
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a service UUID filter drops scan responses, so it stays off the OS watcher"
        );
        watcher
            .configure_for_scan(WindowsScanOptions::default())
            .expect("configure");
        assert!(
            read_back(&watcher).is_empty(),
            "a later scan leaves the OS filter empty"
        );
    }

    #[test]
    fn ingress_epoch_fence_waits_for_an_admitted_callback_before_replacement() {
        let gate = std::sync::Arc::new(ScanIngressEpochGate::default());
        gate.arm(7).expect("arm scan A");
        let admitted = gate
            .enter(7)
            .expect("enter scan A")
            .expect("scan A callback accepted");

        gate.retire(7).expect("retire scan A before its fence");
        assert!(
            gate.enter(7).expect("late scan A callback").is_none(),
            "retirement closes callback admission before it drains the accepted prefix"
        );
        let (fenced, fence_done) = std::sync::mpsc::channel();
        let fence_gate = std::sync::Arc::clone(&gate);
        let fence = std::thread::spawn(move || {
            futures::executor::block_on(fence_gate.fence(7)).expect("fence scan A");
            fenced.send(()).expect("fence completion receiver");
        });
        assert!(
            matches!(
                fence_done.recv_timeout(std::time::Duration::from_millis(20)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the fence cannot complete while scan A's admitted callback holds the gate"
        );
        drop(admitted);
        fence_done
            .recv()
            .expect("fence completes after callback exits");
        fence.join().expect("fence thread");
        gate.arm(8).expect("arm scan B");
        futures::executor::block_on(gate.fence(7))
            .expect("a lost fence reply may retry the retired scan A epoch");
        assert!(
            gate.enter(7)
                .expect("old callback after scan B starts")
                .is_none(),
            "scan A never becomes scan B by reading mutable current state"
        );
        assert!(gate.enter(8).expect("scan B callback").is_some());
    }

    #[test]
    fn source_closure_closes_admission_but_leaves_the_accepted_prefix_for_the_fence() {
        let gate = std::sync::Arc::new(ScanIngressEpochGate::default());
        gate.arm(7).expect("arm scan A");
        let admitted = gate
            .enter(7)
            .expect("enter scan A")
            .expect("scan A callback accepted");

        gate.source_closed().expect("close source admission");
        assert!(
            gate.enter(7).expect("late callback").is_none(),
            "source closure cannot admit a callback after the stream disappears"
        );
        let (fenced, fence_done) = std::sync::mpsc::channel();
        let fence_gate = std::sync::Arc::clone(&gate);
        let fence = std::thread::spawn(move || {
            futures::executor::block_on(fence_gate.fence(7)).expect("fence scan A");
            fenced.send(()).expect("fence completion receiver");
        });
        assert!(matches!(
            fence_done.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(admitted);
        fence_done.recv().expect("accepted prefix drains");
        fence.join().expect("fence thread");
    }

    #[test]
    fn fence_cannot_miss_a_callback_completion_after_its_state_check() {
        let gate = std::sync::Arc::new(ScanIngressEpochGate::default());
        gate.arm(7).expect("arm scan A");
        let admitted = gate
            .enter(7)
            .expect("enter scan A")
            .expect("scan A callback accepted");
        gate.retire(7).expect("retire scan A");

        let held_callback = std::sync::Arc::new(std::sync::Mutex::new(Some(admitted)));
        let release_after_check = {
            let held_callback = std::sync::Arc::clone(&held_callback);
            std::sync::Arc::new(move || {
                drop(
                    held_callback
                        .lock()
                        .expect("held callback lock")
                        .take()
                        .expect("callback remains admitted through the condition check"),
                );
            })
        };
        gate.run_after_next_fence_condition(release_after_check);

        // The hook drops the last permit after `fence` has observed a
        // non-empty accepted prefix and before it awaits. This completes only
        // when the waiter was enabled before that observation.
        futures::executor::block_on(gate.fence(7)).expect("fence observes the wake");
    }

    #[test]
    fn only_terminal_watcher_statuses_authorize_event_source_closure() {
        assert!(watcher_status_is_terminal(
            BluetoothLEAdvertisementWatcherStatus::Stopped
        ));
        assert!(watcher_status_is_terminal(
            BluetoothLEAdvertisementWatcherStatus::Aborted
        ));
        for status in [
            BluetoothLEAdvertisementWatcherStatus::Created,
            BluetoothLEAdvertisementWatcherStatus::Started,
            BluetoothLEAdvertisementWatcherStatus::Stopping,
        ] {
            assert!(
                !watcher_status_is_terminal(status),
                "a source close while {status:?} cannot release callback ownership"
            );
        }
    }
}
