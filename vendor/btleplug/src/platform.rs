//! The `platform` module contains the platform-specific implementations of the various [`api`]
//! traits. Refer for the `api` module for how to use them.

#[cfg(any(target_os = "linux", target_vendor = "apple", target_os = "windows"))]
pub use crate::common::util::{
    marker_graph_identity, SourceGroupConsumer, SourceGroupError, SourceGroupHandle,
    SourceUnresolvedWriteRequest, SourceWriteReceipt, SourceWriteRequest, SourceWriteSelector,
    SourceWriteStatus, SOURCE_MARKER_MAX_REQUEST_BYTES, SOURCE_MARKER_MAX_WRITE_BYTES,
};

#[cfg(all(feature = "reclaim-test-support", any(target_os = "windows", target_vendor = "apple")))]
#[doc(hidden)]
pub use crate::common::util::legacy_overflow_receiver_fixture;
#[cfg(feature = "reclaim-test-support")]
#[doc(hidden)]
pub use crate::common::util::PeerGroupTestSource;

#[cfg(target_os = "linux")]
pub use crate::bluez::peripheral::{
    LeGattBearer, LeGattErrorStage, LeGattReadyToken, LeGattSnapshot, LeGattStatus,
    LE_GATT_OBSERVATION_TIMEOUT,
};
#[cfg(target_os = "linux")]
pub use crate::bluez::{
    adapter::Adapter, manager::Manager, peripheral::Peripheral, peripheral::PeripheralId,
    NotificationActivationIdentity, NotificationActivationOwner, PeerSourceError,
};
#[cfg(target_vendor = "apple")]
pub use crate::corebluetooth::{
    adapter::Adapter, central_delegate::advertisement_extras, manager::Manager,
    peripheral::AdvertisementExtras, peripheral::NotificationActivationOwner,
    peripheral::Peripheral, peripheral::PeripheralId,
};
#[cfg(target_os = "android")]
pub use crate::droidplug::{
    adapter::Adapter, init, manager::Manager, peripheral::Peripheral, peripheral::PeripheralId,
};
#[cfg(target_os = "windows")]
pub use crate::winrtble::{
    adapter::{Adapter, StalledScanWatcher}, ble::watcher::ScanStopped, manager::Manager,
    peripheral::NotificationActivationIdentity, peripheral::NotificationActivationOwner,
    peripheral::Peripheral, peripheral::PeripheralId,
};
#[cfg(target_os = "linux")]
pub use bluez_async::{LeLeaseReleaseReceipt, LeLeaseReleaseScope};
#[cfg(all(target_os = "linux", feature = "reclaim-test-support"))]
pub use crate::bluez::ReclaimSourceFixture;

use crate::api::{self, Central};
use static_assertions::assert_impl_all;
use std::{
    fmt::{Debug, Display},
    hash::Hash,
};

// Ensure that the exported types implement all the expected traits.
assert_impl_all!(Adapter: Central, Clone, Debug, Send, Sized, Sync);
assert_impl_all!(Manager: api::Manager, Clone, Debug, Send, Sized, Sync);
assert_impl_all!(Peripheral: api::Peripheral, Clone, Debug, Send, Sized, Sync);
assert_impl_all!(
    PeripheralId: Clone,
    Debug,
    Display,
    Hash,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Send,
    Sized,
    Sync
);
