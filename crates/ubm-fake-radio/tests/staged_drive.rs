//! Staged-drive integration proofs (U7 slice): the REAL `ubm-core` central
//! driven from SYNTHETIC host events only (no BLE hardware). Each test runs
//! a deterministic scripted program through [`StagedDriver::run_step`] and
//! pins exact observation wires. Expectations transcribe the frozen
//! C-UBM.0.1.2-DRAFT rules; the untouched `ubm-core` is the oracle — a
//! mismatch here means the transcription (not the core) is wrong.

use ubm_fake_radio::StagedDriver;

const SVC: &str = "12345678-1234-5678-1234-56789abcdef0";
const CHAR: &str = "12345678-1234-5678-1234-56789abcdef1";
const DESC: &str = "00002902-0000-1000-8000-00805f9b34fb";

fn driver() -> StagedDriver {
    StagedDriver::open().expect("staged open must succeed")
}

fn run(driver: &mut StagedDriver, line: &str) -> String {
    driver.run_step(line)
}

fn ok(line: &str) -> bool {
    line.contains("\"ok\":true")
}

fn setup_link(driver: &mut StagedDriver) {
    assert!(ok(&run(&mut *driver, "{\"step\":\"cap.project\"}")));
    assert!(ok(&run(
        &mut *driver,
        "{\"step\":\"scan.start\",\"op\":\"scan0\",\"owner\":\"owner-a\"}"
    )));
    assert!(ok(&run(
        &mut *driver,
        "{\"step\":\"scan.platform\",\"op\":\"scan0\",\"event\":\"platform-started\"}"
    )));
    assert!(ok(&run(
        &mut *driver,
        "{\"step\":\"peer.advertise\",\"peer\":\"p\",\"domain\":\"platform-guid\",\"value\":\"peer-1\"}"
    )));
    assert!(ok(&run(
        &mut *driver,
        "{\"step\":\"link.connect\",\"peer\":\"p\",\"lease\":\"lease-a\",\"op\":\"conn0\"}"
    )));
    let established = run(
        &mut *driver,
        "{\"step\":\"link.established\",\"peer\":\"p\",\"op\":\"conn0\"}",
    );
    assert!(ok(&established), "{established}");
}

fn discover_db(driver: &mut StagedDriver) -> String {
    let line = format!(
        "{{\"step\":\"gatt.discover\",\"peer\":\"p\",\"owner\":\"lease-a\",\"services\":[{{\"uuid\":\"{SVC}\",\"occurrence\":0,\"characteristics\":[{{\"uuid\":\"{CHAR}\",\"occurrence\":0,\"properties\":\"read+write+notify\",\"descriptors\":[{{\"uuid\":\"{DESC}\",\"occurrence\":0}}]}}]}}]}}"
    );
    run(&mut *driver, &line)
}

/// The flagship vertical program: scan, connect, discover, read, notify,
/// unsubscribe, destroy — all from synthetic events, all receipts real.
#[test]
fn vertical_scan_to_destroy_reports_real_receipts() {
    let mut driver = driver();
    setup_link(&mut driver);
    let discovered = discover_db(&mut driver);
    assert!(ok(&discovered), "{discovered}");
    assert!(discovered.contains("\"paths\":3"));

    let read = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"deadbeef\",\"settle\":\"success\"}",
    );
    assert!(ok(&read), "{read}");
    assert!(read.contains("\"bytes\":\"deadbeef\""));
    assert!(read.contains("\"terminal\":\"succeeded\""));

    let sub = run(
        &mut driver,
        "{\"step\":\"sub.subscribe\",\"op\":\"sub0\",\"path\":1,\"consumer\":\"c0\"}",
    );
    assert!(ok(&sub), "{sub}");
    let enable = run(
        &mut driver,
        "{\"step\":\"sub.settle-enable\",\"path\":1,\"success\":true}",
    );
    assert!(ok(&enable), "{enable}");
    assert!(enable.contains("\"cccd\":true"));

    let notify = run(
        &mut driver,
        "{\"step\":\"sub.notify\",\"path\":1,\"value\":\"0102\"}",
    );
    assert!(ok(&notify), "{notify}");
    assert!(notify.contains("c0=delivered"));

    let take = run(
        &mut driver,
        "{\"step\":\"sub.take\",\"path\":1,\"consumer\":\"c0\"}",
    );
    assert!(ok(&take), "{take}");
    assert!(take.contains("\"bytes\":\"0102\""));

    let unsub = run(
        &mut driver,
        "{\"step\":\"sub.unsubscribe\",\"path\":1,\"consumer\":\"c0\"}",
    );
    assert!(ok(&unsub), "{unsub}");
    let disable = run(&mut driver, "{\"step\":\"sub.settle-disable\",\"path\":1}");
    assert!(ok(&disable), "{disable}");

    let destroy = run(&mut driver, "{\"step\":\"staged.destroy\"}");
    assert!(ok(&destroy), "{destroy}");
    assert!(destroy.contains("\"state\":\"released\""));
}

/// A second physical scan owner fails closed before any radio effect.
#[test]
fn second_scan_owner_fails_with_scan_already_active() {
    let mut driver = driver();
    assert!(ok(&run(&mut driver, "{\"step\":\"cap.project\"}")));
    assert!(ok(&run(
        &mut driver,
        "{\"step\":\"scan.start\",\"op\":\"scan0\",\"owner\":\"owner-a\"}"
    )));
    let second = run(
        &mut driver,
        "{\"step\":\"scan.start\",\"op\":\"scan1\",\"owner\":\"owner-b\"}",
    );
    assert!(!ok(&second), "{second}");
    assert!(
        second.contains("scan.already-active|core|staged-scan-start|scan.arbitration"),
        "{second}"
    );
}

/// Duplicate completions suppress without a second settlement.
#[test]
fn duplicate_completion_suppresses() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let admitted = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"dispatched\"}",
    );
    assert!(ok(&admitted), "{admitted}");
    let first = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\"}",
    );
    assert!(ok(&first), "{first}");
    assert!(first.contains("\"settle\":\"settled\""), "{first}");
    let dup = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\"}",
    );
    assert!(ok(&dup), "{dup}");
    assert!(dup.contains("\"settle\":\"duplicate-suppressed\""), "{dup}");
}

/// `read.taken` reports the driver-side scripted echo — including after
/// database invalidation (the echo never transits the core, so generation
/// changes cannot alter it, even as the core path itself goes stale).
#[test]
fn read_taken_echoes_scripted_bytes_after_invalidation() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let read = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"admitted\"}",
    );
    assert!(ok(&read), "{read}");
    let taken = run(&mut driver, "{\"step\":\"read.taken\",\"op\":\"read0\"}");
    assert!(ok(&taken), "{taken}");
    assert!(taken.contains("\"bytes\":\"aa\""), "{taken}");
    let changed = run(
        &mut driver,
        "{\"step\":\"gatt.services-changed\",\"peer\":\"p\"}",
    );
    assert!(ok(&changed), "{changed}");
    // Post-invalidation the echo is unchanged ...
    let retaken = run(&mut driver, "{\"step\":\"read.taken\",\"op\":\"read0\"}");
    assert!(ok(&retaken), "{retaken}");
    assert!(retaken.contains("\"bytes\":\"aa\""), "{retaken}");
    // ... while the core path itself went stale.
    let dispatch = run(&mut driver, "{\"step\":\"op.dispatch\",\"op\":\"read0\"}");
    assert!(ok(&dispatch), "{dispatch}");
    let late = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\"}",
    );
    assert!(ok(&late), "{late}");
    assert!(late.contains("\"terminal\":\"failed\""), "{late}");
    // Unknown names read null, never an error.
    let ghost = run(&mut driver, "{\"step\":\"read.taken\",\"op\":\"ghost\"}");
    assert!(ok(&ghost), "{ghost}");
    assert!(ghost.contains("\"bytes\":null"), "{ghost}");
}

/// A failed explicit disconnect on a LIVE link retains cleanup ownership
/// per code: the link stays connected, and destroy later reports
/// `release-failed` instead of a silent clean release.
#[test]
fn live_peer_disconnect_failure_reports_release_failed_terminal() {
    for code in [
        "connection-failed",
        "connection-lost",
        "operation-timed-out",
        "adapter-unavailable",
    ] {
        let mut driver = driver();
        setup_link(&mut driver);
        let failed = run(
            &mut driver,
            &format!("{{\"step\":\"link.disconnect-failed\",\"peer\":\"p\",\"code\":\"{code}\"}}"),
        );
        assert!(ok(&failed), "{code}: {failed}");
        assert!(
            failed.contains("\"connection\":\"connected\""),
            "{code}: {failed}"
        );
        // Retained ownership surfaces at destroy: never a clean release
        // after a reported disconnect failure.
        let destroy = run(&mut driver, "{\"step\":\"staged.destroy\"}");
        assert!(ok(&destroy), "{code}: {destroy}");
        assert!(
            destroy.contains("\"state\":\"release-failed\""),
            "{code}: {destroy}"
        );
    }
    // An unknown code fails closed before touching the core.
    let mut driver = driver();
    setup_link(&mut driver);
    let bad = run(
        &mut driver,
        "{\"step\":\"link.disconnect-failed\",\"peer\":\"p\",\"code\":\"nope\"}",
    );
    assert!(!ok(&bad), "{bad}");
    assert!(bad.contains("staged-disconnect-code"), "{bad}");
}

/// Explicit settle ordinals order nothing by themselves: the first valid
/// contender settles, an ignored contender (even with a higher ordinal)
/// blocks nothing, and a descending ordinal after terminal suppresses as
/// a duplicate instead of reordering.
#[test]
fn descending_settle_ordinals_suppress_after_terminal() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let admitted = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"dispatched\"}",
    );
    assert!(ok(&admitted), "{admitted}");
    // A higher-ordinal but invalid contender is ignored and blocks nothing.
    let ignored = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"timeout\",\"valid\":false,\"ordinal\":9}",
    );
    assert!(ok(&ignored), "{ignored}");
    assert!(
        ignored.contains("\"settle\":\"contender-ignored\""),
        "{ignored}"
    );
    // A lower ordinal then settles truthfully ...
    let first = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\",\"ordinal\":2}",
    );
    assert!(ok(&first), "{first}");
    assert!(first.contains("\"settle\":\"settled\""), "{first}");
    assert!(first.contains("\"terminal\":\"succeeded\""), "{first}");
    // ... and a reordered duplicate suppresses without a second settlement.
    let dup = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\",\"ordinal\":1}",
    );
    assert!(ok(&dup), "{dup}");
    assert!(dup.contains("\"settle\":\"duplicate-suppressed\""), "{dup}");
    assert!(dup.contains("\"suppressed\":1"), "{dup}");
}

/// Service change between admission and settlement settles truthfully
/// (stale handle), then rediscovery re-arms the database.
#[test]
fn invalidation_mid_io_settles_stale_then_rediscovers() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let admitted = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"admitted\"}",
    );
    assert!(ok(&admitted), "{admitted}");
    let changed = run(
        &mut driver,
        "{\"step\":\"gatt.services-changed\",\"peer\":\"p\"}",
    );
    assert!(ok(&changed), "{changed}");
    assert!(changed.contains("\"database\":\"changed\""), "{changed}");
    let settle = run(&mut driver, "{\"step\":\"op.dispatch\",\"op\":\"read0\"}");
    assert!(ok(&settle), "{settle}");
    let late = run(
        &mut driver,
        "{\"step\":\"op.settle\",\"op\":\"read0\",\"kind\":\"success\"}",
    );
    assert!(ok(&late), "{late}");
    assert!(late.contains("\"settle\":\"settled\""), "{late}");
    assert!(late.contains("\"terminal\":\"failed\""), "{late}");
    // Stale paths reject new work until rediscovery.
    let reread = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read1\",\"path\":1,\"value\":\"aa\",\"settle\":\"success\"}",
    );
    assert!(!ok(&reread), "{reread}");
    assert!(reread.contains("gatt.stale-handle"), "{reread}");
}

/// Cancel across the dispatch boundary reports the cancel receipt.
#[test]
fn cancel_after_dispatch_reports_receipt() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let admitted = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"dispatched\"}",
    );
    assert!(ok(&admitted), "{admitted}");
    let cancel = run(&mut driver, "{\"step\":\"op.cancel\",\"op\":\"read0\"}");
    assert!(ok(&cancel), "{cancel}");
    assert!(cancel.contains("\"settle\":\"settled\""), "{cancel}");
    assert!(cancel.contains("aborted"), "{cancel}");
}

/// Overflow terminals under `error` policy stay observable via take-terminal.
#[test]
fn overflow_terminal_is_observable() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let sub = run(
        &mut driver,
        "{\"step\":\"sub.subscribe\",\"op\":\"sub0\",\"path\":1,\"consumer\":\"c0\",\"policy\":\"error\",\"items\":1,\"bytes\":128}",
    );
    assert!(ok(&sub), "{sub}");
    assert!(ok(&run(
        &mut driver,
        "{\"step\":\"sub.settle-enable\",\"path\":1,\"success\":true}"
    )));
    // A 200-byte value against a 128-byte budget terminates under `error`.
    let mut big_hex = String::new();
    for _ in 0..200 {
        big_hex.push_str("ab");
    }
    let big_line = format!("{{\"step\":\"sub.notify\",\"path\":1,\"value\":\"{big_hex}\"}}");
    let big = run(&mut driver, &big_line);
    assert!(ok(&big), "{big}");
    let terminal = run(
        &mut driver,
        "{\"step\":\"sub.take-terminal\",\"path\":1,\"consumer\":\"c0\"}",
    );
    assert!(ok(&terminal), "{terminal}");
    assert!(terminal.contains("\"reason\":\"overflow\""), "{terminal}");
}

/// Link loss races explicit disconnect with exactly one terminal result:
/// loss wins, and a later explicit disconnect fails closed (no double
/// release, no resurrection).
#[test]
fn link_loss_reports_lost_state() {
    let mut driver = driver();
    setup_link(&mut driver);
    let loss = run(&mut driver, "{\"step\":\"link.loss\",\"peer\":\"p\"}");
    assert!(ok(&loss), "{loss}");
    assert!(loss.contains("\"state\":\"lost\""), "{loss}");
    // The race is already decided: explicit disconnect after loss fails
    // closed instead of producing a second terminal.
    let disconnect = run(
        &mut driver,
        "{\"step\":\"link.disconnect\",\"peer\":\"p\",\"lease\":\"lease-a\"}",
    );
    assert!(!ok(&disconnect), "{disconnect}");
    assert!(
        disconnect.contains(
            "lifecycle.invalid-state|core|staged-link-disconnect|central.connection.transition"
        ),
        "{disconnect}"
    );
}

/// Capability projection reports the six staged rows as limited.
#[test]
fn capability_projection_reports_six_limited_rows() {
    let mut driver = driver();
    let project = run(&mut driver, "{\"step\":\"cap.project\"}");
    assert!(ok(&project), "{project}");
    let rows = run(&mut driver, "{\"step\":\"cap.rows\"}");
    assert!(ok(&rows), "{rows}");
    assert!(rows.contains("\"count\":6"), "{rows}");
    assert!(rows.contains("central.scan=limited"), "{rows}");
    let check = run(
        &mut driver,
        "{\"step\":\"cap.check\",\"id\":\"central.scan\"}",
    );
    assert!(ok(&check), "{check}");
    assert!(check.contains("proceed-with-limitation"), "{check}");
    let unknown = run(
        &mut driver,
        "{\"step\":\"cap.check\",\"id\":\"central.teleport\"}",
    );
    assert!(!ok(&unknown), "{unknown}");
    assert!(unknown.contains("capability.unavailable"), "{unknown}");
}

/// Lease joins, borrowing, transfer, and revocation flow through the
/// staged driver with exact lease counts.
#[test]
fn lease_borrow_transfer_release_flows() {
    let mut driver = driver();
    setup_link(&mut driver);
    // Without sharing support the second lease fails closed at arbitration
    // with the frozen `connection.already-owned` identity.
    let denied = run(
        &mut driver,
        "{\"step\":\"link.borrow\",\"peer\":\"p\",\"lease\":\"lease-b\",\"op\":\"borrow0\"}",
    );
    assert!(!ok(&denied), "{denied}");
    assert!(
        denied.contains("connection.already-owned|core|staged-link-borrow|connection.arbitration"),
        "{denied}"
    );
    // A fresh link whose host reports sharing support admits the borrow.
    let mut shared = StagedDriver::open().expect("staged open must succeed");
    assert!(ok(&run(&mut shared, "{\"step\":\"cap.project\"}")));
    assert!(ok(&run(
        &mut shared,
        "{\"step\":\"link.sharing\",\"supported\":true}"
    )));
    assert!(ok(&run(
        &mut shared,
        "{\"step\":\"peer.advertise\",\"peer\":\"p\",\"domain\":\"platform-guid\",\"value\":\"peer-1\"}"
    )));
    assert!(ok(&run(
        &mut shared,
        "{\"step\":\"link.connect\",\"peer\":\"p\",\"lease\":\"lease-a\",\"op\":\"conn0\"}"
    )));
    assert!(ok(&run(
        &mut shared,
        "{\"step\":\"link.established\",\"peer\":\"p\",\"op\":\"conn0\"}"
    )));
    let sharing = run(
        &mut shared,
        "{\"step\":\"link.sharing\",\"supported\":true}",
    );
    assert!(ok(&sharing), "{sharing}");
    let mut driver = shared;
    let borrow = run(
        &mut driver,
        "{\"step\":\"link.borrow\",\"peer\":\"p\",\"lease\":\"lease-b\",\"op\":\"borrow0\"}",
    );
    assert!(ok(&borrow), "{borrow}");
    assert!(borrow.contains("\"lease_count\":2"), "{borrow}");
    let transfer = run(
        &mut driver,
        "{\"step\":\"link.transfer\",\"peer\":\"p\",\"source\":\"lease-b\",\"dest\":\"lease-c\"}",
    );
    assert!(ok(&transfer), "{transfer}");
    assert!(transfer.contains("\"lease_count\":2"), "{transfer}");
    // Releasing one of two leases keeps the link (`released:false`); the
    // last release drops it (`released:true`), per the frozen lease rule.
    let release = run(
        &mut driver,
        "{\"step\":\"link.release\",\"peer\":\"p\",\"lease\":\"lease-c\"}",
    );
    assert!(ok(&release), "{release}");
    assert!(release.contains("\"released\":false"), "{release}");
    assert!(release.contains("\"lease_count\":1"), "{release}");
    let release_last = run(
        &mut driver,
        "{\"step\":\"link.release\",\"peer\":\"p\",\"lease\":\"lease-a\"}",
    );
    assert!(ok(&release_last), "{release_last}");
    assert!(release_last.contains("\"released\":true"), "{release_last}");
    assert!(release_last.contains("\"lease_count\":0"), "{release_last}");
}

/// A snapshot `occurrence` that is not a number fails closed instead of
/// coercing to 0 (path selectors already fail closed on bad numbers).
#[test]
fn snapshot_bad_occurrence_fails_closed() {
    let mut driver = driver();
    setup_link(&mut driver);
    let garbage = run(
        &mut driver,
        &format!(
            "{{\"step\":\"gatt.discover\",\"peer\":\"p\",\"owner\":\"lease-a\",\"services\":[{{\"uuid\":\"{SVC}\",\"occurrence\":\"not-a-number\"}}]}}"
        ),
    );
    assert!(!ok(&garbage), "{garbage}");
    assert!(garbage.contains("staged-bad-snapshot"), "{garbage}");
    let negative = run(
        &mut driver,
        &format!(
            "{{\"step\":\"gatt.discover\",\"peer\":\"p\",\"owner\":\"lease-a\",\"services\":[{{\"uuid\":\"{SVC}\",\"occurrence\":-1}}]}}"
        ),
    );
    assert!(!ok(&negative), "{negative}");
    assert!(negative.contains("staged-bad-snapshot"), "{negative}");
}

/// An invalid `settle` mode rejects with zero core side effects: no op is
/// admitted, no name is registered, nothing dispatches — so a retry under
/// the same name still admits and dispatches cleanly.
#[test]
fn invalid_settle_mode_rejects_before_any_core_transition() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    let bad = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"bogus\"}",
    );
    assert!(!ok(&bad), "{bad}");
    assert!(
        bad.contains("argument.invalid|core|staged-gatt-read|staged-settle-mode"),
        "{bad}"
    );
    // No half-admitted op leaks: the same name admits fresh ...
    let retry = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"admitted\"}",
    );
    assert!(ok(&retry), "{retry}");
    // ... and still dispatches, proving the rejected step left nothing
    // behind (never admitted, never dispatched).
    let dispatch = run(&mut driver, "{\"step\":\"op.dispatch\",\"op\":\"read0\"}");
    assert!(ok(&dispatch), "{dispatch}");
}

/// Timeouts settle via expiry sweep with truthful terminals.
#[test]
fn expiry_sweep_settles_timeouts() {
    let mut driver = driver();
    setup_link(&mut driver);
    let _ = discover_db(&mut driver);
    assert!(ok(&run(
        &mut driver,
        "{\"step\":\"clock.set\",\"now\":1000}"
    )));
    let admitted = run(
        &mut driver,
        "{\"step\":\"gatt.read\",\"op\":\"read0\",\"path\":1,\"value\":\"aa\",\"settle\":\"dispatched\",\"timeout_ms\":10}",
    );
    assert!(ok(&admitted), "{admitted}");
    let sweep = run(&mut driver, "{\"step\":\"op.expire-sweep\",\"now\":5000}");
    assert!(ok(&sweep), "{sweep}");
    assert!(sweep.contains("\"settled\":1"), "{sweep}");
}
