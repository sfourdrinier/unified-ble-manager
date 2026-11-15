use super::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum Observation {
    Status(AsyncStatus),
    Unavailable,
}
struct Control(Arc<Mutex<Observation>>);
impl DiscoveryControl for Control {
    fn pending(&self) -> windows::core::Result<bool> {
        match *self.0.lock().unwrap() {
            Observation::Status(status) => Ok(discovery_status_is_pending(status)),
            Observation::Unavailable => Err(windows::core::Error::from_hresult(
                windows::core::HRESULT(0x80004005_u32 as i32),
            )),
        }
    }
    fn cancel(&self) -> windows::core::Result<()> {
        panic!("discovery must not cancel")
    }
}

#[test]
fn retained_query_needs_quiescence_even_after_canceled_or_unavailable_status() {
    let status = Arc::new(Mutex::new(Observation::Status(AsyncStatus::Started)));
    let pool = OperationPool::default();
    drop(pool.admit(
        DiscoveryOperation(Arc::new(Control(status.clone()))),
        false,
        "query",
    ));
    assert!(pool.retire().is_err());
    *status.lock().unwrap() = Observation::Status(AsyncStatus::Canceled);
    assert!(pool.retire().is_err());
    *status.lock().unwrap() = Observation::Unavailable;
    assert!(pool.retire().is_err());
    *status.lock().unwrap() = Observation::Status(AsyncStatus::Completed);
    assert!(pool.retire().is_ok());
}
