use crate::{
    database::{Database, write_entries},
    model::{ExportFormat, FileEntry, SearchRequest},
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use std::{
    collections::HashMap,
    fs,
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
pub enum FileAction {
    Open(PathBuf),
    Reveal(PathBuf),
    Rename { path: PathBuf, new_name: String },
    Recycle(Vec<PathBuf>),
}

pub fn perform(action: FileAction) -> Result<String> {
    match action {
        FileAction::Open(path) => {
            // Pass an OS path directly: never construct cmd.exe / sh source from a filename.
            fs::symlink_metadata(&path).with_context(|| format!("No existe {}", path.display()))?;
            open::that(&path).with_context(|| format!("No se pudo abrir {}", path.display()))?;
            Ok(format!("Abierto: {}", path.display()))
        }
        FileAction::Reveal(path) => {
            let parent = path.parent().unwrap_or(&path);
            #[cfg(windows)]
            {
                // One argument, not command text; explorer's /select accepts the path after comma.
                let mut arg = std::ffi::OsString::from("/select,");
                arg.push(&path);
                std::process::Command::new("explorer.exe")
                    .arg(arg)
                    .spawn()?;
            }
            #[cfg(not(windows))]
            open::that(parent)?;
            Ok(format!("Carpeta: {}", parent.display()))
        }
        FileAction::Rename { path, new_name } => {
            if new_name.trim().is_empty()
                || [".", ".."].contains(&new_name.as_str())
                || new_name.contains(['/', '\\', '\0'])
            {
                bail!("Use solamente un nombre de archivo, sin rutas ni separadores");
            }
            #[cfg(windows)]
            if new_name.contains(['<', '>', ':', '"', '|', '?', '*'])
                || new_name.ends_with([' ', '.'])
            {
                bail!("El nombre contiene caracteres no válidos en Windows");
            }
            fs::symlink_metadata(&path)?;
            let destination = path.with_file_name(&new_name);
            if destination == path {
                return Ok("El nombre no cambió".into());
            }
            rename_no_replace(&path, &destination).with_context(|| {
                format!(
                    "No se pudo renombrar {} como {}. No se sobrescriben archivos existentes.",
                    path.display(),
                    destination.display()
                )
            })?;
            Ok(format!("Renombrado: {}", destination.display()))
        }
        FileAction::Recycle(paths) => {
            if paths.is_empty() {
                bail!("No hay archivos seleccionados");
            }
            for path in &paths {
                if path.parent().is_none() || path.file_name().is_none() {
                    bail!("No se puede enviar una raíz a la Papelera");
                }
            }
            let count = paths.len();
            trash::delete_all(paths).context(
                "No se pudo enviar la selección a la Papelera; no se realizó un borrado permanente",
            )?;
            Ok(format!("{count} elementos enviados a la Papelera"))
        }
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Canonicalize the parent only: retain symlink identity while obtaining
    // extended-length paths supported by MoveFileExW beyond MAX_PATH.
    let from = canonical_parent_and_name(from)?;
    let to = canonical_parent_and_name(to)?;
    let from: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: Both buffers are valid, NUL-terminated, and live until the call returns.
    // Flags=0 intentionally excludes MOVEFILE_REPLACE_EXISTING.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn rename_no_replace(_from: &Path, _to: &Path) -> Result<()> {
    bail!("El renombrado seguro está disponible en Windows y Linux")
}

pub fn export(
    db_path: &Path,
    destination: &Path,
    request: &SearchRequest,
    format: ExportFormat,
    cancelled: &AtomicBool,
) -> Result<u64> {
    let db = Database::open(db_path)?;
    let destination = checked_export_destination(db_path, destination)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    let count;
    {
        let mut writer = csv::WriterBuilder::new()
            .terminator(csv::Terminator::CRLF)
            .from_writer(temp.as_file_mut());
        match format {
            ExportFormat::Efu => writer.write_record([
                "Filename",
                "Size",
                "Date Modified",
                "Date Created",
                "Attributes",
            ])?,
            ExportFormat::Csv => writer.write_record([
                "Name",
                "Path",
                "Full Path",
                "Size",
                "Date Modified",
                "Date Created",
                "Type",
                "Catalog",
            ])?,
        }
        count = db.for_each_match(request, |entry| {
            if cancelled.load(Ordering::Acquire) {
                bail!("Exportación cancelada");
            }
            match format {
                ExportFormat::Efu => {
                    let attributes = (if entry.is_dir { 16 } else { 32 })
                        | if entry.is_symlink { 1024 } else { 0 };
                    writer.write_record([
                        entry.path,
                        optional_number(entry.size),
                        filetime(entry.modified),
                        filetime(entry.created),
                        attributes.to_string(),
                    ])?;
                }
                ExportFormat::Csv => {
                    writer.write_record([
                        spreadsheet_text(&entry.name),
                        spreadsheet_text(&entry.parent),
                        spreadsheet_text(&entry.path),
                        optional_number(entry.size),
                        iso_date(entry.modified),
                        iso_date(entry.created),
                        if entry.is_dir {
                            "Folder".into()
                        } else {
                            "File".into()
                        },
                        spreadsheet_text(&entry.source),
                    ])?;
                }
            }
            Ok(())
        })?;
        writer.flush()?;
    }
    if cancelled.load(Ordering::Acquire) {
        bail!("Exportación cancelada");
    }
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;
    // Recheck after a long export in case the destination was replaced with a
    // link to an application file while its rows were being written.
    let _ = checked_export_destination(db_path, &destination)?;
    temp.persist(destination).map_err(|e| e.error)?;
    Ok(count)
}

/// Resolve parent aliases while preserving the final component: persist()
/// replaces an existing final symlink rather than writing through it. Both its
/// location and referent must still be checked against protected application
/// files so an export cannot invalidate a path used by another DB connection.
fn checked_export_destination(db_path: &Path, destination: &Path) -> Result<PathBuf> {
    let destination = canonical_parent_and_name(destination)?;
    let destination_referent = canonical_if_present(&destination)?;
    let destination_metadata = metadata_if_present(&destination)?;
    let database_location = canonical_parent_and_name(db_path)?;
    let database_referent = fs::canonicalize(db_path)?;
    let mut protected = Vec::new();
    for database in [&database_location, &database_referent] {
        protected.push(database.to_owned());
        for suffix in ["-wal", "-shm", ".writer.lock"] {
            let mut filename = database.as_os_str().to_owned();
            filename.push(suffix);
            protected.push(PathBuf::from(filename));
        }
    }
    if let Some(parent) = database_location.parent() {
        protected.push(parent.join("settings.json"));
    }
    protected.sort();
    protected.dedup();
    for path in protected {
        let referent = canonical_if_present(&path)?;
        let same_location = same_export_path(&destination, &path)
            || same_export_path(&destination_referent, &path)
            || same_export_path(&destination_referent, &referent);
        let same_file = match (&destination_metadata, metadata_if_present(&path)?) {
            (Some(destination), Some(protected)) => same_file_identity(destination, &protected),
            _ => false,
        };
        if same_location || same_file {
            bail!(
                "No se puede exportar sobre el índice, sus archivos auxiliares ni settings.json. Elija otro destino."
            );
        }
    }
    Ok(destination)
}

fn canonical_parent_and_name(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let filename = path
        .file_name()
        .context("El destino debe ser un archivo, no una raíz o un directorio relativo")?;
    let parent = path
        .parent()
        .context("El destino no tiene una carpeta válida")?;
    Ok(fs::canonicalize(parent)
        .with_context(|| format!("No se pudo resolver la carpeta {}", parent.display()))?
        .join(filename))
}

fn canonical_if_present(path: &Path) -> std::io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(error) => Err(error),
    }
}

fn metadata_if_present(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn same_export_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy().replace('/', "\\").to_lowercase()
            == right.to_string_lossy().replace('/', "\\").to_lowercase()
    } else {
        left == right
    }
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

/// Import is one transaction: invalid or cancelled input leaves the previous catalog intact.
/// The engine serializes this writer with scanning; concurrent searches retain a WAL snapshot.
pub fn import(db_path: &Path, source_file: &Path, cancelled: &AtomicBool) -> Result<u64> {
    let source_file = std::path::absolute(source_file)?;
    let source = format!("efu:{}", source_file.display());
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(BufReader::new(fs::File::open(&source_file)?));
    let headers: HashMap<String, usize> = reader
        .headers()?
        .iter()
        .enumerate()
        .map(|(i, value)| {
            (
                value.trim_start_matches('\u{feff}').trim().to_lowercase(),
                i,
            )
        })
        .collect();
    if !headers.contains_key("filename") {
        bail!("El archivo EFU debe tener una columna Filename");
    }
    let mut db = Database::open(db_path)?;
    let tx = db.conn.transaction()?;
    tx.execute("DELETE FROM files WHERE source=?1", [&source])?;
    let mut batch = Vec::with_capacity(1024);
    let mut count = 0;
    for (line, record) in reader.records().enumerate() {
        if cancelled.load(Ordering::Acquire) {
            bail!("Importación cancelada; se conservó el catálogo anterior");
        }
        let record = record.with_context(|| format!("CSV inválido en fila {}", line + 2))?;
        let field =
            |name: &str| -> &str { headers.get(name).and_then(|i| record.get(*i)).unwrap_or("") };
        let raw_path = field("filename");
        if raw_path.is_empty() || raw_path.contains('\0') {
            bail!("Filename inválido en fila {}", line + 2);
        }
        let raw_attrs = field("attributes");
        let attributes: u32 = if raw_attrs.is_empty() {
            0
        } else if let Some(hex) = raw_attrs
            .strip_prefix("0x")
            .or_else(|| raw_attrs.strip_prefix("0X"))
        {
            u32::from_str_radix(hex, 16)?
        } else {
            raw_attrs
                .parse()
                .with_context(|| format!("Attributes inválido en fila {}", line + 2))?
        };
        let is_dir = attributes & 16 != 0 || raw_path.ends_with(['/', '\\']);
        let mut path =
            resolve_catalog_path(raw_path, source_file.parent().unwrap_or(Path::new(".")));
        if is_dir && path.len() > 3 {
            path = path.trim_end_matches(['/', '\\']).to_owned();
        }
        let (name, parent) = split_filename(&path);
        let size = if field("size").is_empty() {
            None
        } else {
            Some(
                field("size")
                    .parse::<u64>()
                    .with_context(|| format!("Size inválido en fila {}", line + 2))?,
            )
        };
        if size.is_some_and(|size| size > i64::MAX as u64) {
            bail!("Size excede el rango admitido en fila {}", line + 2);
        }
        batch.push(FileEntry {
            id: 0,
            path,
            extension: if is_dir {
                String::new()
            } else {
                name.rsplit_once('.')
                    .filter(|(stem, _)| !stem.is_empty())
                    .map(|(_, ext)| ext.to_lowercase())
                    .unwrap_or_default()
            },
            name,
            parent,
            size,
            modified: parse_date(field("date modified"))
                .with_context(|| format!("Date Modified inválido en fila {}", line + 2))?,
            created: parse_date(field("date created"))
                .with_context(|| format!("Date Created inválido en fila {}", line + 2))?,
            is_dir,
            is_symlink: attributes & 1024 != 0,
            source: source.clone(),
        });
        count += 1;
        if batch.len() == 1024 {
            write_entries(&tx, "", 0, &batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        write_entries(&tx, "", 0, &batch)?;
    }
    if cancelled.load(Ordering::Acquire) {
        bail!("Importación cancelada; se conservó el catálogo anterior");
    }
    tx.commit()?;
    Ok(count)
}

fn resolve_catalog_path(path: &str, base: &Path) -> String {
    let windows_absolute = path.as_bytes().get(1) == Some(&b':')
        && path
            .as_bytes()
            .get(2)
            .is_some_and(|b| *b == b'\\' || *b == b'/');
    if windows_absolute || path.starts_with(['/', '\\']) || path.contains("://") {
        path.to_owned()
    } else {
        base.join(path).to_string_lossy().into_owned()
    }
}

fn split_filename(path: &str) -> (String, String) {
    match path.rfind(['/', '\\']) {
        Some(index) if index + 1 < path.len() => {
            let is_drive_root = index == 2
                && path.as_bytes()[0].is_ascii_alphabetic()
                && path.as_bytes()[1] == b':';
            let parent_end = if index == 0 || is_drive_root {
                index + 1
            } else {
                index
            };
            (path[index + 1..].into(), path[..parent_end].into())
        }
        _ => (path.into(), String::new()),
    }
}

fn parse_date(value: &str) -> Result<Option<i64>> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        let ticks: i128 = value.parse()?;
        return Ok(Some(i64::try_from(
            ticks / 10_000_000 - 11_644_473_600i128,
        )?));
    }
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(date.timestamp()));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Some(
                Local
                    .from_local_datetime(&naive)
                    .single()
                    .context("Hora local ambigua o inexistente")?
                    .timestamp(),
            ));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(Some(
            Local
                .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
                .single()
                .context("Fecha local inválida")?
                .timestamp(),
        ));
    }
    bail!("Se esperaba FILETIME o fecha ISO 8601")
}

fn filetime(seconds: Option<i64>) -> String {
    seconds
        .and_then(|s| (s as i128 + 11_644_473_600i128).checked_mul(10_000_000))
        .filter(|t| *t >= 0)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn iso_date(seconds: Option<i64>) -> String {
    seconds
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .map(|date| date.to_rfc3339())
        .unwrap_or_default()
}
fn optional_number(value: Option<u64>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}
fn spreadsheet_text(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r', '\n']) {
        format!("'{value}")
    } else {
        value.into()
    }
}
