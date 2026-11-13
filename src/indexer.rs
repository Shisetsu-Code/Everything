//! Streaming filesystem indexing and native change notifications.
//!
//! This scanner uses directory enumeration, not raw NTFS MFT/USN enumeration.
//! `walkdir` reuses metadata supplied by Windows directory enumeration. Files
//! are never opened for their contents, and symlinks/junctions are not followed.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::model::FileEntry;

const SCAN_BATCH_SIZE: usize = 1024;
const WATCH_QUEUE_CAPACITY: usize = 8192;
const MAX_WARNINGS: usize = 32;
const MAX_PROTECTED_PATHS: usize = 4096;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const WATCH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct ScanSummary {
    pub scanned: u64,
    pub errors: u64,
    pub cancelled: bool,
    pub complete_roots: Vec<PathBuf>,
    /// Completed traversals with errors, together with every path/subtree that
    /// must retain its old index records. A protected root permits no pruning.
    pub incomplete_roots: Vec<(PathBuf, Vec<PathBuf>)>,
}

struct ProtectedPaths {
    root: PathBuf,
    root_key: PathBuf,
    paths: Vec<(PathBuf, PathBuf)>,
    keys: HashSet<PathBuf>,
    whole_root: bool,
}

impl ProtectedPaths {
    fn new(root: &Path) -> Self {
        let root = absolute_path(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            root_key: path_key(&root),
            root,
            paths: Vec::new(),
            keys: HashSet::new(),
            whole_root: false,
        }
    }

    fn protect(&mut self, path: Option<&Path>) {
        if self.whole_root {
            return;
        }
        let Some(path) = path.and_then(|path| absolute_path(path).ok()) else {
            self.protect_root();
            return;
        };
        let key = path_key(&path);
        if key == self.root_key || !key.starts_with(&self.root_key) {
            // An unlocatable error must never accidentally authorize pruning
            // some portion of a root we did not successfully traverse.
            self.protect_root();
            return;
        }
        if key.ancestors().any(|ancestor| self.keys.contains(ancestor)) {
            return;
        }
        if self.paths.len() == MAX_PROTECTED_PATHS {
            self.protect_root();
            return;
        }
        self.keys.insert(key.clone());
        self.paths.push((key, path));
    }

    fn protect_root(&mut self) {
        self.whole_root = true;
        self.paths.clear();
        self.keys.clear();
    }

    fn into_paths(mut self) -> Vec<PathBuf> {
        if self.whole_root || self.paths.is_empty() {
            return vec![self.root];
        }
        // A directory enumeration error can arrive after errors for children
        // already yielded from it. Keep the shortest covering ancestors, even
        // when those errors were reported later. This also bounds SQL guards.
        self.paths
            .sort_unstable_by_key(|(key, _)| key.components().count());
        let mut kept = HashSet::new();
        let mut paths = Vec::new();
        for (key, path) in self.paths {
            if !key.ancestors().any(|ancestor| kept.contains(ancestor)) {
                kept.insert(key);
                paths.push(path);
            }
        }
        paths
    }
}

#[derive(Debug)]
pub enum ScanEvent {
    Batch {
        root: PathBuf,
        entries: Vec<FileEntry>,
    },
    Progress {
        scanned: u64,
        errors: u64,
        current: String,
    },
    Warning(String),
}

/// Enumerate local Windows drive letters. Network shares and optical drives
/// remain available as explicitly configured roots. On other platforms, start
/// with the user's home instead of scanning pseudo filesystems under `/`.
pub fn default_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
        // The SDK defines these GetDriveTypeW return values in a separate
        // WindowsProgramming module; no extra API feature is needed for them.
        const DRIVE_REMOVABLE: u32 = 2;
        const DRIVE_FIXED: u32 = 3;
        const DRIVE_RAMDISK: u32 = 6;
        // SAFETY: GetLogicalDrives takes no pointers and has no prerequisites.
        let drives = unsafe { GetLogicalDrives() };
        let mut roots = Vec::new();
        for index in 0..26u32 {
            if drives & (1 << index) == 0 {
                continue;
            }
            let drive = [
                u16::from(b'A') + index as u16,
                u16::from(b':'),
                u16::from(b'\\'),
                0,
            ];
            // SAFETY: `drive` is a live, NUL-terminated UTF-16 root path.
            let drive_type = unsafe { GetDriveTypeW(drive.as_ptr()) };
            if matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK) {
                roots.push(PathBuf::from(format!(
                    "{}:\\",
                    char::from(b'A' + index as u8)
                )));
            }
        }
        if !roots.is_empty() {
            return roots;
        }
    }

    directories::UserDirs::new()
        .map(|dirs| vec![dirs.home_dir().to_path_buf()])
        .unwrap_or_else(|| vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))])
}

/// Read link metadata rather than following its target. A missing or denied
/// path remains an I/O error so the caller can distinguish deletion from a
/// transient permission failure.
pub fn entry_from_path(path: &Path) -> io::Result<FileEntry> {
    let path = absolute_path(path)?;
    entry_from_metadata(&path, &fs::symlink_metadata(&path)?)
}

/// Walk each root without retaining the tree in memory. Callback errors abort
/// immediately. Filesystem errors protect affected paths from stale-generation
/// pruning, but never abort traversal of other roots. A cancelled scan returns
/// no pruning eligibility, including for roots completed before cancellation.
pub fn scan(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    cancel: &AtomicBool,
    mut emit: impl FnMut(ScanEvent) -> anyhow::Result<()>,
) -> anyhow::Result<ScanSummary> {
    let policy = PathPolicy::new(roots, excludes);
    let mut summary = ScanSummary::default();
    let mut warnings_sent = 0;
    let mut last_progress = Instant::now();
    let mut current = String::new();

    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        current = root.display().to_string();
        emit(ScanEvent::Progress {
            scanned: summary.scanned,
            errors: summary.errors,
            current: current.clone(),
        })?;
        let errors_before = summary.errors;
        let mut protected = ProtectedPaths::new(root);
        let absolute_root = match absolute_path(root) {
            Ok(path) => path,
            Err(error) => {
                report_scan_error(
                    &mut summary,
                    &mut warnings_sent,
                    &mut protected,
                    None,
                    format!("{}: {error}", root.display()),
                    &mut emit,
                )?;
                summary
                    .incomplete_roots
                    .push((root.clone(), protected.into_paths()));
                continue;
            }
        };
        let mut batch = Vec::with_capacity(SCAN_BATCH_SIZE);
        let mut walk = WalkDir::new(&absolute_root)
            .follow_links(false)
            .follow_root_links(false)
            .max_open(64)
            .into_iter();

        // Use skip_current_dir rather than a filter that silently consumes
        // errors or continues reading an excluded subtree.
        while !cancel.load(Ordering::Relaxed) {
            let Some(next) = walk.next() else { break };
            let entry = match next {
                Ok(entry) => entry,
                Err(error) => {
                    report_scan_error(
                        &mut summary,
                        &mut warnings_sent,
                        &mut protected,
                        error.path(),
                        error.to_string(),
                        &mut emit,
                    )?;
                    continue;
                }
            };
            if policy.is_excluded(entry.path()) {
                if entry.file_type().is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    report_scan_error(
                        &mut summary,
                        &mut warnings_sent,
                        &mut protected,
                        Some(entry.path()),
                        error.to_string(),
                        &mut emit,
                    )?;
                    continue;
                }
            };
            match entry_from_metadata(entry.path(), &metadata) {
                Ok(record) => {
                    batch.push(record);
                    summary.scanned += 1;
                }
                Err(error) => {
                    report_scan_error(
                        &mut summary,
                        &mut warnings_sent,
                        &mut protected,
                        Some(entry.path()),
                        format!("{}: {error}", entry.path().display()),
                        &mut emit,
                    )?;
                }
            }
            if batch.len() == SCAN_BATCH_SIZE {
                emit(ScanEvent::Batch {
                    root: root.clone(),
                    entries: std::mem::replace(&mut batch, Vec::with_capacity(SCAN_BATCH_SIZE)),
                })?;
            }
            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                current = entry.path().display().to_string();
                emit(ScanEvent::Progress {
                    scanned: summary.scanned,
                    errors: summary.errors,
                    current: current.clone(),
                })?;
                last_progress = Instant::now();
            }
        }
        // Preserve already enumerated work on cancellation. Never mark that
        // partially enumerated root complete, even if cancellation arrives in
        // the final callback.
        if !batch.is_empty() {
            emit(ScanEvent::Batch {
                root: root.clone(),
                entries: batch,
            })?;
        }
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        if summary.errors == errors_before {
            summary.complete_roots.push(root.clone());
        } else {
            summary
                .incomplete_roots
                .push((root.clone(), protected.into_paths()));
        }
    }
    summary.cancelled |= cancel.load(Ordering::Relaxed);
    if summary.errors > warnings_sent as u64 {
        emit(ScanEvent::Warning(format!(
            "{} additional filesystem errors were omitted; affected paths are protected from pruning.",
            summary.errors - warnings_sent as u64,
        )))?;
    }
    emit(ScanEvent::Progress {
        scanned: summary.scanned,
        errors: summary.errors,
        current,
    })?;
    // A cancellation requested by the final progress callback is still a
    // cancellation, so the engine does not finalize this scan as successful.
    summary.cancelled |= cancel.load(Ordering::Relaxed);
    if summary.cancelled {
        summary.complete_roots.clear();
        summary.incomplete_roots.clear();
    }
    Ok(summary)
}

fn report_scan_error(
    summary: &mut ScanSummary,
    warnings_sent: &mut usize,
    protected: &mut ProtectedPaths,
    error_path: Option<&Path>,
    message: String,
    emit: &mut impl FnMut(ScanEvent) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    summary.errors += 1;
    // Capture every error, including those beyond the UI's warning limit.
    protected.protect(error_path);
    if *warnings_sent < MAX_WARNINGS {
        emit(ScanEvent::Warning(message))?;
        *warnings_sent += 1;
    }
    Ok(())
}

fn entry_from_metadata(path: &Path, metadata: &Metadata) -> io::Result<FileEntry> {
    // Lossy conversion can map two different filesystem paths to one SQL key
    // and make file actions target the wrong file. Surface unsupported names
    // instead of silently changing their identity.
    let path_text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "path is not valid Unicode and cannot be indexed without losing its identity",
        )
    })?;
    let is_dir = metadata.is_dir();
    Ok(FileEntry {
        id: 0,
        path: path_text.to_owned(),
        name: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        parent: path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default(),
        extension: if is_dir {
            String::new()
        } else {
            path.extension()
                .map(|extension| extension.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        },
        size: if is_dir { None } else { Some(metadata.len()) },
        modified: metadata.modified().ok().and_then(unix_seconds),
        created: metadata.created().ok().and_then(unix_seconds),
        is_dir,
        is_symlink: metadata.file_type().is_symlink(),
        source: String::new(),
    })
}

fn unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok(),
        Err(error) => {
            let duration = error.duration();
            let seconds = i128::from(duration.as_secs()) + i128::from(duration.subsec_nanos() != 0);
            i64::try_from(-seconds).ok()
        }
    }
}

/// Normalize component boundaries without canonicalizing through a symlink.
fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(crate) fn path_key(path: &Path) -> PathBuf {
    let absolute = absolute_path(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let text = absolute.to_string_lossy();
        let normal = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
        };
        PathBuf::from(normal.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        absolute
    }
}

#[derive(Clone)]
struct PathPolicy {
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
}

impl PathPolicy {
    fn new(roots: &[PathBuf], excludes: &[PathBuf]) -> Self {
        Self {
            roots: roots.iter().map(|root| path_key(root)).collect(),
            excludes: excludes.iter().map(|excluded| path_key(excluded)).collect(),
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        let key = path_key(path);
        self.excludes
            .iter()
            .any(|excluded| key.starts_with(excluded))
    }

    fn is_in_scope(&self, path: &Path) -> bool {
        let key = path_key(path);
        self.roots.iter().any(|root| key.starts_with(root))
            && !self
                .excludes
                .iter()
                .any(|excluded| key.starts_with(excluded))
    }

    fn is_ancestor_of_root(&self, path: &Path) -> bool {
        let key = path_key(path);
        self.roots
            .iter()
            .any(|root| root != &key && root.starts_with(&key))
    }

    fn is_root(&self, path: &Path) -> bool {
        let key = path_key(path);
        self.roots.contains(&key)
    }
}

#[derive(Debug, Default)]
pub struct WatchBatch {
    pub paths: Vec<PathBuf>,
    pub rescan_required: bool,
    pub warnings: Vec<String>,
}

/// Owns native watches for as long as the service lives. The native callback
/// never waits on the database or GUI: a full bounded queue marks the index as
/// dirty so the next drain requests reconciliation.
pub struct WatchService {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    lost_events: Arc<AtomicBool>,
    policy: PathPolicy,
    roots: Vec<PathBuf>,
    recursive_watches: HashSet<PathBuf>,
    parent_watches: HashSet<PathBuf>,
    pending_warnings: Vec<String>,
    last_failures: HashSet<String>,
    degraded: bool,
    last_rebind: Instant,
}

impl WatchService {
    pub fn new(roots: &[PathBuf], excludes: &[PathBuf]) -> anyhow::Result<Self> {
        let policy = PathPolicy::new(roots, excludes);
        let callback_policy = policy.clone();
        let lost_events = Arc::new(AtomicBool::new(false));
        let callback_lost = Arc::clone(&lost_events);
        let (tx, rx) = bounded(WATCH_QUEUE_CAPACITY);
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let result = match result {
                    Ok(event) => match scope_watch_event(event, &callback_policy) {
                        Some(event) => Ok(event),
                        None => return,
                    },
                    Err(error) => Err(error),
                };
                enqueue_watch_event(result, &tx, &callback_lost);
            },
            Config::default().with_follow_symlinks(false),
        )
        .context("could not initialize native filesystem monitoring")?;
        let mut service = Self {
            watcher,
            rx,
            lost_events,
            policy,
            roots: roots
                .iter()
                .map(|root| absolute_path(root).unwrap_or_else(|_| root.clone()))
                .collect(),
            recursive_watches: HashSet::new(),
            parent_watches: HashSet::new(),
            pending_warnings: Vec::new(),
            last_failures: HashSet::new(),
            degraded: false,
            last_rebind: Instant::now(),
        };
        service.rebind();
        if service.degraded {
            service.lost_events.store(true, Ordering::Release);
        }
        Ok(service)
    }

    pub fn drain(&mut self) -> WatchBatch {
        let mut batch = WatchBatch {
            rescan_required: self.lost_events.swap(false, Ordering::AcqRel),
            warnings: std::mem::take(&mut self.pending_warnings),
            ..WatchBatch::default()
        };
        let mut seen = HashSet::new();
        let mut rebind = batch.rescan_required;
        // Events may arrive continuously. Never make this loop unbounded.
        for _ in 0..WATCH_QUEUE_CAPACITY {
            let Ok(result) = self.rx.try_recv() else {
                break;
            };
            match result {
                Ok(event) => {
                    batch.rescan_required |= event.need_rescan();
                    rebind |= event.need_rescan();
                    let root_may_have_changed = matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Any
                            | EventKind::Other
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    );
                    for path in event.paths {
                        rebind |= root_may_have_changed && self.policy.is_root(&path);
                        // Retain the exact spelling of both ends of a
                        // case-only Windows rename; the last upsert wins.
                        if seen.insert(path.clone()) {
                            batch.paths.push(path);
                        }
                    }
                }
                Err(error) => {
                    batch.rescan_required = true;
                    rebind = true;
                    if batch.warnings.len() < MAX_WARNINGS {
                        batch.warnings.push(format!(
                            "Filesystem monitoring: {error}; reconciliation is required."
                        ));
                    }
                }
            }
        }
        if self.degraded && self.last_rebind.elapsed() >= WATCH_RETRY_INTERVAL {
            rebind = true;
        }
        if rebind {
            batch.rescan_required = true;
            self.rebind();
            batch.warnings.append(&mut self.pending_warnings);
        }
        batch
    }

    fn rebind(&mut self) {
        for path in self
            .recursive_watches
            .drain()
            .chain(self.parent_watches.drain())
        {
            let _ = self.watcher.unwatch(&path);
        }
        let mut failures = HashSet::new();
        for root in &self.roots {
            if self.policy.is_excluded(root) {
                continue;
            }
            match fs::symlink_metadata(root) {
                Ok(metadata) if metadata.is_dir() => {
                    match self.watcher.watch(root, RecursiveMode::Recursive) {
                        Ok(()) => {
                            self.recursive_watches.insert(root.clone());
                        }
                        Err(error) => {
                            failures.insert(format!("{}: {error}", root.display()));
                        }
                    }
                }
                Ok(_) => {} // Files and symlinks are covered by a parent watch.
                Err(error) => {
                    failures.insert(format!("{}: {error}", root.display()));
                }
            }
        }
        // Watching the root alone does not reliably report deletion of the
        // root itself. A nonrecursive parent watch closes that gap.
        for root in &self.roots {
            if self.policy.is_excluded(root) {
                continue;
            }
            let mut ancestor = root.parent();
            while let Some(parent) = ancestor {
                if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
                    if !self
                        .recursive_watches
                        .iter()
                        .any(|watched| parent.starts_with(watched))
                        && !self.parent_watches.contains(parent)
                    {
                        match self.watcher.watch(parent, RecursiveMode::NonRecursive) {
                            Ok(()) => {
                                self.parent_watches.insert(parent.to_path_buf());
                            }
                            Err(error) => {
                                failures.insert(format!("{}: {error}", parent.display()));
                            }
                        }
                    }
                    break;
                }
                ancestor = parent.parent();
            }
        }
        self.degraded = !failures.is_empty();
        for failure in failures.difference(&self.last_failures).take(MAX_WARNINGS) {
            self.pending_warnings.push(format!(
                "Could not monitor {failure}. Reconciliation and watch retries will continue every 30 seconds."
            ));
        }
        self.last_failures = failures;
        self.last_rebind = Instant::now();
    }
}

fn scope_watch_event(mut event: Event, policy: &PathPolicy) -> Option<Event> {
    // Reading a watched ancestor does not change the index. Filter access
    // before promoting ancestor changes to rescan notices, while retaining a
    // genuine platform-reported loss notice even on an access event.
    if !event.need_rescan() && matches!(event.kind, EventKind::Access(_)) {
        return None;
    }
    // Parent watches detect a root being removed or recreated, including
    // creation of an intermediate missing directory.
    if event
        .paths
        .iter()
        .any(|path| policy.is_ancestor_of_root(path))
    {
        event = event.set_flag(notify::event::Flag::Rescan);
    }
    event.paths.retain(|path| policy.is_in_scope(path));
    if !event.need_rescan() && event.paths.is_empty() {
        None
    } else {
        Some(event)
    }
}

fn enqueue_watch_event(
    event: notify::Result<Event>,
    tx: &Sender<notify::Result<Event>>,
    lost_events: &AtomicBool,
) {
    if event.as_ref().map_or(true, |event| event.need_rescan()) {
        lost_events.store(true, Ordering::Release);
    }
    if matches!(tx.try_send(event), Err(TrySendError::Full(_))) {
        lost_events.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant, UNIX_EPOCH};
    use tempfile::tempdir;

    fn collect(roots: &[PathBuf], excludes: &[PathBuf]) -> (ScanSummary, Vec<FileEntry>) {
        let mut rows = Vec::new();
        let summary = scan(roots, excludes, &AtomicBool::new(false), |event| {
            if let ScanEvent::Batch { entries, .. } = event {
                rows.extend(entries);
            }
            Ok(())
        })
        .unwrap();
        (summary, rows)
    }

    #[test]
    fn includes_hidden_entries_and_prunes_only_the_excluded_subtree() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("skip/nested")).unwrap();
        fs::create_dir(dir.path().join("skip-other")).unwrap();
        fs::write(dir.path().join("skip/nested/secret.txt"), "x").unwrap();
        fs::write(dir.path().join("skip-other/keep.txt"), "x").unwrap();
        fs::write(dir.path().join(".hidden"), "x").unwrap();

        let (summary, rows) = collect(
            &[dir.path().to_path_buf()],
            &[dir.path().join("skip/./nested/..")],
        );
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&".hidden"));
        assert!(names.contains(&"keep.txt"));
        assert!(!names.contains(&"skip"));
        assert!(!names.contains(&"secret.txt"));
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.complete_roots, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn streams_bounded_batches_without_losing_entries() {
        let dir = tempdir().unwrap();
        for i in 0..2050 {
            fs::write(dir.path().join(format!("{i}.txt")), "x").unwrap();
        }
        let mut batch_sizes = Vec::new();
        let summary = scan(
            &[dir.path().to_path_buf()],
            &[],
            &AtomicBool::new(false),
            |event| {
                if let ScanEvent::Batch { root, entries } = event {
                    assert_eq!(root, dir.path());
                    batch_sizes.push(entries.len());
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(batch_sizes.len() >= 3);
        assert!(batch_sizes.iter().all(|&size| (1..=1024).contains(&size)));
        assert_eq!(batch_sizes.iter().sum::<usize>(), 2051);
        assert_eq!(summary.scanned, 2051);
    }

    #[test]
    fn cancellation_keeps_partial_roots_out_of_pruning() {
        let dir = tempdir().unwrap();
        for i in 0..1050 {
            fs::write(dir.path().join(format!("{i}.txt")), "x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut delivered = 0;
        let summary = scan(&[dir.path().to_path_buf()], &[], &cancel, |event| {
            if let ScanEvent::Batch { entries, .. } = event {
                delivered += entries.len();
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
        assert!(summary.cancelled);
        assert!(summary.complete_roots.is_empty());
        assert!(delivered > 0 && delivered < 1051);
        assert_eq!(summary.scanned, delivered as u64);
    }

    #[test]
    fn preexisting_cancellation_does_not_emit_records() {
        let dir = tempdir().unwrap();
        let summary = scan(
            &[dir.path().to_path_buf()],
            &[],
            &AtomicBool::new(true),
            |event| {
                assert!(!matches!(event, ScanEvent::Batch { .. }));
                Ok(())
            },
        )
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.scanned, 0);
        assert!(summary.complete_roots.is_empty());
    }

    #[test]
    fn failed_root_does_not_abort_other_roots_or_become_prunable() {
        let dir = tempdir().unwrap();
        let good = dir.path().join("good");
        let missing = dir.path().join("missing");
        fs::create_dir(&good).unwrap();
        fs::write(good.join("found.txt"), "content").unwrap();
        let (summary, rows) = collect(&[missing, good.clone()], &[]);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.complete_roots, vec![good]);
        assert!(rows.iter().any(|row| row.name == "found.txt"));
    }

    #[test]
    fn missing_root_is_protected_while_other_roots_remain_prunable() {
        let dir = tempdir().unwrap();
        let good = dir.path().join("good");
        let missing = dir.path().join("missing");
        fs::create_dir(&good).unwrap();
        fs::write(good.join("found.txt"), "content").unwrap();
        let (summary, _) = collect(&[missing.clone(), good.clone()], &[]);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.complete_roots, vec![good]);
        assert_eq!(
            summary.incomplete_roots,
            vec![(missing.clone(), vec![missing])]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_known_entry_error_protects_only_the_affected_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempdir().unwrap();
        let unreadable_name = dir.path().join(OsStr::from_bytes(b"invalid-\xff-name"));
        fs::write(&unreadable_name, "cannot represent this name in the index").unwrap();
        fs::write(dir.path().join("readable.txt"), "indexed").unwrap();
        let (summary, entries) = collect(&[dir.path().to_path_buf()], &[]);
        assert_eq!(summary.errors, 1);
        assert!(summary.complete_roots.is_empty());
        assert_eq!(
            summary.incomplete_roots,
            vec![(dir.path().to_path_buf(), vec![unreadable_name])]
        );
        assert!(entries.iter().any(|entry| entry.name == "readable.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn errors_omitted_from_warnings_still_protect_all_affected_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempdir().unwrap();
        let mut expected = HashSet::new();
        for index in 0..40 {
            let mut name = format!("invalid-{index}-").into_bytes();
            name.push(0xff);
            let path = dir.path().join(OsString::from_vec(name));
            fs::write(&path, "x").unwrap();
            expected.insert(path);
        }
        let mut warnings = 0;
        let summary = scan(
            &[dir.path().to_path_buf()],
            &[],
            &AtomicBool::new(false),
            |event| {
                if matches!(event, ScanEvent::Warning(_)) {
                    warnings += 1;
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(summary.errors, 40);
        assert!(warnings > 0 && warnings < summary.errors);
        assert!(summary.complete_roots.is_empty());
        assert_eq!(summary.incomplete_roots.len(), 1);
        let (root, protected) = summary.incomplete_roots.into_iter().next().unwrap();
        assert_eq!(root, dir.path());
        assert_eq!(protected.into_iter().collect::<HashSet<_>>(), expected);
    }

    #[test]
    fn unknown_or_outside_error_paths_protect_the_entire_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        for error_path in [None, Some(dir.path().join("other"))] {
            let mut protected = ProtectedPaths::new(&root);
            protected.protect(error_path.as_deref());
            protected.protect(Some(&root.join("later-error.txt")));
            assert_eq!(protected.into_paths(), vec![root.clone()]);
        }
    }

    #[test]
    fn error_subtrees_coalesce_without_protecting_similarly_named_siblings() {
        let dir = tempdir().unwrap();
        let mut protected = ProtectedPaths::new(dir.path());
        protected.protect(Some(&dir.path().join("private/child.txt")));
        protected.protect(Some(&dir.path().join("private")));
        protected.protect(Some(&dir.path().join("private/another.txt")));
        protected.protect(Some(&dir.path().join("private-other/one.txt")));
        let paths: HashSet<_> = protected.into_paths().into_iter().collect();
        assert_eq!(
            paths,
            HashSet::from([
                dir.path().join("private"),
                dir.path().join("private-other/one.txt"),
            ])
        );
    }

    #[test]
    fn too_many_distinct_error_paths_protect_the_entire_root() {
        let dir = tempdir().unwrap();
        let mut protected = ProtectedPaths::new(dir.path());
        for index in 0..4097 {
            protected.protect(Some(&dir.path().join(format!("error-{index}"))));
        }
        assert_eq!(protected.into_paths(), vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn repeated_errors_in_one_subtree_do_not_exhaust_the_protection_limit() {
        let dir = tempdir().unwrap();
        let subtree = dir.path().join("private");
        let mut protected = ProtectedPaths::new(dir.path());
        protected.protect(Some(&subtree));
        for index in 0..4097 {
            protected.protect(Some(&subtree.join(format!("error-{index}"))));
        }
        assert_eq!(protected.into_paths(), vec![subtree]);
    }

    #[test]
    fn cancellation_revokes_completed_and_incomplete_root_pruning_evidence() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing");
        let good = dir.path().join("good");
        let interrupted = dir.path().join("interrupted");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&interrupted).unwrap();
        for index in 0..1050 {
            fs::write(interrupted.join(format!("{index}.txt")), "x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let summary = scan(
            &[missing, good, interrupted.clone()],
            &[],
            &cancel,
            |event| {
                if let ScanEvent::Batch { root, .. } = event
                    && root == interrupted
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.errors, 1);
        assert!(summary.complete_roots.is_empty());
        assert!(summary.incomplete_roots.is_empty());
    }

    #[test]
    fn final_callback_cancellation_revokes_pruning_evidence() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "x").unwrap();
        let cancel = AtomicBool::new(false);
        let summary = scan(&[dir.path().to_path_buf()], &[], &cancel, |event| {
            if let ScanEvent::Progress { scanned: 2, .. } = event {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
        assert!(summary.cancelled);
        assert!(summary.complete_roots.is_empty());
        assert!(summary.incomplete_roots.is_empty());
    }

    #[test]
    fn callback_failure_stops_the_scan() {
        let dir = tempdir().unwrap();
        let result = scan(
            &[dir.path().to_path_buf()],
            &[],
            &AtomicBool::new(false),
            |_| anyhow::bail!("receiver disconnected"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn records_real_size_and_modified_time() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Photo.JPG");
        fs::write(&path, b"1234567").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_123))
            .unwrap();
        let entry = entry_from_path(&path).unwrap();
        assert_eq!(entry.id, 0);
        assert!(entry.source.is_empty());
        assert_eq!(entry.path, path.to_str().unwrap());
        assert_eq!(entry.parent, dir.path().to_str().unwrap());
        assert_eq!(entry.name, "Photo.JPG");
        assert_eq!(entry.extension, "jpg");
        assert_eq!(entry.size, Some(7));
        assert_eq!(entry.modified, Some(1_600_000_123));
        assert!(!entry.is_dir);
        assert!(!entry.is_symlink);
        assert_eq!(entry_from_path(dir.path()).unwrap().size, None);
    }

    #[cfg(unix)]
    #[test]
    fn indexes_symlinks_without_following_cycles_or_broken_targets() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), "x").unwrap();
        std::os::unix::fs::symlink(".", dir.path().join("loop")).unwrap();
        std::os::unix::fs::symlink("absent", dir.path().join("broken")).unwrap();
        let (summary, rows) = collect(&[dir.path().to_path_buf()], &[]);
        assert_eq!(summary.errors, 0);
        assert_eq!(rows.len(), 4);
        assert!(
            rows.iter()
                .find(|row| row.name == "loop")
                .unwrap()
                .is_symlink
        );
        assert!(
            rows.iter()
                .find(|row| row.name == "broken")
                .unwrap()
                .is_symlink
        );
        let (_, root_link) = collect(&[dir.path().join("loop")], &[]);
        assert_eq!(root_link.len(), 1);
    }

    #[test]
    fn failed_watches_request_reconciliation_with_a_warning() {
        let dir = tempdir().unwrap();
        let mut watcher = WatchService::new(&[dir.path().join("missing")], &[]).unwrap();
        let batch = watcher.drain();
        assert!(batch.rescan_required);
        assert!(!batch.warnings.is_empty());
    }

    #[test]
    fn failed_watches_do_not_cause_a_busy_rescan_loop() {
        let dir = tempdir().unwrap();
        let mut watcher = WatchService::new(&[dir.path().join("missing")], &[]).unwrap();
        assert!(watcher.drain().rescan_required);
        assert!(!watcher.drain().rescan_required);
    }

    #[test]
    fn watcher_queue_overflow_requests_reconciliation_without_blocking() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        tx.send(Ok(notify::Event::new(notify::EventKind::Any)))
            .unwrap();
        let lost_events = AtomicBool::new(false);
        enqueue_watch_event(
            Ok(notify::Event::new(notify::EventKind::Any)),
            &tx,
            &lost_events,
        );
        assert!(lost_events.load(Ordering::Relaxed));
    }

    #[test]
    fn native_rescan_notice_requests_reconciliation_even_without_paths() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let lost_events = AtomicBool::new(false);
        enqueue_watch_event(
            Ok(notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)),
            &tx,
            &lost_events,
        );
        assert!(lost_events.load(Ordering::Relaxed));
    }

    #[test]
    fn reading_a_parent_directory_does_not_request_a_full_rescan() {
        let dir = tempdir().unwrap();
        let policy = PathPolicy::new(&[dir.path().join("root")], &[]);
        let event = notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Read))
            .add_path(dir.path().to_path_buf());
        assert!(scope_watch_event(event.clone(), &policy).is_none());
        // A genuine native overflow notice must still survive filtering.
        assert!(
            scope_watch_event(event.set_flag(notify::event::Flag::Rescan), &policy)
                .unwrap()
                .need_rescan()
        );
    }

    #[test]
    fn native_watcher_preserves_both_rename_paths_and_excludes_noise() {
        let dir = tempdir().unwrap();
        let excluded = dir.path().join("excluded");
        fs::create_dir(&excluded).unwrap();
        let old = dir.path().join("old.txt");
        let new = dir.path().join("new.txt");
        fs::write(&old, "x").unwrap();
        let mut watcher =
            WatchService::new(&[dir.path().to_path_buf()], std::slice::from_ref(&excluded))
                .unwrap();
        fs::rename(&old, &new).unwrap();
        fs::write(excluded.join("noise.txt"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut paths = Vec::new();
        while Instant::now() < deadline {
            paths.extend(watcher.drain().paths);
            if paths.contains(&old) && paths.contains(&new) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(paths.contains(&old), "missing rename source: {paths:?}");
        assert!(
            paths.contains(&new),
            "missing rename destination: {paths:?}"
        );
        assert!(!paths.iter().any(|path| path.starts_with(&excluded)));
    }
}
