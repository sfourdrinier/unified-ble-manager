use ubm_mobile::binding_release_succeeded;

#[test]
fn only_an_unambiguous_success_receipt_releases_a_binding() {
    assert!(binding_release_succeeded(
        r#"{"failures":[],"state":"released"}"#
    ));
    for receipt in [
        r#"{"state":"release-failed","failures":[]}"#,
        r#"{"state":"released","failures":[{"resourceKind":"source"}]}"#,
        r#"{"state":"released"}"#,
        r#"{"state":"released","failures":null}"#,
        r#"{"state":"released","state":"release-failed","failures":[]}"#,
        "not json",
    ] {
        assert!(!binding_release_succeeded(receipt), "{receipt}");
    }
}
