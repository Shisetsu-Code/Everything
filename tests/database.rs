use everything_rs::{
    database::Database,
    model::{FileEntry, SearchRequest, Sort, SortColumn},
};

fn row(path: &str, size: u64) -> FileEntry {
    FileEntry {
        path: path.into(),
        name: path.rsplit('/').next().unwrap().into(),
        parent: path.rsplit_once('/').unwrap().0.into(),
        extension: "txt".into(),
        size: Some(size),
        modified: Some(1_700_000_000),
        ..FileEntry::default()
    }
}

#[test]
fn durable_updates_deletion_and_fts_are_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("index.db");
    let mut db = Database::open(&file).unwrap();
    db.upsert_batch(
        "/docs",
        1,
        &[row("/docs/Árbol.txt", 30), row("/docs/readme.txt", 20)],
    )
    .unwrap();
    assert_eq!(db.total().unwrap(), 2);
    let req = SearchRequest {
        text: "ÁRBOL".into(),
        ..SearchRequest::default()
    };
    assert_eq!(db.search(&req).unwrap().rows[0].size, Some(30));
    db.upsert_batch("/docs", 2, &[row("/docs/Árbol.txt", 60)])
        .unwrap();
    drop(db);
    let mut db = Database::open(&file).unwrap();
    assert_eq!(db.search(&req).unwrap().rows[0].size, Some(60));
    db.remove_subtree("/docs/Árbol.txt").unwrap();
    assert_eq!(db.search(&req).unwrap().total, 0);
    assert_eq!(db.total().unwrap(), 1);
}

#[test]
fn prune_only_explicit_completed_root_and_keep_offline_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let mut offline = row("/a/offline.txt", 10);
    offline.source = "catalog.efu".into();
    db.upsert_batch("/a", 1, &[row("/a/stale.txt", 1), offline])
        .unwrap();
    db.upsert_batch("/b", 1, &[row("/b/unavailable.txt", 2)])
        .unwrap();
    db.upsert_batch("/a", 2, &[row("/a/current.txt", 3)])
        .unwrap();
    db.prune_root("/a", 2).unwrap();
    assert_eq!(db.total().unwrap(), 3);
    assert_eq!(
        db.search(&SearchRequest {
            text: "unavailable".into(),
            ..SearchRequest::default()
        })
        .unwrap()
        .total,
        1
    );
}

#[test]
fn deletion_respects_directory_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    db.upsert_batch(
        "/",
        1,
        &[
            row("/folder/a.txt", 1),
            row("/folder/deep/b.txt", 1),
            row("/folder-other/c.txt", 1),
        ],
    )
    .unwrap();
    db.remove_subtree("/folder").unwrap();
    assert_eq!(db.total().unwrap(), 1);
    assert_eq!(
        db.search(&SearchRequest::default()).unwrap().rows[0].path,
        "/folder-other/c.txt"
    );
}

#[test]
fn paging_and_size_order_have_no_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let rows: Vec<_> = (0..25)
        .map(|i| row(&format!("/data/{i:03}.txt"), 25 - i))
        .collect();
    db.upsert_batch("/data", 1, &rows).unwrap();
    let req = SearchRequest {
        limit: 10,
        sort: Sort {
            column: SortColumn::Size,
            descending: true,
        },
        ..SearchRequest::default()
    };
    let first = db.search(&req).unwrap();
    let second = db.search(&SearchRequest { offset: 10, ..req }).unwrap();
    assert_eq!(first.total, 25);
    assert_eq!(first.rows.len(), 10);
    assert_eq!(first.rows[0].size, Some(25));
    assert_eq!(second.rows[0].size, Some(15));
    assert!(
        first
            .rows
            .iter()
            .all(|a| second.rows.iter().all(|b| a.id != b.id))
    );
}

#[test]
fn partial_scan_prunes_readable_areas_and_preserves_failed_subtrees() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    db.upsert_batch(
        "/disk",
        1,
        &[
            row("/disk/denied/keep.txt", 1),
            row("/disk/denied-other/stale.txt", 1),
            row("/disk/accessible/gone.txt", 1),
        ],
    )
    .unwrap();
    db.upsert_batch("/disk", 2, &[row("/disk/accessible/current.txt", 2)])
        .unwrap();
    db.prune_root_except("/disk", 2, &["/disk/denied".into()])
        .unwrap();
    let results = db.search(&SearchRequest::default()).unwrap();
    assert_eq!(results.total, 2);
    assert!(results.rows.iter().any(|row| row.name == "keep.txt"));
    assert!(results.rows.iter().any(|row| row.name == "current.txt"));
    db.prune_root_except("/disk", 3, &["/disk".into()]).unwrap();
    assert_eq!(db.total().unwrap(), 2);
}

#[test]
fn offline_catalog_keeps_case_distinct_paths_on_all_platforms() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let mut upper = row("/data/A.txt", 1);
    upper.source = "efu:unix-archive".into();
    let mut lower = row("/data/a.txt", 2);
    lower.source = upper.source.clone();
    db.upsert_batch("", 0, &[upper, lower]).unwrap();
    assert_eq!(db.total().unwrap(), 2);
}

#[cfg(windows)]
#[test]
fn windows_live_identity_normalizes_extended_and_plain_paths() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&temp.path().join("index.db")).unwrap();
    let mut entry = row("/placeholder.txt", 1);
    entry.path = r"C:\data\File.txt".into();
    db.upsert_batch(r"C:\", 1, std::slice::from_ref(&entry))
        .unwrap();
    entry.path = r"\\?\C:\data\File.txt".into();
    db.upsert_batch(r"C:\", 2, &[entry]).unwrap();
    assert_eq!(db.total().unwrap(), 1);
    db.remove_subtree(r"C:\data").unwrap();
    assert_eq!(db.total().unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn subtree_deletion_treats_a_unix_backslash_as_part_of_the_name() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    db.upsert_batch(
        "/root",
        1,
        &[
            row(r"/root/a\b", 1),
            row(r"/root/a\b/child.txt", 1),
            row(r"/root/a\b\neighbor.txt", 1),
            row(r"/root/a\b-other.txt", 1),
        ],
    )
    .unwrap();
    assert_eq!(db.remove_subtree(r"/root/a\b").unwrap(), 2);
    let remaining = db.search(&SearchRequest::default()).unwrap().rows;
    assert_eq!(remaining.len(), 2);
    assert!(
        remaining
            .iter()
            .any(|entry| entry.path == r"/root/a\b\neighbor.txt")
    );
    assert!(
        remaining
            .iter()
            .any(|entry| entry.path == r"/root/a\b-other.txt")
    );
    db.integrity_check().unwrap();
}

#[cfg(unix)]
#[test]
fn subtree_pruning_and_descendants_respect_unix_backslash_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    db.upsert_batch(
        "/root",
        1,
        &[
            row(r"/root/a\b/stale.txt", 1),
            row(r"/root/a\b\neighbor.txt", 1),
            row(r"/root/trailing\/child.txt", 1),
            row(r"/root/trailing\neighbor.txt", 1),
        ],
    )
    .unwrap();
    db.upsert_batch("/root", 2, &[row(r"/root/a\b/fresh.txt", 1)])
        .unwrap();
    assert_eq!(db.prune_subtree(r"/root/a\b", 2).unwrap(), 1);
    assert_eq!(db.remove_descendants(r"/root/trailing\").unwrap(), 1);
    let remaining = db.search(&SearchRequest::default()).unwrap().rows;
    assert_eq!(remaining.len(), 3);
    for path in [
        r"/root/a\b/fresh.txt",
        r"/root/a\b\neighbor.txt",
        r"/root/trailing\neighbor.txt",
    ] {
        assert!(
            remaining.iter().any(|entry| entry.path == path),
            "missing {path}"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_path_identity_normalizes_slashes_for_updates_and_removals() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    db.upsert_batch("C:/Data", 1, &[row("C:/Data/file.txt", 1)])
        .unwrap();
    db.upsert_batch("C:/Data", 2, &[row("c:/data/file.txt", 2)])
        .unwrap();
    assert_eq!(db.total().unwrap(), 1);
    assert_eq!(
        db.root_for_path(r"C:\DATA\FILE.TXT").unwrap(),
        Some("C:/Data".into())
    );
    assert_eq!(db.remove_subtree(r"C:\Data").unwrap(), 1);
    assert_eq!(db.total().unwrap(), 0);
}
