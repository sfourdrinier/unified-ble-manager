//! One vendor-to-radio continuity boundary, separate from NAPI's queue.

use crate::{DesktopError, RadioEvent, boundary::ObservedConnectionParameters};
#[cfg(any(target_os = "windows", test))]
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{broadcast, mpsc};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

fn source_error(error: btleplug::PlatformError) -> DesktopError {
    crate::btleplug_backend::map_radio(
        "connection.parameters",
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Platform,
    )(btleplug::Error::Platform(error))
}

/// Registration identity fences queued callbacks independently of the peer ID.
#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
pub(crate) struct SourceEpochs {
    next: AtomicU64,
    active: Mutex<HashMap<String, u64>>,
}
#[cfg(any(target_os = "windows", test))]
impl SourceEpochs {
    pub(crate) fn admit(&self, peer: &str) -> Result<u64, DesktopError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let quota = || {
            DesktopError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "connection.parameters",
            )
        };
        if !active.contains_key(peer) && active.len() >= 4096 {
            return Err(quota());
        }
        let epoch = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| quota())?;
        active.insert(peer.to_owned(), epoch);
        Ok(epoch)
    }
    pub(crate) fn is_current(&self, peer: &str, epoch: u64) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(peer)
            == Some(&epoch)
    }
    pub(crate) fn retire(&self, peer: &str) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(peer);
    }
}

pub(crate) async fn relay<
    T: Clone + Send,
    Map: Fn(T) -> ObservedConnectionParameters + Send,
    Lost: Fn(u64) + Send,
    Out: Send,
    Wrap: Fn(RadioEvent) -> Out + Send,
>(
    mut reports: broadcast::Receiver<Result<T, btleplug::PlatformError>>,
    events: mpsc::Sender<Out>,
    peer: String,
    normalize: Map,
    lost: Lost,
    wrap: Wrap,
) {
    loop {
        let event = match reports.recv().await {
            Ok(Ok(value)) => {
                let value = normalize(value);
                RadioEvent::ConnectionParameters {
                    peer_id: peer.clone(),
                    interval_us: value.interval_us,
                    latency: value.latency,
                    supervision_timeout_us: value.supervision_timeout_us,
                }
            }
            Ok(Err(platform)) => RadioEvent::ConnectionParameterSourceFailed {
                peer_id: peer.clone(),
                error: source_error(platform),
            },
            Err(broadcast::error::RecvError::Lagged(mut missed)) => {
                // Retained samples predate the live getter reconciliation. Do
                // not publish them afterward as newer facts. Retained failures
                // keep their original cause and are delivered after the gap.
                let mut failures = Vec::new();
                let mut closed = false;
                let mut drained = 0usize;
                loop {
                    if drained == 4096 {
                        let error = DesktopError::new(BleErrorCode::StreamQuota, BleErrorDomain::Stream, "connection.parameters")
                            .with_detail("native parameter source could not establish a bounded reconciliation fence");
                        let _ = events
                            .send(wrap(RadioEvent::ConnectionParameterSourceFailed {
                                peer_id: peer,
                                error,
                            }))
                            .await;
                        return;
                    }
                    match reports.try_recv() {
                        Ok(Ok(_)) => {
                            missed = missed.saturating_add(1);
                            drained += 1;
                        }
                        Ok(Err(error)) => {
                            failures.push(error);
                            drained += 1;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(more)) => {
                            missed = missed.saturating_add(more);
                            drained += 1;
                        }
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => {
                            closed = true;
                            break;
                        }
                    }
                }
                lost(missed);
                if events
                    .send(wrap(RadioEvent::ConnectionParameterGap {
                        peer_id: peer.clone(),
                        missed,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                for error in failures {
                    if events
                        .send(wrap(RadioEvent::ConnectionParameterSourceFailed {
                            peer_id: peer.clone(),
                            error: source_error(error),
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if closed {
                    let _ = events.send(wrap(closed_source(&peer))).await;
                    return;
                }
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                let _ = events.send(wrap(closed_source(&peer))).await;
                return;
            }
        };
        if events.send(wrap(event)).await.is_err() {
            return;
        }
    }
}

fn closed_source(peer: &str) -> RadioEvent {
    RadioEvent::ConnectionParameterSourceFailed {
        peer_id: peer.into(),
        error: DesktopError::new(
            BleErrorCode::StreamClosed,
            BleErrorDomain::Stream,
            "connection.parameters",
        )
        .with_detail("native parameter source closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RadioEvent, boundary::ObservedConnectionParameters};

    fn observed(interval: u32) -> btleplug::api::ConnectionParameters {
        btleplug::api::ConnectionParameters {
            interval_us: interval,
            latency: 1,
            supervision_timeout_us: 4_000_000,
        }
    }
    fn normalize(value: btleplug::api::ConnectionParameters) -> ObservedConnectionParameters {
        ObservedConnectionParameters {
            interval_us: value.interval_us,
            latency: value.latency,
            supervision_timeout_us: value.supervision_timeout_us,
        }
    }

    #[test]
    fn replacement_and_retirement_reject_queued_callbacks_from_the_old_source() {
        let sources = SourceEpochs::default();
        let first = sources.admit("peer").unwrap();
        assert!(sources.is_current("peer", first));
        let second = sources.admit("peer").unwrap();
        assert!(
            !sources.is_current("peer", first),
            "old callback failure cannot contaminate the replacement"
        );
        assert!(sources.is_current("peer", second));
        sources.retire("peer");
        assert!(!sources.is_current("peer", second));
        for _ in 0..1024 {
            let epoch = sources.admit("peer").unwrap();
            assert!(sources.is_current("peer", epoch));
            sources.retire("peer");
        }
        assert!(sources.active.lock().unwrap().is_empty());
    }

    #[test]
    fn live_source_capacity_is_reclaimed_without_retaining_retirement_history() {
        let sources = SourceEpochs::default();
        for index in 0..4096 {
            sources.admit(&format!("peer-{index}")).unwrap();
        }
        assert_eq!(
            sources.admit("excess").unwrap_err().code(),
            BleErrorCode::StreamQuota
        );
        let replacement = sources.admit("peer-0").unwrap();
        assert!(sources.is_current("peer-0", replacement));
        sources.retire("peer-1");
        assert!(sources.admit("excess").is_ok());
    }

    #[tokio::test]
    async fn actual_callback_failure_and_source_close_are_explicit_radio_events() {
        let (source, reports) = tokio::sync::broadcast::channel(2);
        let (events, mut received) = tokio::sync::mpsc::channel(8);
        let original =
            btleplug::PlatformError::new("winrt", "0x80070005", "callback getter refused")
                .with("hresult", "0x80070005");
        source
            .send(btleplug::connection_parameters_source::callback_answer(
                || Err(btleplug::Error::Platform(original)),
            ))
            .unwrap();
        drop(source);
        relay(
            reports,
            events,
            "source-peer".into(),
            normalize,
            |_| {},
            std::convert::identity,
        )
        .await;
        let Some(RadioEvent::ConnectionParameterSourceFailed { peer_id, error }) =
            received.recv().await
        else {
            panic!("getter failure")
        };
        assert_eq!(peer_id, "source-peer");
        assert_eq!(error.platform().unwrap().code, "0x80070005");
        let Some(RadioEvent::ConnectionParameterSourceFailed { error, .. }) = received.recv().await
        else {
            panic!("closed source")
        };
        assert_eq!(
            error.code(),
            ubm_core::contracts::BleErrorCode::StreamClosed
        );
        assert!(received.recv().await.is_none());
    }

    #[tokio::test]
    async fn vendor_lag_discards_retained_pre_reconciliation_values_and_preserves_queued_failure() {
        let (source, reports) = tokio::sync::broadcast::channel(2);
        let (events, mut received) = tokio::sync::mpsc::channel(8);
        for interval in [10_000, 20_000, 30_000, 40_000] {
            source.send(Ok(observed(interval))).unwrap();
        }
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = drops.clone();
        let task = tokio::spawn(relay(
            reports,
            events,
            "source-peer".into(),
            normalize,
            move |lost| {
                count.fetch_add(lost, std::sync::atomic::Ordering::Relaxed);
            },
            std::convert::identity,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::ConnectionParameterGap { missed: 4, .. })
        ));
        source.send(Ok(observed(90_000))).unwrap();
        let Some(RadioEvent::ConnectionParameters { interval_us, .. }) = received.recv().await
        else {
            panic!("fresh callback")
        };
        assert_eq!(
            interval_us, 90_000,
            "queued older values cannot overwrite fresh reconciliation"
        );
        assert_eq!(drops.load(std::sync::atomic::Ordering::Relaxed), 4);
        for _ in 0..4 {
            source.send(Ok(observed(30_000))).unwrap();
        }
        source
            .send(Err(btleplug::PlatformError::new(
                "winrt",
                "0x80004005",
                "queued getter failed",
            )))
            .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::ConnectionParameterGap { .. })
        ));
        let Some(RadioEvent::ConnectionParameterSourceFailed { error, .. }) = received.recv().await
        else {
            panic!("retained failure")
        };
        assert_eq!(error.platform().unwrap().code, "0x80004005");
        drop(source);
        task.await.unwrap();
    }
}
