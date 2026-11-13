use everything_rs::{
    database::Database,
    model::{ExportFormat, FileEntry, SearchRequest},
    operations::{self, FileAction},
};
use std::{fs, sync::atomic::AtomicBool};

#[test]
fn efu_roundtrip_preserves_unicode_size_and_times() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("one.db");
    let mut db = Database::open(&db_path).unwrap();
    let entry = FileEntry {
        path: "C:\\Fotos\\Árbol, \"final\".jpg".into(),
        name: "Árbol, \"final\".jpg".into(),
        parent: "C:\\Fotos".into(),
        extension: "jpg".into(),
        size: Some(1234),
        modified: Some(1_700_000_000),
        created: Some(1_600_000_000),
        ..FileEntry::default()
    };
    db.upsert_batch("C:\\", 1, std::slice::from_ref(&entry))
        .unwrap();
    let output = dir.path().join("list.efu");
    assert_eq!(
        operations::export(
            &db_path,
            &output,
            &SearchRequest::default(),
            ExportFormat::Efu,
            &AtomicBool::new(false)
        )
        .unwrap(),
        1
    );
    let second = dir.path().join("two.db");
    assert_eq!(
        operations::import(&second, &output, &AtomicBool::new(false)).unwrap(),
        1
    );
    let page = Database::open(&second)
        .unwrap()
        .search(&SearchRequest::default())
        .unwrap();
    assert_eq!(page.rows[0].path, entry.path);
    assert_eq!(page.rows[0].size, entry.size);
    assert_eq!(page.rows[0].modified, entry.modified);
    assert!(!page.rows[0].source.is_empty());
}

#[test]
fn failed_import_does_not_replace_existing_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    let input = dir.path().join("list.efu");
    fs::write(&input, "Filename,Size\nC:\\good.txt,12\n").unwrap();
    operations::import(&db_path, &input, &AtomicBool::new(false)).unwrap();
    fs::write(
        &input,
        "Filename,Size\nC:\\other.txt,9\nC:\\broken.txt,not-a-number\n",
    )
    .unwrap();
    assert!(operations::import(&db_path, &input, &AtomicBool::new(false)).is_err());
    let page = Database::open(&db_path)
        .unwrap()
        .search(&SearchRequest::default())
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].name, "good.txt");
}

#[test]
fn rename_never_overwrites_an_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("old.txt");
    let target = dir.path().join("new.txt");
    fs::write(&from, "old").unwrap();
    fs::write(&target, "new").unwrap();
    assert!(
        operations::perform(FileAction::Rename {
            path: from.clone(),
            new_name: "new.txt".into()
        })
        .is_err()
    );
    assert_eq!(fs::read_to_string(&from).unwrap(), "old");
    assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    operations::perform(FileAction::Rename {
        path: from.clone(),
        new_name: "fresh.txt".into(),
    })
    .unwrap();
    assert!(!from.exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("fresh.txt")).unwrap(),
        "old"
    );
}

#[test]
fn rename_rejects_traversal_and_empty_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keep.txt");
    fs::write(&path, "keep").unwrap();
    for name in [
        "",
        ".",
        "..",
        "../escape.txt",
        "sub/file.txt",
        "sub\\file.txt",
    ] {
        assert!(
            operations::perform(FileAction::Rename {
                path: path.clone(),
                new_name: name.into()
            })
            .is_err()
        );
        assert!(path.exists());
    }
}

#[test]
fn export_cannot_replace_the_writer_lease_file() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    let db = Database::open(&db_path).unwrap();
    let lock = dir.path().join("index.db.writer.lock");
    fs::write(&lock, "lease inode").unwrap();
    assert!(
        operations::export(
            &db_path,
            &lock,
            &SearchRequest::default(),
            ExportFormat::Csv,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), "lease inode");
    db.integrity_check().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_rename_supports_paths_beyond_max_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = dir.path().to_path_buf();
    for i in 0..12 {
        parent.push(format!("long_directory_segment_{i:02}"));
    }
    fs::create_dir_all(&parent).unwrap();
    let source = parent.join("source.txt");
    fs::write(&source, "long path").unwrap();
    operations::perform(FileAction::Rename {
        path: source.clone(),
        new_name: "renamed.txt".into(),
    })
    .unwrap();
    assert!(!source.exists());
    assert_eq!(
        fs::read_to_string(parent.join("renamed.txt")).unwrap(),
        "long path"
    );
}

#[test]
fn export_rejects_database_and_sqlite_sidecar_destinations() {
    for filename in ["index.db", "index.db-wal", "index.db-shm"] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        drop(Database::open(&db_path).unwrap());
        let destination = dir.path().join(filename);
        let result = operations::export(
            &db_path,
            &destination,
            &SearchRequest::default(),
            ExportFormat::Csv,
            &AtomicBool::new(false),
        );
        assert!(result.is_err(), "export replaced protected file {filename}");
        Database::open(&db_path).unwrap().integrity_check().unwrap();
    }
}

#[test]
fn export_rejects_application_settings_without_overwriting_them() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    drop(Database::open(&db_path).unwrap());
    let destination = dir.path().join("settings.json");
    let original = b"{\"theme\":\"Dark\"}";
    fs::write(&destination, original).unwrap();
    assert!(
        operations::export(
            &db_path,
            &destination,
            &SearchRequest::default(),
            ExportFormat::Csv,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert_eq!(fs::read(&destination).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn export_rejects_symlink_and_hardlink_database_aliases() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    drop(Database::open(&db_path).unwrap());
    let symbolic = dir.path().join("symbolic.csv");
    let hard = dir.path().join("hard.csv");
    symlink(&db_path, &symbolic).unwrap();
    fs::hard_link(&db_path, &hard).unwrap();
    for destination in [&symbolic, &hard] {
        assert!(
            operations::export(
                &db_path,
                destination,
                &SearchRequest::default(),
                ExportFormat::Csv,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        Database::open(&db_path).unwrap().integrity_check().unwrap();
    }
    assert!(
        fs::symlink_metadata(&symbolic)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn export_rejects_database_reached_through_parent_symlink() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    fs::create_dir(&store).unwrap();
    let db_path = store.join("index.db");
    drop(Database::open(&db_path).unwrap());
    let alias = dir.path().join("alias");
    symlink(&store, &alias).unwrap();
    for filename in ["index.db", "index.db-wal", "index.db-shm"] {
        assert!(
            operations::export(
                &db_path,
                &alias.join(filename),
                &SearchRequest::default(),
                ExportFormat::Csv,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        Database::open(&db_path).unwrap().integrity_check().unwrap();
    }
}

#[test]
fn importing_windows_root_children_keeps_the_root_separator() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    let input = dir.path().join("list.efu");
    fs::write(&input, "Filename,Size\nC:\\file.txt,12\n").unwrap();
    operations::import(&db_path, &input, &AtomicBool::new(false)).unwrap();
    let page = Database::open(&db_path)
        .unwrap()
        .search(&SearchRequest {
            text: r#"parent:"C:\""#.into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].parent, r"C:\");
}

#[test]
fn importing_dotfiles_preserves_extension_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    let input = dir.path().join("list.efu");
    fs::write(
        &input,
        "Filename,Size\nC:\\data\\.bashrc,12\nC:\\data\\.config.JSON,12\n",
    )
    .unwrap();
    operations::import(&db_path, &input, &AtomicBool::new(false)).unwrap();
    let db = Database::open(&db_path).unwrap();
    let rows = db.search(&SearchRequest::default()).unwrap().rows;
    assert_eq!(
        rows.iter()
            .find(|entry| entry.name == ".bashrc")
            .unwrap()
            .extension,
        ""
    );
    assert_eq!(
        rows.iter()
            .find(|entry| entry.name == ".config.JSON")
            .unwrap()
            .extension,
        "json"
    );
    assert_eq!(
        db.search(&SearchRequest {
            text: "ext:bashrc".into(),
            ..Default::default()
        })
        .unwrap()
        .total,
        0
    );
}
