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
