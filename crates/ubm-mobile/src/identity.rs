//! The mobile owner's names for the scopes of its central.
//!
//! The shared central names no host ([`ubm_desktop::HostIdentity`]); this is
//! the name the React Native owner gives it, in the formats the legacy React
//! Native backends published (origin/main
//! `src/backends/corebluetooth/corebluetooth-attachment-lifecycle.ts`, with
//! the identity options of `react-native-android-provider.ts` and
//! `react-native-apple-provider.ts`):
//!
//! | field | Android | Apple |
//! |---|---|---|
//! | backend instance | `react-native-android-backend-{n}` | `react-native-apple-backend-{n}` |
//! | backend generation | `"1"`, then `"2"`… per reset | same |
//! | adapter generation | `"1"`, then `"2"`… per reset | same |
//! | attachment | `{instance}:{backend gen}:{adapter gen}` | same |
//! | adapter | `android-default-adapter` | `apple-corebluetooth-default-adapter` |
//!
//! `n` is the central's process open ordinal, as legacy numbered its backend
//! instances per process from 1.

use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, CoreError, Generation,
};
use ubm_desktop::{AttachmentEpoch, HostIdentity};

use crate::radio::MobilePlatform;

/// The React Native owner's identity on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobileIdentity {
    platform: MobilePlatform,
}

impl MobileIdentity {
    #[must_use]
    pub const fn new(platform: MobilePlatform) -> Self {
        Self { platform }
    }

    /// Legacy attachment scope (`attachmentScope`).
    #[must_use]
    pub const fn scope(self) -> &'static str {
        match self.platform {
            MobilePlatform::Android => "react-native-android",
            MobilePlatform::Apple => "react-native-apple",
        }
    }

    /// Legacy adapter native id (`REACT_NATIVE_*_DEFAULT_ADAPTER_NATIVE_ID`).
    #[must_use]
    pub const fn adapter_id(self) -> &'static str {
        match self.platform {
            MobilePlatform::Android => "android-default-adapter",
            MobilePlatform::Apple => "apple-corebluetooth-default-adapter",
        }
    }

    fn instance(self, epoch: AttachmentEpoch<'_>) -> String {
        format!("{}-backend-{}", self.scope(), epoch.ordinal)
    }

    /// Legacy generations start at 1 and advance by one per reset.
    fn generation(epoch: AttachmentEpoch<'_>) -> u64 {
        epoch.resets.saturating_add(1)
    }
}

impl HostIdentity for MobileIdentity {
    fn namespace(&self) -> &str {
        "ubm-mobile.host"
    }

    fn log_tag(&self) -> &str {
        "ubm-mobile"
    }

    fn attachment(&self, epoch: AttachmentEpoch<'_>) -> Result<AttachmentTuple, CoreError> {
        let instance = self.instance(epoch);
        let generation = Self::generation(epoch).to_string();
        Ok(AttachmentTuple::new(
            AttachmentId::new(format!("{instance}:{generation}:{generation}"))?,
            BackendInstanceId::new(instance)?,
            BackendGeneration::new(generation.clone())?,
            AdapterId::new(self.adapter_id())?,
            AdapterGeneration::new(generation)?,
        ))
    }

    fn kernel_generation(&self, epoch: AttachmentEpoch<'_>) -> Result<Generation, CoreError> {
        Generation::new(format!(
            "{}:kernel:{}",
            self.instance(epoch),
            Self::generation(epoch)
        ))
    }
}
