use crate::{
    config::AppConfig,
    database::Database,
    indexer::{self, ScanEvent, ScanSummary, WatchService},
    model::{ExportFormat, FileEntry, SearchPage, SearchRequest},
    operations,
};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, bounded, tick};
use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub use crate::operations::FileAction;

#[derive(Debug)]
pub enum Command {
    Search(SearchRequest),
    Rebuild,
    CancelIndex,
    Configure(AppConfig),
    Export {
        path: PathBuf,
        request: SearchRequest,
        format: ExportFormat,
    },
    Import {
        path: PathBuf,
    },
    FileAction(FileAction),
    Shutdown,
    RefreshPaths(Vec<PathBuf>),
}

#[derive(Debug)]
pub enum Event {
    Ready {
        total: u64,
    },
    IndexProgress {
        scanned: u64,
        errors: u64,
        current: String,
    },
    IndexFinished {
        total: u64,
        cancelled: bool,
        errors: u64,
    },
    Results {
        request_id: u64,
        page: SearchPage,
    },
    SearchError {
        request_id: u64,
        message: String,
    },
    Configured {
        config: AppConfig,
    },
    ConfigurationError {
        message: String,
        config: AppConfig,
    },
    Changed {
        total: u64,
    },
    Error(String),
    Warning(String),
    OperationComplete(String),
}

/// Only channel operations and atomics are used from the render thread.
pub struct Engine {
    pub events: Receiver<Event>,
    control: Sender<Command>,
    search: Sender<SearchRequest>,
    pending_search: Receiver<SearchRequest>,
    tasks: Sender<Command>,
    cancel_index: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl Engine {
    pub fn start(db_path: PathBuf, config_path: PathBuf, config: AppConfig) -> Result<Self> {
        let db_path = std::path::absolute(db_path)?;
        let parent = db_path.parent().context("El índice no tiene carpeta")?;
        std::fs::create_dir_all(parent)?;
        let db_path = std::fs::canonicalize(parent)?
            .join(db_path.file_name().context("Nombre de índice inválido")?);
        let config_path = std::path::absolute(config_path)?;
        let config = normalize_config(config, &db_path)?;
        let lease = crate::lease::WriterLease::acquire(&db_path)?;
        config.save(&config_path)?;
        // Initialize the small schema before readers open it, avoiding schema races.
        drop(Database::open(&db_path)?);
        let (events_tx, events) = bounded(128);
        let (control, controls) = bounded(32);
        let (search, searches) = bounded(1);
        let (tasks, task_rx) = bounded(16);
        let cancel_index = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let (db, tx, search_generation, stop) = (
                db_path.clone(),
                events_tx.clone(),
                generation.clone(),
                shutdown.clone(),
            );
            let rx = searches.clone();
            thread::Builder::new()
                .name("search".into())
                .spawn(move || search_worker(db, rx, tx, search_generation, stop))?;
        }
        {
            let (db, tx, stop, control_tx) = (
                db_path.clone(),
                events_tx.clone(),
                shutdown.clone(),
                control.clone(),
            );
            thread::Builder::new()
                .name("file-operations".into())
                .spawn(move || task_worker(db, task_rx, tx, stop, control_tx))?;
        }
        {
            let (tx, cancel, stop) = (events_tx, cancel_index.clone(), shutdown.clone());
            thread::Builder::new()
                .name("index-writer".into())
                .spawn(move || {
                    let _lease = lease;
                    if let Err(error) =
                        index_worker(db_path, config_path, config, controls, &tx, cancel, stop)
                    {
                        let _ = tx.send(Event::Error(format!("Indexador: {error:#}")));
                    }
                })?;
        }
        Ok(Self {
            events,
            control,
            search,
            pending_search: searches,
            tasks,
            cancel_index,
            generation,
            shutdown,
        })
    }

    pub fn send(&self, command: Command) -> Result<()> {
        match command {
            Command::Search(mut request) => {
                self.generation.store(request.id, Ordering::Release);
                loop {
                    match self.search.try_send(request) {
                        Ok(()) => return Ok(()),
                        Err(crossbeam_channel::TrySendError::Full(value)) => {
                            request = value;
                            let _ = self.pending_search.try_recv();
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                            return Err(anyhow!("El buscador se cerró"));
                        }
                    }
                }
            }
            Command::CancelIndex => {
                self.cancel_index.store(true, Ordering::Release);
                self.control
                    .try_send(Command::CancelIndex)
                    .context("Cola del indexador ocupada")?;
            }
            Command::Shutdown => {
                self.shutdown.store(true, Ordering::Release);
                self.cancel_index.store(true, Ordering::Release);
                self.generation.store(u64::MAX, Ordering::Release);
                let _ = self.control.try_send(Command::Shutdown);
            }
            cmd @ (Command::Export { .. } | Command::FileAction(_)) => self
                .tasks
                .try_send(cmd)
                .context("La cola de operaciones está ocupada; inténtelo de nuevo")?,
            cmd => self
                .control
                .try_send(cmd)
                .context("La cola del indexador está ocupada")?,
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.send(Command::Shutdown);
    }
}

fn search_worker(
    path: PathBuf,
    rx: Receiver<SearchRequest>,
    tx: Sender<Event>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let mut db = match Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            let _ = tx.send(Event::Error(e.to_string()));
            return;
        }
    };
    while !stop.load(Ordering::Acquire) {
        let Ok(mut request) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        while let Ok(newer) = rx.try_recv() {
            request = newer;
        }
        let current = request.id;
        let gen_ref = generation.clone();
        let stop_ref = stop.clone();
        if let Err(error) = db.conn.progress_handler(
            1000,
            Some(move || {
                gen_ref.load(Ordering::Acquire) != current || stop_ref.load(Ordering::Acquire)
            }),
        ) {
            let _ = tx.send(Event::SearchError {
                request_id: current,
                message: error.to_string(),
            });
            continue;
        }
        let result = db.search(&request);
        if generation.load(Ordering::Acquire) != current || stop.load(Ordering::Acquire) {
            continue;
        }
        let event = match result {
            Ok(page) => Event::Results {
                request_id: current,
                page,
            },
            Err(error) => Event::SearchError {
                request_id: current,
                message: format!("{error:#}"),
            },
        };
        if tx.send(event).is_err() {
            break;
        }
    }
}

enum ScanMessage {
    Data(ScanEvent),
    Finished(std::result::Result<ScanSummary, String>),
}
struct ActiveScan {
    rx: Receiver<ScanMessage>,
    generation: i64,
    full: bool,
}

fn commit_scan_batch(
    db: &mut Database,
    root: &str,
    generation: i64,
    entries: &[FileEntry],
    cancel: &AtomicBool,
) -> Result<bool> {
    if cancel.load(Ordering::Acquire) {
        return Ok(false);
    }
    db.upsert_scan_batch(root, generation, entries)?;
    Ok(true)
}

fn begin_scan(
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    generation: i64,
    full: bool,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<ActiveScan> {
    cancel.store(false, Ordering::Release);
    let (tx, rx) = bounded(8);
    thread::Builder::new()
        .name("filesystem-scan".into())
        .spawn(move || {
            let result = indexer::scan(&roots, &excludes, &cancel, |event| {
                let mut message = ScanMessage::Data(event);
                loop {
                    if cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    match tx.send_timeout(message, Duration::from_millis(50)) {
                        Ok(()) => return Ok(()),
                        Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => message = value,
                        Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                            return Err(anyhow!("El indexador se cerró"));
                        }
                    }
                }
            })
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(ScanMessage::Finished(result));
        })?;
    Ok(ActiveScan {
        rx,
        generation,
        full,
    })
}

fn make_watcher(
    config: &AppConfig,
    excludes: &[PathBuf],
    events: &Sender<Event>,
) -> Option<WatchService> {
    if !config.watch || config.roots.is_empty() {
        return None;
    }
    match WatchService::new(&config.roots, excludes) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            let _ = events.send(Event::Warning(format!("Vigilancia no disponible: {error:#}. Se reintentará; se usará reconciliación periódica.")));
            None
        }
    }
}

fn index_worker(
    db_path: PathBuf,
    config_path: PathBuf,
    mut config: AppConfig,
    controls: Receiver<Command>,
    events: &Sender<Event>,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut db = Database::open(&db_path)?;
    let mut excludes = effective_excludes(&config, &db_path, &config_path);
    db.remove_unconfigured_roots(
        &config
            .roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )?;
    for excluded in &excludes {
        db.remove_subtree(&excluded.to_string_lossy())?;
    }
    // Watch before scanning: all changes during the scan will be replayed afterwards.
    let mut watcher = make_watcher(&config, &excludes, events);
    events.send(Event::Ready { total: db.total()? })?;
    let ticker = tick(Duration::from_millis(200));
    let mut active: Option<ActiveScan> = None;
    let mut full_requested = config.auto_scan;
    let mut pending_config: Option<AppConfig> = None;
    let mut pending_paths = HashMap::<String, PathBuf>::new();
    let mut pending_scopes = Vec::<PathBuf>::new();
    let mut dirty = false;
    let mut last_changed = Instant::now();
    let mut last_reconcile = Instant::now();
    let mut last_watch_retry = Instant::now();
    let mut rescan_after_overflow = false;
    let mut pending_imports: VecDeque<PathBuf> = VecDeque::new();

    // Restore the ordinary write policy on every exit, including write errors.
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            if active.is_none() {
                if let Some(path) = pending_imports.pop_front() {
                    match operations::import(&db_path, &path, &stop) {
                        Ok(count) => {
                            events.send(Event::OperationComplete(format!(
                                "Importados {count} registros de {} (catálogo sin conexión)",
                                path.display()
                            )))?;
                        }
                        Err(error) => {
                            events.send(Event::Error(format!("{error:#}")))?;
                        }
                    }
                    events.send(Event::Changed { total: db.total()? })?;
                    dirty = true;
                }
                if let Some(new_config) = pending_config.take() {
                    config = new_config;
                    excludes = effective_excludes(&config, &db_path, &config_path);
                    watcher = make_watcher(&config, &excludes, events);
                    pending_paths.clear();
                    pending_scopes.clear();
                    db.remove_unconfigured_roots(
                        &config
                            .roots
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect::<Vec<_>>(),
                    )?;
                    for excluded in &excludes {
                        db.remove_subtree(&excluded.to_string_lossy())?;
                    }
                    full_requested = true;
                    dirty = true;
                    if config.roots.is_empty() {
                        full_requested = false;
                        db.finish_scan_writes()?;
                        events.send(Event::IndexFinished {
                            total: db.total()?,
                            cancelled: false,
                            errors: 0,
                        })?;
                    }
                }
                if full_requested && !config.roots.is_empty() {
                    full_requested = false;
                    pending_scopes.clear();
                    db.begin_scan_writes()?;
                    let generation = db.next_generation()?;
                    active = Some(begin_scan(
                        config.roots.clone(),
                        excludes.clone(),
                        generation,
                        true,
                        cancel.clone(),
                        stop.clone(),
                    )?);
                    last_reconcile = Instant::now();
                    let _ = events.try_send(Event::IndexProgress {
                        scanned: 0,
                        errors: 0,
                        current: "Iniciando indexación".into(),
                    });
                } else if !pending_scopes.is_empty() {
                    let roots = compact_roots(std::mem::take(&mut pending_scopes));
                    db.begin_scan_writes()?;
                    let generation = db.next_generation()?;
                    active = Some(begin_scan(
                        roots,
                        excludes.clone(),
                        generation,
                        false,
                        cancel.clone(),
                        stop.clone(),
                    )?);
                }
            }

            let never = crossbeam_channel::never();
            let scan_rx = active.as_ref().map(|a| &a.rx).unwrap_or(&never);
            crossbeam_channel::select! {
                recv(controls) -> message => {
                    let Ok(command) = message else { break; };
                    match command {
                        Command::Rebuild => { full_requested = true; cancel.store(true, Ordering::Release); }
                        Command::CancelIndex => { full_requested = false; pending_scopes.clear(); cancel.store(true, Ordering::Release); }
                        Command::Configure(new_config) => {
                            match normalize_config(new_config, &db_path) {
                                Ok(new_config) => {
                                    if let Err(error) = new_config.save(&config_path) {
                                        events.send(Event::ConfigurationError { message: error.to_string(), config: pending_config.as_ref().unwrap_or(&config).clone() })?;
                                    } else {
                                        // Acknowledge each persisted request in command order, even when
                                        // only the most recent configuration will start a new scan.
                                        events.send(Event::Configured { config: new_config.clone() })?;
                                        if config.roots != new_config.roots || config.excludes != new_config.excludes || config.watch != new_config.watch {
                                            pending_config = Some(new_config); cancel.store(true, Ordering::Release);
                                        } else {
                                            if pending_config.take().is_some() { full_requested = true; }
                                            config = new_config;
                                        }
                                    }
                                }
                                Err(error) => { events.send(Event::ConfigurationError { message: error.to_string(), config: pending_config.as_ref().unwrap_or(&config).clone() })?; }
                            }
                        }
                        Command::Shutdown => break,
                        Command::RefreshPaths(paths) => { for path in paths { pending_paths.insert(watch_key(&path), path); } }
                        Command::Import { path } => {
                            if pending_imports.len() < 16 {
                                pending_imports.push_back(path);
                                if active.as_ref().is_some_and(|scan| scan.full) { full_requested = true; }
                                cancel.store(true, Ordering::Release);
                            } else { events.send(Event::Error("La cola de importaciones está llena".into()))?; }
                        }
                        _ => {}
                    }
                }
                recv(scan_rx) -> message => {
                    match message {
                        Ok(ScanMessage::Data(ScanEvent::Batch { root, entries })) => {
                            let owner = config.roots.iter().find(|candidate| path_within(&root, candidate)).unwrap_or(&root);
                            let generation = active.as_ref().expect("scan message requires active scan").generation;
                            dirty |= commit_scan_batch(&mut db, &owner.to_string_lossy(), generation, &entries, &cancel)?;
                        }
                        Ok(ScanMessage::Data(ScanEvent::Progress { scanned, errors, current })) => { let _ = events.try_send(Event::IndexProgress { scanned, errors, current }); }
                        Ok(ScanMessage::Data(ScanEvent::Warning(message))) => { let _ = events.try_send(Event::Warning(message)); }
                        Ok(ScanMessage::Finished(result)) => {
                            let scan = active.take().expect("finished requires active scan");
                            match result {
                                Ok(summary) => {
                                    if !summary.cancelled && !cancel.load(Ordering::Acquire) {
                                        for root in summary.complete_roots {
                                            if scan.full { db.prune_root(&root.to_string_lossy(), scan.generation)?; }
                                            else { db.prune_subtree(&root.to_string_lossy(), scan.generation)?; }
                                        }
                                        for (root, protected) in summary.incomplete_roots {
                                            if scan.full { db.prune_root_except(&root.to_string_lossy(), scan.generation, &protected)?; }
                                            else { db.prune_subtree_except(&root.to_string_lossy(), scan.generation, &protected)?; }
                                        }
                                    }
                                    let _ = events.try_send(Event::IndexProgress { scanned: summary.scanned, errors: summary.errors, current: "Sincronizando índice".into() });
                                    db.finish_scan_writes()?;
                                    events.send(Event::IndexFinished { total: db.total()?, cancelled: summary.cancelled || cancel.load(Ordering::Acquire), errors: summary.errors })?;
                                }
                                Err(message) => {
                                    db.finish_scan_writes()?;
                                    if cancel.load(Ordering::Acquire) {
                                        events.send(Event::IndexFinished { total: db.total()?, cancelled: true, errors: 0 })?;
                                    } else { events.send(Event::Error(message))?; events.send(Event::IndexFinished { total: db.total()?, cancelled: false, errors: 1 })?; }
                                }
                            }
                            dirty = true;
                        }
                        Err(_) => { active = None; db.finish_scan_writes()?; events.send(Event::Error("El lector de archivos se cerró inesperadamente".into()))?; }
                    }
                }
                recv(ticker) -> _ => {
                    if let Some(watch) = &mut watcher {
                        let batch = watch.drain();
                        for warning in batch.warnings { let _ = events.try_send(Event::Warning(warning)); }
                        if batch.rescan_required { rescan_after_overflow = true; }
                        for path in batch.paths {
                            if pending_paths.len() >= 50_000 { pending_paths.clear(); rescan_after_overflow = true; break; }
                            pending_paths.insert(watch_key(&path), path);
                        }
                    }
                    if watcher.is_none() && config.watch && last_watch_retry.elapsed() > Duration::from_secs(60) {
                        watcher = make_watcher(&config, &excludes, events);
                        last_watch_retry = Instant::now();
                    }
                    // Read and update only a bounded number of filesystem paths per tick.
                    if active.is_none() && pending_config.is_none() {
                        let paths: Vec<_> = pending_paths.values().take(128).cloned().collect();
                        for path in paths {
                            pending_paths.remove(&watch_key(&path));
                            if excludes.iter().any(|excluded| path_within(&path, excluded)) { continue; }
                            let Some(owner) = config.roots.iter().find(|root| path_within(&path, root)) else { continue; };
                            match indexer::entry_from_path(&path) {
                                Ok(entry) => {
                                    if entry.is_dir && !entry.is_symlink { pending_scopes.push(path.clone()); }
                                    else { db.remove_descendants(&entry.path)?; }
                                    db.upsert_batch(&owner.to_string_lossy(), db.next_generation()?, &[entry])?;
                                    dirty = true;
                                }
                                Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => { db.remove_subtree(&path.to_string_lossy())?; dirty = true; }
                                Err(error) => { let _ = events.try_send(Event::Warning(format!("{}: {error}", path.display()))); }
                            }
                        }
                    }
                    if active.is_none() && rescan_after_overflow && last_reconcile.elapsed() > Duration::from_secs(5) {
                        full_requested = true; rescan_after_overflow = false;
                    }
                    if config.watch && watcher.is_none() && last_reconcile.elapsed() > Duration::from_secs(300) { full_requested = true; }
                    if dirty && last_changed.elapsed() > Duration::from_secs(2) {
                        let _ = events.try_send(Event::Changed { total: db.total()? });
                        dirty = false; last_changed = Instant::now();
                    }
                }
            }
        }
        Ok(())
    })();
    cancel.store(true, Ordering::Release);
    let finished = db.finish_scan_writes();
    result.and(finished)
}

fn task_worker(
    db_path: PathBuf,
    rx: Receiver<Command>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    control: Sender<Command>,
) {
    while !stop.load(Ordering::Acquire) {
        let Ok(command) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let modified_paths = match &command {
            Command::FileAction(FileAction::Rename { path, new_name }) => {
                vec![path.clone(), path.with_file_name(new_name)]
            }
            Command::FileAction(FileAction::Recycle(paths)) => paths.clone(),
            _ => Vec::new(),
        };
        let outcome: Result<String> = match command {
            Command::Export {
                path,
                request,
                format,
            } => operations::export(&db_path, &path, &request, format, &stop)
                .map(|n| format!("Exportados {n} resultados a {}", path.display())),
            Command::FileAction(action) => operations::perform(action),
            _ => continue,
        };
        match outcome {
            Ok(message) => {
                let _ = events.send(Event::OperationComplete(message));
            }
            Err(error) => {
                let _ = events.send(Event::Error(format!("{error:#}")));
            }
        }
        // Also reconcile partially completed operations and when monitoring is disabled.
        if !modified_paths.is_empty() {
            let _ = control.send_timeout(
                Command::RefreshPaths(modified_paths),
                Duration::from_secs(1),
            );
        }
    }
}

pub(crate) fn normalize_config(mut config: AppConfig, db_path: &Path) -> Result<AppConfig> {
    let roots: Result<Vec<_>> = config
        .roots
        .iter()
        .map(|p| normalized_absolute(p).map_err(Into::into))
        .collect();
    config.roots = compact_roots(roots?);
    config.excludes = config
        .excludes
        .iter()
        .map(|path| normalized_absolute(path))
        .collect::<std::io::Result<Vec<_>>>()?;
    config.page_size = config.page_size.clamp(100, 2000);
    if let Some(data_dir) = db_path.parent()
        && config.roots.iter().any(|root| path_within(root, data_dir))
    {
        return Err(anyhow!(
            "La carpeta de datos del programa no puede contener una raíz de búsqueda. Use una carpeta de datos dedicada, separada de los archivos a indexar."
        ));
    }
    Ok(config)
}

fn effective_excludes(config: &AppConfig, db_path: &Path, config_path: &Path) -> Vec<PathBuf> {
    let mut excludes = config.excludes.clone();
    if let Some(parent) = db_path.parent() {
        excludes.push(parent.to_owned());
    }
    excludes.push(config_path.to_owned());
    excludes
}

pub(crate) fn path_within(path: &Path, root: &Path) -> bool {
    indexer::path_key(path).starts_with(indexer::path_key(root))
}

fn compact_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort_by_key(|p| p.components().count());
    let mut output: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !output.iter().any(|parent| path_within(&root, parent)) {
            output.push(root);
        }
    }
    output
}

fn normalized_absolute(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

fn watch_key(path: &Path) -> String {
    indexer::path_key(path).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_and_scoped_scan_completion_waits_for_checkpoint() {
        for full in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("files");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("keep.txt"), "keep").unwrap();
            let db_path = temporary.path().join("data/index.db");
            let config_path = temporary.path().join("data/config.json");
            let config = AppConfig {
                roots: vec![root.clone()],
                auto_scan: full,
                watch: false,
                ..AppConfig::default()
            };
            let (controls_tx, controls_rx) = bounded(2);
            // A rendezvous keeps the writer blocked on IndexFinished until we
            // inspect the WAL. Merely waiting for readiness does not receive it.
            let (events_tx, events_rx) = bounded(0);
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let worker = {
                let (path, cancel, stop) = (db_path.clone(), cancel.clone(), stop.clone());
                thread::spawn(move || {
                    index_worker(
                        path,
                        config_path,
                        config,
                        controls_rx,
                        &events_tx,
                        cancel,
                        stop,
                    )
                })
            };
            assert!(matches!(
                events_rx.recv_timeout(Duration::from_secs(20)).unwrap(),
                Event::Ready { total: 0 }
            ));
            if !full {
                controls_tx.send(Command::RefreshPaths(vec![root])).unwrap();
            }
            let mut ready = crossbeam_channel::Select::new();
            ready.recv(&events_rx);
            ready.ready_timeout(Duration::from_secs(20)).unwrap();
            let observer = rusqlite::Connection::open(&db_path).unwrap();
            let (logged, checkpointed): (i64, i64) = observer
                .query_row("PRAGMA wal_checkpoint(NOOP)", [], |row| {
                    Ok((row.get(1)?, row.get(2)?))
                })
                .unwrap();
            let finished = events_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            stop.store(true, Ordering::Release);
            cancel.store(true, Ordering::Release);
            let _ = controls_tx.send(Command::Shutdown);
            worker.join().unwrap().unwrap();
            drop(observer);
            assert!(matches!(
                finished,
                Event::IndexFinished {
                    total: 2,
                    cancelled: false,
                    errors: 0
                }
            ));
            assert!(logged > 0, "the scan must have written WAL frames");
            assert_eq!(
                logged, checkpointed,
                "IndexFinished was offered before the final checkpoint; full={full}"
            );
        }
    }

    #[test]
    fn cancelled_scan_batches_preserve_committed_rows_without_writing_queued_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let mut db = Database::open(&temporary.path().join("index.db")).unwrap();
        let kept = FileEntry {
            path: "/index/kept.txt".into(),
            name: "kept.txt".into(),
            parent: "/index".into(),
            extension: "txt".into(),
            size: Some(7),
            ..FileEntry::default()
        };
        db.upsert_batch("/index", 1, &[kept]).unwrap();
        let queued = [FileEntry {
            path: "/index/queued.txt".into(),
            name: "queued.txt".into(),
            parent: "/index".into(),
            extension: "txt".into(),
            size: Some(11),
            ..FileEntry::default()
        }];
        let cancel = AtomicBool::new(true);
        assert!(!commit_scan_batch(&mut db, "/index", 2, &queued, &cancel).unwrap());
        assert_eq!(db.total().unwrap(), 1);
        assert_eq!(
            db.search(&SearchRequest {
                text: "queued".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .total,
            0
        );
        db.integrity_check().unwrap();

        cancel.store(false, Ordering::Release);
        assert!(commit_scan_batch(&mut db, "/index", 3, &queued, &cancel).unwrap());
        assert_eq!(db.total().unwrap(), 2);
        assert_eq!(
            db.search(&SearchRequest {
                text: "queued".into(),
                ..SearchRequest::default()
            })
            .unwrap()
            .rows[0]
                .size,
            Some(11)
        );
        db.integrity_check().unwrap();
    }
}
