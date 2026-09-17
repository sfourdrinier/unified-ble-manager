//! Core-side scan residuals found during R14/R15.
//!
//! Both residuals are root-caused to `ubm-core` scan/session handling:
//!
//! * Residual 1: `cancel -> stop -> restart` is refused with
//!   `scan.already-active`. When the kernel op is reaped before
//!   `note_scan_platform` runs, the session never advances (stays
//!   `Stopping`) because session advancement is coupled to the kernel
//!   call inside `Central::note_scan_platform`.
//! * Residual 2: a radio start landing after its timeout still effects an
//!   ownerless live OS scan. At core level this is observable as a late
//!   `platform_scan_started` activating (`Starting -> Active`) a session
//!   whose backing op already died (timed out / cancelled / reaped), and
//!   as `expire_sweep` leaving a `Starting` session live behind a timed-out
//!   op. The desktop timeout arm cannot distinguish failure from late
//!   success, so the core must fail closed here.

use ubm_core::central::{
    Central, CentralConfig, ScanPlatformEvent, ScanSessionState, validate_scan_request,
};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, Generation,
};
use ubm_core::ownership::{EffectBatch, OpStateView};

fn fixture_central() -> Central {
    let attachment = AttachmentTuple::new(
        AttachmentId::new("attach-01").expect("attachment id"),
        BackendInstanceId::new("backend-01").expect("backend id"),
        BackendGeneration::new("bg-3").expect("backend generation"),
        AdapterId::new("adapter-01").expect("adapter id"),
        AdapterGeneration::new("ag-2").expect("adapter generation"),
    );
    Central::new(
        attachment,
        Generation::new("kernel-gen-1").expect("generation"),
        CentralConfig::default(),
    )
    .expect("central")
}

fn batch() -> EffectBatch {
    EffectBatch::new(64)
}

fn start_request(timeout_ms: u64) -> ubm_core::central::ScanRequest {
    validate_scan_request(&[], "all", "none", timeout_ms, false, &[]).expect("scan request")
}

/// Residual 1: a cancelled-then-reaped scan that is stopped must settle to
/// `Stopped` (not wedge in `Stopping`), so a fresh start is admitted.
#[test]
fn residual1_cancelled_reaped_stop_restarts_cleanly() {
    let mut central = fixture_central();
    let mut out = batch();
    let request = start_request(5000);
    let id = central
        .start_scan(&request, None, "owner-a", 1000, &mut out)
        .expect("start scan");
    central
        .platform_scan_started(&id)
        .expect("platform started");
    let _ = central.cancel_op(&id, 1001, &mut out).expect("cancel op");
    central
        .report_release_success(&id)
        .expect("reap cancelled op");
    central.stop_scan(&id, 1002, &mut out).expect("stop scan");
    assert_eq!(
        central.scan_session_state(&id),
        Some(ScanSessionState::Stopping),
        "stop requested"
    );
    let state = central
        .note_scan_platform(&id, ScanPlatformEvent::PlatformStopped, 1003, &mut out)
        .expect("platform confirm advances a reaped session");
    assert_eq!(state, ScanSessionState::Stopped, "session stopped");
    central
        .start_scan(&request, None, "owner-a", 1004, &mut out)
        .expect("restart admitted after cancelled-then-stopped scan");
}

/// Residual 1, failed leg: the same reaped-op decoupling holds for the
/// `StopFailed -> Failed` terminal step.
#[test]
fn residual1_cancelled_reaped_stop_failure_reaches_failed() {
    let mut central = fixture_central();
    let mut out = batch();
    let request = start_request(5000);
    let id = central
        .start_scan(&request, None, "owner-a", 1000, &mut out)
        .expect("start scan");
    central
        .platform_scan_started(&id)
        .expect("platform started");
    let _ = central.cancel_op(&id, 1001, &mut out).expect("cancel op");
    central
        .report_release_success(&id)
        .expect("reap cancelled op");
    central.stop_scan(&id, 1002, &mut out).expect("stop scan");
    let state = central
        .note_scan_platform(&id, ScanPlatformEvent::StopFailed, 1003, &mut out)
        .expect("stop failure advances a reaped session");
    assert_eq!(state, ScanSessionState::Failed, "session failed");
    central
        .start_scan(&request, None, "owner-a", 1004, &mut out)
        .expect("restart admitted after failed stop");
}

/// Residual 2: a kernel timeout must fail the `Starting` scan session, so no
/// live ownerless session survives behind the timed-out op.
#[test]
fn residual2_expire_sweep_fails_timed_out_scan_session() {
    let mut central = fixture_central();
    let mut out = batch();
    let request = start_request(500);
    let id = central
        .start_scan(&request, None, "owner-a", 1000, &mut out)
        .expect("start scan");
    let (settled, _) = central.expire_sweep(1501, &mut out).expect("sweep");
    assert_eq!(settled, 1, "scan op timed out");
    assert_eq!(
        central.operation_state(&id),
        Some(OpStateView::Terminal(
            ubm_core::contracts::OperationTerminalKind::TimedOut
        )),
        "backing op timed out"
    );
    assert_eq!(
        central.scan_session_state(&id),
        Some(ScanSessionState::Failed),
        "timed-out start fails the session"
    );
    central
        .start_scan(&request, None, "owner-a", 1502, &mut out)
        .expect("restart admitted after timed-out start");
}

/// Residual 2: a radio start landing after its timeout must never activate
/// an ownerless session (`Starting -> Active` on a dead backing op). The
/// late start fails closed and leaves the session terminal.
#[test]
fn residual2_late_start_after_timeout_never_activates() {
    let mut central = fixture_central();
    let mut out = batch();
    let request = start_request(500);
    let id = central
        .start_scan(&request, None, "owner-a", 1000, &mut out)
        .expect("start scan");
    let (settled, _) = central.expire_sweep(1501, &mut out).expect("sweep");
    assert_eq!(settled, 1, "scan op timed out");
    let late = central.platform_scan_started(&id);
    assert!(
        late.is_err(),
        "late radio start after timeout must fail closed, got {late:?}"
    );
    assert_ne!(
        central.scan_session_state(&id),
        Some(ScanSessionState::Active),
        "no ownerless active session"
    );
    assert!(
        central
            .scan_session_state(&id)
            .is_some_and(|state| state.is_terminal()),
        "late start leaves the session terminal"
    );
    central
        .start_scan(&request, None, "owner-a", 1502, &mut out)
        .expect("restart admitted after late start");
}

/// Residual 2, cancelled leg: a late start after cancel+reap must also fail
/// closed instead of activating an ownerless session.
#[test]
fn residual2_late_start_after_cancel_reap_never_activates() {
    let mut central = fixture_central();
    let mut out = batch();
    let request = start_request(5000);
    let id = central
        .start_scan(&request, None, "owner-a", 1000, &mut out)
        .expect("start scan");
    let _ = central.cancel_op(&id, 1001, &mut out).expect("cancel op");
    central
        .report_release_success(&id)
        .expect("reap cancelled op");
    let late = central.platform_scan_started(&id);
    assert!(
        late.is_err(),
        "late radio start after cancel+reap must fail closed, got {late:?}"
    );
    assert_ne!(
        central.scan_session_state(&id),
        Some(ScanSessionState::Active),
        "no ownerless active session"
    );
}
