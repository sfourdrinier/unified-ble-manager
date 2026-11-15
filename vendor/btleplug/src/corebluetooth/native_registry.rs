//! Objects in this registry are created, accessed and destroyed on the delegate
//! queue. Tokens contain no native pointers and can be sent to another actor.
//! This is not yet the connection-attempt executor: the legacy actor still owns
//! native objects and dispatches operations, and retrieval is not registered.

use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
use uuid::Uuid;

// IDs never repeat across manager lifetimes in this process.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct NativeObjectToken(u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct NativeObjectLease {
    object: NativeObjectToken,
    id: u64,
}

impl NativeObjectLease {
    pub(super) fn object(self) -> NativeObjectToken { self.object }
}

struct Entry<N> {
    token: NativeObjectToken,
    object: N,
    leases: HashSet<u64>,
}

pub(super) struct NativeObjectRegistry<M, N> {
    manager: M,
    peers: HashMap<Uuid, Vec<Entry<N>>>,
    // Keep this queue-local even if a test uses Send-safe native substitutes.
    _queue_local: PhantomData<Rc<()>>,
}

impl<M, N> NativeObjectRegistry<M, N> {
    pub(super) fn new(manager: M) -> Self {
        Self {
            manager,
            peers: HashMap::new(),
            _queue_local: PhantomData,
        }
    }

    pub(super) fn manager(&self) -> &M {
        &self.manager
    }

    pub(super) fn register(
        &mut self,
        peer: Uuid,
        object: N,
        same: impl Fn(&N, &N) -> bool,
    ) -> Option<NativeObjectToken> {
        if let Some(entry) = self
            .peers
            .get(&peer)
            .and_then(|objects| objects.iter().find(|entry| same(&entry.object, &object)))
        {
            return Some(entry.token);
        }
        let id = NEXT_TOKEN
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        let token = NativeObjectToken(id);
        self.peers.entry(peer).or_default().push(Entry { token, object, leases: HashSet::new() });
        Some(token)
    }

    pub(super) fn resolve(&self, peer: Uuid, token: NativeObjectToken) -> Option<&N> {
        self.peers
            .get(&peer)?
            .iter()
            .find(|entry| entry.token == token)
            .map(|entry| &entry.object)
    }

    /// Resolve a callback's native identity without registering a new object.
    /// UUID equality alone is not evidence that a callback belongs to an entry.
    pub(super) fn lookup(&self, peer: Uuid, object: &N, same: impl Fn(&N, &N) -> bool) -> Option<NativeObjectToken> {
        self.peers.get(&peer)?.iter().find(|entry| same(&entry.object, object)).map(|entry| entry.token)
    }

    /// Every owner gets a distinct receipt; duplicate retirement cannot
    /// release another owner's retain. All operations remain queue-confined.
    #[allow(dead_code)]
    pub(super) fn lease(&mut self, peer: Uuid, token: NativeObjectToken) -> Option<NativeObjectLease> {
        let entry = self.peers.get_mut(&peer)?.iter_mut().find(|entry| entry.token == token)?;
        let id = NEXT_TOKEN.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1)).ok()?;
        entry.leases.insert(id);
        Some(NativeObjectLease { object: token, id })
    }

    #[allow(dead_code)]
    pub(super) fn retire(&mut self, peer: Uuid, lease: NativeObjectLease) -> bool {
        let Some(entries) = self.peers.get_mut(&peer) else { return false; };
        let Some(index) = entries.iter().position(|entry| entry.token == lease.object) else { return false; };
        if !entries[index].leases.remove(&lease.id) { return false; }
        if entries[index].leases.is_empty() {
            entries.remove(index);
        }
        if entries.is_empty() { self.peers.remove(&peer); }
        true
    }

    /// Undo a registration whose first lease could not be allocated. Never
    /// remove an object already owned by another cycle.
    pub(super) fn discard_unleased(&mut self, peer: Uuid, token: NativeObjectToken) {
        if let Some(entries) = self.peers.get_mut(&peer) {
            entries.retain(|entry| entry.token != token || !entry.leases.is_empty());
            if entries.is_empty() { self.peers.remove(&peer); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    use uuid::Uuid;

    #[derive(Debug)]
    struct Object {
        dropped: Rc<Cell<usize>>,
    }
    impl Drop for Object {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
    fn same(a: &Rc<Object>, b: &Rc<Object>) -> bool {
        Rc::ptr_eq(a, b)
    }

    #[test]
    fn discovery_does_not_register_unowned_diagnostic_objects() {
        let delegate = include_str!("central_delegate.rs");
        let discovery = delegate.split("fn delegate_centralmanager_diddiscoverperipheral_advertisementdata_rssi(")
            .nth(1).unwrap().split("let advertisement_name =").next().unwrap();
        assert!(!discovery.contains("register_native_peripheral"),
            "discovery must not register objects until their ownership is integrated");
        assert!(!delegate.contains("objects.register("),
            "the delegate must not retain diagnostic objects in the registry");
    }

    #[test]
    fn high_volume_retirement_preserves_active_exact_object_lease() {
        let drops = Rc::new(Cell::new(0));
        let mut registry = NativeObjectRegistry::new(());
        let peer = Uuid::from_u128(1);
        let active = Rc::new(Object { dropped: drops.clone() });
        let weak_active = Rc::downgrade(&active);
        let active_token = registry.register(peer, active, same).unwrap();
        let lease = registry.lease(peer, active_token).unwrap();
        for i in 0..20_000 {
            let observed_peer = if i % 2 == 0 { peer } else { Uuid::from_u128(i + 2) };
            let object = Rc::new(Object { dropped: drops.clone() });
            let weak = Rc::downgrade(&object);
            let token = registry.register(observed_peer, object, same).unwrap();
            let transient = registry.lease(observed_peer, token).unwrap();
            assert!(registry.retire(observed_peer, transient));
            assert!(weak.upgrade().is_none());
            assert!(registry.resolve(peer, active_token).is_some());
            assert_eq!(registry.peers.values().map(Vec::len).sum::<usize>(), 1);
        }
        assert_eq!(drops.get(), 20_000);
        assert!(weak_active.upgrade().is_some());
        assert!(registry.retire(peer, lease));
        assert!(weak_active.upgrade().is_none());
        assert!(registry.peers.is_empty());
    }

    #[test]
    fn callback_lookup_requires_the_exact_registered_object_and_peer() {
        let drops = Rc::new(Cell::new(0));
        let mut registry = NativeObjectRegistry::new(Rc::new(Object { dropped: drops.clone() }));
        let peer = Uuid::from_u128(72);
        let object = Rc::new(Object { dropped: drops.clone() });
        let other = Rc::new(Object { dropped: drops });
        let token = registry.register(peer, object.clone(), same).unwrap();
        assert_eq!(registry.lookup(peer, &object, same), Some(token));
        assert_eq!(registry.lookup(peer, &other, same), None);
        assert_eq!(registry.lookup(Uuid::from_u128(73), &object, same), None);
        let lease = registry.lease(peer, token).unwrap();
        assert!(registry.retire(peer, lease));
        assert_eq!(registry.lookup(peer, &object, same), None);
    }

    #[test]
    fn leases_retire_exact_object_only_after_last_release() {
        let drops = Rc::new(Cell::new(0));
        let mut registry = NativeObjectRegistry::new(Rc::new(Object { dropped: drops.clone() }));
        let peer = Uuid::from_u128(70);
        let token = registry.register(peer, Rc::new(Object { dropped: drops.clone() }), same).unwrap();
        let other = registry.register(peer, Rc::new(Object { dropped: drops.clone() }), same).unwrap();
        let first = registry.lease(peer, token).unwrap();
        let second = registry.lease(peer, token).unwrap();
        assert!(registry.retire(peer, first));
        assert!(!registry.retire(peer, first), "one lease cannot be retired twice");
        assert!(registry.resolve(peer, token).is_some());
        assert!(!registry.retire(Uuid::from_u128(71), second));
        assert!(registry.retire(peer, second));
        assert!(registry.resolve(peer, token).is_none());
        assert!(registry.resolve(peer, other).is_some());
        assert_eq!(drops.get(), 1);
        let fresh = registry.register(peer, Rc::new(Object { dropped: drops.clone() }), same).unwrap();
        assert_ne!(token, fresh, "a retired identity must never be reused");
        assert!(!registry.retire(peer, second));
    }

    #[test]
    fn native_registry_retains_distinct_objects_for_one_peer() {
        let drops = Rc::new(Cell::new(0));
        let manager = Rc::new(Object {
            dropped: drops.clone(),
        });
        let mut registry = NativeObjectRegistry::new(manager.clone());
        let peer = Uuid::from_u128(7);
        let first = Rc::new(Object {
            dropped: drops.clone(),
        });
        let second = Rc::new(Object {
            dropped: drops.clone(),
        });
        let old = registry.register(peer, first.clone(), same).unwrap();
        assert_eq!(registry.register(peer, first.clone(), same).unwrap(), old);
        let replacement = registry.register(peer, second.clone(), same).unwrap();
        assert_ne!(old, replacement);
        drop(first);
        drop(second);
        drop(manager);
        assert_eq!(drops.get(), 0);
        assert!(registry.resolve(peer, old).is_some());
        assert!(registry.resolve(peer, replacement).is_some());
        assert!(registry.resolve(Uuid::from_u128(8), old).is_none());
        drop(registry);
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn native_registry_lives_and_drops_on_serial_delegate_queue() {
        use super::super::ffi;
        use std::{
            ffi::{c_void, CString},
            sync::mpsc,
            time::Duration,
        };
        static KEY: u8 = 0;
        type Registry = NativeObjectRegistry<Rc<Object>, Rc<Object>>;
        struct Setup {
            queue: ffi::dispatch_queue_t,
            done: mpsc::Sender<(NativeObjectToken, NativeObjectToken)>,
        }
        unsafe extern "C" fn setup(raw: *mut c_void) {
            let job = unsafe { Box::from_raw(raw.cast::<Setup>()) };
            let drops = Rc::new(Cell::new(0));
            let mut registry = Registry::new(Rc::new(Object {
                dropped: drops.clone(),
            }));
            let peer = Uuid::from_u128(10);
            let old = registry
                .register(
                    peer,
                    Rc::new(Object {
                        dropped: drops.clone(),
                    }),
                    same,
                )
                .unwrap();
            let new = registry
                .register(peer, Rc::new(Object { dropped: drops }), same)
                .unwrap();
            unsafe {
                ffi::dispatch_queue_set_specific(
                    job.queue,
                    std::ptr::addr_of!(KEY).cast(),
                    Box::into_raw(Box::new(registry)).cast(),
                    None,
                )
            };
            job.done.send((old, new)).unwrap();
        }
        struct Inspect {
            old: NativeObjectToken,
            new: NativeObjectToken,
            done: mpsc::Sender<bool>,
        }
        unsafe extern "C" fn inspect(raw: *mut c_void) {
            let job = unsafe { Box::from_raw(raw.cast::<Inspect>()) };
            let ptr = unsafe { ffi::dispatch_get_specific(std::ptr::addr_of!(KEY).cast()) }
                .cast::<Registry>();
            let registry = unsafe { &*ptr };
            let peer = Uuid::from_u128(10);
            job.done
                .send(
                    registry.resolve(peer, job.old).is_some()
                        && registry.resolve(peer, job.new).is_some()
                        && registry.resolve(Uuid::from_u128(11), job.old).is_none(),
                )
                .unwrap();
        }
        struct Cleanup {
            queue: ffi::dispatch_queue_t,
            done: mpsc::Sender<usize>,
        }
        unsafe extern "C" fn cleanup(raw: *mut c_void) {
            let job = unsafe { Box::from_raw(raw.cast::<Cleanup>()) };
            let ptr = unsafe { ffi::dispatch_get_specific(std::ptr::addr_of!(KEY).cast()) }
                .cast::<Registry>();
            let registry = unsafe { Box::from_raw(ptr) };
            let drops = registry.manager().dropped.clone();
            unsafe {
                ffi::dispatch_queue_set_specific(
                    job.queue,
                    std::ptr::addr_of!(KEY).cast(),
                    std::ptr::null_mut(),
                    None,
                )
            };
            drop(registry);
            job.done.send(drops.get()).unwrap();
        }
        fn require_send<T: Send>(_: &T) {}
        let label = CString::new("ubm.native-registry-test").unwrap();
        let queue =
            unsafe { ffi::dispatch_queue_create(label.as_ptr(), ffi::DISPATCH_QUEUE_SERIAL) };
        assert!(!queue.is_null());
        let (tx, rx) = mpsc::channel();
        let job = Box::new(Setup { queue, done: tx });
        unsafe { ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), setup) };
        let (old, new) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Box::new(Inspect { old, new, done: tx });
        require_send(&job);
        unsafe { ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), inspect) };
        assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        let (tx, rx) = mpsc::channel();
        let job = Box::new(Cleanup { queue, done: tx });
        unsafe { ffi::dispatch_async_f(queue, Box::into_raw(job).cast(), cleanup) };
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 3);
        unsafe { ffi::dispatch_release(queue) };
    }
    #[test]
    fn native_tokens_cannot_resolve_in_another_manager_registry() {
        let drops = Rc::new(Cell::new(0));
        let mut first = NativeObjectRegistry::new(Rc::new(Object {
            dropped: drops.clone(),
        }));
        let mut second = NativeObjectRegistry::new(Rc::new(Object {
            dropped: drops.clone(),
        }));
        let peer = Uuid::from_u128(17);
        let a = first
            .register(
                peer,
                Rc::new(Object {
                    dropped: drops.clone(),
                }),
                same,
            )
            .unwrap();
        let b = second
            .register(peer, Rc::new(Object { dropped: drops }), same)
            .unwrap();
        assert_ne!(a, b, "tokens must not alias across manager lifetimes");
        assert!(second.resolve(peer, a).is_none());
    }
}
