use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation::{
    ContinuationFuture, ContinuationHost, ContinuationSession, NativeContinuation, Result, envelope,
};
use ubm_desktop::continuation_outbox::{
    Observation, Outbox, RecordMatcher, WakeSink, encode_base64,
};

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}
#[derive(Default)]
struct SessionClosed(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
impl Drop for SessionClosed {
    fn drop(&mut self) {
        if let Some(closed) = self.0.get_mut().unwrap().take() {
            closed
                .send(())
                .expect("fixture teardown must await the final owner");
        }
    }
}
struct Session {
    outbox: Outbox,
    journal: Mutex<Option<std::sync::Weak<ubm_desktop::continuation_journal::ContinuationJournal>>>,
    calls: Arc<Mutex<Vec<String>>>,
    write_budgets: Mutex<Vec<u64>>,
    consumers: Mutex<Vec<String>>,
    generation: Mutex<u64>,
    reply: Mutex<Option<Vec<u8>>>,
    fail_write: Mutex<bool>,
    mtu_error: Mutex<Option<Value>>,
    runtime_thread: std::thread::ThreadId,
    worker_threads: Mutex<Vec<std::thread::ThreadId>>,
    blocking_call: Mutex<Option<(String, std::sync::mpsc::Receiver<()>)>>,
    call_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    blocked_observe: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    observe_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    hold_worker_after_observe: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    // Rust drops fields in declaration order: this witness follows Outbox's
    // journal handle and every other field, not merely Session's Drop body.
    closed: SessionClosed,
}
struct Host(Arc<Session>);
impl ContinuationHost for Host {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        Ok(self.0.clone())
    }
}
impl ContinuationSession for Session {
    fn attach_journal(
        &self,
        journal: Arc<ubm_desktop::continuation_journal::ContinuationJournal>,
        context: Value,
    ) -> Result<()> {
        *self.journal.lock().unwrap() = Some(Arc::downgrade(&journal));
        self.worker_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.outbox
            .attach_journal(journal, context)
            .map_err(ubm_desktop::continuation::recording_failure)
    }
    fn register_journal_consumer(&self, consumer: &str, metadata: Value) -> Result<()> {
        self.worker_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.outbox
            .register_journal_consumer(consumer, metadata)
            .map_err(ubm_desktop::continuation::recording_failure)
    }
    fn observe(&self, consumer: &str, matcher: RecordMatcher) -> Result<Observation> {
        if let Some(blocked) = self.blocked_observe.lock().unwrap().take() {
            if let Some(entered) = self.observe_entered.lock().unwrap().take() {
                entered.send(()).unwrap();
            }
            blocked
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        }
        self.worker_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        let observation = self
            .outbox
            .observe(consumer, matcher)
            .map_err(|_| json!({"code":"lifecycle.invalid-state"}))?;
        if let Some((entered, release)) = self.hold_worker_after_observe.lock().unwrap().take() {
            tokio::task::spawn_blocking(move || {
                let _ = entered.send(());
                release
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            });
        }
        Ok(observation)
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            self.worker_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            let blocked = {
                let mut pending = self.blocking_call.lock().unwrap();
                if pending
                    .as_ref()
                    .is_some_and(|(operation, _)| operation == op)
                {
                    pending.take().map(|(_, receiver)| receiver)
                } else {
                    None
                }
            };
            if let Some(receiver) = blocked {
                self.call_entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                if receiver
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .is_err()
                {
                    return envelope(Err(
                        json!({"code":"platform.failure","detail":"runtime could not release blocking storage boundary"}),
                    ));
                }
            }
            self.calls.lock().unwrap().push(op.into());
            let args: Value = serde_json::from_str(args).unwrap();
            let value = match op {
                "connection.request-mtu" => {
                    if let Some(error) = self.mtu_error.lock().unwrap().clone() {
                        return envelope(Err(error));
                    }
                    json!({"mtu":247})
                }
                "gatt.subscribe" => {
                    self.consumers
                        .lock()
                        .unwrap()
                        .push(args["consumer"].as_str().unwrap().into());
                    json!({})
                }
                "session.reconcile" => {
                    json!({"links":[{"peerId":"peer","state":"connected","databaseState":"current","connectionGeneration":"connection-1","databaseGeneration":format!("db-{}",self.generation.lock().unwrap())}],"subscriptions":self.consumers.lock().unwrap().iter().map(|consumer|json!({"consumer":consumer,"state":"live"})).collect::<Vec<_>>()})
                }
                "session.quiesce" => {
                    let loss = self.outbox.seal();
                    json!({"state":"sealed","afterCutoffItems":loss.items,"afterCutoffBytes":loss.bytes})
                }
                "session.continuation-dispose" => {
                    json!({"state":"released","failures":[],"afterCutoffItems":0,"afterCutoffBytes":0})
                }
                "gatt.write" => {
                    self.write_budgets
                        .lock()
                        .unwrap()
                        .push(args["budgetMs"].as_u64().unwrap());
                    if let Some(bytes) = self.reply.lock().unwrap().clone() {
                        self.outbox.push_data(json!({"t":"value","consumer":self.consumers.lock().unwrap().last().unwrap(),"valueB64":encode_base64(&bytes)})).unwrap();
                    }
                    if *self.fail_write.lock().unwrap() {
                        return envelope(Err(
                            json!({"code":"platform.failure","detail":"ATT refused"}),
                        ));
                    }
                    json!({})
                }
                _ => json!({}),
            };
            envelope(Ok(value))
        })
    }
    fn drain(&self, items: u32, bytes: u32) -> ContinuationFuture<'_> {
        Box::pin(async move {
            self.outbox
                .drain(items as usize, bytes as usize)
                .to_string()
        })
    }
}
fn fixture() -> (NativeContinuation, Arc<Session>, String) {
    let session = Arc::new(Session {
        outbox: Outbox::new(1, Arc::new(NoWake)),
        journal: Mutex::default(),
        calls: Arc::default(),
        write_budgets: Mutex::default(),
        consumers: Mutex::default(),
        generation: Mutex::new(1),
        reply: Mutex::new(Some(vec![240, 2, 0, 0])),
        fail_write: Mutex::new(false),
        mtu_error: Mutex::new(None),
        runtime_thread: std::thread::current().id(),
        worker_threads: Mutex::default(),
        blocking_call: Mutex::default(),
        call_entered: Mutex::default(),
        blocked_observe: Mutex::default(),
        observe_entered: Mutex::default(),
        hold_worker_after_observe: Mutex::default(),
        closed: SessionClosed::default(),
    });
    let selector = json!({"serviceUuid":"0000180d-0000-1000-8000-00805f9b34fb","characteristicUuid":"00002a37-0000-1000-8000-00805f9b34fb"});
    let declaration=json!({"onAppearance":"native","peerId":"peer","resubscribe":[selector.clone()],"setup":[{"selector":selector,"value":[2,0],"timeoutMs":20,"response":{"subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":4,"status":{"offset":3,"accepted":[0]}}}]}).to_string();
    (
        NativeContinuation::new(Arc::new(Host(session.clone()))),
        session,
        declaration,
    )
}

async fn close_fixture(
    engine: NativeContinuation,
    session: Arc<Session>,
) -> std::sync::Weak<ubm_desktop::continuation_journal::ContinuationJournal> {
    let journal = session.journal.lock().unwrap().clone().unwrap();
    let (closed, completion) = tokio::sync::oneshot::channel();
    *session.closed.0.lock().unwrap() = Some(closed);
    engine.stop_recovery();
    // Timed-out blocking work may still own Session; its final retirement, not
    // gate release, is the fence. The process cache retains the inactive journal.
    drop(engine);
    drop(session);
    tokio::time::timeout(std::time::Duration::from_secs(2), completion)
        .await
        .expect("all fixture workers must release their session")
        .expect("final session field must publish completion");
    journal
}

fn assert_fixture_owner_retired(
    journal: std::sync::Weak<ubm_desktop::continuation_journal::ContinuationJournal>,
) {
    assert_eq!(
        journal.strong_count(),
        1,
        "only the inactive process cache may retain the retired fixture journal"
    );
}

/// Parent-owned cleanup follows process exit; the child still proves retirement
/// of every async worker/session before marking the original scenario complete.
fn isolated_fixture_process(test: &str) -> Option<std::path::PathBuf> {
    const DIRECTORY_ENV: &str = "UBM_SETUP_FIXTURE_DIRECTORY";
    const TEST_ENV: &str = "UBM_SETUP_FIXTURE_TEST";
    if std::env::var(TEST_ENV).ok().as_deref() == Some(test) {
        return Some(std::env::var_os(DIRECTORY_ENV).unwrap().into());
    }
    let directory = std::env::temp_dir().join(format!(
        "ubm-setup-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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
        b"all assertions passed"
    );
    std::fs::remove_dir_all(directory).unwrap();
    None
}

fn complete_fixture_process(directory: &std::path::Path) {
    std::fs::write(
        directory.join("scenario-complete"),
        b"all assertions passed",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_deadline_includes_blocked_durable_observer_registration() {
    let Some(root) =
        isolated_fixture_process("setup_deadline_includes_blocked_durable_observer_registration")
    else {
        return;
    };
    let (engine, session, declaration) = fixture();
    let directory = root.join(format!(
        "ubm-observe-deadline-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration).unwrap();
    order["recording"] = json!({"id":"observe-deadline","maxBytes":1048576,"maxRecords":1000});
    order["setup"][0]["timeoutMs"] = json!(40);
    let (release, blocked) = std::sync::mpsc::channel();
    let (entered, started) = tokio::sync::oneshot::channel();
    *session.blocked_observe.lock().unwrap() = Some(blocked);
    *session.observe_entered.lock().unwrap() = Some(entered);
    let worker = engine.clone();
    let running = tokio::spawn(async move { worker.execute("peer", &order.to_string()).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(500), running)
        .await
        .expect("step deadline must include observer admission")
        .unwrap()
        .unwrap_err();
    assert_eq!(outcome["code"], "operation.timed-out");
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "gatt.write")
            .count(),
        0,
        "a timed-out observer cannot dispatch a late setup write"
    );
    release.send(()).unwrap();
    assert_fixture_owner_retired(close_fixture(engine, session).await);
    complete_fixture_process(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_setup_write_receives_only_the_budget_remaining_after_observer_admission() {
    let Some(root) = isolated_fixture_process(
        "durable_setup_write_receives_only_the_budget_remaining_after_observer_admission",
    ) else {
        return;
    };
    let (engine, session, declaration) = fixture();
    let directory = root.join(format!(
        "ubm-setup-budget-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    engine.configure_recording_directory(&directory).unwrap();
    let mut order: Value = serde_json::from_str(&declaration).unwrap();
    order["recording"] = json!({"id":"setup-budget","maxBytes":1048576,"maxRecords":1000});
    order["setup"][0]["timeoutMs"] = json!(1000);
    let (release, blocked) = std::sync::mpsc::channel();
    let (entered, started) = tokio::sync::oneshot::channel();
    *session.blocked_observe.lock().unwrap() = Some(blocked);
    *session.observe_entered.lock().unwrap() = Some(entered);
    let worker = engine.clone();
    let running = tokio::spawn(async move { worker.execute("peer", &order.to_string()).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    release.send(()).unwrap();
    running.await.unwrap().unwrap();
    let budgets = session.write_budgets.lock().unwrap().clone();
    assert_eq!(budgets.len(), 1);
    assert!(
        budgets[0] > 0 && budgets[0] < 950,
        "actual budget was {} ms",
        budgets[0]
    );
    assert_fixture_owner_retired(close_fixture(engine, session).await);
    complete_fixture_process(&root);
}

#[test]
fn queued_durable_setup_write_cannot_start_after_its_deadline() {
    let Some(root) =
        isolated_fixture_process("queued_durable_setup_write_cannot_start_after_its_deadline")
    else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (engine, session, declaration) = fixture();
        let directory = root.join(format!(
            "ubm-queued-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        engine.configure_recording_directory(&directory).unwrap();
        let mut order: Value = serde_json::from_str(&declaration).unwrap();
        order["recording"] = json!({"id":"queued-write","maxBytes":1048576,"maxRecords":1000});
        order["setup"][0]["timeoutMs"] = json!(100);
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        *session.hold_worker_after_observe.lock().unwrap() = Some((entered, blocked));
        let worker = engine.clone();
        let running = tokio::spawn(async move { worker.execute("peer", &order.to_string()).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        let failure = tokio::time::timeout(std::time::Duration::from_secs(1), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(failure["code"], "operation.timed-out");
        let calls = Arc::clone(&session.calls);
        release.send(()).unwrap();
        let journal = close_fixture(engine, session).await;
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|op| op.as_str() == "gatt.write")
                .count(),
            0,
            "a queued worker must recheck the deadline before native dispatch"
        );
        assert_fixture_owner_retired(journal);
    });
    complete_fixture_process(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn native_session_and_ack_observer_never_run_storage_on_runtime_thread() {
    let Some(root) = isolated_fixture_process(
        "native_session_and_ack_observer_never_run_storage_on_runtime_thread",
    ) else {
        return;
    };
    for operation in [
        "connection.connect",
        "gatt.subscribe",
        "session.reconcile",
        "session.quiesce",
        "session.continuation-dispose",
    ] {
        let (engine, session, declaration) = fixture();
        let directory = root.join(format!(
            "ubm-storage-thread-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        engine.configure_recording_directory(&directory).unwrap();
        let mut order: Value = serde_json::from_str(&declaration).unwrap();
        order["recording"] = json!({"id":"thread-test","maxBytes":1048576,"maxRecords":1000});
        order["setup"][0]["timeoutMs"] = json!(1000);
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, started) = tokio::sync::oneshot::channel();
        let order = order.to_string();
        if matches!(
            operation,
            "session.reconcile" | "session.quiesce" | "session.continuation-dispose"
        ) {
            engine.execute("peer", &order).await.unwrap();
        }
        let token = if operation == "session.continuation-dispose" {
            Some(
                engine.prepare_claim(256, 65536).await.unwrap()["claimToken"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        } else {
            None
        };
        *session.blocking_call.lock().unwrap() = Some((operation.to_owned(), blocked));
        *session.call_entered.lock().unwrap() = Some(entered);
        let worker = engine.clone();
        let running = tokio::spawn(async move {
            match operation {
                "session.quiesce" => worker.prepare_claim(256, 65536).await,
                "session.continuation-dispose" => {
                    worker.acknowledge_claim(token.as_deref().unwrap()).await
                }
                _ => worker.execute("peer", &order).await,
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started)
            .await
            .expect("target operation must actually enter")
            .unwrap();
        release
            .send(())
            .expect("the runtime remained responsive while native storage boundary was blocked");
        running.await.unwrap().unwrap();
        assert!(
            session
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == operation),
            "{operation} must execute through the real engine dispatcher"
        );
        let claim = engine.prepare_claim(256, 65536).await.unwrap();
        let receipt = engine
            .acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(receipt["disposed"], true);
        let threads = session.worker_threads.lock().unwrap().clone();
        assert!(!threads.is_empty());
        assert!(
            threads
                .iter()
                .all(|thread| *thread != session.runtime_thread)
        );
        assert_fixture_owner_retired(close_fixture(engine, session).await);
    }
    complete_fixture_process(&root);
}

#[tokio::test]
async fn link_negotiation_precedes_setup_and_reports_actual_mtu_once_per_connection() {
    let (engine, session, declaration) = fixture();
    let mut order: Value = serde_json::from_str(&declaration).unwrap();
    order["link"] = json!({"mtu":{"requested":512,"timeoutMs":10000,"onUnsupported":"continue"}});
    let outcome = engine.execute("peer", &order.to_string()).await.unwrap();
    assert_eq!(
        outcome["link"],
        json!({"mtu":{"requested":512,"outcome":"negotiated","mtu":247}})
    );
    *session.generation.lock().unwrap() = 2;
    engine.execute("peer", &order.to_string()).await.unwrap();
    let calls = session.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.as_str() == "connection.request-mtu")
            .count(),
        1
    );
    assert!(
        calls
            .iter()
            .position(|call| call == "connection.request-mtu")
            < calls.iter().position(|call| call == "gatt.write")
    );
}

#[tokio::test]
async fn only_explicit_unsupported_policy_can_continue_past_link_refusal() {
    for (code, policy, success) in [
        ("capability.unsupported", "continue", true),
        ("capability.unsupported", "fail", false),
        ("platform.failure", "continue", false),
    ] {
        let (engine, session, declaration) = fixture();
        *session.mtu_error.lock().unwrap() = Some(
            json!({"code":code,"domain":"capability","operation":"connection.request-mtu","detail":"native refusal"}),
        );
        let mut order: Value = serde_json::from_str(&declaration).unwrap();
        order["link"] = json!({"mtu":{"requested":512,"timeoutMs":10000,"onUnsupported":policy}});
        let result = engine.execute("peer", &order.to_string()).await;
        assert_eq!(result.is_ok(), success);
        if success {
            assert_eq!(
                result.unwrap()["link"]["mtu"]["error"]["detail"],
                "native refusal"
            );
        } else {
            assert!(engine.execute("peer", &order.to_string()).await.is_err());
            let calls = session.calls.lock().unwrap();
            assert!(!calls.iter().any(|call| call == "gatt.write"));
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.as_str() == "connection.request-mtu")
                    .count(),
                1,
                "refused prerequisite remains fenced to its connection generation"
            );
        }
    }
}
#[tokio::test]
async fn early_application_reply_is_retained_and_setup_runs_once_per_generation() {
    let (engine, session, declaration) = fixture();
    engine.execute("peer", &declaration).await.unwrap();
    engine.execute("peer", &declaration).await.unwrap();
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.as_str() == "gatt.write")
            .count(),
        1
    );
    assert_eq!(
        session.outbox.drain(10, 4096)["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn application_success_does_not_override_att_failure_or_allow_same_generation_retry() {
    let (engine, session, declaration) = fixture();
    *session.fail_write.lock().unwrap() = true;
    assert_eq!(
        engine.execute("peer", &declaration).await.unwrap_err()["detail"],
        "ATT refused"
    );
    *session.fail_write.lock().unwrap() = false;
    assert!(engine.execute("peer", &declaration).await.is_err());
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.as_str() == "gatt.write")
            .count(),
        1
    );
    *session.generation.lock().unwrap() = 2;
    session.consumers.lock().unwrap().clear();
    engine.execute("peer", &declaration).await.unwrap();
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.as_str() == "gatt.write")
            .count(),
        2
    );
}
#[tokio::test]
async fn matching_rejection_is_specific_and_unrelated_reply_times_out() {
    for (reply, code) in [
        (vec![240, 2, 0, 6], "platform.failure"),
        (vec![240, 3, 0, 0], "operation.timed-out"),
    ] {
        let (engine, session, declaration) = fixture();
        *session.reply.lock().unwrap() = Some(reply);
        assert_eq!(
            engine.execute("peer", &declaration).await.unwrap_err()["code"],
            code
        );
        assert_eq!(
            session.outbox.drain(10, 4096)["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn optional_final_byte_rejects_multipart_reply_and_accepts_absent_or_zero() {
    for (reply, success) in [
        (vec![240, 2, 0, 0], true),
        (vec![240, 2, 0, 0, 0], true),
        (vec![240, 2, 0, 0, 1], false),
    ] {
        let (engine, session, declaration) = fixture();
        let mut declaration: Value = serde_json::from_str(&declaration).unwrap();
        declaration["setup"][0]["response"]["maxLength"] = json!(5);
        declaration["setup"][0]["response"]["trailing"] = json!({"offset":4,"accepted":[0]});
        *session.reply.lock().unwrap() = Some(reply);
        assert_eq!(
            engine
                .execute("peer", &declaration.to_string())
                .await
                .is_ok(),
            success
        );
    }
}

#[tokio::test]
async fn malformed_setup_is_rejected_before_native_session_opens() {
    for (key, value) in [
        ("value", json!([])),
        ("value", json!([256])),
        ("timeoutMs", json!(20001)),
        ("unknown", json!(true)),
        (
            "response",
            json!({"subscriptionIndex":1,"prefix":[240],"minLength":2,"maxLength":2,"status":{"offset":1,"accepted":[0]}}),
        ),
    ] {
        let (engine, session, declaration) = fixture();
        let mut declaration: Value = serde_json::from_str(&declaration).unwrap();
        declaration["setup"][0][key] = value;
        assert_eq!(
            engine
                .execute("peer", &declaration.to_string())
                .await
                .unwrap_err()["code"],
            "argument.invalid"
        );
        assert!(session.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn changed_setup_cannot_replace_owned_declaration_and_no_reply_step_still_writes() {
    let (engine, session, declaration) = fixture();
    let mut declaration: Value = serde_json::from_str(&declaration).unwrap();
    declaration["setup"][0]
        .as_object_mut()
        .unwrap()
        .remove("response");
    *session.reply.lock().unwrap() = None;
    engine
        .execute("peer", &declaration.to_string())
        .await
        .unwrap();
    declaration["setup"][0]["value"] = json!([3, 0]);
    assert_eq!(
        engine
            .execute("peer", &declaration.to_string())
            .await
            .unwrap_err()["code"],
        "lifecycle.invalid-state"
    );
    assert_eq!(
        session
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.as_str() == "gatt.write")
            .count(),
        1
    );
}
