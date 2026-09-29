//! Test-only process fence for fixtures with process-owned recording authorities.
//! The parent owns the fresh directory and removes it only after the child has
//! completed every scenario assertion and exited, releasing its cached OS handles.

pub fn isolated_fixture_process(test: &str) -> Option<std::path::PathBuf> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const DIRECTORY_ENV: &str = "UBM_RECORDING_FIXTURE_DIRECTORY";
    const TEST_ENV: &str = "UBM_RECORDING_FIXTURE_TEST";
    if std::env::var(TEST_ENV).ok().as_deref() == Some(test) {
        return Some(std::env::var_os(DIRECTORY_ENV).unwrap().into());
    }
    let directory = std::env::temp_dir().join(format!(
        "ubm-recording-fixture-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let outcome = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(DIRECTORY_ENV, &directory)
        .env(TEST_ENV, test)
        .status()
        .unwrap();
    assert!(
        outcome.success(),
        "fixture scenario {test} failed: {outcome}"
    );
    assert_eq!(
        std::fs::read(directory.join("scenario-complete")).unwrap(),
        b"all assertions passed",
        "the selected child must actually complete its scenario"
    );
    std::fs::remove_dir_all(directory).unwrap();
    None
}

pub fn complete_fixture_process(directory: &std::path::Path) {
    std::fs::write(
        directory.join("scenario-complete"),
        b"all assertions passed",
    )
    .unwrap();
}
