use everything_rs::{
    database::{Database, WriteProfile},
    model::{FileEntry, SearchRequest, Sort, SortColumn},
};
use rusqlite::Connection;
use std::collections::BTreeMap;

type BatchWriter = fn(&mut Database, &str, i64, &[FileEntry]) -> anyhow::Result<()>;
const BATCH_WRITERS: [BatchWriter; 2] = [Database::upsert_batch, Database::upsert_scan_batch];

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
fn startup_cleanup_preserves_exact_root_owners_and_offline_catalogs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    assert_eq!(db.remove_unconfigured_roots(&[]).unwrap(), 0);
    assert_eq!(db.remove_subtree("/missing").unwrap(), 0);
    for (root, path) in [
        ("", "/empty-owner.txt"),
        ("/active", "/active/keep.txt"),
        ("/active/nested", "/active/nested/keep.txt"),
        ("/álbum_日本", "/álbum_日本/keep.txt"),
        ("/active-other", "/active-other/remove.txt"),
        ("/Active", "/case-owner/remove.txt"),
    ] {
        db.upsert_batch(root, 1, &[row(path, 1)]).unwrap();
    }
    let mut offline = row("/active-other/remove.txt", 7);
    offline.source = "archive.efu".into();
    db.upsert_batch("/active-other", 1, &[offline]).unwrap();
    let roots = vec!["".into(), "/active".into(), "/álbum_日本".into()];
    assert_eq!(db.remove_unconfigured_roots(&roots).unwrap(), 3);
    assert_eq!(db.total().unwrap(), 4);
    assert_eq!(
        db.root_for_path("/active/keep.txt").unwrap(),
        Some("/active".into())
    );
    assert_eq!(db.remove_unconfigured_roots(&roots).unwrap(), 0);
    assert_eq!(db.remove_subtree("/álbum_日本/").unwrap(), 1);
    assert_eq!(db.remove_unconfigured_roots(&[]).unwrap(), 2);
    let remaining = db.search(&SearchRequest::default()).unwrap().rows;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, "/active-other/remove.txt");
    assert_eq!(remaining[0].source, "archive.efu");
    assert_eq!(remaining[0].size, Some(7));
    db.integrity_check().unwrap();
}

#[test]
fn subtree_cleanup_handles_volume_root_overlap_without_deleting_offline_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let (volume, other) = if cfg!(windows) {
        ("C:\\", "D:\\keep.txt")
    } else {
        ("/", "relative/keep.txt")
    };
    let paths = [
        volume.to_owned(),
        format!("{volume}folder"),
        format!("{volume}folder{}nested.txt", std::path::MAIN_SEPARATOR),
        format!("{volume}folder-other.txt"),
        other.to_owned(),
    ];
    let mut entries: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| FileEntry {
            path: path.clone(),
            name: format!("volume_entry_{i}"),
            ..FileEntry::default()
        })
        .collect();
    let mut offline = entries[2].clone();
    offline.source = "archive.efu".into();
    entries.push(offline);
    db.upsert_batch(volume, 1, &entries).unwrap();
    assert_eq!(db.remove_subtree(volume).unwrap(), 4);
    assert_eq!(db.total().unwrap(), 2);
    let remaining = db.search(&SearchRequest::default()).unwrap().rows;
    assert!(remaining.iter().any(|entry| entry.path == other));
    assert!(remaining.iter().any(|entry| entry.source == "archive.efu"));
    db.integrity_check().unwrap();
}

#[test]
fn startup_cleanup_rolls_back_all_deletions_and_fts_on_late_failure() {
    type Cleanup = fn(&mut Database) -> anyhow::Result<usize>;
    let cleanups: [Cleanup; 2] = [
        |db| db.remove_subtree("/stale"),
        |db| db.remove_unconfigured_roots(&["/keep".into()]),
    ];
    for cleanup in cleanups {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index.db");
        let mut db = Database::open(&file).unwrap();
        db.upsert_batch(
            "/a-owner",
            1,
            &[row("/stale", 1), row("/stale/a-first.txt", 2)],
        )
        .unwrap();
        db.upsert_batch("/z-owner", 1, &[row("/stale/z-reject.txt", 3)])
            .unwrap();
        db.upsert_batch("/keep", 1, &[row("/keep/other.txt", 4)])
            .unwrap();
        let mut offline = row("/stale/z-reject.txt", 5);
        offline.source = "archive.efu".into();
        db.upsert_batch("/z-owner", 1, &[offline]).unwrap();
        let original = db.search(&SearchRequest::default()).unwrap().rows;
        let sql = Connection::open(&file).unwrap();
        sql.execute_batch(
            "CREATE TRIGGER reject_late_cleanup BEFORE DELETE ON files
             WHEN old.path='/stale/z-reject.txt' AND old.source='' BEGIN
               SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM files WHERE source='' AND path IN ('/stale','/stale/a-first.txt'))
                 THEN 1 ELSE RAISE(ABORT,'earlier deletions missing') END;
               SELECT RAISE(ABORT,'test rejected late cleanup');
             END;",
        )
        .unwrap();
        let error = cleanup(&mut db).unwrap_err();
        assert!(error.to_string().contains("test rejected late cleanup"));
        assert_eq!(db.total().unwrap(), 5);
        assert_eq!(db.search(&SearchRequest::default()).unwrap().rows, original);
        db.integrity_check().unwrap();
        sql.execute_batch("DROP TRIGGER reject_late_cleanup")
            .unwrap();
        assert_eq!(cleanup(&mut db).unwrap(), 3);
        assert_eq!(db.total().unwrap(), 2);
        db.integrity_check().unwrap();
    }
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

#[test]
fn unchanged_scan_refreshes_ownership_without_rewriting_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("index.db");
    let mut db = Database::open(&file).unwrap();
    let mut entries: Vec<_> = (0..600)
        .map(|i| row(&format!("/root/entry_{i:04}.txt"), i))
        .collect();
    entries[0].size = None;
    entries[0].modified = None;
    db.upsert_batch("/root", 1, &entries).unwrap();
    let sql = Connection::open(&file).unwrap();
    sql.execute_batch(
        "CREATE TABLE scan_audit(metadata_updates INTEGER, all_updates INTEGER);
         INSERT INTO scan_audit VALUES(0,0);
         CREATE TRIGGER audit_metadata AFTER UPDATE OF path,name,parent,extension,name_fold,path_fold,parent_fold,size,modified,created,is_dir,is_symlink ON files BEGIN
           UPDATE scan_audit SET metadata_updates=metadata_updates+1;
         END;
         CREATE TRIGGER audit_any_update AFTER UPDATE ON files BEGIN
           UPDATE scan_audit SET all_updates=all_updates+1;
         END;",
    )
    .unwrap();

    db.upsert_scan_batch("/new-owner", 2, &entries).unwrap();
    let updates: (i64, i64) = sql
        .query_row("SELECT * FROM scan_audit", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(updates, (0, 600));
    let refreshed: i64 = sql
        .query_row(
            "SELECT count(*) FROM files WHERE root='/new-owner' AND generation=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(refreshed, 600);
    db.upsert_scan_batch("/new-owner", 2, &entries).unwrap();
    let updates: i64 = sql
        .query_row("SELECT all_updates FROM scan_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(updates, 600, "identical generation must not rewrite rows");
    assert_eq!(db.total().unwrap(), 600);
    db.integrity_check().unwrap();
}

#[test]
fn write_profile_is_opt_in_accumulates_batches_and_resets_without_changing_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let entries: Vec<_> = (0..513)
        .map(|i| row(&format!("/root/profile_{i:04}.txt"), i))
        .collect();
    db.upsert_batch("/root", 1, &entries).unwrap();
    assert_eq!(db.take_write_profile(), None);

    db.set_profile_writes(true);
    assert_eq!(db.take_write_profile(), Some(WriteProfile::default()));
    db.upsert_batch("/root", 2, &entries).unwrap();
    db.upsert_scan_batch("/root", 3, &entries).unwrap();
    let profile = db.take_write_profile().unwrap();
    assert_eq!(profile.batches, 2);
    assert_eq!(profile.rows, 1026);
    assert!(!profile.begin.is_zero());
    assert!(!profile.upsert.is_zero());
    assert!(!profile.touch.is_zero());
    assert!(!profile.commit.is_zero());
    assert!(profile.cache_writes > 0);
    assert_eq!(db.take_write_profile(), Some(WriteProfile::default()));

    // A write outside upsert_* must not leak into the next batch's counters.
    db.next_generation().unwrap();
    db.upsert_scan_batch("/root", 3, &entries).unwrap();
    let unchanged = db.take_write_profile().unwrap();
    assert_eq!(unchanged.batches, 1);
    assert_eq!(unchanged.rows, 513);
    assert_eq!(unchanged.cache_writes, 0);
    assert_eq!(unchanged.cache_spills, 0);
    assert_eq!(db.total().unwrap(), 513);
    assert_eq!(
        db.search(&SearchRequest {
            text: "profile_".into(),
            ..SearchRequest::default()
        })
        .unwrap()
        .total,
        513
    );
    db.integrity_check().unwrap();

    db.set_profile_writes(false);
    db.upsert_scan_batch("/root", 4, &entries).unwrap();
    assert_eq!(db.take_write_profile(), None);
    db.set_profile_writes(true);
    assert_eq!(db.take_write_profile(), Some(WriteProfile::default()));
}

#[test]
fn scan_updates_each_metadata_field_and_nulls_without_replacing_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let mutations: [fn(&mut FileEntry); 8] = [
        |entry| entry.name = "Árbol-renamed.txt".into(),
        |entry| entry.parent = "/different-parent".into(),
        |entry| entry.extension = "pdf".into(),
        |entry| entry.size = None,
        |entry| entry.modified = None,
        |entry| entry.created = Some(0),
        |entry| entry.is_dir = true,
        |entry| entry.is_symlink = true,
    ];
    let entries: Vec<_> = (0..8)
        .map(|i| row(&format!("/root/entry_{i}.txt"), 70))
        .collect();
    db.upsert_batch("/root", 1, &entries).unwrap();
    let original: BTreeMap<_, _> = db
        .search(&SearchRequest::default())
        .unwrap()
        .rows
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let mut changed = entries;
    for (entry, mutate) in changed.iter_mut().zip(mutations) {
        entry.id = original[&entry.path].id;
        mutate(entry);
    }
    db.upsert_scan_batch("/root", 2, &changed).unwrap();
    let actual: BTreeMap<_, _> = db
        .search(&SearchRequest::default())
        .unwrap()
        .rows
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for entry in &changed {
        assert_eq!(&actual[&entry.path], entry);
    }
    assert_eq!(
        db.search(&SearchRequest {
            text: "árbol-renamed".into(),
            ..SearchRequest::default()
        })
        .unwrap()
        .total,
        1
    );
    db.upsert_scan_batch("/root", 3, &original.values().cloned().collect::<Vec<_>>())
        .unwrap();
    let restored: BTreeMap<_, _> = db
        .search(&SearchRequest::default())
        .unwrap()
        .rows
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    assert_eq!(restored, original);
    assert_eq!(
        db.search(&SearchRequest {
            text: "árbol-renamed".into(),
            ..SearchRequest::default()
        })
        .unwrap()
        .total,
        0
    );
    db.integrity_check().unwrap();
}

#[test]
fn large_batches_preserve_duplicate_order_across_sql_chunks() {
    for write in BATCH_WRITERS {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
        let duplicate = row("/root/duplicate.txt", 1);
        db.upsert_batch("/root", 1, std::slice::from_ref(&duplicate))
            .unwrap();
        let original_id = db.search(&SearchRequest::default()).unwrap().rows[0].id;
        // 3,005 rows exceed SQLite's parameter limit if emitted as one statement.
        let mut entries: Vec<_> = (0..3005)
            .map(|i| row(&format!("/root/filler_{i:04}.txt"), i))
            .collect();
        for (index, size) in [
            (0, 1),
            (1, 2),
            (255, 1),
            (256, 3),
            (512, 3),
            (513, 4),
            (3004, 5),
        ] {
            entries[index] = row("/root/duplicate.txt", size);
        }
        for (index, size) in [(254, 10), (257, 11), (3003, 12)] {
            entries[index] = row("/root/new-duplicate.txt", size);
        }
        write(&mut db, "/root", 2, &entries).unwrap();
        assert_eq!(db.total().unwrap(), 2997);
        let results = db
            .search(&SearchRequest {
                text: "duplicate".into(),
                ..SearchRequest::default()
            })
            .unwrap();
        assert_eq!(results.total, 2);
        let existing = results
            .rows
            .iter()
            .find(|entry| entry.path == duplicate.path)
            .unwrap();
        assert_eq!(existing.id, original_id);
        assert_eq!(existing.size, Some(5));
        assert_eq!(
            results
                .rows
                .iter()
                .find(|entry| entry.name == "new-duplicate.txt")
                .unwrap()
                .size,
            Some(12)
        );
        assert_eq!(
            db.search(&SearchRequest {
                text: "filler_".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .total,
            2995
        );
        write(&mut db, "/root", 3, &[]).unwrap();
        assert_eq!(db.total().unwrap(), 2997);
        db.integrity_check().unwrap();
    }
}

#[test]
fn failed_later_sql_chunk_rolls_back_rows_fts_and_generation() {
    for write in BATCH_WRITERS {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index.db");
        let mut db = Database::open(&file).unwrap();
        db.upsert_batch(
            "/original",
            1,
            &[row("/root/existing.txt", 1), row("/root/unseen.txt", 2)],
        )
        .unwrap();
        let original = db.search(&SearchRequest::default()).unwrap().rows;
        let sql = Connection::open(&file).unwrap();
        sql.execute_batch(
            "CREATE TRIGGER reject_test_row BEFORE INSERT ON files WHEN new.name='reject.txt' BEGIN
               SELECT RAISE(ABORT,'test rejected row');
             END;",
        )
        .unwrap();
        let mut entries: Vec<_> = (0..600)
            .map(|i| row(&format!("/root/newfile_{i:04}.txt"), i))
            .collect();
        entries[0] = row("/root/existing.txt", 99);
        entries[0].name = "renamed-during-failed-batch.txt".into();
        entries[599] = row("/root/reject.txt", 1);
        assert!(write(&mut db, "/new-owner", 2, &entries).is_err());
        assert_eq!(db.total().unwrap(), 2);
        assert_eq!(db.search(&SearchRequest::default()).unwrap().rows, original);
        let untouched: i64 = sql
            .query_row(
                "SELECT count(*) FROM files WHERE root='/original' AND generation=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(untouched, 2);
        for text in ["newfile", "renamed-during-failed-batch"] {
            assert_eq!(
                db.search(&SearchRequest {
                    text: text.into(),
                    ..SearchRequest::default()
                })
                .unwrap()
                .total,
                0
            );
        }
        db.integrity_check().unwrap();
        write(&mut db, "/root", 3, &[row("/root/after-error.txt", 3)]).unwrap();
        assert_eq!(db.total().unwrap(), 3);
    }
}

#[test]
fn failed_late_scan_touch_rolls_back_every_chunk_and_keeps_connection_usable() {
    for action in ["ABORT", "ROLLBACK"] {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index.db");
        let mut db = Database::open(&file).unwrap();
        let mut entries: Vec<_> = (0..600)
            .map(|i| row(&format!("/root/entry_{i:04}.txt"), i))
            .collect();
        db.upsert_batch("/original", 1, &entries).unwrap();
        let request = SearchRequest {
            limit: 1000,
            ..SearchRequest::default()
        };
        let original = db.search(&request).unwrap().rows;
        assert_eq!(original.len(), 600);
        let sql = Connection::open(&file).unwrap();
        // The rejected row has unchanged metadata, so only the generation touch
        // can invoke this trigger. Check that earlier chunks and the current
        // chunk have already changed metadata before injecting the late failure.
        sql.execute_batch(&format!(
            "CREATE TRIGGER reject_late_touch BEFORE UPDATE OF root,generation ON files
             WHEN new.path='/root/entry_0599.txt' AND new.generation=2 BEGIN
               SELECT CASE WHEN
                 EXISTS(SELECT 1 FROM files WHERE path='/root/entry_0000.txt' AND name='renamed-before-touch-failure.txt' AND size=99) AND
                 EXISTS(SELECT 1 FROM files WHERE path='/root/entry_0300.txt' AND root='/new-owner' AND generation=2) AND
                 EXISTS(SELECT 1 FROM files WHERE path='/root/new-before-touch-failure.txt') AND
                 EXISTS(SELECT 1 FROM files WHERE path='/root/entry_0550.txt' AND size=99999)
                 THEN 1 ELSE RAISE(ABORT,'earlier scan changes missing') END;
               SELECT RAISE({action},'test rejected late touch');
             END;"
        ))
        .unwrap();
        entries[0].size = Some(99);
        entries[0].name = "renamed-before-touch-failure.txt".into();
        entries[257] = row("/root/new-before-touch-failure.txt", 10);
        entries[550].size = Some(99999);
        db.set_profile_writes(true);
        let error = db.upsert_scan_batch("/new-owner", 2, &entries).unwrap_err();
        assert!(error.to_string().contains("test rejected late touch"));
        let profile = db.take_write_profile().unwrap();
        assert_eq!(profile.batches, 1);
        assert_eq!(profile.rows, 600);
        assert!(!profile.upsert.is_zero());
        assert!(!profile.touch.is_zero());
        assert!(profile.commit.is_zero());
        assert_eq!(db.total().unwrap(), 600);
        assert_eq!(db.search(&request).unwrap().rows, original);
        let untouched: i64 = sql
            .query_row(
                "SELECT count(*) FROM files WHERE root='/original' AND generation=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(untouched, 600);
        assert_eq!(
            db.search(&SearchRequest {
                text: "before-touch-failure".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .total,
            0
        );
        db.integrity_check().unwrap();
        db.upsert_scan_batch("/root", 3, &[row("/root/after-error.txt", 3)])
            .unwrap();
        assert_eq!(db.total().unwrap(), 601);
        db.integrity_check().unwrap();
    }
}

#[test]
fn committed_scan_batches_keep_searches_live_and_preserve_old_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("index.db");
    let mut writer = Database::open(&file).unwrap();
    writer
        .upsert_batch(
            "/root",
            1,
            &[row("/root/old.txt", 1), row("/root/unseen.txt", 2)],
        )
        .unwrap();
    let reader = Database::open(&file).unwrap();
    let snapshot = Connection::open(&file).unwrap();
    snapshot.execute_batch("BEGIN;").unwrap();
    let before: i64 = snapshot
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 2);
    let entries: Vec<_> = (0..513)
        .map(|i| row(&format!("/root/newentry_{i:04}.txt"), i))
        .collect();
    writer.begin_scan_writes().unwrap();
    writer.upsert_scan_batch("/root", 2, &entries).unwrap();
    assert_eq!(
        reader
            .search(&SearchRequest {
                text: "newentry".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .total,
        513
    );
    assert_eq!(reader.total().unwrap(), 515);
    writer.finish_scan_writes().unwrap();
    let old_count: i64 = snapshot
        .query_row("SELECT value FROM metadata WHERE key='total'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_count, 2, "finishing must preserve the active snapshot");
    let new_reader = Database::open(&file).unwrap();
    assert_eq!(new_reader.total().unwrap(), 515);
    snapshot.execute_batch("COMMIT;").unwrap();
    writer.checkpoint().unwrap();
    let after: i64 = snapshot
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 515);
    let unseen: i64 = snapshot
        .query_row("SELECT count(*) FROM files WHERE generation=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        unseen, 2,
        "stopping after a partial scan must retain unseen records"
    );
    drop(snapshot);
    drop(reader);
    drop(new_reader);
    drop(writer);
    let reopened = Database::open(&file).unwrap();
    assert_eq!(reopened.total().unwrap(), 515);
    assert_eq!(
        reopened
            .search(&SearchRequest {
                text: "newentry".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .total,
        513
    );
    reopened.integrity_check().unwrap();
}

#[cfg(windows)]
#[test]
fn scan_batches_normalize_live_windows_identity_and_keep_offline_case_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(&tmp.path().join("index.db")).unwrap();
    let mut live = row("/placeholder.txt", 1);
    live.path = r"\\?\C:\Data\File.txt".into();
    db.upsert_scan_batch(r"C:\", 1, std::slice::from_ref(&live))
        .unwrap();
    let id = db.search(&SearchRequest::default()).unwrap().rows[0].id;
    live.path = "c:/data/file.txt".into();
    live.size = None;
    let mut upper = row("/archive/A.txt", 2);
    upper.source = "efu:archive".into();
    let mut lower = row("/archive/a.txt", 3);
    lower.source = upper.source.clone();
    db.upsert_scan_batch(r"C:\", 2, &[live, upper, lower])
        .unwrap();
    assert_eq!(db.total().unwrap(), 3);
    let local = db
        .search(&SearchRequest::default())
        .unwrap()
        .rows
        .into_iter()
        .find(|entry| entry.source.is_empty())
        .unwrap();
    assert_eq!(local.id, id);
    assert_eq!(local.size, None);
    assert_eq!(db.remove_subtree(r"C:\DATA").unwrap(), 1);
    assert_eq!(db.total().unwrap(), 2);
    db.integrity_check().unwrap();
}
