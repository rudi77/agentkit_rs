use std::fs;
use tandem_app::repository::Repository;

#[test]
fn a_search_can_target_one_file_without_searching_a_sibling_directory() {
    let root = std::env::temp_dir().join(format!(
        "tandem-path-filter-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("src-other")).unwrap();
    fs::write(root.join("src/Api.cs"), "void DeleteRecord() {}\n").unwrap();
    fs::write(root.join("src-other/Api.cs"), "void DeleteRecord() {}\n").unwrap();
    let repo = Repository::open(&root, vec![]).unwrap();
    let single = repo.search("DeleteRecord", "src/Api.cs", false).unwrap();
    assert_eq!(single["matches"].as_array().unwrap().len(), 1);
    let directory = repo.search("DeleteRecord", "src", false).unwrap();
    assert_eq!(directory["matches"].as_array().unwrap().len(), 1);
    assert_eq!(repo.list_files("src", "*").unwrap()["total"], 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn skipped_binary_files_are_visible_as_a_search_limit() {
    let root = std::env::temp_dir().join(format!(
        "tandem-binary-filter-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("binary.data"), [0, 1, 2, 0]).unwrap();
    let repo = Repository::open(&root, vec![]).unwrap();
    let result = repo.search("anything", "", false).unwrap();
    assert_eq!(result["skipped_files"], 1);
    assert!(!repo.warnings().is_empty());
    fs::remove_dir_all(root).unwrap();
}
