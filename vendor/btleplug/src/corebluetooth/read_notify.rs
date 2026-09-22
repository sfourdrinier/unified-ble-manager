// UBM patch (UBM_PATCHES.md #14): read/notify provenance on CoreBluetooth.
//
// CoreBluetooth reports a read response and a notification through the same
// callback (`peripheral:didUpdateValueForCharacteristic:error:`), so while a
// characteristic can notify, a value update cannot be attributed to a pending
// read. Upstream handed the next update to whichever read was waiting and
// never told the caller, so a notification could silently become a read
// result. A read now always runs: pending reads complete in request order,
// each with the provenance CoreBluetooth can honestly give it, and a value
// that may be a notification still reaches the notification stream.

use crate::api::ReadProvenance;
use crate::PlatformError;

/// The legacy addon's code for an enable that left the characteristic not
/// notifying.
pub const ENABLE_NOT_NOTIFYING_CODE: u32 = 411;

/// What one characteristic has in flight, as the reply queues record it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadNotifyState {
    pub is_notifying: bool,
    pub pending_reads: bool,
    pub pending_subscribe: bool,
    pub pending_unsubscribe: bool,
}

impl ReadNotifyState {
    /// A value update could be a notification: the characteristic notifies,
    /// or a notification state change is in flight.
    #[must_use]
    pub const fn could_be_notification(self) -> bool {
        self.is_notifying || self.pending_subscribe || self.pending_unsubscribe
    }

    /// Where one successful value update goes.
    #[must_use]
    pub const fn route_value(self) -> ValueRoute {
        let could_be_notification = self.could_be_notification();
        if self.pending_reads {
            ValueRoute {
                read: Some(if could_be_notification {
                    ReadProvenance::ReadOrNotification
                } else {
                    ReadProvenance::ReadResponse
                }),
                notification: could_be_notification,
            }
        } else {
            ValueRoute {
                read: None,
                notification: true,
            }
        }
    }
}

/// Where one successful value update goes: the oldest pending read (with
/// the provenance it completes with) and/or the notification stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueRoute {
    pub read: Option<ReadProvenance>,
    pub notification: bool,
}

/// The failure of an enable that left the characteristic not notifying.
#[must_use]
pub fn enable_not_notifying_error() -> PlatformError {
    PlatformError::new(
        "corebluetooth",
        ENABLE_NOT_NOTIFYING_CODE.to_string(),
        "CCCD enable failed — characteristic is not notifying",
    )
    .with("nsErrorDomain", "UBMCoreBluetooth")
}

#[cfg(test)]
mod tests {
    use super::{ReadNotifyState, ValueRoute};
    use crate::api::ReadProvenance;

    fn state(
        is_notifying: bool,
        reads: bool,
        subscribe: bool,
        unsubscribe: bool,
    ) -> ReadNotifyState {
        ReadNotifyState {
            is_notifying,
            pending_reads: reads,
            pending_subscribe: subscribe,
            pending_unsubscribe: unsubscribe,
        }
    }

    #[test]
    fn a_read_on_an_idle_characteristic_completes_as_the_read_response() {
        assert_eq!(
            state(false, true, false, false).route_value(),
            ValueRoute {
                read: Some(ReadProvenance::ReadResponse),
                notification: false
            }
        );
    }

    #[test]
    fn a_read_while_notification_is_possible_completes_ambiguously_and_still_notifies() {
        for could_notify in [
            state(true, true, false, false),
            state(false, true, true, false),
            state(false, true, false, true),
        ] {
            assert_eq!(
                could_notify.route_value(),
                ValueRoute {
                    read: Some(ReadProvenance::ReadOrNotification),
                    notification: true
                },
                "{could_notify:?}"
            );
        }
    }

    #[test]
    fn a_value_with_no_pending_read_is_a_notification() {
        assert_eq!(
            state(true, false, false, false).route_value(),
            ValueRoute {
                read: None,
                notification: true
            }
        );
        assert_eq!(
            ReadNotifyState::default().route_value(),
            ValueRoute {
                read: None,
                notification: true
            }
        );
    }
}
