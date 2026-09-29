use serde_json::json;
use std::sync::Arc;
use ubm_desktop::continuation_outbox::{Outbox, WakeSink};

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

#[tokio::test]
async fn observation_is_armed_before_ingress_and_never_consumes_retained_records() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    let mut observed = outbox
        .observe(
            "current",
            Arc::new(|record| record["valueB64"] == "8AIAAA=="),
        )
        .unwrap();
    outbox
        .push_data(json!({"t":"value","consumer":"old","valueB64":"8AIAAA=="}))
        .unwrap();
    outbox
        .push_data(json!({"t":"value","consumer":"current","valueB64":"AQ=="}))
        .unwrap();
    assert!(observed.receiver.try_recv().is_err());
    outbox
        .push_data(json!({"t":"value","consumer":"current","valueB64":"8AIAAA=="}))
        .unwrap();
    assert_eq!(
        (&mut observed.receiver).await.unwrap()["consumer"],
        "current"
    );
    assert_eq!(
        outbox.drain(10, 4096)["records"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn dropped_observation_releases_admission_and_seal_refuses_new_observation() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    let observation = outbox.observe("current", Arc::new(|_| true)).unwrap();
    assert!(outbox.observe("other", Arc::new(|_| true)).is_err());
    drop(observation);
    assert!(outbox.observe("other", Arc::new(|_| true)).is_ok());
    outbox.seal();
    assert!(outbox.observe("current", Arc::new(|_| true)).is_err());
}

#[tokio::test]
async fn terminal_closes_pending_observation_without_consuming_the_terminal() {
    let outbox = Outbox::new(1, Arc::new(NoWake));
    let mut observation = outbox.observe("current", Arc::new(|_| false)).unwrap();
    outbox.push_control(json!({"t":"stream-end","consumer":"current","reason":"source-failed"}));
    assert_eq!(
        observation.receiver.try_recv().unwrap()["reason"],
        "source-failed"
    );
    assert_eq!(
        outbox.drain(1, 4096)["records"][0]["reason"],
        "source-failed"
    );
}
