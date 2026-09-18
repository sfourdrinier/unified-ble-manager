//! CCCD delivery-mode planning (finding 39).
//!
//! A subscribe enables notifications or indications. Which one the link
//! actually gets is decided by the characteristic's properties and, when it
//! offers both, by the platform. [`plan_delivery`] answers — before any
//! radio effect — what the enable will write, from those two facts only:
//!
//! - one delivery property: every platform can only write that mode, so
//!   the answer is certain;
//! - neither: nothing can be enabled (`gatt.property-not-supported`);
//! - both: the platform's documented rule ([`BothPropertiesRule`]) decides,
//!   or the adapter writes the CCCD itself where it can select the mode.
//!
//! A hard requirement the characteristic lacks is `gatt.property-not-supported`
//! (the legacy CoreBluetooth check and the Tauri 4.x check both answered
//! this); a requirement the characteristic offers but the platform will not
//! write is `capability.limited`. Both fail before any effect, so no
//! requirement is accepted that nothing here enforces.
//!
//! Evidence for each platform rule (read 2026-09-17):
//! - CoreBluetooth: Apple's `CBPeripheral.setNotifyValue(_:for:)` reference,
//!   Discussion — "If the specified characteristic's configuration allows
//!   both notifications and indications, calling this method enables
//!   notifications only."
//! - BlueZ: `src/shared/gatt-client.c` `notify_data_write_ccc` writes
//!   `0x0001` when the characteristic has `BT_GATT_CHRC_PROP_NOTIFY`, else
//!   `0x0002` for indicate (the path `Device1`/`GattCharacteristic1.StartNotify`
//!   takes; btleplug 0.12 subscribes through `StartNotify`).
//! - WinRT through btleplug 0.12: `winrtble/utils.rs` `to_descriptor_value`
//!   writes `Indicate` whenever the characteristic can indicate. The
//!   Windows adapter (`os::windows`) rewrites the CCCD afterwards, which is
//!   what makes the mode selectable there (legacy WinRT preferred notify,
//!   `src/backends/winrt/winrt-handles.ts` `notificationModeForPath`).

use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

use crate::boundary::{DeliveryMode, ObservedDelivery, PropertyFlags};
use crate::errors::DesktopError;

/// What a platform writes when a characteristic offers both notify and
/// indicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BothPropertiesRule {
    /// The platform always writes this mode and nothing here can change it.
    PlatformWrites(DeliveryMode),
    /// The platform writes `platform_writes`, and the adapter can rewrite
    /// the CCCD afterwards; without a requirement it selects `preferred`.
    AdapterSelects {
        platform_writes: DeliveryMode,
        preferred: DeliveryMode,
    },
    /// No documented rule: the mode is not known, so no requirement can be
    /// accepted for a characteristic that offers both.
    Undocumented,
}

/// How one enable proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryPlan {
    /// Subscribe through the platform; it writes the mode reported here.
    Platform(ObservedDelivery),
    /// Subscribe through the platform, then the adapter writes this mode.
    /// The platform's own write stands until the rewrite succeeds.
    AdapterWrites {
        mode: DeliveryMode,
        platform_writes: DeliveryMode,
    },
}

/// The rule of the platform this crate is compiled for.
#[must_use]
pub const fn platform_rule() -> BothPropertiesRule {
    if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        BothPropertiesRule::PlatformWrites(DeliveryMode::Notification)
    } else if cfg!(target_os = "windows") {
        BothPropertiesRule::AdapterSelects {
            platform_writes: DeliveryMode::Indication,
            preferred: DeliveryMode::Notification,
        }
    } else {
        BothPropertiesRule::Undocumented
    }
}

/// Whether the OS of this build answers a subscribe to a characteristic
/// that declares neither notify nor indicate (finding 98): BlueZ's
/// `StartNotify`, which the legacy BlueZ backend called without a property
/// check. CoreBluetooth and WinRT had the check in their legacy backends
/// and keep it.
#[must_use]
pub const fn os_answers_unflagged_subscribe() -> bool {
    cfg!(target_os = "linux")
}

const OPERATION: &str = "gatt.subscribe.delivery";

fn observed(mode: DeliveryMode) -> ObservedDelivery {
    match mode {
        DeliveryMode::Notification => ObservedDelivery::Notification,
        DeliveryMode::Indication => ObservedDelivery::Indication,
    }
}

fn property_missing(detail: String) -> DesktopError {
    DesktopError::new(
        BleErrorCode::GattPropertyNotSupported,
        BleErrorDomain::Gatt,
        OPERATION,
    )
    .with_detail(detail)
}

fn platform_limited(detail: String) -> DesktopError {
    DesktopError::new(
        BleErrorCode::CapabilityLimited,
        BleErrorDomain::Capability,
        OPERATION,
    )
    .with_detail(detail)
}

/// [`plan_delivery`] for a radio whose OS may answer an unflagged
/// subscribe itself (finding 98): without a hard requirement, a
/// characteristic that declares neither property is handed to the OS, and
/// the mode it enables is not known. A hard requirement is still checked
/// against the properties, as the legacy public layer did.
pub fn plan_delivery_for_os(
    properties: PropertyFlags,
    requested: Option<DeliveryMode>,
    rule: BothPropertiesRule,
    os_answers_unflagged: bool,
) -> Result<DeliveryPlan, DesktopError> {
    if os_answers_unflagged && !properties.notify && !properties.indicate && requested.is_none() {
        return Ok(DeliveryPlan::Platform(ObservedDelivery::Unknown));
    }
    plan_delivery(properties, requested, rule)
}

/// Decide, before any effect, which CCCD mode one enable writes.
pub fn plan_delivery(
    properties: PropertyFlags,
    requested: Option<DeliveryMode>,
    rule: BothPropertiesRule,
) -> Result<DeliveryPlan, DesktopError> {
    let single = match (properties.notify, properties.indicate) {
        (false, false) => {
            return Err(property_missing(
                "the characteristic supports neither notify nor indicate".to_owned(),
            ));
        }
        (true, false) => Some(DeliveryMode::Notification),
        (false, true) => Some(DeliveryMode::Indication),
        (true, true) => None,
    };
    if let Some(only) = single {
        return match requested {
            Some(required) if required != only => Err(property_missing(format!(
                "{} required, the characteristic only supports {}",
                required.as_str(),
                only.as_str()
            ))),
            _ => Ok(DeliveryPlan::Platform(observed(only))),
        };
    }
    match rule {
        BothPropertiesRule::PlatformWrites(written) => match requested {
            Some(required) if required != written => Err(platform_limited(format!(
                "{} required, this platform enables {} when a characteristic supports both",
                required.as_str(),
                written.as_str()
            ))),
            _ => Ok(DeliveryPlan::Platform(observed(written))),
        },
        BothPropertiesRule::AdapterSelects {
            platform_writes,
            preferred,
        } => {
            let mode = requested.unwrap_or(preferred);
            if mode == platform_writes {
                Ok(DeliveryPlan::Platform(observed(mode)))
            } else {
                Ok(DeliveryPlan::AdapterWrites {
                    mode,
                    platform_writes,
                })
            }
        }
        BothPropertiesRule::Undocumented => match requested {
            Some(required) => Err(platform_limited(format!(
                "{} required, this platform does not document which mode it enables \
                 when a characteristic supports both",
                required.as_str()
            ))),
            None => Ok(DeliveryPlan::Platform(ObservedDelivery::Unknown)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BothPropertiesRule, DeliveryPlan, plan_delivery, plan_delivery_for_os, platform_rule,
    };
    use crate::boundary::{DeliveryMode, ObservedDelivery, PropertyFlags};

    fn props(notify: bool, indicate: bool) -> PropertyFlags {
        PropertyFlags {
            read: false,
            write: false,
            write_without_response: false,
            notify,
            indicate,
        }
    }

    const NOTIFY_FIRST: BothPropertiesRule =
        BothPropertiesRule::PlatformWrites(DeliveryMode::Notification);
    const WINDOWS: BothPropertiesRule = BothPropertiesRule::AdapterSelects {
        platform_writes: DeliveryMode::Indication,
        preferred: DeliveryMode::Notification,
    };

    #[test]
    fn a_single_property_answers_with_certainty_on_every_platform() {
        for rule in [NOTIFY_FIRST, WINDOWS, BothPropertiesRule::Undocumented] {
            assert_eq!(
                plan_delivery(props(true, false), None, rule).expect("notify only"),
                DeliveryPlan::Platform(ObservedDelivery::Notification)
            );
            assert_eq!(
                plan_delivery(props(true, false), Some(DeliveryMode::Notification), rule)
                    .expect("satisfied requirement proceeds"),
                DeliveryPlan::Platform(ObservedDelivery::Notification)
            );
            assert_eq!(
                plan_delivery(props(false, true), None, rule).expect("indicate only"),
                DeliveryPlan::Platform(ObservedDelivery::Indication)
            );
            assert_eq!(
                plan_delivery(props(false, true), Some(DeliveryMode::Indication), rule)
                    .expect("satisfied requirement proceeds"),
                DeliveryPlan::Platform(ObservedDelivery::Indication)
            );
        }
    }

    #[test]
    fn a_requirement_the_characteristic_lacks_fails_before_any_effect() {
        let error = plan_delivery(
            props(true, false),
            Some(DeliveryMode::Indication),
            NOTIFY_FIRST,
        )
        .expect_err("indication required on a notify-only characteristic");
        assert_eq!(error.code_str(), "gatt.property-not-supported");
        let error = plan_delivery(
            props(false, true),
            Some(DeliveryMode::Notification),
            WINDOWS,
        )
        .expect_err("notification required on an indicate-only characteristic");
        assert_eq!(error.code_str(), "gatt.property-not-supported");
        let error =
            plan_delivery(props(false, false), None, NOTIFY_FIRST).expect_err("nothing to enable");
        assert_eq!(error.code_str(), "gatt.property-not-supported");
    }

    #[test]
    fn both_properties_follow_the_documented_notify_first_rule() {
        assert_eq!(
            plan_delivery(props(true, true), None, NOTIFY_FIRST).expect("default"),
            DeliveryPlan::Platform(ObservedDelivery::Notification)
        );
        assert_eq!(
            plan_delivery(
                props(true, true),
                Some(DeliveryMode::Notification),
                NOTIFY_FIRST
            )
            .expect("the platform writes what is required"),
            DeliveryPlan::Platform(ObservedDelivery::Notification)
        );
        let error = plan_delivery(
            props(true, true),
            Some(DeliveryMode::Indication),
            NOTIFY_FIRST,
        )
        .expect_err("the platform will not write indicate");
        assert_eq!(error.code_str(), "capability.limited");
    }

    #[test]
    fn windows_selects_by_rewriting_the_cccd_and_prefers_notify() {
        assert_eq!(
            plan_delivery(props(true, true), None, WINDOWS).expect("legacy notify preference"),
            DeliveryPlan::AdapterWrites {
                mode: DeliveryMode::Notification,
                platform_writes: DeliveryMode::Indication,
            }
        );
        assert_eq!(
            plan_delivery(props(true, true), Some(DeliveryMode::Notification), WINDOWS)
                .expect("hard notification requirement"),
            DeliveryPlan::AdapterWrites {
                mode: DeliveryMode::Notification,
                platform_writes: DeliveryMode::Indication,
            }
        );
        assert_eq!(
            plan_delivery(props(true, true), Some(DeliveryMode::Indication), WINDOWS)
                .expect("the platform already writes indicate"),
            DeliveryPlan::Platform(ObservedDelivery::Indication)
        );
    }

    #[test]
    fn an_undocumented_platform_never_accepts_a_requirement_it_cannot_prove() {
        assert_eq!(
            plan_delivery(props(true, true), None, BothPropertiesRule::Undocumented)
                .expect("no requirement"),
            DeliveryPlan::Platform(ObservedDelivery::Unknown)
        );
        for required in [DeliveryMode::Notification, DeliveryMode::Indication] {
            let error = plan_delivery(
                props(true, true),
                Some(required),
                BothPropertiesRule::Undocumented,
            )
            .expect_err("unprovable requirement");
            assert_eq!(error.code_str(), "capability.limited");
        }
    }

    #[test]
    fn this_build_uses_its_platform_rule() {
        let rule = platform_rule();
        if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            assert_eq!(rule, NOTIFY_FIRST);
        } else if cfg!(target_os = "windows") {
            assert_eq!(rule, WINDOWS);
        } else {
            assert_eq!(rule, BothPropertiesRule::Undocumented);
        }
    }

    /// Finding 98: an OS that answers an unflagged subscribe (BlueZ) gets
    /// it without a requirement; with one, or on an OS that does not, the
    /// missing property is refused before any effect.
    #[test]
    fn f98_an_unflagged_subscribe_goes_to_an_os_that_answers_it() {
        let unflagged = props(false, false);
        assert_eq!(
            plan_delivery_for_os(unflagged, None, NOTIFY_FIRST, true).expect("handed to the OS"),
            DeliveryPlan::Platform(ObservedDelivery::Unknown)
        );
        for (requested, os_answers) in [(Some(DeliveryMode::Notification), true), (None, false)] {
            assert_eq!(
                plan_delivery_for_os(unflagged, requested, NOTIFY_FIRST, os_answers)
                    .expect_err("refused")
                    .code_str(),
                "gatt.property-not-supported"
            );
        }
        assert_eq!(
            plan_delivery_for_os(props(true, false), None, NOTIFY_FIRST, true).expect("notify"),
            DeliveryPlan::Platform(ObservedDelivery::Notification)
        );
    }
}
