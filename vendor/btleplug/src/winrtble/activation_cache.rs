use std::collections::HashMap;
use std::sync::Arc;

pub(super) fn graph_activation<T>(
    entries: &mut HashMap<(u64, u16), Arc<T>>,
    revision: u64,
    handle: u16,
    create: impl FnOnce() -> Arc<T>,
) -> Arc<T> {
    entries.entry((revision, handle)).or_insert_with(create).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_does_not_discard_an_old_registration_owner() {
        let mut entries = HashMap::new();
        let old = graph_activation(&mut entries, 1, 7, || Arc::new("old"));
        let new = graph_activation(&mut entries, 2, 7, || Arc::new("new"));
        assert!(Arc::ptr_eq(entries.get(&(1, 7)).unwrap(), &old));
        assert!(Arc::ptr_eq(entries.get(&(2, 7)).unwrap(), &new));
    }
}
