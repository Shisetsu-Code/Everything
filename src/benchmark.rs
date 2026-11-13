//! Reproducible, isolated filesystem benchmark of the production scanner and engine.

use crate::{
    config::AppConfig,
    database::Database,
    engine::{Command, Engine, Event},
    indexer::{self, ScanEvent},
    model::SearchRequest,
};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::RecvTimeoutError;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const CONTENT: &[u8] = b"Everything filesystem benchmark.\n";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Measure real directory enumeration and the same asynchronous engine used by
/// the GUI. The generated tree and database never touch the user's configuration.
/// Timings deliberately include native watcher setup in the initial index.
pub fn filesystem(files: u64, timeout_seconds: u64) -> Result<Value> {
    ensure!(
        (1_000..=1_000_000).contains(&files),
        "Use entre 1.000 y 1.000.000 archivos"
    );
    ensure!(
        (1..=86_400).contains(&timeout_seconds),
        "Use entre 1 y 86.400 segundos por fase"
    );
    let timeout = Duration::from_secs(timeout_seconds);
    let workspace = tempfile::Builder::new()
        .prefix("everything-fs-benchmark-")
        .tempdir()?;
    let root = workspace.path().join("files");
    let db_path = workspace.path().join("data").join("index.db");
    let config_path = workspace.path().join("data").join("settings.json");
    let created = Instant::now();
    let directories = create_fixture(&root, files, created + timeout)?;
    let fixture_creation_seconds = created.elapsed().as_secs_f64();
    let entries = files + directories;
    let mut warnings = Vec::new();

    let started = Instant::now();
    let deadline = started + timeout;
    let cancel = AtomicBool::new(false);
    let mut observed_files = 0;
    let summary = indexer::scan(std::slice::from_ref(&root), &[], &cancel, |event| {
        if Instant::now() >= deadline {
            cancel.store(true, Ordering::Release);
            bail!("Tiempo agotado durante metadata_scan");
        }
        match event {
            ScanEvent::Batch { entries, .. } => {
                observed_files += entries.iter().filter(|entry| !entry.is_dir).count() as u64;
            }
            ScanEvent::Warning(message) => warnings.push(format!("metadata_scan: {message}")),
            ScanEvent::Progress { .. } => {}
        }
        Ok(())
    })?;
    let metadata_scan = phase(
        started,
        files,
        summary.scanned,
        summary.errors,
        summary.cancelled,
    );
    ensure!(
        !summary.cancelled && summary.scanned == entries && observed_files == files,
        "El recorrido no enumeró la fixture completa: {} entradas, {observed_files} archivos; se esperaban {entries} entradas y {files} archivos",
        summary.scanned
    );

    let started = Instant::now();
    let engine = Engine::start(
        db_path.clone(),
        config_path,
        AppConfig {
            roots: vec![root],
            excludes: Vec::new(),
            auto_scan: true,
            watch: true,
            ..AppConfig::default()
        },
    )?;
    let mut session = Session {
        engine: Some(engine),
        workspace: Some(workspace),
        scan_finished: false,
    };
    // Always stop the engine before an error unwinds and removes its fixture.
    let measured = (|| -> Result<_> {
        let engine = session.engine.as_ref().expect("running benchmark");
        let initial_index = wait_finished(
            engine,
            started,
            timeout,
            "initial_index",
            files,
            entries,
            &mut warnings,
        )?;
        session.scan_finished = true;
        let initial_files = search_count(engine, 1, "file:", timeout, &mut warnings)?;
        ensure!(
            initial_files == files,
            "La búsqueda inicial devolvió {initial_files} archivos; se esperaban {files}"
        );

        let started = Instant::now();
        engine.send(Command::Rebuild)?;
        session.scan_finished = false;
        let unchanged_rescan = wait_finished(
            engine,
            started,
            timeout,
            "unchanged_rescan",
            files,
            entries,
            &mut warnings,
        )?;
        session.scan_finished = true;
        let mut counts = Vec::new();
        for (index, (query, expected)) in [
            ("file:", files),
            ("folder:", directories),
            ("file: ext:pdf", files.div_ceil(4)),
            ("informe_á_日本_00000000", 1),
        ]
        .into_iter()
        .enumerate()
        {
            let actual = search_count(engine, index as u64 + 2, query, timeout, &mut warnings)?;
            ensure!(
                actual == expected,
                "La búsqueda {query:?} devolvió {actual}; se esperaban {expected}"
            );
            counts.push(actual);
        }
        Ok((initial_index, unchanged_rescan, initial_files, counts))
    })();
    let shutdown_started = Instant::now();
    let shutdown = session.shutdown();
    let shutdown_seconds = shutdown_started.elapsed().as_secs_f64();
    let (initial_index, unchanged_rescan, initial_files, counts) = match (measured, shutdown) {
        (Ok(value), Ok(())) => value,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(shutdown)) => {
            return Err(error.context(format!("Cierre del benchmark: {shutdown:#}")));
        }
    };

    let checked = Instant::now();
    {
        let db = Database::open(&db_path)?;
        db.integrity_check()?;
        ensure!(db.total()? == entries, "El índice cambió durante el cierre");
    }
    let integrity_check_seconds = checked.elapsed().as_secs_f64();
    let database_bytes = fs::metadata(&db_path)?.len();
    Ok(json!({
        "kind": "real_filesystem_engine",
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism()?.get(),
        "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "watch": true,
        "timeout_seconds_per_phase": timeout_seconds,
        "cache_note": "Metadata scan runs before initial indexing; unchanged rescan uses a warmed filesystem cache.",
        "fixture": { "files": files, "directories": directories, "entries": entries, "bytes_per_file": CONTENT.len() },
        "fixture_creation_seconds": fixture_creation_seconds,
        "metadata_scan": metadata_scan,
        "initial_index": initial_index,
        "unchanged_rescan": unchanged_rescan,
        "verification": {
            "initial_file_matches": initial_files,
            "file_matches": counts[0],
            "folder_matches": counts[1],
            "pdf_matches": counts[2],
            "unicode_matches": counts[3],
            "integrity_checked": true,
            "integrity_check_seconds": integrity_check_seconds,
        },
        "database_bytes": database_bytes,
        "shutdown_seconds": shutdown_seconds,
        "warnings": warnings,
    }))
}

fn create_fixture(root: &Path, files: u64, deadline: Instant) -> Result<u64> {
    fs::create_dir(root)?;
    let mut directories = 1;
    for index in 0..files {
        ensure!(
            Instant::now() < deadline,
            "Tiempo agotado creando archivos de prueba"
        );
        let group = root.join(format!("group_{:04}", index / 1_000));
        let bucket = group.join(format!("bucket_{:02}", index % 1_000 / 100));
        if index % 1_000 == 0 {
            fs::create_dir(&group)?;
            directories += 1;
        }
        if index % 100 == 0 {
            fs::create_dir(&bucket)?;
            directories += 1;
        }
        let extension = ["pdf", "rs", "txt", "png"][index as usize % 4];
        let stem = if index % 10 == 0 {
            "informe_á_日本"
        } else {
            "document"
        };
        fs::write(
            bucket.join(format!("{stem}_{index:08}.{extension}")),
            CONTENT,
        )?;
    }
    Ok(directories)
}

fn phase(started: Instant, files: u64, entries: u64, errors: u64, cancelled: bool) -> Value {
    let seconds = started.elapsed().as_secs_f64();
    json!({
        "seconds": seconds,
        "files_per_second": files as f64 / seconds.max(f64::EPSILON),
        "entries_per_second": entries as f64 / seconds.max(f64::EPSILON),
        "entries": entries,
        "errors": errors,
        "cancelled": cancelled,
    })
}

fn next_event(engine: &Engine, deadline: Instant, phase: &str) -> Result<Event> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        let _ = engine.send(Command::CancelIndex);
        bail!("Tiempo agotado durante {phase}");
    }
    match engine.events.recv_timeout(remaining) {
        Ok(event) => Ok(event),
        Err(RecvTimeoutError::Timeout) => {
            let _ = engine.send(Command::CancelIndex);
            bail!("Tiempo agotado durante {phase}");
        }
        Err(RecvTimeoutError::Disconnected) => bail!("El motor se cerró durante {phase}"),
    }
}

fn wait_finished(
    engine: &Engine,
    started: Instant,
    timeout: Duration,
    name: &str,
    files: u64,
    expected_entries: u64,
    warnings: &mut Vec<String>,
) -> Result<Value> {
    loop {
        match next_event(engine, started + timeout, name)? {
            Event::IndexFinished {
                total,
                cancelled,
                errors,
            } => {
                let result = phase(started, files, total, errors, cancelled);
                ensure!(!cancelled, "La fase {name} se canceló");
                ensure!(
                    total == expected_entries,
                    "{name} terminó con {total} entradas; se esperaban {expected_entries}"
                );
                return Ok(result);
            }
            Event::Warning(message) => warnings.push(format!("{name}: {message}")),
            Event::Error(message) => bail!("{name}: {message}"),
            _ => {}
        }
    }
}

fn search_count(
    engine: &Engine,
    id: u64,
    text: &str,
    timeout: Duration,
    warnings: &mut Vec<String>,
) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    engine.send(Command::Search(SearchRequest {
        id,
        text: text.to_owned(),
        limit: 1,
        ..SearchRequest::default()
    }))?;
    loop {
        match next_event(engine, deadline, "verification")? {
            Event::Results { request_id, page } if request_id == id => return Ok(page.total),
            Event::SearchError {
                request_id,
                message,
            } if request_id == id => bail!("Búsqueda {text:?}: {message}"),
            Event::Error(message) => bail!("Verificación: {message}"),
            Event::Warning(message) => warnings.push(format!("verification: {message}")),
            _ => {}
        }
    }
}

/// Event-channel disconnection proves that the writer, search and task workers
/// have released their handles before TempDir cleanup, including on Windows.
/// Keep the directory on a stuck shutdown instead of deleting under a worker.
struct Session {
    engine: Option<Engine>,
    workspace: Option<TempDir>,
    scan_finished: bool,
}

impl Session {
    fn shutdown(&mut self) -> Result<()> {
        let Some(engine) = self.engine.take() else {
            return Ok(());
        };
        engine.send(Command::Shutdown)?;
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match engine.events.recv_timeout(remaining) {
                Err(RecvTimeoutError::Disconnected) => {
                    if !self.scan_finished {
                        // The scanner is detached and owns no Event sender. A
                        // cancelled traversal could still be returning from I/O.
                        let retained = self
                            .workspace
                            .take()
                            .context("Falta la carpeta de prueba")?
                            .keep();
                        bail!(
                            "El recorrido no confirmó su finalización; se conservaron los archivos de prueba en {}",
                            retained.display()
                        );
                    }
                    return Ok(());
                }
                Ok(_) if !remaining.is_zero() => {}
                _ => {
                    let retained = self
                        .workspace
                        .take()
                        .context("Falta la carpeta de prueba")?
                        .keep();
                    bail!(
                        "El motor no se cerró a tiempo; se conservaron los archivos de prueba en {}",
                        retained.display()
                    );
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("{error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_benchmark_rejects_unbounded_work_before_creating_a_fixture() {
        for files in [0, 999, 1_000_001, u64::MAX] {
            assert!(filesystem(files, 30).is_err(), "accepted {files} files");
        }
        for timeout in [0, 86_401, u64::MAX] {
            assert!(
                filesystem(1_000, timeout).is_err(),
                "accepted {timeout} seconds"
            );
        }
    }

    #[test]
    fn filesystem_benchmark_indexes_and_reconciles_real_unicode_files() {
        let report = filesystem(1_000, 30).unwrap();
        assert_eq!(report["kind"], "real_filesystem_engine");
        assert_eq!(report["fixture"]["files"], 1_000);
        assert_eq!(report["fixture"]["directories"], 12);
        assert_eq!(report["fixture"]["entries"], 1_012);
        assert_eq!(report["watch"], true);
        for phase in ["metadata_scan", "initial_index", "unchanged_rescan"] {
            assert_eq!(report[phase]["entries"], 1_012, "{phase}");
            assert_eq!(report[phase]["errors"], 0, "{phase}");
            assert_eq!(report[phase]["cancelled"], false, "{phase}");
            assert!(report[phase]["seconds"].is_number(), "{phase}");
            assert!(report[phase]["files_per_second"].is_number(), "{phase}");
        }
        assert_eq!(report["verification"]["file_matches"], 1_000);
        assert_eq!(report["verification"]["folder_matches"], 12);
        assert_eq!(report["verification"]["pdf_matches"], 250);
        assert_eq!(report["verification"]["unicode_matches"], 1);
        assert_eq!(report["verification"]["integrity_checked"], true);
        assert!(report["database_bytes"].as_u64().unwrap() > 0);
    }
}
