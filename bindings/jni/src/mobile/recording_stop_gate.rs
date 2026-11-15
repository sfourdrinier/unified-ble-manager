/// Until the registry identifies the legacy collection owner, allow a stop
/// only if there is no possible owner, or exactly the requesting binding.
pub(crate) fn permits_stop(
    owner: Option<i64>,
    legacy: bool,
    installing: bool,
    bindings: &[i64],
) -> bool {
    if legacy || installing {
        return false;
    }
    match owner {
        None => bindings.is_empty(),
        Some(binding) => bindings.len() == 1 && bindings[0] == binding,
    }
}

#[cfg(test)]
mod tests {
    use super::permits_stop;

    #[test]
    fn offline_stop_requires_no_retained_host_or_installation() {
        assert!(permits_stop(None, false, false, &[]));
        assert!(!permits_stop(None, false, false, &[17]));
        assert!(!permits_stop(None, true, false, &[]));
        assert!(!permits_stop(None, false, true, &[]));
    }

    #[test]
    fn bound_stop_refuses_other_owners_or_unpublished_bindings() {
        assert!(permits_stop(Some(17), false, false, &[17]));
        assert!(!permits_stop(Some(17), false, false, &[17, 18]));
        assert!(!permits_stop(Some(17), false, false, &[18]));
        assert!(!permits_stop(Some(17), true, false, &[17]));
        assert!(!permits_stop(Some(17), false, true, &[17]));
    }
}
