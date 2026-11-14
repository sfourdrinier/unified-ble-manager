pub mod adapter;
pub mod manager;
pub mod peripheral;

mod notification_owner;
mod peer_source;
pub use crate::common::util::{SourceGroupConsumer, SourceGroupError, SourceGroupHandle};
pub use notification_owner::{NotificationActivationIdentity, NotificationActivationOwner};
pub use peer_source::PeerSourceError;
#[cfg(feature = "reclaim-test-support")]
pub use peer_source::ReclaimSourceFixture;
