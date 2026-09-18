// UBM patch (UBM_PATCHES.md #14): read/notify provenance on CoreBluetooth.
//
// CoreBluetooth reports a read response and a notification through the same
// callback (`peripheral:didUpdateValueForCharacteristic:error:`), so while a
// characteristic notifies, a value update cannot be attributed to a pending
// read. Upstream handed the next update to whichever read was waiting, so a
// notification could become a read result and the read response a
// notification. The legacy CoreBluetooth addon refused the ambiguous cases
// instead (`native/electron/corebluetooth/src/addon.mm`,
// `corebluetooth-read-notify-provenance.ts`); these decisions are the same.

use crate::PlatformError;

/// The legacy addon's code for a read on a notifying characteristic.
pub const INDEPENDENT_READ_CODE: u32 = 413;
/// The legacy addon's code for a second read while one is pending.
pub const OVERLAPPING_READ_CODE: u32 = 414;
/// The legacy addon's code for a subscribe while a read is pending.
pub const SUBSCRIBE_WHILE_READ_CODE: u32 = 415;
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
    /// A value update cannot be attributed to a read: the characteristic
    /// notifies, or a notification state change is in flight.
    #[must_use]
    pub const fn read_is_ambiguous(self) -> bool {
        self.is_notifying || self.pending_subscribe || self.pending_unsubscribe
    }

    /// Whether a new read may start; the refusal otherwise.
    pub fn admit_read(self) -> Result<(), PlatformError> {
        if self.read_is_ambiguous() {
            return Err(independent_read_error());
        }
        if self.pending_reads {
            return Err(refusal(
                OVERLAPPING_READ_CODE,
                "A read is already pending for this characteristic",
            ));
        }
        Ok(())
    }

    /// Whether a notification state change may start; the refusal
    /// otherwise.
    pub fn admit_notify_change(self) -> Result<(), PlatformError> {
        if self.pending_reads {
            return Err(refusal(
                SUBSCRIBE_WHILE_READ_CODE,
                "A notification state change cannot start while a read is pending for this characteristic",
            ));
        }
        Ok(())
    }

    /// Where one successful value update goes.
    #[must_use]
    pub const fn route_value(self) -> ValueRoute {
        if self.pending_reads && !self.read_is_ambiguous() {
            ValueRoute::CompleteRead
        } else if self.pending_reads {
            ValueRoute::RejectRead
        } else {
            ValueRoute::Notification
        }
    }
}

/// Where one successful value update goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRoute {
    /// The pending read's response.
    CompleteRead,
    /// Unattributable: the pending read fails as ambiguous.
    RejectRead,
    /// A notification.
    Notification,
}

/// The refusal of a read on a notifying characteristic.
#[must_use]
pub fn independent_read_error() -> PlatformError {
    refusal(
        INDEPENDENT_READ_CODE,
        "Independent read is ambiguous while this characteristic is notifying",
    )
}

/// The failure of an enable that left the characteristic not notifying.
#[must_use]
pub fn enable_not_notifying_error() -> PlatformError {
    refusal(
        ENABLE_NOT_NOTIFYING_CODE,
        "CCCD enable failed — characteristic is not notifying",
    )
}

fn refusal(code: u32, message: &str) -> PlatformError {
    PlatformError::new("corebluetooth", code.to_string(), message)
        .with("nsErrorDomain", "UBMCoreBluetooth")
}

#[cfg(test)]
mod tests {
    use super::{ReadNotifyState, ValueRoute};

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

    fn code(result: Result<(), crate::PlatformError>) -> Option<String> {
        result.err().map(|error| {
            assert_eq!(error.domain, "corebluetooth");
            error.code
        })
    }

    #[test]
    fn a_read_on_a_notifying_characteristic_is_refused_413() {
        for ambiguous in [
            state(true, false, false, false),
            state(false, false, true, false),
            state(false, false, false, true),
        ] {
            assert_eq!(code(ambiguous.admit_read()).as_deref(), Some("413"));
        }
    }

    #[test]
    fn a_second_read_is_refused_414() {
        assert_eq!(
            code(state(false, true, false, false).admit_read()).as_deref(),
            Some("414")
        );
        assert_eq!(code(ReadNotifyState::default().admit_read()), None);
    }

    #[test]
    fn a_notify_change_during_a_read_is_refused_415() {
        assert_eq!(
            code(state(false, true, false, false).admit_notify_change()).as_deref(),
            Some("415")
        );
        assert_eq!(
            code(state(true, false, false, false).admit_notify_change()),
            None
        );
    }

    #[test]
    fn a_value_goes_to_a_read_only_when_it_cannot_be_a_notification() {
        assert_eq!(
            state(false, true, false, false).route_value(),
            ValueRoute::CompleteRead
        );
        assert_eq!(
            state(true, true, false, false).route_value(),
            ValueRoute::RejectRead
        );
        assert_eq!(
            state(true, false, false, false).route_value(),
            ValueRoute::Notification
        );
        assert_eq!(
            ReadNotifyState::default().route_value(),
            ValueRoute::Notification
        );
    }
}
