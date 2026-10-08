//! Exact callback ownership; these controls do not prove physical radio behavior.
use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_and_duplicate_callbacks_do_not_drain_the_live_graph() {
        let mut scope = DiscoveryReservations::default();
        assert!(scope.take_root());
        assert!(scope.register_services([7, 19]));
        assert!(!scope.take_includes(20));
        assert!(scope.take_includes(7));
        assert!(!scope.take_includes(7));
        assert!(!scope.is_drained());
        assert!(scope.take_characteristics(7));
        assert!(scope.register_descriptors([71, 72]));
        assert!(scope.take_includes(19));
        assert!(scope.take_characteristics(19));
        assert!(scope.take_descriptor(71));
        assert!(!scope.is_drained());
        assert!(scope.take_descriptor(72));
        assert!(scope.is_drained());
    }
    #[test]
    fn failure_keeps_native_callbacks_owned_until_their_actual_retirement() {
        let mut scope = DiscoveryReservations::default();
        scope.take_root();
        scope.register_services([7]);
        scope.fail();
        assert!(scope.halted());
        assert!(!scope.register_services([19]));
        assert!(scope.take_includes(7));
        assert!(!scope.is_drained());
        assert!(scope.take_characteristics(7));
        assert!(scope.is_drained());
    }
    #[test]
    fn cyclic_inclusions_do_not_reopen_a_completed_callback_reservation() {
        let mut scope = DiscoveryReservations::default();
        scope.take_root();
        assert!(scope.register_services([7]));
        assert!(scope.take_includes(7));
        assert!(scope.register_services([19]));
        assert!(scope.take_includes(19));
        assert!(scope.register_services([7]));
        assert!(!scope.take_includes(7));
        assert!(scope.take_characteristics(7));
        assert!(scope.take_characteristics(19));
        assert!(scope.is_drained());
    }
    #[test]
    fn capacity_refusal_keeps_prior_callback_reservations_intact() {
        let mut scope = DiscoveryReservations::default();
        scope.take_root();
        assert!(scope.register_services(0..4096));
        assert!(!scope.register_services([4096]));
        assert!(!scope.is_drained());
        for id in 0..4096 {
            assert!(scope.take_includes(id));
            assert!(scope.take_characteristics(id));
        }
        assert!(scope.is_drained());
    }
}

#[derive(Debug)]
pub(crate) struct DiscoveryReservations {
    root: bool,
    failed: bool,
    services: HashSet<usize>,
    includes: HashSet<usize>,
    characteristics: HashSet<usize>,
    descriptors: HashSet<usize>,
}
impl Default for DiscoveryReservations {
    fn default() -> Self {
        Self {
            root: true,
            failed: false,
            services: HashSet::new(),
            includes: HashSet::new(),
            characteristics: HashSet::new(),
            descriptors: HashSet::new(),
        }
    }
}
impl DiscoveryReservations {
    pub(crate) fn take_root(&mut self) -> bool {
        std::mem::replace(&mut self.root, false)
    }
    pub(crate) fn register_services(
        &mut self,
        identities: impl IntoIterator<Item = usize>,
    ) -> bool {
        if self.failed {
            return false;
        }
        let added: HashSet<_> = identities
            .into_iter()
            .filter(|id| !self.services.contains(id))
            .collect();
        if self.services.len().saturating_add(added.len()) > 4096 {
            return false;
        }
        self.includes.extend(added.iter().copied());
        self.characteristics.extend(added.iter().copied());
        self.services.extend(added);
        true
    }
    pub(crate) fn register_descriptors(
        &mut self,
        identities: impl IntoIterator<Item = usize>,
    ) -> bool {
        if self.failed {
            return false;
        }
        let added: HashSet<_> = identities.into_iter().collect();
        if self.descriptors.len().saturating_add(added.len()) > 65536 {
            return false;
        }
        self.descriptors.extend(added);
        true
    }
    pub(crate) fn take_includes(&mut self, id: usize) -> bool {
        self.includes.remove(&id)
    }
    pub(crate) fn take_characteristics(&mut self, id: usize) -> bool {
        self.characteristics.remove(&id)
    }
    pub(crate) fn take_descriptor(&mut self, id: usize) -> bool {
        self.descriptors.remove(&id)
    }
    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }
    pub(crate) fn halted(&self) -> bool {
        self.failed
    }
    pub(crate) fn is_drained(&self) -> bool {
        !self.root
            && self.includes.is_empty()
            && self.characteristics.is_empty()
            && self.descriptors.is_empty()
    }
}
