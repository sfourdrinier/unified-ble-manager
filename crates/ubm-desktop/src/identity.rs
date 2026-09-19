//! Host identity of a central.
//!
//! [`crate::DesktopCentral`] is the shared central of every Rust host — the
//! Node/Electron addon, the Tauri shell and the mobile owner — so it never
//! chooses the spelling of the identities it mints. It decides *when* a scope
//! begins (the open, and each adapter reset); the host that owns the central
//! decides what that scope is called, through the [`HostIdentity`] in its
//! [`crate::CentralProfile`]. The desktop hosts pass [`DesktopIdentity`],
//! which keeps the names they always published; the mobile owner passes its
//! own, so no desktop name ever reaches a phone.

use std::fmt;

use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, BleErrorCode, BleErrorDomain, CoreError, Generation,
};

use crate::errors::DesktopError;

/// One attachment scope of one central: the open (`resets == 0`) or the
/// scope after the `resets`-th adapter reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentEpoch<'a> {
    /// Process-wide open ordinal of the central, fixed for its lifetime.
    pub ordinal: u64,
    /// Adapter resets the central has taken before this scope.
    pub resets: u64,
    /// The adapter label the radio boundary reported at open.
    pub adapter: &'a str,
}

/// The names a host gives the scopes of the central it owns.
///
/// Every name must be non-empty; a host that cannot name a scope answers
/// the core's `argument.invalid`, which fails the open (or the reset) instead
/// of being replaced by a name the host did not choose.
pub trait HostIdentity: Send + Sync + fmt::Debug {
    /// Namespace of the central's own failures (`{namespace}.open`).
    fn namespace(&self) -> &str;

    /// Prefix of the diagnostics the central writes to the process log.
    fn log_tag(&self) -> &str;

    /// Refuse an identity the host was misconfigured with, before the radio
    /// is read. The default accepts.
    fn validate(&self) -> Result<(), DesktopError> {
        Ok(())
    }

    /// The attachment tuple of `epoch`.
    fn attachment(&self, epoch: AttachmentEpoch<'_>) -> Result<AttachmentTuple, CoreError>;

    /// The core kernel generation of `epoch`. Distinct for every epoch of
    /// one central.
    fn kernel_generation(&self, epoch: AttachmentEpoch<'_>) -> Result<Generation, CoreError>;
}

/// The desktop hosts' names (napi, Tauri): `desktop-attachment-{ordinal}`,
/// `ubm-desktop-{backend_label}-{owner}`, `desktop-backend-gen-{ordinal}`,
/// the boundary's adapter label, `desktop-adapter-gen-{ordinal}`, and
/// `desktop-kernel-gen-{ordinal}`; a reset appends `-r{resets}` to every
/// generation and to the attachment id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopIdentity {
    backend_label: String,
    owner: String,
}

impl DesktopIdentity {
    /// `backend_label` names the radio (`btleplug`, `synthetic`), `owner`
    /// the host (`node`, `tauri`). Both must be non-empty ([`Self::validate`]).
    #[must_use]
    pub fn new(backend_label: &str, owner: &str) -> Self {
        Self {
            backend_label: backend_label.to_owned(),
            owner: owner.to_owned(),
        }
    }

    fn suffixed(prefix: &str, epoch: AttachmentEpoch<'_>) -> String {
        if epoch.resets == 0 {
            format!("{prefix}-{}", epoch.ordinal)
        } else {
            format!("{prefix}-{}-r{}", epoch.ordinal, epoch.resets)
        }
    }
}

impl HostIdentity for DesktopIdentity {
    fn namespace(&self) -> &str {
        "desktop"
    }

    fn log_tag(&self) -> &str {
        "ubm-desktop"
    }

    fn validate(&self) -> Result<(), DesktopError> {
        if self.owner.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.owner",
            ));
        }
        if self.backend_label.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.backend-label",
            ));
        }
        Ok(())
    }

    fn attachment(&self, epoch: AttachmentEpoch<'_>) -> Result<AttachmentTuple, CoreError> {
        Ok(AttachmentTuple::new(
            AttachmentId::new(Self::suffixed("desktop-attachment", epoch))?,
            BackendInstanceId::new(format!("ubm-desktop-{}-{}", self.backend_label, self.owner))?,
            BackendGeneration::new(Self::suffixed("desktop-backend-gen", epoch))?,
            AdapterId::new(epoch.adapter)?,
            AdapterGeneration::new(Self::suffixed("desktop-adapter-gen", epoch))?,
        ))
    }

    fn kernel_generation(&self, epoch: AttachmentEpoch<'_>) -> Result<Generation, CoreError> {
        Generation::new(Self::suffixed("desktop-kernel-gen", epoch))
    }
}
