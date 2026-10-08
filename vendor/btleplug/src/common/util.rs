// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use crate::api::ValueNotification;
use futures::future::ready;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::broadcast::Receiver;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// UBM patch (UBM_PATCHES.md #10): a receiver that fell behind the bounded
/// broadcast learns how many notifications it missed on the next one it
/// gets (`ValueNotification::lost_before`). Upstream filtered the lag out.
pub fn notifications_stream_from_broadcast_receiver(
    receiver: Receiver<ValueNotification>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    Box::pin(
        BroadcastStream::new(receiver)
            .scan(0u64, |missed, item| {
                ready(Some(match item {
                    Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                        *missed = missed.saturating_add(skipped);
                        None
                    }
                    Ok(mut notification) => {
                        notification.lost_before = notification
                            .lost_before
                            .saturating_add(std::mem::take(missed));
                        Some(notification)
                    }
                }))
            })
            .filter_map(ready),
    )
}

/// Private native ordering envelope: the public notification contract is unchanged.
#[derive(Clone, Debug)]
pub struct NotificationEnvelope {
    sequence: u64,
    notification: ValueNotification,
}

type NotificationKey = (uuid::Uuid, u64, uuid::Uuid, u64);

#[derive(Default)]
struct FaultState {
    sequence: u64,
    faults: std::collections::BTreeMap<NotificationKey, NotificationEnvelope>,
    quota: Option<NotificationEnvelope>,
    enable_epochs: std::collections::BTreeMap<NotificationKey, u64>,
}

/// Terminal callback evidence has a separate bounded lifetime from lossy data.
/// Only confirmed native disable or disconnect releases it.
pub struct NotificationFaults {
    state: std::sync::Mutex<FaultState>,
    capacity: usize,
}

impl NotificationFaults {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(FaultState::default()),
            capacity,
        }
    }

    fn key(note: &ValueNotification) -> NotificationKey {
        (
            note.service_uuid,
            note.service_instance,
            note.uuid,
            note.instance,
        )
    }

    #[cfg(any(test, target_vendor = "apple"))]
    pub fn publish(
        &self,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        notification: ValueNotification,
    ) {
        let mut state = self.state.lock().unwrap();
        Self::publish_locked(&mut state, self.capacity, sender, notification);
    }

    fn publish_locked(
        state: &mut FaultState,
        capacity: usize,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        mut notification: ValueNotification,
    ) {
        if state.quota.is_some() {
            return;
        }
        let sequence = match state.sequence.checked_add(1) {
            Some(sequence) => sequence,
            None => u64::MAX,
        };
        let full = notification.source_failure.is_some()
            && !state.faults.contains_key(&Self::key(&notification))
            && state.faults.len() >= capacity;
        if full || sequence == u64::MAX {
            let original = notification.source_failure.take();
            let mut error = crate::PlatformError::new(
                "ubm-native",
                "notification-fault-quota",
                "Bounded native notification terminal evidence capacity exhausted",
            );
            if let Some(original) = original {
                error = error
                    .with("originalDomain", original.domain)
                    .with("originalCode", original.code)
                    .with("originalMessage", original.message);
            }
            notification.value.clear();
            notification.source_failure = Some(error);
            let envelope = NotificationEnvelope {
                sequence,
                notification,
            };
            state.sequence = sequence;
            state.quota = Some(envelope.clone());
            // No listeners is not an error: retained evidence remains owned.
            let _ = sender.send(envelope);
            return;
        }
        state.sequence = sequence;
        let envelope = NotificationEnvelope {
            sequence,
            notification,
        };
        if envelope.notification.source_failure.is_some() {
            state
                .faults
                .entry(Self::key(&envelope.notification))
                .or_insert_with(|| envelope.clone());
        }
        let _ = sender.send(envelope);
    }

    #[cfg(any(target_os = "windows", test))]
    pub fn begin(&self, characteristic: &crate::api::Characteristic) -> Result<u64, crate::Error> {
        let mut state = self.state.lock().unwrap();
        let key = (
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        );
        if !state.enable_epochs.contains_key(&key) && state.enable_epochs.len() >= self.capacity {
            return Err(crate::Error::Platform(crate::PlatformError::new(
                "ubm-native",
                "notification-fault-quota",
                "Native notification subscription capacity exhausted",
            )));
        }
        state.sequence = state.sequence.checked_add(1).ok_or_else(|| {
            crate::Error::Platform(crate::PlatformError::new(
                "ubm-native",
                "notification-fault-quota",
                "Native notification epoch exhausted",
            ))
        })?;
        let epoch = state.sequence;
        state.enable_epochs.insert(key, epoch);
        Ok(epoch)
    }

    #[cfg(any(target_os = "windows", test))]
    pub fn publish_for_epoch(
        &self,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        notification: ValueNotification,
        epoch: u64,
    ) {
        // The callback's captured native enable identity must still be live.
        // Publish under the same lock as disable so a queued old callback
        // cannot resurrect a retired terminal in a fresh enable.
        let mut state = self.state.lock().unwrap();
        if state.enable_epochs.get(&Self::key(&notification)) != Some(&epoch) {
            return;
        }
        Self::publish_locked(&mut state, self.capacity, sender, notification);
    }

    pub fn clear_key(&self, key: NotificationKey) {
        let mut state = self.state.lock().unwrap();
        state.faults.remove(&key);
        state.enable_epochs.remove(&key);
    }

    pub fn clear(&self, characteristic: &crate::api::Characteristic) {
        self.clear_key((
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        ));
    }

    pub fn clear_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.faults.clear();
        state.enable_epochs.clear();
        state.quota = None;
        // Preserve the sequence watermark so old queued evidence cannot gain
        // a fresh subscription identity after a confirmed native close.
    }
}

pub fn retained_notifications(
    receiver: Receiver<NotificationEnvelope>,
    faults: std::sync::Arc<NotificationFaults>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    let (floor, initial) = {
        let state = faults.state.lock().unwrap();
        let mut initial: Vec<_> = state
            .faults
            .values()
            .chain(state.quota.iter())
            .cloned()
            .collect();
        initial.sort_by_key(|fault| fault.sequence);
        (state.sequence, initial)
    };
    let initial_faults = faults.clone();
    let initial = futures::stream::iter(initial).filter_map(move |envelope| {
        let state = initial_faults.state.lock().unwrap();
        let live = state
            .faults
            .get(&NotificationFaults::key(&envelope.notification))
            .is_some_and(|fault| fault.sequence == envelope.sequence)
            || state
                .quota
                .as_ref()
                .is_some_and(|fault| fault.sequence == envelope.sequence);
        ready(live.then_some(envelope.notification))
    });
    Box::pin(
        initial.chain(
            BroadcastStream::new(receiver)
                .scan((0u64, floor), move |(missed, last_sequence), item| {
                    let mut out = Vec::new();
                    match item {
                        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                            *missed = missed.saturating_add(skipped)
                        }
                        Ok(envelope) => {
                            if *missed > 0 {
                                let state = faults.state.lock().unwrap();
                                let mut retained: Vec<_> = state
                                    .faults
                                    .values()
                                    .chain(state.quota.iter())
                                    .filter(|fault| {
                                        fault.sequence > *last_sequence
                                            && fault.sequence < envelope.sequence
                                    })
                                    .cloned()
                                    .collect();
                                retained.sort_by_key(|fault| fault.sequence);
                                // These overwritten records were terminal controls,
                                // not lost application notification values.
                                *missed = missed.saturating_sub(retained.len() as u64);
                                out.extend(retained.into_iter().map(|fault| fault.notification));
                            }
                            let current = if envelope.notification.source_failure.is_some() {
                                let state = faults.state.lock().unwrap();
                                state
                                    .faults
                                    .get(&NotificationFaults::key(&envelope.notification))
                                    .is_some_and(|fault| fault.sequence == envelope.sequence)
                                    || state
                                        .quota
                                        .as_ref()
                                        .is_some_and(|fault| fault.sequence == envelope.sequence)
                            } else {
                                true
                            };
                            if envelope.sequence > floor && current {
                                out.push(envelope.notification);
                            }
                            *last_sequence = (*last_sequence).max(envelope.sequence);
                            if let Some(first) = out.first_mut() {
                                first.lost_before =
                                    first.lost_before.saturating_add(std::mem::take(missed));
                            }
                        }
                    }
                    ready(Some(futures::stream::iter(out)))
                })
                .flatten(),
        ),
    )
}

#[cfg(test)]
mod ubm_lag_tests {
    use super::notifications_stream_from_broadcast_receiver;
    use crate::api::ValueNotification;
    use futures::stream::StreamExt;
    use uuid::Uuid;

    fn value(byte: u8) -> ValueNotification {
        ValueNotification {
            uuid: Uuid::nil(),
            instance: 0,
            service_uuid: Uuid::nil(),
            service_instance: 0,
            value: vec![byte],
            source_failure: None,
            lost_before: 0,
        }
    }

    #[test]
    fn a_lagging_receiver_learns_what_it_missed() {
        // The real bounded broadcast every platform peripheral uses: two
        // slots, five sends before the receiver reads.
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = notifications_stream_from_broadcast_receiver(receiver);
        for byte in 1..=5 {
            sender.send(value(byte)).expect("receiver alive");
        }
        drop(sender);
        let received: Vec<(u8, u64)> = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(note) = stream.next().await {
                out.push((note.value[0], note.lost_before));
            }
            out
        });
        assert_eq!(
            received,
            vec![(4, 3), (5, 0)],
            "the three overwritten values are reported on the next one"
        );
    }

    #[test]
    fn a_receiver_that_keeps_up_loses_nothing() {
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = notifications_stream_from_broadcast_receiver(receiver);
        let received = futures::executor::block_on(async {
            let mut out = Vec::new();
            for byte in 1..=4 {
                sender.send(value(byte)).expect("receiver alive");
                out.push(stream.next().await.expect("value").lost_before);
            }
            out
        });
        assert_eq!(received, vec![0, 0, 0, 0]);
    }
}

#[cfg(test)]
mod retained_fault_tests {
    use super::*;
    use crate::{PlatformError, api::Characteristic};
    use uuid::Uuid;

    fn note(instance: u64, failure: bool) -> ValueNotification {
        ValueNotification {
            uuid: Uuid::nil(),
            instance,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            value: vec![instance as u8],
            lost_before: 0,
            source_failure: failure
                .then(|| PlatformError::new("corebluetooth", "17", "original NSError")),
        }
    }

    #[test]
    fn another_attribute_flood_cannot_overwrite_the_original_fault() {
        for original in [
            PlatformError::new("corebluetooth", "17", "original NSError")
                .with("nsErrorDomain", "CBATTErrorDomain"),
            PlatformError::new("windows-hresult", "0x800710DF", "original HRESULT")
                .with("operation", "gatt.notification.value"),
        ] {
            let ingress = std::sync::Arc::new(NotificationFaults::new(4));
            let (sender, receiver) = tokio::sync::broadcast::channel(2);
            let mut stream = retained_notifications(receiver, ingress.clone());
            let mut fault = note(1, true);
            fault.source_failure = Some(original.clone());
            ingress.publish(&sender, fault);
            for _ in 0..5 {
                ingress.publish(&sender, note(2, false));
            }
            let first = futures::executor::block_on(stream.next()).expect("retained terminal");
            assert_eq!(first.instance, 1);
            assert_eq!(first.service_instance, 7);
            assert_eq!(first.source_failure.expect("original fault"), original);
            assert_eq!(first.lost_before, 3);
        }
    }

    #[test]
    fn confirmed_disable_and_new_receiver_do_not_replay_an_old_fault() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(4));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        ingress.publish(&sender, note(1, true));
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        ingress.clear(&characteristic);
        let mut stream = retained_notifications(sender.subscribe(), ingress.clone());
        for _ in 0..5 {
            ingress.publish(&sender, note(2, false));
        }
        let first = futures::executor::block_on(stream.next()).expect("new source");
        assert_eq!(first.instance, 2);
        assert!(first.source_failure.is_none());
    }

    #[test]
    fn queued_callback_from_retired_enable_cannot_poison_new_enable() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(4));
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notifications(receiver, ingress.clone());
        let old = ingress.begin(&characteristic).expect("enable");
        ingress.publish_for_epoch(&sender, note(1, true), old);
        ingress.clear(&characteristic);
        let fresh = ingress.begin(&characteristic).expect("fresh enable");
        ingress.publish_for_epoch(&sender, note(1, true), old);
        ingress.publish_for_epoch(&sender, note(1, false), fresh);
        let first = futures::executor::block_on(stream.next()).expect("fresh value");
        assert!(first.source_failure.is_none());
        assert_eq!(first.instance, 1);
    }

    #[test]
    fn retained_fault_capacity_is_bounded_and_explicitly_fail_closed() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notifications(receiver, ingress.clone());
        ingress.publish(&sender, note(1, true));
        ingress.publish(&sender, note(2, true));
        ingress.publish(&sender, note(3, false));
        assert_eq!(ingress.state.lock().unwrap().faults.len(), 1);
        let first = futures::executor::block_on(stream.next()).expect("first fault");
        assert_eq!(first.instance, 1);
        let quota = futures::executor::block_on(stream.next()).expect("quota terminal");
        assert_eq!(
            quota.source_failure.expect("failure").code,
            "notification-fault-quota"
        );
    }
}
