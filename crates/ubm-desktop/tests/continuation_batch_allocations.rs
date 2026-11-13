use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use ubm_desktop::continuation_journal::{APPEND_BATCH_MAX, ContinuationJournal, JournalQuota};
use ubm_desktop::continuation_outbox::{DataIngressFailure, Outbox, WakeSink};

struct CountingAllocator;
thread_local! { static LARGEST: Cell<Option<usize>> = const { Cell::new(None) }; }
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.with(|largest| {
            if let Some(n) = largest.get() {
                largest.set(Some(n.max(layout.size())));
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LARGEST.with(|largest| {
            if let Some(n) = largest.get() {
                largest.set(Some(n.max(size)));
            }
        });
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

#[test]
fn refused_large_suffix_is_counted_without_large_serialization_allocation() {
    let dir = std::env::temp_dir().join(format!("ubm-batch-allocation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let path = dir.join("journal.sqlite");
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 2,
    };
    #[cfg(windows)]
    let protector = Arc::new(
        ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
    );
    #[cfg(windows)]
    ubm_desktop::continuation_journal::ScanRecoveryStorageProtector::provision_directory(
        protector.as_ref(),
        &dir,
    )
    .unwrap();
    #[cfg(windows)]
    let journal = ContinuationJournal::open_with_scan_recovery_storage(
        &path,
        "r",
        &json!({}),
        quota,
        Some(protector),
    );
    #[cfg(not(windows))]
    let journal = ContinuationJournal::open(&path, "r", &json!({}), quota);
    let journal = Arc::new(journal.unwrap());
    let outbox = Outbox::new(1, Arc::new(NoWake));
    outbox.attach_journal(journal.clone(), json!({})).unwrap();
    outbox.register_journal_consumer("c", json!({})).unwrap();
    outbox
        .push_data(json!({"t":"value", "consumer":"c"}))
        .unwrap();
    let mut records = vec![
        json!({"t":"value", "consumer":"c"}),
        json!({"t":"value", "consumer":"c", "s":"x".repeat(4 * 1024 * 1024)}),
    ];
    records.extend((2..APPEND_BATCH_MAX).map(|_| json!({"t":"value", "consumer":"c"})));
    records.push(json!({"t":"value", "consumer":"c", "s":"y".repeat(4 * 1024 * 1024)}));
    let bytes: u64 = records.iter().map(|v| v.to_string().len() as u64).sum();
    LARGEST.with(|n| n.set(Some(0)));
    let result = outbox.push_data_batch(records);
    let largest = LARGEST.with(|n| n.replace(None).unwrap());
    assert_eq!(result.accepted, 0);
    let rejected = result.rejected.unwrap();
    assert!(
        matches!(rejected.failure, DataIngressFailure::Storage { ref error, .. } if error.kind == "storage.full")
    );
    assert_eq!(rejected.items, (APPEND_BATCH_MAX + 1) as u64);
    assert_eq!(rejected.bytes, bytes);
    assert!(
        largest < 1024 * 1024,
        "largest allocation was {largest} bytes"
    );
    drop(outbox);
    drop(journal);
    std::fs::remove_dir_all(dir).unwrap();
}
