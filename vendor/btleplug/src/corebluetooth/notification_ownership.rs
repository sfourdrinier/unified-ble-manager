// vendor/btleplug/src/corebluetooth/notification_ownership.rs
//! Shared physical notification ownership for independently acquired claims.

use std::collections::HashMap;
use std::future::Future;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Active,
    Indeterminate,
}

#[derive(Debug, Default)]
pub(super) struct NotificationOwnership {
    epochs: Mutex<HashMap<u64, HashMap<u64, Claim>>>,
}

impl NotificationOwnership {
    pub(super) async fn acquire<F, Fut>(
        &self,
        epoch: u64,
        claim: u64,
        enable: F,
    ) -> crate::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = crate::Result<()>>,
    {
        let mut epochs = self.epochs.lock().await;
        let total_claims: usize = epochs.values().map(HashMap::len).sum();
        if let Some(existing) = epochs.get(&epoch).and_then(|claims| claims.get(&claim)) {
            return match existing {
                Claim::Active => Ok(()),
                Claim::Indeterminate => Err(indeterminate()),
            };
        }
        if total_claims >= 4096 {
            return Err(crate::Error::Platform(crate::PlatformError::new(
                "corebluetooth",
                "notification-claim-capacity",
                "Notification claim capacity is full",
            )));
        }
        let claims = epochs.entry(epoch).or_default();
        if claims.values().any(|state| *state == Claim::Indeterminate) {
            return Err(indeterminate());
        }
        if !claims.is_empty() {
            claims.insert(claim, Claim::Active);
            return Ok(());
        }
        claims.insert(claim, Claim::Indeterminate);
        match enable().await {
            Ok(()) => {
                claims.insert(claim, Claim::Active);
                Ok(())
            }
            Err(error) => {
                if matches!(&error, crate::Error::Platform(platform) if super::read_notify::is_enable_not_notifying(platform))
                {
                    claims.remove(&claim);
                    if claims.is_empty() {
                        epochs.remove(&epoch);
                    }
                }
                Err(error)
            }
        }
    }

    pub(super) async fn release<F, Fut>(
        &self,
        epoch: u64,
        claim: u64,
        disable: F,
    ) -> crate::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = crate::Result<()>>,
    {
        let mut epochs = self.epochs.lock().await;
        let Some(claims) = epochs.get_mut(&epoch) else {
            return Ok(());
        };
        if !claims.contains_key(&claim) {
            return Ok(());
        }
        if claims.len() > 1 {
            claims.remove(&claim);
            return Ok(());
        }
        claims.insert(claim, Claim::Indeterminate);
        disable().await?;
        claims.remove(&claim);
        epochs.remove(&epoch);
        Ok(())
    }
}

fn indeterminate() -> crate::Error {
    crate::Error::Platform(crate::PlatformError::new(
        "corebluetooth",
        "notification-enable-indeterminate",
        "An earlier enable requires cleanup before it can be shared",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn concurrent_claims_share_enable_and_last_release_disables() {
        let hub = Arc::new(NotificationOwnership::default());
        let enables = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for claim in [1, 2] {
            let hub = hub.clone();
            let enables = enables.clone();
            tasks.push(tokio::spawn(async move {
                hub.acquire(1, claim, || async move {
                    enables.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(())
                })
                .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(enables.load(Ordering::SeqCst), 1);
        let stops = AtomicUsize::new(0);
        hub.release(1, 1, || async {
            stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        hub.release(1, 2, || async {
            stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn observed_not_notifying_releases_claim_and_allows_another_attempt() {
        let hub = NotificationOwnership::default();
        let refusal = super::super::read_notify::enable_not_notifying_error();
        assert!(matches!(
            hub.acquire(1, 1, || async { Err(crate::Error::Platform(refusal)) })
                .await,
            Err(crate::Error::Platform(_))
        ));
        let enables = AtomicUsize::new(0);
        hub.acquire(1, 2, || async {
            enables.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(enables.load(Ordering::SeqCst), 1);
        hub.release(1, 2, || async { Ok(()) }).await.unwrap();
    }

    #[tokio::test]
    async fn similarly_coded_unidentified_error_remains_indeterminate() {
        let hub = NotificationOwnership::default();
        let failure = crate::PlatformError::new("corebluetooth", "411", "unknown platform failure");
        assert!(
            hub.acquire(1, 1, || async { Err(crate::Error::Platform(failure)) })
                .await
                .is_err()
        );
        assert!(hub.acquire(1, 2, || async { Ok(()) }).await.is_err());
        hub.release(1, 1, || async { Ok(()) }).await.unwrap();
    }

    #[tokio::test]
    async fn failed_enable_and_stop_retain_ownership_until_cleanup_succeeds() {
        let hub = NotificationOwnership::default();
        let failure = || crate::Error::RuntimeError("radio refused".into());
        assert!(
            hub.acquire(1, 1, || async { Err(failure()) })
                .await
                .is_err()
        );
        assert!(hub.acquire(1, 2, || async { Ok(()) }).await.is_err());
        assert!(
            hub.release(1, 1, || async { Err(failure()) })
                .await
                .is_err()
        );
        assert!(hub.acquire(1, 2, || async { Ok(()) }).await.is_err());
        hub.release(1, 1, || async { Ok(()) }).await.unwrap();
        hub.acquire(1, 2, || async { Ok(()) }).await.unwrap();
        assert!(
            hub.release(1, 2, || async { Err(failure()) })
                .await
                .is_err()
        );
        assert!(hub.acquire(1, 3, || async { Ok(()) }).await.is_err());
        hub.release(1, 2, || async { Ok(()) }).await.unwrap();
    }

    #[tokio::test]
    async fn new_epoch_does_not_join_or_release_an_old_epoch() {
        let hub = NotificationOwnership::default();
        hub.acquire(1, 1, || async { Ok(()) }).await.unwrap();
        let enables = AtomicUsize::new(0);
        hub.acquire(2, 2, || async {
            enables.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(enables.load(Ordering::SeqCst), 1);
        assert!(
            hub.release(1, 1, || async {
                Err(crate::Error::RuntimeError("invalidated".into()))
            })
            .await
            .is_err()
        );
        hub.release(2, 2, || async { Ok(()) }).await.unwrap();
    }
}
