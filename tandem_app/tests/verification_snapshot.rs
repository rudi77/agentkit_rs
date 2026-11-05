use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};
use tandem_app::repository::Repository;

#[test]
fn verification_binds_all_allowed_contents_and_detects_other_files_and_path_changes() {
    let root = std::env::temp_dir().join(format!(
        "tandem-verification-snapshot-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("First.cs"), "return 1;").unwrap();
    fs::write(root.join("Second.cs"), "return 2;").unwrap();
    fs::write(root.join(".env"), "PRIVATE").unwrap();
    fs::write(root.join("bin/output.bin"), [0u8; 100]).unwrap();
    let snapshot = || {
        Repository::open(&root, vec![])
            .unwrap()
            .workspace_snapshot()
            .unwrap()
    };
    let before = snapshot();
    assert_eq!(before.files.len(), 2);
    assert!(!before.files.contains_key(".env"));
    assert!(!before.files.contains_key("bin/output.bin"));
    fs::write(root.join("Second.cs"), "return 9;").unwrap();
    let after = snapshot();
    assert_ne!(after.fingerprint, before.fingerprint);
    assert_eq!(after.files["First.cs"], before.files["First.cs"]);
    assert_ne!(after.files["Second.cs"], before.files["Second.cs"]);
    fs::write(root.join("Added.cs"), "return 3;").unwrap();
    let added = snapshot();
    assert_ne!(added.fingerprint, after.fingerprint);
    assert!(added.files.contains_key("Added.cs"));
    fs::write(root.join("bin/output.bin"), [1u8; 100]).unwrap();
    assert_eq!(snapshot().fingerprint, added.fingerprint);
}
