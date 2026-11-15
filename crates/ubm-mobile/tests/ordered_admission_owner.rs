use std::sync::Arc;
use ubm_mobile::{
    HostOptions, MobileHost, MobilePlatform, OrderedAdmissionOwner, PlatformRadio, RadioRequest,
    WakeSink,
};

struct OwnerRadio(OrderedAdmissionOwner);
impl PlatformRadio for OwnerRadio {
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.0)
    }
    fn submit(&self, _: RadioRequest) {
        panic!("host open must not submit radio work")
    }
    fn cancel(&self, _: u64) {}
}
struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_hosts_preserve_the_native_owners_identity() {
    let first =
        OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abc").unwrap();
    let replacement =
        OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abd").unwrap();
    async fn open(id: OrderedAdmissionOwner) -> MobileHost {
        MobileHost::open(
            Arc::new(OwnerRadio(id)),
            Arc::new(NoWake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "identity-test".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap()
    }
    let a = open(first).await;
    let b = open(first).await;
    let c = open(replacement).await;
    assert_eq!(a.ordered_admission_owner(), b.ordered_admission_owner());
    assert_ne!(a.ordered_admission_owner(), c.ordered_admission_owner());
}
