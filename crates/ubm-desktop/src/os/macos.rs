//! macOS CoreBluetooth adapter: what btleplug 0.12 does not expose (PR210
//! decision 7, PARITY-INVENTORY §1).
//!
//! `+[CBManager authorization]` is a class property: reading it needs no
//! `CBCentralManager` (btleplug owns the only one) and never prompts; only
//! radio use prompts. Mirrors the legacy addon
//! (`native/electron/corebluetooth/index.js`) and the Tauri host
//! (`native/tauri/src/btleplug_dispatcher.rs` `platform_authorization`).

use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

use crate::boundary::AdapterAuthorization;
use crate::errors::DesktopError;

/// `CBManagerAuthorization` raw values (macOS 10.15+).
const NOT_DETERMINED: isize = 0;
const RESTRICTED: isize = 1;
const DENIED: isize = 2;
const ALLOWED_ALWAYS: isize = 3;

/// Map a raw `CBManagerAuthorization`. A value outside the documented enum
/// is not forced into a word: it is a platform failure naming the value.
pub(crate) fn map_authorization(raw: isize) -> Result<AdapterAuthorization, DesktopError> {
    match raw {
        ALLOWED_ALWAYS => Ok(AdapterAuthorization::Granted),
        DENIED => Ok(AdapterAuthorization::Denied),
        RESTRICTED => Ok(AdapterAuthorization::Restricted),
        NOT_DETERMINED => Ok(AdapterAuthorization::NotDetermined),
        other => Err(DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "adapter.authorization",
        )
        .with_detail(format!(
            "CoreBluetooth reported an unrecognized authorization value {other}"
        ))),
    }
}

/// Read `+[CBManager authorization]`. The class and the class method are
/// checked first: on a system without them the answer is
/// `capability.unsupported`, never a guessed authorization.
pub(crate) fn authorization() -> Result<AdapterAuthorization, DesktopError> {
    use objc2::{runtime::AnyClass, sel};
    use objc2_core_bluetooth::CBManager;

    let unsupported = |detail: &str| {
        DesktopError::new(
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
            "adapter.authorization",
        )
        .with_detail(detail.to_owned())
    };
    let Some(class) = AnyClass::get("CBManager") else {
        return Err(unsupported("CoreBluetooth is not loaded in this process"));
    };
    if !class.metaclass().responds_to(sel!(authorization)) {
        return Err(unsupported(
            "this macOS version does not expose +[CBManager authorization]",
        ));
    }
    // SAFETY: `+[CBManager authorization]` was just verified to exist on the
    // metaclass; it takes no arguments and returns `CBManagerAuthorization`
    // (an `NSInteger`), the encoding this binding declares.
    let raw = unsafe { CBManager::authorization_class() };
    map_authorization(raw.0)
}

#[cfg(test)]
mod tests {
    use super::{authorization, map_authorization};
    use crate::boundary::AdapterAuthorization;

    #[test]
    fn every_documented_value_maps_and_others_fail_loudly() {
        assert_eq!(
            map_authorization(3).expect("allowed"),
            AdapterAuthorization::Granted
        );
        assert_eq!(
            map_authorization(2).expect("denied"),
            AdapterAuthorization::Denied
        );
        assert_eq!(
            map_authorization(1).expect("restricted"),
            AdapterAuthorization::Restricted
        );
        assert_eq!(
            map_authorization(0).expect("not determined"),
            AdapterAuthorization::NotDetermined
        );
        assert_eq!(
            map_authorization(9).expect_err("unrecognized").code_str(),
            "platform.failure"
        );
    }

    /// Reads the live class property. Reading never prompts and needs no
    /// Bluetooth permission, so this runs on any macOS host; the value
    /// itself depends on the host's privacy settings.
    #[test]
    fn the_live_class_property_answers_with_a_documented_value() {
        let answer = authorization().expect("CoreBluetooth answers on macOS 10.15+");
        assert!(matches!(
            answer,
            AdapterAuthorization::Granted
                | AdapterAuthorization::Denied
                | AdapterAuthorization::Restricted
                | AdapterAuthorization::NotDetermined
        ));
    }
}
