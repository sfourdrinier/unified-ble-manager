//! Connection-owned server-match leases; local callbacks have separate ownership.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub(crate) struct MatchFailure {
    pub name: String,
    pub message: String,
}

impl MatchFailure {
    pub fn new(name: &str, message: &str) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }
}

type Operation = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(), MatchFailure>> + Send>> + Send + Sync,
>;

#[derive(Default)]
struct RuleState {
    references: usize,
    scopes: HashMap<u64, usize>,
    installed: bool,
    cleanup_scope: Option<u64>,
    failure: Option<MatchFailure>,
}

struct Rule {
    text: String,
    state: Mutex<RuleState>,
    operation: tokio::sync::Mutex<()>,
    running: AtomicBool,
    settled: tokio::sync::Notify,
}

/// All adapters sharing a connection share server registrations, but retain
/// independent local callbacks. A failed final removal cannot consume another
/// adapter's live registration, and a new acquisition waits for its resolution.
pub(crate) struct MatchRegistry {
    rules: Mutex<HashMap<String, Arc<Rule>>>,
    add: Operation,
    remove: Operation,
}

pub(crate) struct MatchLease {
    owner: Arc<MatchRegistry>,
    rule: Arc<Rule>,
    scope: u64,
}

impl MatchRegistry {
    pub fn new(add: Operation, remove: Operation) -> Arc<Self> {
        Arc::new(Self {
            rules: Mutex::new(HashMap::new()),
            add,
            remove,
        })
    }

    fn prune(&self) {
        self.rules.lock().expect("match table").retain(|_, rule| {
            Arc::strong_count(rule) != 1 || rule.state.lock().expect("match state").installed
        });
    }

    pub async fn acquire(
        self: &Arc<Self>,
        text: String,
        scope: u64,
    ) -> Result<MatchLease, MatchFailure> {
        self.prune();
        let rule = self
            .rules
            .lock()
            .expect("match table")
            .entry(text.clone())
            .or_insert_with(|| {
                Arc::new(Rule {
                    text,
                    state: Mutex::new(RuleState::default()),
                    operation: tokio::sync::Mutex::new(()),
                    running: AtomicBool::new(false),
                    settled: tokio::sync::Notify::new(),
                })
            })
            .clone();
        let gate = rule.operation.lock().await;
        let already_owned = {
            let mut state = rule.state.lock().expect("match state");
            if state.references > 0 {
                state.references += 1;
                *state.scopes.entry(scope).or_default() += 1;
                true
            } else {
                false
            }
        };
        if !already_owned {
            self.remove_locked(&rule).await?;
            {
                let mut state = rule.state.lock().expect("match state");
                // A cancelled AddMatch has an uncertain outcome. Its guard
                // retains/removes the possible registration, never forgets it.
                state.installed = true;
                state.cleanup_scope = Some(scope);
            }
            let mut rollback = AttemptGuard {
                owner: self.clone(),
                rule: rule.clone(),
                armed: true,
            };
            match (self.add)(rule.text.clone()).await {
                Ok(()) => {
                    let mut state = rule.state.lock().expect("match state");
                    state.references = 1;
                    state.scopes.insert(scope, 1);
                    state.cleanup_scope = None;
                    state.failure = None;
                    rollback.armed = false;
                }
                Err(error) => {
                    let uncertain = matches!(
                        error.name.as_str(),
                        "org.freedesktop.DBus.Error.NoReply"
                            | "org.freedesktop.DBus.Error.Timeout"
                            | "org.freedesktop.DBus.Error.Disconnected"
                    );
                    let mut state = rule.state.lock().expect("match state");
                    state.installed = uncertain;
                    state.failure = Some(error.clone());
                    rollback.armed = uncertain;
                    return Err(error);
                }
            }
        }
        drop(gate);
        Ok(MatchLease {
            owner: self.clone(),
            rule,
            scope,
        })
    }

    async fn remove_locked(&self, rule: &Rule) -> Result<(), MatchFailure> {
        {
            let state = rule.state.lock().expect("match state");
            if !state.installed || state.references != 0 {
                return Ok(());
            }
        }
        let outcome = (self.remove)(rule.text.clone()).await;
        let mut state = rule.state.lock().expect("match state");
        match outcome {
            Ok(()) => {
                state.installed = false;
                state.cleanup_scope = None;
                state.failure = None;
                Ok(())
            }
            Err(error) if error.name == "org.freedesktop.DBus.Error.MatchRuleNotFound" => {
                // This is the SERVER's authoritative final-rule absence,
                // never dbus-rs's unrelated missing LOCAL callback token.
                state.installed = false;
                state.cleanup_scope = None;
                state.failure = None;
                Ok(())
            }
            Err(error) => {
                state.failure = Some(error.clone());
                Err(error)
            }
        }
    }

    fn schedule(self: &Arc<Self>, rule: Arc<Rule>) {
        if rule.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            let failure = MatchFailure::new(
                "ubm.runtime.unavailable",
                "D-Bus match cleanup retained: no running executor",
            );
            eprintln!("bluez-async: {}", failure.message);
            rule.state.lock().expect("match state").failure = Some(failure);
            rule.running.store(false, Ordering::SeqCst);
            rule.settled.notify_waiters();
            return;
        };
        let owner = self.clone();
        let flight = FlightGuard {
            rule: rule.clone(),
            completed: false,
        };
        runtime.spawn(async move {
            let _gate = rule.operation.lock().await;
            let mut flight = flight;
            loop {
                let outcome = owner.remove_locked(&rule).await;
                if let Err(error) = &outcome {
                    eprintln!(
                        "bluez-async: D-Bus match cleanup retained: {}: {}",
                        error.name, error.message
                    );
                }
                match finish_flight(flight, outcome.is_ok()) {
                    Some(retained) => flight = retained,
                    None => break,
                }
            }
        });
    }

    /// Await only final releases owed by this adapter scope. Dropping this
    /// waiter does not cancel the independently owned removal already running.
    pub async fn drain(self: &Arc<Self>, scope: u64) -> Result<(), MatchFailure> {
        let rules: Vec<_> = self
            .rules
            .lock()
            .expect("match table")
            .values()
            .filter(|rule| {
                let state = rule.state.lock().expect("match state");
                state.cleanup_scope == Some(scope) || state.scopes.contains_key(&scope)
            })
            .cloned()
            .collect();
        let mut first = None;
        for rule in rules {
            if rule
                .state
                .lock()
                .expect("match state")
                .scopes
                .contains_key(&scope)
            {
                first.get_or_insert_with(|| {
                    MatchFailure::new(
                        "ubm.cleanup.pending",
                        "An owned D-Bus event stream is still live",
                    )
                });
                continue;
            }
            self.schedule(rule.clone());
            loop {
                let settled = rule.settled.notified();
                tokio::pin!(settled);
                settled.as_mut().enable();
                if !rule.running.load(Ordering::SeqCst) {
                    break;
                }
                settled.await;
            }
            let state = rule.state.lock().expect("match state");
            if state.cleanup_scope == Some(scope) && state.installed && first.is_none() {
                first = Some(state.failure.clone().unwrap_or_else(|| {
                    MatchFailure::new(
                        "ubm.cleanup.pending",
                        "D-Bus match cleanup has not been confirmed",
                    )
                }));
            }
        }
        self.prune();
        first.map_or(Ok(()), Err)
    }
}

struct AttemptGuard {
    owner: Arc<MatchRegistry>,
    rule: Arc<Rule>,
    armed: bool,
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if self.armed {
            self.owner.schedule(self.rule.clone());
        }
    }
}

struct FlightGuard {
    rule: Arc<Rule>,
    completed: bool,
}
fn finish_flight(mut flight: FlightGuard, succeeded: bool) -> Option<FlightGuard> {
    let rule = flight.rule.clone();
    let state = rule.state.lock().expect("match state");
    let retry = succeeded && state.references == 0 && state.installed;
    if retry {
        // Keep the same flight and operation gate: drain waiters must never
        // observe an idle gap before this coalesced last release is attempted.
        return Some(flight);
    }
    flight.completed = true;
    // Publish idle while holding the same lock as last-lease release. A
    // coalesced release before this point is retried; a later one schedules
    // its own flight. Genuine server refusals never spin automatically.
    drop(flight);
    None
}
impl Drop for FlightGuard {
    fn drop(&mut self) {
        if !self.completed {
            let failure = MatchFailure::new(
                "ubm.cleanup.cancelled",
                "D-Bus cleanup executor stopped before confirmation; ownership retained",
            );
            eprintln!("bluez-async: {}", failure.message);
            self.rule.state.lock().expect("match state").failure = Some(failure);
        }
        self.rule.running.store(false, Ordering::SeqCst);
        self.rule.settled.notify_waiters();
    }
}

impl Drop for MatchLease {
    fn drop(&mut self) {
        let last = {
            let mut state = self.rule.state.lock().expect("match state");
            state.references -= 1;
            let count = state
                .scopes
                .get_mut(&self.scope)
                .expect("owned match scope");
            *count -= 1;
            if *count == 0 {
                state.scopes.remove(&self.scope);
            }
            if state.references == 0 {
                state.cleanup_scope = Some(self.scope);
                true
            } else {
                false
            }
        };
        if last {
            self.owner.schedule(self.rule.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn operations(
        adds: Arc<AtomicUsize>,
        removes: Arc<AtomicUsize>,
        refuse: Arc<AtomicUsize>,
    ) -> Arc<MatchRegistry> {
        MatchRegistry::new(
            Arc::new(move |_| {
                adds.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }),
            Arc::new(move |_| {
                removes.fetch_add(1, Ordering::SeqCst);
                let failed = refuse
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                Box::pin(async move {
                    if failed {
                        Err(MatchFailure::new(
                            "org.freedesktop.DBus.Error.AccessDenied",
                            "refused",
                        ))
                    } else {
                        Ok(())
                    }
                })
            }),
        )
    }

    #[tokio::test]
    async fn last_release_during_a_noop_flight_keeps_a_removal_attempt() {
        let removes = Arc::new(AtomicUsize::new(0));
        let registry = operations(
            Arc::new(AtomicUsize::new(0)),
            removes.clone(),
            Arc::new(AtomicUsize::new(0)),
        );
        let lease = registry.acquire("racing".into(), 1).await.unwrap();
        let rule = lease.rule.clone();
        // A prior flight has already observed the new live lease and no-oped,
        // but has not published its finished flag yet. Last release races it.
        rule.running.store(true, Ordering::SeqCst);
        drop(lease);
        let flight = FlightGuard {
            rule: rule.clone(),
            completed: false,
        };
        let retry = finish_flight(flight, true);
        assert!(
            retry.is_some(),
            "coalescing must retain the last-release request"
        );
        assert!(
            rule.running.load(Ordering::SeqCst),
            "a drain must not observe an idle gap before the coalesced removal"
        );
        registry.remove_locked(&rule).await.unwrap();
        assert!(finish_flight(retry.unwrap(), true).is_none());
        registry.drain(1).await.unwrap();
        assert_eq!(removes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn active_owned_stream_is_not_a_clean_close_and_other_scope_is_unaffected() {
        let registry = operations(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let held = registry.acquire("owned".into(), 1).await.unwrap();
        assert_eq!(
            registry.drain(1).await.unwrap_err().name,
            "ubm.cleanup.pending"
        );
        registry.drain(2).await.unwrap();
        drop(held);
        registry.drain(1).await.unwrap();
    }

    #[tokio::test]
    async fn server_absence_is_idempotent_but_local_missing_token_is_not() {
        for (name, expected) in [
            ("org.freedesktop.DBus.Error.MatchRuleNotFound", true),
            ("org.freedesktop.DBus.Error.Failed", false),
        ] {
            let registry = MatchRegistry::new(
                Arc::new(|_| Box::pin(async { Ok(()) })),
                Arc::new(move |_| {
                    Box::pin(
                        async move { Err(MatchFailure::new(name, "No match with that id found")) },
                    )
                }),
            );
            drop(registry.acquire("rule".into(), 1).await.unwrap());
            assert_eq!(registry.drain(1).await.is_ok(), expected);
        }
    }

    #[tokio::test]
    async fn refused_add_does_not_release_another_accepted_rule() {
        let removes = Arc::new(AtomicUsize::new(0));
        let registry = MatchRegistry::new(
            Arc::new(|rule| {
                Box::pin(async move {
                    if rule == "refused" {
                        Err(MatchFailure::new(
                            "org.freedesktop.DBus.Error.AccessDenied",
                            "refused",
                        ))
                    } else {
                        Ok(())
                    }
                })
            }),
            {
                let removes = removes.clone();
                Arc::new(move |_| {
                    removes.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })
            },
        );
        let accepted = registry.acquire("accepted".into(), 1).await.unwrap();
        assert!(registry.acquire("refused".into(), 1).await.is_err());
        assert_eq!(removes.load(Ordering::SeqCst), 0);
        drop(accepted);
        registry.drain(1).await.unwrap();
        assert_eq!(removes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shared_rule_removes_only_after_last_adapter_lease() {
        let adds = Arc::new(AtomicUsize::new(0));
        let removes = Arc::new(AtomicUsize::new(0));
        let registry = operations(adds.clone(), removes.clone(), Arc::new(AtomicUsize::new(0)));
        let first = registry.acquire("same".into(), 1).await.unwrap();
        let second = registry.acquire("same".into(), 2).await.unwrap();
        drop(first);
        registry.drain(1).await.unwrap();
        assert_eq!(adds.load(Ordering::SeqCst), 1);
        assert_eq!(removes.load(Ordering::SeqCst), 0);
        drop(second);
        registry.drain(2).await.unwrap();
        assert_eq!(removes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refusal_is_retained_and_retry_does_not_poison_success() {
        let removes = Arc::new(AtomicUsize::new(0));
        let registry = operations(
            Arc::new(AtomicUsize::new(0)),
            removes.clone(),
            Arc::new(AtomicUsize::new(2)),
        );
        drop(registry.acquire("refused".into(), 7).await.unwrap());
        tokio::task::yield_now().await;
        assert_eq!(
            registry.drain(7).await.unwrap_err().name,
            "org.freedesktop.DBus.Error.AccessDenied"
        );
        registry.drain(7).await.unwrap();
        assert_eq!(removes.load(Ordering::SeqCst), 3);
        registry.drain(7).await.unwrap();
        assert_eq!(removes.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn held_removal_survives_bounded_wait_and_fences_new_acquire() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let adds = Arc::new(AtomicUsize::new(0));
        let registry = MatchRegistry::new(
            {
                let adds = adds.clone();
                Arc::new(move |_| {
                    adds.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })
            },
            {
                let entered = entered.clone();
                let resume = resume.clone();
                Arc::new(move |_| {
                    let entered = entered.clone();
                    let resume = resume.clone();
                    Box::pin(async move {
                        entered.notify_one();
                        resume.notified().await;
                        Ok(())
                    })
                })
            },
        );
        drop(registry.acquire("held".into(), 1).await.unwrap());
        entered.notified().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), registry.drain(1))
                .await
                .is_err()
        );
        let next = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.acquire("held".into(), 2).await })
        };
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        assert_eq!(adds.load(Ordering::SeqCst), 1);
        resume.notify_one();
        let lease = next.await.unwrap().unwrap();
        assert_eq!(adds.load(Ordering::SeqCst), 2);
        registry.drain(1).await.unwrap();
        drop(lease);
        resume.notify_one();
        registry.drain(2).await.unwrap();
    }
}
