use tandem_app::{validate_patch, Patch, Snapshot};

#[test]
fn overlapping_occurrences_are_ambiguous_too() {
    let snapshot = Snapshot {
        path: "x".into(),
        prefix: String::new(),
        editable: "aaaa".into(),
        suffix: String::new(),
    };
    let patch = Patch {
        path: "x".into(),
        old_text: "aaa".into(),
        new_text: "b".into(),
        reason: "Test".into(),
    };
    assert!(validate_patch(&snapshot, &patch).is_err());
}

#[test]
fn replacement_text_is_literal_including_dollar_sequences() {
    let snapshot = Snapshot {
        path: "x".into(),
        prefix: "🦀".into(),
        editable: "old".into(),
        suffix: "ä".into(),
    };
    let patch = Patch {
        path: "x".into(),
        old_text: "old".into(),
        new_text: "$& $1 $$".into(),
        reason: "Test".into(),
    };
    assert_eq!(validate_patch(&snapshot, &patch).unwrap(), "🦀$& $1 $$ä");
}
