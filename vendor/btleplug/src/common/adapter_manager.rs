/// Implements common functionality for adapters across platforms.
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
use crate::api::{CentralEvent, Peripheral};
use crate::platform::PeripheralId;
use dashmap::{DashMap, mapref::one::RefMut};
use futures::stream::{Stream, StreamExt};
use log::trace;
use std::pin::Pin;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug)]
pub struct AdapterManager<PeripheralType>
where
    PeripheralType: Peripheral,
{
    peripherals: DashMap<PeripheralId, PeripheralType>,
    events_channel: broadcast::Sender<CentralEvent>,
}

impl<PeripheralType: Peripheral + 'static> Default for AdapterManager<PeripheralType> {
    fn default() -> Self {
        let (broadcast_sender, _) = broadcast::channel(crate::ubm::EVENT_CAPACITY);
        AdapterManager {
            peripherals: DashMap::new(),
            events_channel: broadcast_sender,
        }
    }
}

impl<PeripheralType> AdapterManager<PeripheralType>
where
    PeripheralType: Peripheral + 'static,
{
    pub fn emit(&self, event: CentralEvent) {
        // UBM patch (UBM_PATCHES.md #19, finding 127): a disconnected
        // peripheral stays known (upstream removed it, so a reconnect failed
        // until a new scan saw the device again); the legacy backends kept
        // every peripheral they had seen.
        if let Err(lost) = self.events_channel.send(event) {
            trace!("Lost central event, while nothing subscribed: {:?}", lost);
        }
    }

    pub fn event_stream(&self) -> Pin<Box<dyn Stream<Item = CentralEvent> + Send>> {
        let receiver = self.events_channel.subscribe();
        // UBM patch (UBM_PATCHES.md #10): a lagging receiver is told how
        // many events it lost; upstream filtered the lag out.
        Box::pin(BroadcastStream::new(receiver).map(|item| match item {
            Ok(event) => event,
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(skipped)) => {
                CentralEvent::EventsLost { skipped }
            }
        }))
    }

    // Only used on Windows since UBM patch #19 (Apple replaces instead).
    #[allow(dead_code)]
    pub fn add_peripheral(&self, peripheral: PeripheralType) {
        assert!(
            !self.peripherals.contains_key(&peripheral.id()),
            "Adding a peripheral that's already in the map."
        );
        self.peripherals.insert(peripheral.id(), peripheral);
    }

    /// UBM patch (UBM_PATCHES.md #19): a peripheral the OS resolved again
    /// (CoreBluetooth `retrievePeripheralsWithIdentifiers` after a reset)
    /// replaces the entry it supersedes.
    #[allow(dead_code)]
    pub fn replace_peripheral(&self, peripheral: PeripheralType) {
        self.peripherals.insert(peripheral.id(), peripheral);
    }

    pub fn clear_peripherals(&self) {
        self.peripherals.clear();
    }

    pub fn peripherals(&self) -> Vec<PeripheralType> {
        self.peripherals
            .iter()
            .map(|val| val.value().clone())
            .collect()
    }

    // Only used on windows and macOS/iOS, so turn off deadcode so we don't get warnings on android/linux.
    #[allow(dead_code)]
    pub fn peripheral_mut(
        &self,
        id: &PeripheralId,
    ) -> Option<RefMut<'_, PeripheralId, PeripheralType>> {
        self.peripherals.get_mut(id)
    }

    pub fn peripheral(&self, id: &PeripheralId) -> Option<PeripheralType> {
        self.peripherals.get(id).map(|val| val.value().clone())
    }
}

#[cfg(test)]
mod ubm_lag_tests {
    use super::AdapterManager;
    use crate::api::{CentralEvent, CentralState};
    use crate::platform::Peripheral;
    use futures::stream::StreamExt;

    #[test]
    fn a_lagging_event_receiver_is_told_what_it_lost() {
        let manager = AdapterManager::<Peripheral>::default();
        let mut events = manager.event_stream();
        // The broadcast holds EVENT_CAPACITY events (patch 13); four more
        // arrive before the reader reads.
        assert!(crate::ubm::EVENT_CAPACITY >= 256, "never below legacy");
        for _ in 0..crate::ubm::EVENT_CAPACITY + 4 {
            manager.emit(CentralEvent::StateUpdate(CentralState::PoweredOn));
        }
        let first = futures::executor::block_on(events.next()).expect("an event");
        assert!(
            matches!(first, CentralEvent::EventsLost { skipped: 4 }),
            "the four overwritten events are reported first, saw {first:?}"
        );
        let next = futures::executor::block_on(events.next()).expect("an event");
        assert!(matches!(
            next,
            CentralEvent::StateUpdate(CentralState::PoweredOn)
        ));
    }

    /// UBM patch #19 (finding 127): a disconnected peripheral stays known,
    /// so a reconnect finds it without a new scan, as the legacy backends
    /// did; a peripheral the OS re-resolves replaces the old entry instead
    /// of panicking.
    #[cfg(target_vendor = "apple")]
    #[tokio::test]
    async fn a_disconnected_peripheral_stays_known() {
        use crate::api::Peripheral as _;
        use std::sync::Arc;
        let manager = Arc::new(AdapterManager::<Peripheral>::default());
        let uuid = uuid::Uuid::from_u128(0x1234);
        let make = || {
            let (_events, event_receiver) = futures::channel::mpsc::channel(1);
            let (message_sender, _messages) = futures::channel::mpsc::channel(1);
            Peripheral::new(
                uuid,
                None,
                None,
                Arc::downgrade(&manager),
                event_receiver,
                message_sender,
            )
        };
        manager.add_peripheral(make());
        let id = make().id();
        manager.emit(CentralEvent::DeviceDisconnected(id.clone()));
        assert!(
            manager.peripheral(&id).is_some(),
            "still known after a disconnect"
        );
        manager.replace_peripheral(make());
        assert_eq!(manager.peripherals().len(), 1, "replaced, not duplicated");
    }
}
