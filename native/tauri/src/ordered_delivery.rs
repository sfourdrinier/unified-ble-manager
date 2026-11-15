// native/tauri/src/ordered_delivery.rs
//! One retained IPC delivery. An acknowledgement is idempotent on retry.

use crate::IpcValue;

#[derive(Default)]
pub(super) struct OrderedDelivery {
    next_id: u64,
    retained: Option<(String, IpcValue)>,
    last_acknowledged: Option<String>,
}

impl OrderedDelivery {
    pub(super) fn acknowledge(&mut self, ack: Option<&str>) -> Result<(), ()> {
        let Some(ack) = ack else {
            return Ok(());
        };
        if self.retained.as_ref().is_some_and(|(id, _)| id == ack) {
            self.retained = None;
            self.last_acknowledged = Some(ack.to_owned());
            Ok(())
        } else if self.last_acknowledged.as_deref() == Some(ack) {
            Ok(())
        } else {
            Err(())
        }
    }

    pub(super) fn can_retain(&self) -> bool {
        self.retained.is_none() && self.next_id < u64::MAX
    }

    pub(super) fn take_for_revocation(&mut self) -> Option<IpcValue> {
        self.retained.take().map(|(_, value)| value)
    }

    pub(super) fn retained(&self) -> Option<(&str, &IpcValue)> {
        self.retained
            .as_ref()
            .map(|(id, value)| (id.as_str(), value))
    }

    pub(super) fn retain_native(&mut self, id: u64, value: IpcValue) -> Result<(), ()> {
        if self.retained.is_some() || id == 0 || id <= self.next_id {
            return Err(());
        }
        self.next_id = id;
        self.retained = Some((id.to_string(), value));
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn retain(&mut self, value: IpcValue) -> Result<(), ()> {
        if !self.can_retain() {
            return Err(());
        }
        self.next_id = self.next_id.checked_add(1).ok_or(())?;
        self.retained = Some((self.next_id.to_string(), value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_delivery_id_refuses_before_consuming_another_record() {
        let mut delivery = OrderedDelivery {
            next_id: u64::MAX,
            ..OrderedDelivery::default()
        };
        assert!(!delivery.can_retain());
        assert!(delivery.retain(IpcValue::Null).is_err());
        assert!(delivery.retained().is_none());
    }

    #[test]
    fn exact_native_record_survives_response_retry() {
        use ubm_core::ordered_notifications::{ObservedDelivery, Poll, Record};
        let encoded = ubm_desktop::ordered_wire::encode(Poll::Record(Record::Value {
            ordinal: u64::MAX,
            member: 9,
            membership: ubm_core::ordered_notifications::Membership {
                member: 9,
                epoch: u64::MAX,
            },
            value: vec![0, 255],
            delivery: ObservedDelivery::Unknown,
            provenance: ubm_core::central::NotificationProvenance::Unknown,
        }));
        let item = IpcValue::from_wire(encoded).unwrap();
        let mut delivery = OrderedDelivery::default();
        delivery.retain(item.clone()).unwrap();
        assert_eq!(delivery.retained(), Some(("1", &item)));
        assert_eq!(delivery.retained(), Some(("1", &item)));
        delivery.acknowledge(Some("1")).unwrap();
        assert!(delivery.retained().is_none());
    }

    #[test]
    fn terminal_unknown_omissions_survive_ipc_replay() {
        use ubm_core::ordered_notifications::{EndReason, Poll, Terminal};
        let record = ubm_desktop::ordered_wire::encode(Poll::Terminal(Terminal {
            reason: EndReason::Closed,
            tail_uncertain: true,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: true,
        }));
        let item = IpcValue::from_wire(record).unwrap();
        let mut delivery = OrderedDelivery::default();
        delivery.retain(item.clone()).unwrap();
        for _ in 0..2 {
            let (id, retained) = delivery.retained().unwrap();
            assert_eq!(id, "1");
            let wire = retained.clone().into_wire();
            assert_eq!(wire["failureSummaryVersion"], 1);
            assert_eq!(wire["failureObservationsOmitted"], true);
            assert_eq!(wire["additionalFailureCount"], "0");
        }
        delivery.acknowledge(Some("1")).unwrap();
        assert!(delivery.retained().is_none());
    }

    #[test]
    fn lost_response_replays_until_ack_and_old_ack_is_idempotent() {
        let mut delivery = OrderedDelivery::default();
        delivery.retain(IpcValue::String("first".into())).unwrap();
        assert_eq!(
            delivery.retained(),
            Some(("1", &IpcValue::String("first".into())))
        );
        assert_eq!(
            delivery.retained(),
            Some(("1", &IpcValue::String("first".into())))
        );
        assert!(delivery.acknowledge(Some("2")).is_err());
        assert_eq!(delivery.retained().unwrap().0, "1");
        delivery.acknowledge(Some("1")).unwrap();
        delivery.retain(IpcValue::String("second".into())).unwrap();
        delivery.acknowledge(Some("1")).unwrap();
        assert_eq!(delivery.retained().unwrap().0, "2");
        delivery.acknowledge(Some("2")).unwrap();
        assert!(delivery.acknowledge(Some("1")).is_err());
    }
}
