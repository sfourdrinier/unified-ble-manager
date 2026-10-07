//! Native mobile session owns admission, copied payloads and the readiness wait.
mod common;
use common::*;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use ubm_mobile::{
    Advertisement, MobilePlatform, MobileSession, RadioCompletion, RadioIngress, RadioRequest,
    RequestKind,
};

fn submit(
    session: &MobileSession,
    op: &str,
    args: Value,
) -> tokio::sync::oneshot::Receiver<String> {
    let args = admitted_args(session, op, &args.to_string());
    let (tx, rx) = tokio::sync::oneshot::channel();
    session.invoke(
        op,
        &args,
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    rx
}

async fn opened() -> (
    Arc<ubm_mobile::MobileHost>,
    MobileSession,
    Arc<Scripted>,
    Arc<AtomicBool>,
) {
    let ready = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&ready);
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::Discover { .. } => {
            let mut services = polar_services();
            services[0].characteristics[0].properties.write = true;
            services[0].characteristics[0]
                .properties
                .write_without_response = true;
            Reply::Now(RadioCompletion::Discovered(services))
        }
        RadioRequest::ReadWriteReadiness { .. } => {
            Reply::Now(RadioCompletion::Ready(observed.load(Ordering::SeqCst)))
        }
        other => polar_responder(other),
    }));
    let (host, _) = open(&radio, MobilePlatform::Apple).await;
    let session = host.open_session("readiness-owner").unwrap();
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: POLAR.into(),
        ..Advertisement::default()
    }));
    ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId":POLAR,"lease":"owner","operationId":"connect"}).to_string(),
    )
    .await);
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId":POLAR,"lease":"owner","operationId":"discover"}).to_string(),
    )
    .await);
    (host, session, radio, ready)
}

async fn wait_for_probe(radio: &Scripted) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while radio.count(RequestKind::ReadWriteReadiness) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native readiness probe");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_admission_keeps_ready_write_before_an_ordinary_write() {
    let (host, session, radio, ready) = opened().await;
    let first = submit(
        &session,
        "gatt.write-when-ready",
        json!({"peerId":POLAR,"selector":selector(),"valueB64":"Kg==","mode":"without-response","operationId":"first","budgetMs":5000}),
    );
    let second = submit(
        &session,
        "gatt.write",
        json!({"peerId":POLAR,"selector":selector(),"valueB64":"Ag==","mode":"without-response","operationId":"second","budgetMs":5000}),
    );
    wait_for_probe(&radio).await;
    assert_eq!(radio.count(RequestKind::Write), 0);
    ready.store(true, Ordering::SeqCst);
    host.ingest(RadioIngress::WriteReadiness {
        peer_id: POLAR.into(),
        ready: true,
    });
    assert_eq!(
        ok(&tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap())["commitState"],
        "unknown"
    );
    assert_eq!(
        ok(&tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap())["commitState"],
        "unknown"
    );
    let values: Vec<Vec<u8>> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            RadioRequest::Write { value, .. } => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(values, vec![vec![42], vec![2]]);
    ok(&call(&session, "session.dispose", "{}").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_change_settles_waiting_write_without_a_readiness_edge() {
    let (host, session, radio, _) = opened().await;
    let first = submit(
        &session,
        "gatt.write-when-ready",
        json!({"peerId":POLAR,"selector":selector(),"valueB64":"Kg==","mode":"without-response","operationId":"first","budgetMs":5000}),
    );
    wait_for_probe(&radio).await;
    host.ingest(RadioIngress::ServicesChanged {
        peer_id: POLAR.into(),
    });
    let failure = parse(
        &tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(failure["ok"], false, "{failure}");
    assert_eq!(failure["error"]["code"], "gatt.stale-handle", "{failure}");
    assert_eq!(failure["commit"], "not-dispatched", "{failure}");
    assert_eq!(radio.count(RequestKind::Write), 0);
    ok(&call(&session, "session.dispose", "{}").await);
}
