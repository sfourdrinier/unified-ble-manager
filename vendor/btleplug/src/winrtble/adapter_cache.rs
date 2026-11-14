// The adapter cache is keyed by the peer identity used for admission.
pub(crate) fn cache_resolved_peer<K, P>(
    candidate: P,
    identity: impl FnOnce(&P) -> K,
    insert: impl FnOnce(K, P) -> P,
) -> P {
    let key = identity(&candidate);
    insert(key, candidate)
}

#[cfg(test)]
mod tests {
    use super::cache_resolved_peer;
    use std::collections::HashMap;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    struct Identity(u64, Option<bool>);

    fn admit(
        cache: &mut HashMap<Identity, Identity>,
        _requested: Identity,
        resolved: Identity,
    ) -> Identity {
        cache_resolved_peer(
            resolved,
            |peer| *peer,
            |key, peer| *cache.entry(key).or_insert(peer),
        )
    }

    #[test]
    fn resolved_identity_is_immediately_lookupable_and_deduplicated() {
        let unknown = Identity(42, None);
        let public = Identity(42, Some(false));
        for typed_first in [false, true] {
            let mut cache = HashMap::new();
            if typed_first {
                admit(&mut cache, public, public);
            }
            let returned = admit(&mut cache, unknown, public);
            assert_eq!(cache.get(&returned), Some(&returned));
            admit(&mut cache, public, public);
            assert_eq!(cache.len(), 1);
            assert!(!cache.contains_key(&unknown));
        }
    }

    #[test]
    fn changing_resolution_keeps_equal_address_bits_distinct() {
        let unknown = Identity(42, None);
        let public = Identity(42, Some(false));
        let random = Identity(42, Some(true));
        for (first, second) in [(public, random), (random, public)] {
            let mut cache = HashMap::new();
            assert_eq!(admit(&mut cache, unknown, first), first);
            assert_eq!(admit(&mut cache, unknown, second), second);
            assert_eq!(cache.get(&first), Some(&first));
            assert_eq!(cache.get(&second), Some(&second));
            assert_eq!(cache.len(), 2);
        }
    }
}
