//! Desktop central capability parity table (HOST-DESKTOP).
//!
//! Every required desktop central capability maps to exactly one verdict:
//! [`CapabilityVerdict::BtleplugProvides`], [`CapabilityVerdict::NarrowOsAdapterNeeded`],
//! or [`CapabilityVerdict::LimitationCandidate`]. Gaps stay open —
//! re-describing a gap never closes it — and the full mapping ships as the
//! committed `PARITY_GAPS.md` report. [`register_desktop_capabilities`]
//! projects the same table into the real core as capability descriptors so
//! the runtime truth matches the report.

use ubm_core::central::{CapabilityDescriptor, CapabilityState, Central, EvidenceLevel};
use ubm_core::contracts::CoreError;

/// Parity verdict for one required desktop central capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityVerdict {
    /// Implemented through a supported btleplug path in this crate.
    /// Reported as `limited` until physical-radio evidence qualifies it.
    BtleplugProvides,
    /// Needs a narrow per-OS adapter on top of btleplug. Open work.
    NarrowOsAdapterNeeded,
    /// Preapproved-limitation candidate: explicitly bounded behavior with a
    /// named limitation, never silent downgrade. The gap stays open in the
    /// report until the limitation is approved.
    LimitationCandidate,
}

impl CapabilityVerdict {
    /// Frozen verdict string for the report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BtleplugProvides => "btleplug-provides",
            Self::NarrowOsAdapterNeeded => "narrow-OS-adapter-needed",
            Self::LimitationCandidate => "preapproved-limitation-candidate",
        }
    }
}

/// One required desktop central capability and its parity verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopCapability {
    /// Frozen capability id.
    pub id: &'static str,
    /// Parity verdict.
    pub verdict: CapabilityVerdict,
    /// Primary acceptance scenario.
    pub scenario: &'static str,
    /// Report note: what provides it, what adapter is missing, or which
    /// limitation bounds it.
    pub note: &'static str,
    /// Limitation code for `limited` descriptors (`None` for unsupported).
    pub limitation: Option<&'static str>,
}

/// Required desktop central capabilities: the frozen desktop matrix rows
/// this host must answer. Mobile-only roles (Apple restoration,
/// Android service) are out of the desktop scope and noted in
/// `PARITY_GAPS.md`, not carried as desktop rows.
pub const DESKTOP_CAPABILITIES: &[DesktopCapability] = &[
    DesktopCapability {
        id: "discovery:continuous-scan",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "scan.owner-join-authority-and-signature",
        note: "One global scan owner with explicit stop and event-source-close cleanup.",
        limitation: Some("one-global-scan-owner"),
    },
    DesktopCapability {
        id: "discovery:advertisement-watch",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "scan.observation-delivery",
        note: "Advertisement facts depend on OS delivery (privacy-masked addresses/fields stay absent, never synthesized).",
        limitation: Some("os-delivery-bounded"),
    },
    DesktopCapability {
        id: "scan:platform-options",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "scan.platform-options",
        note: "Active/passive/PHY scan knobs need per-OS adapters; btleplug exposes service-UUID filters only.",
        limitation: None,
    },
    DesktopCapability {
        id: "peer:resolve-reference",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "peer.resolve-reference",
        note: "Platform-guid resolution for observed peers; address domains need OS identity adapters.",
        limitation: Some("platform-guid-only"),
    },
    DesktopCapability {
        id: "peer:address-targeting",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.address-targeting",
        note: "CoreBluetooth hides addresses entirely; address targeting needs a narrow OS identity adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "peer:known",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.known-peers",
        note: "OS-known peer retrieval (retrievePeripherals/retrieveConnectedPeripherals) needs a narrow adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "peer:system-connected",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.system-connected",
        note: "Adopting OS-connected peripherals needs a narrow adapter per platform.",
        limitation: None,
    },
    DesktopCapability {
        id: "peer:bonded",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.bonded",
        note: "The bond store is OS-side; reading it needs a narrow adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "peer:origin-authorized",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "peer.origin-authorized",
        note: "Caller authentication is owned by the host shell (Tauri/Node), not the radio layer.",
        limitation: Some("shell-owned-auth"),
    },
    DesktopCapability {
        id: "peer:restored",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.restored",
        note: "Desktop processes start without restored BLE handles; adopting OS-restored peers needs a narrow restoration adapter per platform.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:direct",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "connection.lease-joins-borrowing-transfer-and-revocation",
        note: "Direct connection and ownership cleanup are implemented; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
    },
    DesktopCapability {
        id: "connection:when-available",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.when-available",
        note: "Deferred auto-connect needs an OS adapter/daemon path beyond btleplug connect.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:rssi",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "connection.rssi-and-att-mtu-capability-contract",
        note: "RSSI reported only when the OS measures it; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
    },
    DesktopCapability {
        id: "connection:effective-mtu",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.rssi-and-att-mtu-capability-contract",
        note: "The OS-measured MTU already feeds every write through mtu()-3 into the core maximum-write-length (fail-closed when unmeasured); exposing the negotiated value as a host read needs an adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:request-mtu",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.mtu-request",
        note: "MTU request is an OS control path (Android requestMtu / Apple negotiated) needing an adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:priority",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.priority",
        note: "Connection-priority control needs a per-OS adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:parameters",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.parameters",
        note: "Connection-parameter update needs a per-OS adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:phy",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.phy",
        note: "PHY selection needs a per-OS adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "connection:subrate",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.subrate",
        note: "Subrate control needs a per-OS adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:state",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.state",
        note: "Observed link-security readout needs a per-OS adapter; never inferred from a flag.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:pair",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.pair",
        note: "Pairing ceremony is OS-mediated and needs a narrow adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:cancel-pairing",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.cancel-pairing",
        note: "Pairing cancellation needs the same OS adapter as pairing.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:unpair",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.unpair",
        note: "Unpair/remove-bond needs a per-OS adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:custom-ceremony",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.custom-ceremony",
        note: "No cryptographic handshake is invented here; custom ceremony needs a reviewed profile plus adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "security:pairing-generation",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.pairing-generation",
        note: "Bond-generation tracking rides the pairing adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "gatt:descriptors",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "gatt.descriptor-discovery-read-write",
        note: "Descriptor discovery, reads, and writes are implemented; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
    },
    DesktopCapability {
        id: "gatt:indications",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "gatt.indications",
        note: "Subscribed notification values buffer per consumer and are observable through the take API; the btleplug stream does not distinguish indications from notifications, so delivery kind is reported as unknown.",
        limitation: Some("delivery-kind-unknown"),
    },
    DesktopCapability {
        id: "gatt:service-changed",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "gatt.service-changed",
        note: "Service-changed arrives only where the OS surfaces it (CoreBluetooth); Win/Linux need an adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "gatt:maximum-write-length",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "gatt.maximum-write-length",
        note: "Measured MTU is wired into the core maximum-write-length on every characteristic/descriptor write (fail-closed when unmeasured); a dedicated maximumWriteLength host query needs an adapter.",
        limitation: None,
    },
    DesktopCapability {
        id: "gatt:long-write",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.long-write",
        note: "Prepared-write transactions have no btleplug path; long-write is rejected, never silently single-written.",
        limitation: Some("no-prepared-write-path"),
    },
    DesktopCapability {
        id: "gatt:reliable-write",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.reliable-write",
        note: "Atomic execute has no btleplug path; rejected explicitly per OS availability.",
        limitation: Some("no-atomic-execute-path"),
    },
    DesktopCapability {
        id: "gatt:write-without-response-readiness",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.write-readiness",
        note: "No readiness signal exists on this path; writes are fire-and-forget within OS queue bounds.",
        limitation: Some("no-readiness-signal"),
    },
    DesktopCapability {
        id: "gatt:high-throughput-acquire",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.high-throughput",
        note: "No burst/throughput negotiation path; bounded sequential ops only.",
        limitation: Some("bounded-sequential-only"),
    },
    DesktopCapability {
        id: "background:desktop-maintain-connection",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "background.desktop-maintain",
        note: "The OS keeps the link only while the host process lives; no execution promise beyond OS policy.",
        limitation: Some("process-lifetime-only"),
    },
    DesktopCapability {
        id: "lifecycle:page-persistence",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "lifecycle.page-persistence",
        note: "Page/lease lifecycle across reloads is owned by the host shell, not the radio layer.",
        limitation: Some("shell-owned-lifecycle"),
    },
    DesktopCapability {
        id: "discovery:system-chooser",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "chooser.system",
        note: "Desktop has no system chooser; explicit user selection needs an OS picker adapter.",
        limitation: None,
    },
];

/// Project the parity table into a live core so runtime capability truth
/// matches the report. Provided rows register `limited` (deterministic,
/// unqualified); adapter-needed rows register `unsupported`; limitation
/// candidates register `limited` with their named limitation.
pub fn register_desktop_capabilities(core: &mut Central) -> Result<(), CoreError> {
    for capability in DESKTOP_CAPABILITIES {
        let (state, evidence, limitations): (CapabilityState, EvidenceLevel, Vec<&str>) =
            match capability.verdict {
                CapabilityVerdict::BtleplugProvides | CapabilityVerdict::LimitationCandidate => (
                    CapabilityState::Limited,
                    EvidenceLevel::Deterministic,
                    capability.limitation.into_iter().collect(),
                ),
                CapabilityVerdict::NarrowOsAdapterNeeded => (
                    CapabilityState::Unsupported,
                    EvidenceLevel::Blocked,
                    Vec::from([NOT_IMPLEMENTED]),
                ),
            };
        core.register_capability(CapabilityDescriptor::new(
            capability.id,
            state,
            &[("availability", 1)],
            &limitations,
            &format!("ubm-desktop-capability-{}", capability.id),
            evidence,
            env!("CARGO_PKG_VERSION"),
            "ubm-desktop-capability-manifest-v1",
            &[capability.scenario],
        )?)?;
    }
    Ok(())
}

/// Limitation code for rows with no implementation yet.
const NOT_IMPLEMENTED: &str = "not-implemented";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{DESKTOP_CAPABILITIES, register_desktop_capabilities};
    use ubm_core::central::{Central, CentralConfig};
    use ubm_core::contracts::{
        AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
        BackendInstanceId, Generation,
    };
    use ubm_core::ownership::EffectBatch;

    fn test_core() -> Central {
        let attachment = AttachmentTuple::new(
            AttachmentId::new("test-attachment").expect("id"),
            BackendInstanceId::new("test-backend").expect("id"),
            BackendGeneration::new("test-backend-gen").expect("id"),
            AdapterId::new("test-adapter").expect("id"),
            AdapterGeneration::new("test-adapter-gen").expect("id"),
        );
        let generation = Generation::new("test-gen").expect("generation");
        Central::new(attachment, generation, CentralConfig::default()).expect("core")
    }

    #[test]
    fn every_row_has_a_verdict_and_scenario() {
        assert!(
            !DESKTOP_CAPABILITIES.is_empty(),
            "matrix must not shrink to empty"
        );
        let mut ids = HashSet::new();
        for capability in DESKTOP_CAPABILITIES {
            assert!(!capability.id.is_empty(), "capability id required");
            assert!(!capability.scenario.is_empty(), "scenario required");
            assert!(!capability.note.is_empty(), "report note required");
            assert!(ids.insert(capability.id), "duplicate row {}", capability.id);
            match capability.verdict {
                super::CapabilityVerdict::NarrowOsAdapterNeeded => {
                    assert!(
                        capability.limitation.is_none(),
                        "open work carries no limitation claim"
                    );
                }
                _ => {
                    assert!(
                        capability.limitation.is_some(),
                        "provided/bounded rows name their limitation"
                    );
                }
            }
        }
    }

    /// Machine-checked parity (L7): the code table, the committed
    /// `PARITY_GAPS.md` report, and the frozen backend-contract matrix
    /// agree. A dropped row (L1), a dangling scenario link (L3), or count
    /// drift fails here instead of shipping silently.
    #[test]
    fn parity_report_matches_code_and_frozen_matrix() {
        use std::collections::HashSet;

        use super::CapabilityVerdict;

        let report = include_str!("../PARITY_GAPS.md");
        // Every code row appears in the report with its scenario.
        for capability in DESKTOP_CAPABILITIES {
            assert!(
                report.contains(capability.id),
                "report drops row {}",
                capability.id
            );
            assert!(
                report.contains(capability.scenario),
                "report drops scenario {} for {}",
                capability.scenario,
                capability.id
            );
        }
        // The Counts section matches the code table exactly.
        let mut provides = 0usize;
        let mut narrow = 0usize;
        let mut limitation = 0usize;
        for capability in DESKTOP_CAPABILITIES {
            match capability.verdict {
                CapabilityVerdict::BtleplugProvides => provides += 1,
                CapabilityVerdict::NarrowOsAdapterNeeded => narrow += 1,
                CapabilityVerdict::LimitationCandidate => limitation += 1,
            }
        }
        let total = DESKTOP_CAPABILITIES.len();
        for expected in [
            format!("{total} required desktop rows"),
            format!("{provides} btleplug-provides"),
            format!("{narrow} narrow-OS-adapter-needed"),
            format!("{limitation} preapproved-limitation-candidate"),
        ] {
            assert!(
                report.contains(&expected),
                "report counts drifted: missing `{expected}`"
            );
        }
        // The desktop set covers the frozen matrix minus the explicitly
        // scoped-out mobile roles (read-only reference: the frozen file
        // is never modified by this crate).
        let frozen = include_str!("../../../src/backend-contract/capabilities.ts");
        let mut frozen_ids: HashSet<&str> = HashSet::new();
        let mut quoted = false;
        let mut start = 0usize;
        let bytes = frozen.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'\'' {
                if quoted {
                    let token = &frozen[start..index];
                    if is_capability_id(token) {
                        frozen_ids.insert(token);
                    }
                    quoted = false;
                } else {
                    start = index + 1;
                    quoted = true;
                }
            }
            index += 1;
        }
        assert_eq!(
            frozen_ids.len(),
            38,
            "frozen matrix changed size: update this test and the desktop rows together"
        );
        let scoped_out = [
            "background:apple-restoration",
            "background:android-connected-device-service",
        ];
        for scoped in scoped_out {
            assert!(
                frozen_ids.contains(scoped),
                "frozen matrix lost scoped row {scoped}"
            );
        }
        let desktop_ids: HashSet<&str> = DESKTOP_CAPABILITIES.iter().map(|row| row.id).collect();
        let expected_ids: HashSet<&str> = frozen_ids
            .difference(&scoped_out.into_iter().collect())
            .copied()
            .collect();
        assert_eq!(
            desktop_ids, expected_ids,
            "desktop rows diverged from the frozen matrix"
        );
    }

    /// Frozen capability ids are `namespace:name` (lowercase, dashes).
    fn is_capability_id(token: &str) -> bool {
        let mut parts = token.split(':');
        let head = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        !(parts.next().is_some()
            || head.is_empty()
            || tail.is_empty()
            || !head
                .chars()
                .all(|cell| cell.is_ascii_lowercase() || cell == '-')
            || !tail
                .chars()
                .all(|cell| cell.is_ascii_lowercase() || cell == '-'))
    }

    #[test]
    fn registration_projects_truth_into_the_core() {
        use ubm_core::central::CapabilityAdmission;
        use ubm_core::contracts::BleErrorCode;

        let mut core = test_core();
        register_desktop_capabilities(&mut core).expect("register");
        // The core's own `parity_rows` covers its six generic rows; the
        // desktop catalog projection is verified row by row through the
        // capability gate instead.
        let mut limited = 0usize;
        let mut unsupported = 0usize;
        for capability in DESKTOP_CAPABILITIES {
            match core.check_capability(capability.id, "desktop.probe") {
                Ok(CapabilityAdmission::ProceedWithLimitation) => limited += 1,
                Ok(CapabilityAdmission::Proceed) => {
                    panic!("row {} must carry a limitation", capability.id)
                }
                Err(error) => {
                    assert!(
                        matches!(
                            capability.verdict,
                            super::CapabilityVerdict::NarrowOsAdapterNeeded
                        ),
                        "only open adapter work gates closed, got {}",
                        capability.id
                    );
                    assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
                    unsupported += 1;
                }
            }
        }
        assert!(limited > 0 && unsupported > 0, "both sides present");
        let _ = EffectBatch::new(64);
    }
}
