use std::fs;
use std::path::PathBuf;
use tandem_app::repository::{fingerprint, Overlay, Repository};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "tandem-repository-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("AGENTS.md"), "Lies [CLAUDE.md](CLAUDE.md).\n").unwrap();
        fs::write(
            root.join("CLAUDE.md"),
            "Keine bestehenden Tests ändern.\nLies [Regeln](src/CLAUDE.md).\n",
        )
        .unwrap();
        fs::write(
            root.join("src/CLAUDE.md"),
            "PascalCase im öffentlichen Vertrag.\n",
        )
        .unwrap();
        fs::write(
            root.join("src/Service.cs"),
            "class Service {\n  void DeleteRecord() {}\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/DeleteTests.cs"),
            "void DeleteRecord_returns_405() { service.DeleteRecord(); }\n",
        )
        .unwrap();
        Self(root)
    }
    fn open(&self) -> Repository {
        Repository::open(&self.0, vec![]).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn secrets_and_ignored_files_are_denied_even_when_requested_directly() {
    let f = Fixture::new();
    for name in [
        ".env",
        "appsettings.Development.json",
        "secrets.json",
        "private.txt",
        "ignored.txt",
    ] {
        fs::write(f.0.join(name), "DO_NOT_DISCLOSE_THIS_SENTINEL").unwrap();
    }
    fs::write(f.0.join(".gitignore"), "private.txt\n").unwrap();
    fs::write(f.0.join(".tandemignore"), "ignored.txt\n!.env\n").unwrap();
    let repo = f.open();
    for name in [
        ".env",
        "appsettings.Development.json",
        "secrets.json",
        "private.txt",
        "ignored.txt",
    ] {
        assert!(repo.read_file(name, 1, 10).is_err(), "{name}");
    }
    let listing = repo.list_files("", "").unwrap().to_string();
    assert!(!listing.contains("appsettings"));
    assert!(!listing.contains("private.txt"));
    assert_eq!(
        repo.search("DO_NOT_DISCLOSE_THIS_SENTINEL", "", false)
            .unwrap()["matches"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn absolute_and_parent_paths_cannot_escape_the_workspace() {
    let f = Fixture::new();
    let repo = f.open();
    for path in [
        "../outside",
        "src/../../outside",
        "src\\..\\Service.cs",
        "/etc/passwd",
        "C:\\Windows\\win.ini",
        "//server/share",
    ] {
        assert!(repo.check_path(path).is_err(), "{path}");
    }
}

#[test]
fn instructions_are_followed_and_sources_bind_exact_contents_and_lines() {
    let f = Fixture::new();
    let repo = f.open();
    let result = repo.read_file("src/Service.cs", 2, 1).unwrap();
    assert_eq!(result["content"], "2\t  void DeleteRecord() {}");
    assert_eq!(
        result["source"]["sha256"],
        fingerprint(&fs::read(f.0.join("src/Service.cs")).unwrap())
    );
    let instructions = serde_json::to_string(&repo.instructions()).unwrap();
    assert!(instructions.contains("Keine bestehenden Tests"));
    assert!(instructions.contains("PascalCase"));
    assert!(repo
        .sources()
        .iter()
        .any(|s| s.path == "AGENTS.md" && s.kind == "instruction"));
}

#[test]
fn editor_overlay_is_read_without_writing_the_file() {
    let f = Fixture::new();
    let disk = fs::read(f.0.join("src/Service.cs")).unwrap();
    let repo = Repository::open(
        &f.0,
        vec![Overlay {
            path: "src/Service.cs".into(),
            content: "// Ungespeichert 🦀\nvoid Changed() {}".into(),
        }],
    )
    .unwrap();
    let read = repo.read_file("src/Service.cs", 1, 2).unwrap();
    assert!(read["content"].as_str().unwrap().contains("Ungespeichert"));
    assert_eq!(read["source"]["editor"], true);
    assert_eq!(fs::read(f.0.join("src/Service.cs")).unwrap(), disk);
    assert!(Repository::open(
        &f.0,
        vec![Overlay {
            path: ".env".into(),
            content: "secret".into()
        }]
    )
    .is_err());
}

#[test]
fn source_changes_and_new_files_invalidate_an_old_analysis() {
    let f = Fixture::new();
    let repo = f.open();
    repo.read_file("src/Service.cs", 1, 20).unwrap();
    let stamp = repo.catalog_id().unwrap();
    assert!(repo.validate(&repo.sources(), &stamp).unwrap());
    fs::write(f.0.join("src/Service.cs"), "// geändert\n").unwrap();
    assert!(!repo.validate(&repo.sources(), &stamp).unwrap());
    let fresh = f.open();
    let stamp = fresh.catalog_id().unwrap();
    fs::write(f.0.join("tests/NewTest.cs"), "// neuer relevanter Test\n").unwrap();
    assert!(!fresh.validate(&fresh.sources(), &stamp).unwrap());
}

#[test]
fn reference_search_reports_tests_as_textual_evidence() {
    let f = Fixture::new();
    let repo = f.open();
    let result = repo.search("DeleteRecord", "", true).unwrap();
    assert!(result["semantics"].as_str().unwrap().contains("Textuelle"));
    assert!(result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["source"]["kind"] == "test"));
    assert!(repo.read_file("src/Service.cs", 1, 241).is_err());
}

#[test]
fn directory_links_are_not_followed() {
    let f = Fixture::new();
    let external = Fixture::new();
    let link = f.0.join("linked");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&external.0, &link).unwrap();
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&external.0)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "Test-Junction konnte nicht angelegt werden"
        );
    }
    let repo = f.open();
    assert!(repo.read_file("linked/src/Service.cs", 1, 10).is_err());
    assert!(!repo
        .list_files("", "linked")
        .unwrap()
        .to_string()
        .contains("Service.cs"));
}
