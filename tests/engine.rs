//! Exercise the real scanner, native watcher, SQLite writer, and search worker
//! together. All waits are driven by worker events and share a 20 second test
//! deadline; no test depends on a fixed indexing or notification delay.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use everything_rs::config::AppConfig;
use everything_rs::engine::{Command, Engine, Event};
use everything_rs::model::{SearchPage, SearchRequest};
use tempfile::{TempDir, tempdir};

struct RunningIndex {
    _workspace: TempDir,
    engine: Engine,
    next_query: u64,
    deadline: Instant,
}

#[test]
fn second_engine_cannot_write_the_same_index() {
    let workspace = tempdir().unwrap();
    let root = workspace.path().join("files");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("keep.txt"), "keep").unwrap();
    let data = workspace.path().join("data");
    let config = AppConfig {
        roots: vec![root.clone()],
        ..AppConfig::default()
    };
    let mut first = RunningIndex::start(
        workspace,
        root,
        Vec::new(),
        Instant::now() + Duration::from_secs(20),
    );
    assert!(Engine::start(data.join("index.sqlite"), data.join("config.json"), config).is_err());
    assert_eq!(first.query("keep.txt").total, 1);
    first.shutdown();
}

#[test]
fn startup_applies_changed_roots_and_preserves_offline_catalogs() {
    use everything_rs::{database::Database, model::FileEntry};
    let workspace = tempdir().unwrap();
    let root = workspace.path().join("new-files");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("current.txt"), "current").unwrap();
    let data = workspace.path().join("data");
    fs::create_dir(&data).unwrap();
    let mut db = Database::open(&data.join("index.sqlite")).unwrap();
    db.upsert_batch(
        "/old-root",
        1,
        &[
            FileEntry {
                path: "/old-root/stale.txt".into(),
                name: "stale.txt".into(),
                ..FileEntry::default()
            },
            FileEntry {
                path: "/old-root/catalog.txt".into(),
                name: "catalog.txt".into(),
                source: "efu:archive".into(),
                ..FileEntry::default()
            },
        ],
    )
    .unwrap();
    drop(db);
    let mut running = RunningIndex::start(
        workspace,
        root,
        Vec::new(),
        Instant::now() + Duration::from_secs(20),
    );
    assert_eq!(running.query("stale.txt").total, 0);
    assert_eq!(running.query("catalog.txt").total, 1);
    assert_eq!(running.query("current.txt").total, 1);
    running.shutdown();
}

impl RunningIndex {
    fn start(
        workspace: TempDir,
        configured_root: PathBuf,
        excludes: Vec<PathBuf>,
        deadline: Instant,
    ) -> Self {
        let data = workspace.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let engine = Engine::start(
            data.join("index.sqlite"),
            data.join("config.json"),
            AppConfig {
                roots: vec![configured_root],
                excludes,
                auto_scan: true,
                watch: true,
                ..AppConfig::default()
            },
        )
        .unwrap();
        let instance = Self {
            _workspace: workspace,
            engine,
            next_query: 1,
            deadline,
        };
        loop {
            match instance.recv_event() {
                Some(Event::IndexFinished {
                    cancelled, errors, ..
                }) => {
                    assert!(!cancelled, "initial scan was cancelled");
                    assert_eq!(errors, 0, "initial scan had filesystem errors");
                    break;
                }
                Some(Event::Error(error)) => panic!("index worker failed: {error}"),
                _ => {}
            }
        }
        instance
    }

    fn recv_event(&self) -> Option<Event> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "engine did not reach the expected state within 20 seconds"
        );
        match self
            .engine
            .events
            .recv_timeout(remaining.min(Duration::from_millis(100)))
        {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("all engine workers stopped unexpectedly")
            }
        }
    }

    fn query(&mut self, text: &str) -> SearchPage {
        let id = self.next_query;
        self.next_query += 1;
        self.engine
            .send(Command::Search(SearchRequest {
                id,
                text: text.to_owned(),
                limit: 500,
                ..SearchRequest::default()
            }))
            .unwrap();
        loop {
            match self.recv_event() {
                Some(Event::Results { request_id, page }) if request_id == id => return page,
                Some(Event::SearchError {
                    request_id,
                    message,
                }) if request_id == id => panic!("search failed: {message}"),
                Some(Event::Error(error)) => panic!("engine failed: {error}"),
                _ => {}
            }
        }
    }

    fn await_files(&mut self, expected: &[PathBuf]) {
        let expected: BTreeSet<String> = expected
            .iter()
            .map(|path| path.to_str().unwrap().to_owned())
            .collect();
        loop {
            let page = self.query("file:");
            let actual: BTreeSet<String> = page.rows.into_iter().map(|row| row.path).collect();
            if actual == expected {
                assert_eq!(page.total, expected.len() as u64);
                return;
            }
            assert!(
                Instant::now() < self.deadline,
                "index did not converge; expected {expected:?}, found {actual:?}"
            );
            // Give worker notifications a chance to announce the next state
            // before submitting another real database query.
            loop {
                match self.recv_event() {
                    Some(Event::Changed { .. } | Event::IndexFinished { .. }) | None => break,
                    Some(Event::Error(error)) => panic!("engine failed: {error}"),
                    _ => {}
                }
            }
        }
    }

    fn shutdown(self) {
        self.engine.send(Command::Shutdown).unwrap();
        let deadline = (Instant::now() + Duration::from_secs(3)).min(self.deadline);
        loop {
            assert!(
                Instant::now() < deadline,
                "engine workers did not stop after Shutdown"
            );
            if let Err(RecvTimeoutError::Disconnected) =
                self.engine.events.recv_timeout(Duration::from_millis(50))
            {
                break;
            }
        }
        // Worker handles have closed before TempDir cleanup, including on Windows.
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn realtime_changes_reconcile_created_moved_renamed_and_replaced_subtrees() {
    let deadline = Instant::now() + Duration::from_secs(20);
    let workspace = tempdir().unwrap();
    let root = workspace.path().join("files");
    write(&root.join("original/nested/one.txt"), "one");
    write(&root.join("original/two.txt"), "two");
    write(&root.join("excluded/nested/ignored.txt"), "not indexed");
    fs::create_dir(root.join("placeholder")).unwrap();
    // Noncanonical lexical roots/exclusions must keep working after the scan,
    // when watcher events arrive with ordinary absolute paths.
    let configured_root = root.join("placeholder/..");
    let excludes = vec![root.join("excluded/nested/..")];
    let mut index = RunningIndex::start(workspace, configured_root, excludes, deadline);
    index.await_files(&[
        root.join("original/nested/one.txt"),
        root.join("original/two.txt"),
    ]);

    write(&root.join("created.txt"), "new file");
    write(&root.join("excluded/noise.txt"), "still excluded");
    index.await_files(&[
        root.join("original/nested/one.txt"),
        root.join("original/two.txt"),
        root.join("created.txt"),
    ]);

    fs::rename(root.join("original"), root.join("renamed")).unwrap();
    index.await_files(&[
        root.join("renamed/nested/one.txt"),
        root.join("renamed/two.txt"),
        root.join("created.txt"),
    ]);

    fs::remove_file(root.join("created.txt")).unwrap();
    index.await_files(&[
        root.join("renamed/nested/one.txt"),
        root.join("renamed/two.txt"),
    ]);

    // The delete and recreate may collapse into one watcher path. The current
    // object is a regular file, so stale indexed descendants must disappear.
    fs::remove_dir_all(root.join("renamed")).unwrap();
    fs::write(root.join("renamed"), "replacement file").unwrap();
    index.await_files(&[root.join("renamed")]);

    fs::remove_file(root.join("renamed")).unwrap();
    index.await_files(&[]);

    // A populated directory moved into a watched root can produce only one
    // directory notification. Indexing that subtree must find all children.
    let incoming = index._workspace.path().join("incoming");
    write(&incoming.join("deep/imported.txt"), "imported");
    write(&incoming.join(".hidden"), "hidden files are searchable");
    fs::rename(&incoming, root.join("incoming")).unwrap();
    index.await_files(&[
        root.join("incoming/deep/imported.txt"),
        root.join("incoming/.hidden"),
    ]);
    index.shutdown();
}

#[test]
fn rapid_search_requests_keep_ids_and_the_latest_query_result_aligned() {
    let deadline = Instant::now() + Duration::from_secs(20);
    let workspace = tempdir().unwrap();
    let root = workspace.path().join("files");
    write(&root.join("alpha.txt"), "alpha");
    write(&root.join("beta.txt"), "beta");
    let index = RunningIndex::start(workspace, root, Vec::new(), deadline);

    const LAST: u64 = 128;
    for id in 1..=LAST {
        index
            .engine
            .send(Command::Search(SearchRequest {
                id,
                text: if id % 2 == 0 { "beta.txt" } else { "alpha.txt" }.into(),
                ..SearchRequest::default()
            }))
            .unwrap();
    }
    loop {
        match index.recv_event() {
            Some(Event::Results { request_id, page }) => {
                let expected = if request_id % 2 == 0 {
                    "beta.txt"
                } else {
                    "alpha.txt"
                };
                assert_eq!(page.total, 1);
                assert_eq!(page.rows.len(), 1);
                assert_eq!(
                    page.rows[0].name, expected,
                    "result payload was tagged with another query's ID"
                );
                if request_id == LAST {
                    break;
                }
                assert!(request_id < LAST);
            }
            Some(Event::SearchError {
                request_id,
                message,
            }) => panic!("request {request_id} failed: {message}"),
            Some(Event::Error(error)) => panic!("engine failed: {error}"),
            _ => {}
        }
    }
    // Already queued old results are allowed before the latest one; the worker
    // must not emit an older result after completing the latest request.
    let quiet_deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < quiet_deadline {
        if let Some(Event::Results { request_id, .. }) = index.recv_event() {
            assert!(request_id >= LAST, "stale query overwrote a newer result");
        }
    }
    index.shutdown();
}

#[cfg(windows)]
#[test]
fn case_only_rename_keeps_one_record_with_the_new_spelling() {
    let deadline = Instant::now() + Duration::from_secs(20);
    let workspace = tempdir().unwrap();
    let root = workspace.path().join("files");
    write(&root.join("MixedCase.txt"), "x");
    let mut index = RunningIndex::start(workspace, root.clone(), Vec::new(), deadline);
    index.await_files(&[root.join("MixedCase.txt")]);
    fs::rename(root.join("MixedCase.txt"), root.join("mixedcase.txt")).unwrap();
    index.await_files(&[root.join("mixedcase.txt")]);
    index.shutdown();
}
