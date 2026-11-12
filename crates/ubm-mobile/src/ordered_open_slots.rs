//! Process-shared admission credit for mobile ordered opens.
//!
//! MobileHost attaches this core to the negotiated native-owner incarnation.
//! V2 open and acknowledgement carry the incarnation, slot and generation
//! alongside the exact original request identity. Creating another registry for
//! the same owner defeats the bound. A replacement must use a fresh incarnation.
//!
//! The fingerprint must contain the complete original request, without lossy
//! hashing. A caller must retain a validated terminal disposition (including
//! source and cleanup ownership) before acknowledging it. `confirm_retired`
//! requires an observed exact native retirement response; it is not a request
//! to retire, nor evidence supplied by a timeout, cancellation, or host change.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) type Incarnation = [u8; 16];

/// Allocated once per attached host, scoped to the shared native owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HostIdentity {
    incarnation: Incarnation,
    serial: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupMapping {
    pub(crate) host: HostIdentity,
    pub(crate) collection_id: u64,
    pub(crate) native_group_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRequest<F> {
    pub(crate) request_id: u64,
    pub(crate) group_id: u64,
    pub(crate) peer: String,
    pub(crate) fingerprint: F,
    /// None only for the legacy, not-yet-integrated coordinator callers.
    pub(crate) local_group: Option<GroupMapping>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotIdentity {
    pub(crate) incarnation: Incarnation,
    pub(crate) slot: u32,
    pub(crate) generation: u64,
}

/// The nonce is local only and prevents an abandoned, unsubmitted reservation
/// from acting on another reservation that reused the same wire generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lease<F> {
    pub(crate) identity: SlotIdentity,
    pub(crate) request: OpenRequest<F>,
    nonce: u64,
}

impl Lease<crate::radio::OrderedOpenSpec> {
    /// Build all three v2 requests from the same immutable lease. Call only
    /// after `begin_dispatch` has won against local cancellation.
    pub(crate) fn wire_identity(
        &self,
        owner: crate::radio::OrderedAdmissionOwner,
    ) -> Result<crate::radio::OrderedOpenIdentity, Error> {
        if owner.bytes() != self.identity.incarnation || self.request.local_group.is_none() {
            return Err(Error::IdentityMismatch);
        }
        let identity = crate::radio::OrderedOpenIdentity {
            version: crate::radio::ORDERED_ADMISSION_SLOT_VERSION,
            owner_incarnation: owner,
            slot: self.identity.slot,
            slot_generation: self.identity.generation,
            original_request_id: self.request.request_id,
            native_group_id: self.request.group_id,
            peer_id: self.request.peer.clone(),
            spec: self.request.fingerprint.clone(),
        };
        identity.validate().map_err(|_| Error::IdentityMismatch)?;
        Ok(identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outstanding<F, D> {
    pub(crate) lease: Lease<F>,
    pub(crate) dispatched: bool,
    pub(crate) disposition: Option<D>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidCapacity,
    AlreadyRegistered,
    UnknownIncarnation,
    Capacity,
    Exhausted,
    IdentityMismatch,
    AlreadyDispatched,
    NotDispatched,
    NotTerminal,
    ConflictingDisposition,
    OwnerBusy,
    OwnerRetired,
    GroupAlreadyMapped,
}

struct Slot<F, D> {
    // Last generation whose dispatch became possible. A local cancellation
    // does not consume a wire generation.
    generation: u64,
    active: Option<Outstanding<F, D>>,
}

struct State<F, D> {
    next_nonce: u64,
    next_host: u64,
    next_group: u64,
    groups: HashMap<(HostIdentity, u64), GroupRecord<D>>,
    slots: Vec<Slot<F, D>>,
    retired: bool,
    quiescent: bool,
}

struct GroupRecord<D> {
    mapping: GroupMapping,
    dispatched: bool,
    retired: Option<(SlotIdentity, D)>,
}

pub(crate) struct Coordinator<F, D> {
    incarnation: Incarnation,
    state: Mutex<State<F, D>>,
    // One in-flight native attempt per owner, independent of the calling host.
    pub(crate) retirement: tokio::sync::Mutex<OwnerRetirement>,
}

#[derive(Default)]
pub(crate) struct OwnerRetirement {
    pub(crate) identity: Option<crate::OrderedAdmissionOwner>,
    pub(crate) fence_confirmed: bool,
    pub(crate) native_quiescent: bool,
    pub(crate) uncertain_failure: Option<ubm_desktop::DesktopError>,
    pub(crate) attempt_unsettled: bool,
}

/// Keeps coordinators, including unresolved debt, alive independently of
/// callers. The real host's lifetime must own this registry and its workers.
pub(crate) struct CoordinatorRegistry<F, D> {
    owners: Mutex<HashMap<Incarnation, Owner<F, D>>>,
}

struct Owner<F, D> {
    coordinator: Arc<Coordinator<F, D>>,
    // Keep the native adapter and its Rust cleanup state alive through host
    // replacement. Released only with explicit native quiescence and no debt.
    cleanup_anchors: Vec<(Arc<dyn Any + Send + Sync>, bool)>,
}

impl<F, D> Default for CoordinatorRegistry<F, D> {
    fn default() -> Self {
        Self {
            owners: Mutex::new(HashMap::new()),
        }
    }
}

impl<F, D> CoordinatorRegistry<F, D> {
    pub(crate) fn register(
        &self,
        incarnation: Incarnation,
        capacity: usize,
    ) -> Result<Arc<Coordinator<F, D>>, Error> {
        if capacity == 0 || u32::try_from(capacity).is_err() {
            return Err(Error::InvalidCapacity);
        }
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        if owners.contains_key(&incarnation) {
            return Err(Error::AlreadyRegistered);
        }
        let slots = (0..capacity)
            .map(|_| Slot {
                generation: 0,
                active: None,
            })
            .collect();
        let coordinator = Arc::new(Coordinator {
            incarnation,
            retirement: tokio::sync::Mutex::new(OwnerRetirement::default()),
            state: Mutex::new(State {
                next_nonce: 1,
                next_host: 1,
                next_group: 1,
                groups: HashMap::new(),
                slots,
                retired: false,
                quiescent: false,
            }),
        });
        owners.insert(
            incarnation,
            Owner {
                coordinator: coordinator.clone(),
                cleanup_anchors: Vec::new(),
            },
        );
        Ok(coordinator)
    }

    pub(crate) fn attach(&self, incarnation: Incarnation) -> Result<Arc<Coordinator<F, D>>, Error> {
        let owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners.get(&incarnation).ok_or(Error::UnknownIncarnation)?;
        if owner
            .coordinator
            .state
            .lock()
            .expect("ordered open coordinator poisoned")
            .retired
        {
            return Err(Error::OwnerRetired);
        }
        Ok(owner.coordinator.clone())
    }

    /// Atomic attach-or-create: an incarnation can never have two coordinators.
    pub(crate) fn attach_or_register(
        &self,
        incarnation: Incarnation,
        capacity: usize,
        anchor: Arc<dyn Any + Send + Sync>,
    ) -> Result<Arc<Coordinator<F, D>>, Error> {
        if capacity == 0 || u32::try_from(capacity).is_err() {
            return Err(Error::InvalidCapacity);
        }
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        if let Some(owner) = owners.get_mut(&incarnation) {
            let state = owner
                .coordinator
                .state
                .lock()
                .expect("ordered open coordinator poisoned");
            if state.retired {
                return Err(Error::OwnerRetired);
            }
            if state.slots.len() != capacity {
                return Err(Error::InvalidCapacity);
            }
            drop(state);
            owner.cleanup_anchors.push((anchor, false));
            return Ok(owner.coordinator.clone());
        }
        let coordinator = Arc::new(Coordinator {
            incarnation,
            retirement: tokio::sync::Mutex::new(OwnerRetirement::default()),
            state: Mutex::new(State {
                next_nonce: 1,
                next_host: 1,
                next_group: 1,
                groups: HashMap::new(),
                slots: (0..capacity)
                    .map(|_| Slot {
                        generation: 0,
                        active: None,
                    })
                    .collect(),
                retired: false,
                quiescent: false,
            }),
        });
        owners.insert(
            incarnation,
            Owner {
                coordinator: coordinator.clone(),
                cleanup_anchors: vec![(anchor, false)],
            },
        );
        Ok(coordinator)
    }

    /// Stop new attachments and reservations while existing debt is settled.
    pub(crate) fn begin_exclusive_retirement(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
        radio: &Arc<dyn Any + Send + Sync>,
        host: &Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get_mut(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        if owner.cleanup_anchors.iter().any(|(anchor, settled)| {
            !settled && !Arc::ptr_eq(anchor, radio) && !Arc::ptr_eq(anchor, host)
        }) {
            return Err(Error::OwnerBusy);
        }
        if !owner
            .cleanup_anchors
            .iter()
            .any(|(anchor, _)| Arc::ptr_eq(anchor, radio))
            || !owner
                .cleanup_anchors
                .iter()
                .any(|(anchor, _)| Arc::ptr_eq(anchor, host))
        {
            return Err(Error::IdentityMismatch);
        }
        coordinator
            .state
            .lock()
            .expect("ordered open coordinator poisoned")
            .retired = true;
        Ok(())
    }

    pub(crate) fn begin_retirement(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
    ) -> Result<(), Error> {
        let owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        coordinator
            .state
            .lock()
            .expect("ordered open coordinator poisoned")
            .retired = true;
        Ok(())
    }

    pub(crate) fn add_anchor(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
        anchor: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get_mut(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        if coordinator
            .state
            .lock()
            .expect("ordered open coordinator poisoned")
            .retired
        {
            return Err(Error::OwnerRetired);
        }
        owner.cleanup_anchors.push((anchor, false));
        Ok(())
    }

    /// Finish constructing a host whose radio was attached before retirement.
    /// This does not admit another radio or another reservation.
    pub(crate) fn add_host_anchor(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
        radio: &Arc<dyn Any + Send + Sync>,
        host: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get_mut(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        if !owner
            .cleanup_anchors
            .iter()
            .any(|(candidate, settled)| Arc::ptr_eq(candidate, radio) && !settled)
        {
            return Err(Error::IdentityMismatch);
        }
        owner.cleanup_anchors.push((host, false));
        Ok(())
    }

    /// Confirm only the exact host whose cleanup has positively settled.
    pub(crate) fn confirm_cleanup(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
        anchor: &Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get_mut(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        let (_, settled) = owner
            .cleanup_anchors
            .iter_mut()
            .find(|(candidate, _)| Arc::ptr_eq(candidate, anchor))
            .ok_or(Error::IdentityMismatch)?;
        *settled = true;
        Ok(())
    }

    /// The constructing host never published this radio or dispatched work.
    /// Its anchor stays retained until the owner's eventual quiescence proof.
    pub(crate) fn confirm_unpublished(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
        anchor: &Arc<dyn Any + Send + Sync>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let owner = owners
            .get_mut(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&owner.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        let (_, settled) = owner
            .cleanup_anchors
            .iter_mut()
            .find(|(candidate, _)| Arc::ptr_eq(candidate, anchor))
            .ok_or(Error::IdentityMismatch)?;
        *settled = true;
        Ok(())
    }

    /// Remove a settled owner only after the native owner has proved that no
    /// old work or resources can survive. This method cannot establish that
    /// proof; its caller must own the native teardown handshake. Incarnations
    /// must never be reused, even after removal from this registry.
    pub(crate) fn retire_after_native_quiescence(
        &self,
        coordinator: &Arc<Coordinator<F, D>>,
    ) -> Result<(), Error> {
        let mut owners = self.owners.lock().expect("ordered open registry poisoned");
        let registered = owners
            .get(&coordinator.incarnation)
            .ok_or(Error::UnknownIncarnation)?;
        if !Arc::ptr_eq(&registered.coordinator, coordinator) {
            return Err(Error::IdentityMismatch);
        }
        let mut state = coordinator
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        // A quiescence observation is meaningful only after new attachment and
        // reservations were fenced. Otherwise work can start after the proof.
        if !state.retired {
            return Err(Error::OwnerBusy);
        }
        if state.slots.iter().any(|slot| slot.active.is_some())
            || !state.groups.is_empty()
            || registered
                .cleanup_anchors
                .iter()
                .any(|(_, settled)| !settled)
        {
            return Err(Error::OwnerBusy);
        }
        // Serialize with acquire, including clients holding an old Arc.
        state.quiescent = true;
        let removed = owners.remove(&coordinator.incarnation);
        drop(state);
        drop(owners);
        drop(removed);
        Ok(())
    }
}

impl<F: Clone + Eq, D: Clone + Eq> Coordinator<F, D> {
    pub(crate) fn native_quiescence_confirmed(&self) -> bool {
        self.state
            .lock()
            .expect("ordered open coordinator poisoned")
            .quiescent
    }
    pub(crate) fn register_host(&self) -> Result<HostIdentity, Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        if state.retired {
            return Err(Error::OwnerRetired);
        }
        let serial = state.next_host;
        state.next_host = serial.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(HostIdentity {
            incarnation: self.incarnation,
            serial,
        })
    }

    pub(crate) fn allocate_group(
        &self,
        host: HostIdentity,
        collection_id: u64,
    ) -> Result<GroupMapping, Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        if state.retired {
            return Err(Error::OwnerRetired);
        }
        if host.incarnation != self.incarnation
            || host.serial == 0
            || host.serial >= state.next_host
        {
            return Err(Error::IdentityMismatch);
        }
        if state.groups.contains_key(&(host, collection_id)) {
            return Err(Error::GroupAlreadyMapped);
        }
        let native_group_id = state.next_group;
        state.next_group = native_group_id.checked_add(1).ok_or(Error::Exhausted)?;
        let mapping = GroupMapping {
            host,
            collection_id,
            native_group_id,
        };
        state.groups.insert(
            (host, collection_id),
            GroupRecord {
                mapping,
                dispatched: false,
                retired: None,
            },
        );
        Ok(mapping)
    }

    pub(crate) fn group(&self, host: HostIdentity, collection_id: u64) -> Option<GroupMapping> {
        self.state
            .lock()
            .expect("ordered open coordinator poisoned")
            .groups
            .get(&(host, collection_id))
            .map(|record| record.mapping)
    }

    pub(crate) fn host_group_count(&self, host: HostIdentity) -> usize {
        self.state
            .lock()
            .expect("ordered open coordinator poisoned")
            .groups
            .keys()
            .filter(|(candidate, _)| *candidate == host)
            .count()
    }

    /// Only valid before dispatch became possible.
    pub(crate) fn forget_unsubmitted_group(
        &self,
        mapping: GroupMapping,
        native_group_id: u64,
    ) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let Some(record) = state.groups.get(&(mapping.host, mapping.collection_id)) else {
            return Err(Error::IdentityMismatch);
        };
        if mapping.native_group_id != native_group_id || record.mapping != mapping {
            return Err(Error::IdentityMismatch);
        }
        if state.slots.iter().any(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| active.lease.request.local_group == Some(mapping))
        }) {
            return Err(Error::OwnerBusy);
        }
        if record.dispatched {
            return Err(Error::AlreadyDispatched);
        }
        state.groups.remove(&(mapping.host, mapping.collection_id));
        Ok(())
    }

    /// Caller must have positively forgotten the native removal receipt for
    /// this exact group; an open receipt alone is never sufficient.
    pub(crate) fn forget_removed_group(
        &self,
        mapping: GroupMapping,
        native_group_id: u64,
    ) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let record = state
            .groups
            .get(&(mapping.host, mapping.collection_id))
            .ok_or(Error::IdentityMismatch)?;
        if record.mapping != mapping || mapping.native_group_id != native_group_id {
            return Err(Error::IdentityMismatch);
        }
        if state.slots.iter().any(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| active.lease.request.local_group == Some(mapping))
        }) {
            return Err(Error::OwnerBusy);
        }
        if record.retired.is_none() {
            return Err(Error::NotTerminal);
        }
        state.groups.remove(&(mapping.host, mapping.collection_id));
        Ok(())
    }

    /// Reserves locally. No native identity may be sent until begin_dispatch
    /// succeeds, and every sender must use this same transition.
    pub(crate) fn acquire(&self, request: OpenRequest<F>) -> Result<Lease<F>, Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        if state.retired {
            return Err(Error::OwnerRetired);
        }
        if let Some(mapping) = request.local_group {
            if request.group_id != mapping.native_group_id
                || state
                    .groups
                    .get(&(mapping.host, mapping.collection_id))
                    .is_none_or(|record| record.mapping != mapping || record.dispatched)
            {
                return Err(Error::IdentityMismatch);
            }
            if state.slots.iter().any(|slot| {
                slot.active
                    .as_ref()
                    .is_some_and(|active| active.lease.request.local_group == Some(mapping))
            }) {
                return Err(Error::GroupAlreadyMapped);
            }
        }
        let index = match state
            .slots
            .iter()
            .position(|slot| slot.active.is_none() && slot.generation < u64::MAX)
        {
            Some(index) => index,
            None if state.slots.iter().any(|slot| slot.active.is_some()) => {
                return Err(Error::Capacity);
            }
            None => return Err(Error::Exhausted),
        };
        let nonce = state.next_nonce;
        state.next_nonce = nonce.checked_add(1).ok_or(Error::Exhausted)?;
        let slot = &mut state.slots[index];
        let lease = Lease {
            identity: SlotIdentity {
                incarnation: self.incarnation,
                slot: index as u32,
                generation: slot.generation + 1,
            },
            request,
            nonce,
        };
        slot.active = Some(Outstanding {
            lease: lease.clone(),
            dispatched: false,
            disposition: None,
        });
        Ok(lease)
    }

    fn find<'a>(
        &self,
        state: &'a mut State<F, D>,
        lease: &Lease<F>,
    ) -> Result<&'a mut Slot<F, D>, Error> {
        if lease.identity.incarnation != self.incarnation {
            return Err(Error::IdentityMismatch);
        }
        let slot = state
            .slots
            .get_mut(lease.identity.slot as usize)
            .ok_or(Error::IdentityMismatch)?;
        if slot
            .active
            .as_ref()
            .is_none_or(|active| active.lease != *lease)
        {
            return Err(Error::IdentityMismatch);
        }
        Ok(slot)
    }

    /// Atomically prevents local cancellation from reclaiming the credit before
    /// any sender can enter the foreign boundary. No lock is held across it.
    pub(crate) fn begin_dispatch(&self, lease: &Lease<F>) -> Result<SlotIdentity, Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        if state.retired {
            return Err(Error::OwnerRetired);
        }
        let slot = self.find(&mut state, lease)?;
        let active = slot.active.as_mut().expect("validated active slot");
        if active.dispatched {
            return Err(Error::AlreadyDispatched);
        }
        active.dispatched = true;
        slot.generation = lease.identity.generation;
        if let Some(mapping) = lease.request.local_group {
            state
                .groups
                .get_mut(&(mapping.host, mapping.collection_id))
                .expect("lease retains mapping")
                .dispatched = true;
        }
        Ok(lease.identity)
    }

    /// Valid only if all senders are gated by begin_dispatch. Once dispatch is
    /// possible, the debt must instead be recovered and acknowledged.
    pub(crate) fn release_unsubmitted(&self, lease: &Lease<F>) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let slot = self.find(&mut state, lease)?;
        if slot
            .active
            .as_ref()
            .expect("validated active slot")
            .dispatched
        {
            return Err(Error::AlreadyDispatched);
        }
        slot.active = None;
        Ok(())
    }

    /// Caller validates the exact terminal native outcome and retains everything
    /// needed to own or clean up its effects in `disposition` before calling.
    /// A host-owned cleanup worker must retain its own ownership independently:
    /// confirmation removes this receipt, including its stored disposition.
    /// An identical retry is idempotent; conflicting outcomes are rejected.
    pub(crate) fn retain_terminal(
        &self,
        lease: &Lease<F>,
        observed: SlotIdentity,
        disposition: D,
    ) -> Result<(), Error> {
        if observed != lease.identity {
            return Err(Error::IdentityMismatch);
        }
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let slot = self.find(&mut state, lease)?;
        let active = slot.active.as_mut().expect("validated active slot");
        if !active.dispatched {
            return Err(Error::NotDispatched);
        }
        if let Some(existing) = &active.disposition {
            return if *existing == disposition {
                Ok(())
            } else {
                Err(Error::ConflictingDisposition)
            };
        }
        active.disposition = Some(disposition);
        Ok(())
    }

    /// Call only after observing native Retired for this exact lease. Losing an
    /// acknowledgement or reply leaves the record intact for retry.
    pub(crate) fn confirm_retired(
        &self,
        lease: &Lease<F>,
        receipt: crate::radio::OrderedOpenRetired,
    ) -> Result<(), Error> {
        let observed = receipt.slot_identity;
        if observed.owner_incarnation.bytes() != lease.identity.incarnation
            || observed.slot != lease.identity.slot
            || observed.slot_generation != lease.identity.generation
        {
            return Err(Error::IdentityMismatch);
        }
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let slot = self.find(&mut state, lease)?;
        let disposition = slot
            .active
            .as_ref()
            .expect("validated active slot")
            .disposition
            .clone()
            .ok_or(Error::NotTerminal)?;
        slot.active = None;
        if let Some(mapping) = lease.request.local_group {
            state
                .groups
                .get_mut(&(mapping.host, mapping.collection_id))
                .expect("lease retains mapping")
                .retired = Some((lease.identity, disposition));
        }
        Ok(())
    }

    pub(crate) fn outstanding(&self) -> Vec<Outstanding<F, D>> {
        self.state
            .lock()
            .expect("ordered open coordinator poisoned")
            .slots
            .iter()
            .filter_map(|slot| slot.active.clone())
            .collect()
    }

    fn forget_if_retired(
        &self,
        mapping: GroupMapping,
        retired: SlotIdentity,
        proves_no_effect: impl FnOnce(&D) -> bool,
    ) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .expect("ordered open coordinator poisoned");
        let record = state
            .groups
            .get(&(mapping.host, mapping.collection_id))
            .ok_or(Error::IdentityMismatch)?;
        if record.mapping != mapping
            || !record
                .retired
                .as_ref()
                .is_some_and(|(identity, disposition)| {
                    *identity == retired && proves_no_effect(disposition)
                })
        {
            return Err(Error::IdentityMismatch);
        }
        state.groups.remove(&(mapping.host, mapping.collection_id));
        Ok(())
    }
}

impl<F: Clone + Eq> Coordinator<F, crate::radio::OrderedOpenTerminalV2> {
    /// Remove only after the exact retained failure established no native
    /// effect and its matching native retirement was confirmed.
    pub(crate) fn forget_no_effect_group(
        &self,
        mapping: GroupMapping,
        retired: SlotIdentity,
    ) -> Result<(), Error> {
        use crate::radio::{OrderedOpenOwnershipV2, OrderedOpenTerminalV2};
        self.forget_if_retired(mapping, retired, |disposition| {
            matches!(
                disposition,
                OrderedOpenTerminalV2::Failed {
                    ownership: OrderedOpenOwnershipV2::NotCreated,
                    ..
                }
            )
        })
    }
}

impl<F: Clone + Eq> Coordinator<F, crate::radio::RadioCompletion> {
    /// Forget a mapped group only after the matching v2 native result proves
    /// that no source was created and that exact slot was retired.
    pub(crate) fn forget_no_effect_radio_group(
        &self,
        mapping: GroupMapping,
        retired: SlotIdentity,
    ) -> Result<(), Error> {
        use crate::radio::{
            OrderedOpenCompletionV2, OrderedOpenOutcomeV2, OrderedOpenOwnershipV2, RadioCompletion,
        };
        self.forget_if_retired(mapping, retired, |disposition| match disposition {
            RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(result))
                if result.slot_identity.owner_incarnation.bytes() == retired.incarnation
                    && result.slot_identity.slot == retired.slot
                    && result.slot_identity.slot_generation == retired.generation
                    && matches!(
                        &result.outcome,
                        OrderedOpenOutcomeV2::Failed {
                            ownership: OrderedOpenOwnershipV2::NotCreated,
                            ..
                        }
                    ) =>
            {
                true
            }
            _ => false,
        })
    }
}

impl<D: Clone + Eq> Coordinator<crate::radio::OrderedOpenSpec, D> {
    /// Validate the complete wire descriptor before consuming a generation.
    pub(crate) fn begin_dispatch_v2(
        &self,
        lease: &Lease<crate::radio::OrderedOpenSpec>,
        owner: crate::radio::OrderedAdmissionOwner,
    ) -> Result<crate::radio::OrderedOpenIdentity, Error> {
        let identity = lease.wire_identity(owner)?;
        self.begin_dispatch(lease)?;
        Ok(identity)
    }
}

#[cfg(test)]
#[path = "ordered_open_slots_tests.rs"]
mod tests;
