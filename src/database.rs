use crate::{
    model::{FileEntry, SearchPage, SearchRequest, Sort, SortColumn},
    query,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter, types::Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const COLUMNS: &str = "f.id,f.path,f.name,f.parent,f.extension,f.size,f.modified,f.created,f.is_dir,f.is_symlink,f.source";
// Keep parameter lists and cached SQL bounded even when callers submit a large batch.
const WRITE_CHUNK_SIZE: usize = 256;
const ENTRY_COLUMN_COUNT: usize = 16;

/// Each worker owns a connection; a connection is never shared with the GUI.
pub struct Database {
    pub(crate) conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("No se pudo abrir el índice {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-32768; PRAGMA temp_store=FILE; PRAGMA foreign_keys=ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            bail!("El índice fue creado por una versión más nueva; no se modificó");
        }
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
        }
        query::register_functions(&conn)?;
        Ok(Self { conn })
    }

    pub fn total(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT value FROM metadata WHERE key='total'", [], |r| {
                r.get::<_, i64>(0)
            })?
            .max(0) as u64)
    }

    pub fn next_generation(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "UPDATE metadata SET value=value+1 WHERE key='generation' RETURNING value",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn upsert_batch(
        &mut self,
        root: &str,
        generation: i64,
        entries: &[FileEntry],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_entries(&tx, root, generation, entries)?;
        tx.commit()?;
        Ok(())
    }

    /// Keep unchanged metadata and its sort indexes intact while marking scanned rows current.
    /// Both metadata changes and generation updates commit together, including the live FTS index.
    pub fn upsert_scan_batch(
        &mut self,
        root: &str,
        generation: i64,
        entries: &[FileEntry],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_entry_chunks(&tx, root, generation, entries, true)?;
        tx.commit()?;
        Ok(())
    }

    /// Called ONLY after an entirely successful traversal of this particular root.
    pub fn prune_root(&mut self, root: &str, generation: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM files WHERE source='' AND root=?1 AND generation<>?2",
            params![root, generation],
        )?)
    }

    pub fn remove_unconfigured_roots(&mut self, roots: &[String]) -> Result<usize> {
        let known: Vec<String> = self
            .conn
            .prepare("SELECT DISTINCT root FROM files WHERE source=''")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        for root in known {
            if !roots.contains(&root) {
                removed += tx.execute("DELETE FROM files WHERE source='' AND root=?1", [root])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Exact path plus separator-bounded descendants. Offline catalogs are immutable here.
    pub fn remove_subtree(&mut self, path: &str) -> Result<usize> {
        let path = path_key(path);
        let (lower, upper) = subtree_bounds(&path);
        Ok(self.conn.execute(
            "DELETE FROM files WHERE source='' AND (path_key=?1 OR (path_key>=?2 AND path_key<?3))",
            params![path, lower, upper],
        )?)
    }

    pub fn prune_subtree(&mut self, path: &str, generation: i64) -> Result<usize> {
        let path = path_key(path);
        let (lower, upper) = subtree_bounds(&path);
        Ok(self.conn.execute("DELETE FROM files WHERE source='' AND generation<>?4 AND (path_key=?1 OR (path_key>=?2 AND path_key<?3))", params![path, lower, upper, generation])?)
    }

    /// Prune confirmed areas of a partially readable root, preserving every failed subtree.
    pub fn prune_root_except(
        &mut self,
        root: &str,
        generation: i64,
        protected: &[std::path::PathBuf],
    ) -> Result<usize> {
        self.prune_protected(
            "source='' AND root=?1 AND generation<>?2",
            &[Value::Text(root.into()), Value::Integer(generation)],
            protected,
        )
    }

    pub fn prune_subtree_except(
        &mut self,
        path: &str,
        generation: i64,
        protected: &[std::path::PathBuf],
    ) -> Result<usize> {
        let path = path_key(path);
        let (lower, upper) = subtree_bounds(&path);
        self.prune_protected(
            "source='' AND generation<>?4 AND (path_key=?1 OR (path_key>=?2 AND path_key<?3))",
            &[
                Value::Text(path),
                Value::Text(lower),
                Value::Text(upper),
                Value::Integer(generation),
            ],
            protected,
        )
    }

    fn prune_protected(
        &mut self,
        predicate: &str,
        values: &[Value],
        protected: &[std::path::PathBuf],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS scan_protection(path_key TEXT PRIMARY KEY,lower_bound TEXT NOT NULL,upper_bound TEXT NOT NULL); DELETE FROM scan_protection;")?;
        {
            let mut insert =
                tx.prepare("INSERT OR IGNORE INTO scan_protection VALUES(?1,?2,?3)")?;
            for path in protected {
                let Some(path) = path.to_str() else {
                    return Ok(0);
                };
                let key = path_key(path);
                let (lower, upper) = subtree_bounds(&key);
                insert.execute(params![key, lower, upper])?;
            }
        }
        let deleted = tx.execute(&format!("DELETE FROM files WHERE {predicate} AND NOT EXISTS (SELECT 1 FROM scan_protection p WHERE files.path_key=p.path_key OR (files.path_key>=p.lower_bound AND files.path_key<p.upper_bound))"), params_from_iter(values.iter()))?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn remove_descendants(&mut self, path: &str) -> Result<usize> {
        let (lower, upper) = subtree_bounds(&path_key(path));
        Ok(self.conn.execute(
            "DELETE FROM files WHERE source='' AND path_key>=?1 AND path_key<?2",
            params![lower, upper],
        )?)
    }

    pub fn root_for_path(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT root FROM files WHERE path_key=?1 AND source=''",
                [path_key(path)],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn search(&self, request: &SearchRequest) -> Result<SearchPage> {
        let start = Instant::now();
        let compiled = query::compile(&request.text, &request.options)?;
        // A read transaction makes count and rows one consistent snapshot while indexing continues.
        let tx = self.conn.unchecked_transaction()?;
        let total: i64 = if compiled.where_sql == "1" {
            tx.query_row("SELECT value FROM metadata WHERE key='total'", [], |r| {
                r.get(0)
            })?
        } else {
            tx.query_row(
                &format!("SELECT count(*) FROM files f WHERE {}", compiled.where_sql),
                params_from_iter(compiled.params.iter()),
                |r| r.get(0),
            )?
        };
        let limit = request.limit.clamp(1, 2000);
        let offset = request.offset.min(i64::MAX as u64);
        let sql = format!(
            "SELECT {COLUMNS} FROM files f WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
            compiled.where_sql,
            order_sql(request.sort)
        );
        let mut values = compiled.params;
        values.push(Value::Integer(limit as i64));
        values.push(Value::Integer(offset as i64));
        let rows = {
            let mut stmt = tx.prepare(&sql)?;
            stmt.query_map(params_from_iter(values.iter()), read_entry)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.commit()?;
        Ok(SearchPage {
            rows,
            total: total.max(0) as u64,
            offset,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    /// Streaming export: no vector of all result rows and no changing OFFSET snapshot.
    pub fn for_each_match(
        &self,
        request: &SearchRequest,
        mut emit: impl FnMut(FileEntry) -> Result<()>,
    ) -> Result<u64> {
        let compiled = query::compile(&request.text, &request.options)?;
        let sql = format!(
            "SELECT {COLUMNS} FROM files f WHERE {} ORDER BY {}",
            compiled.where_sql,
            order_sql(request.sort)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(compiled.params.iter()), read_entry)?;
        let mut count = 0;
        for row in rows {
            emit(row?)?;
            count += 1;
        }
        Ok(count)
    }

    pub fn remove_catalog(&mut self, source: &str) -> Result<()> {
        if source.is_empty() {
            bail!("No se puede borrar el catálogo local como una importación");
        }
        self.conn
            .execute("DELETE FROM files WHERE source=?1", [source])?;
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if result != "ok" {
            bail!("Integridad del índice: {result}");
        }
        self.conn.execute(
            "INSERT INTO file_fts(file_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )?;
        Ok(())
    }
}

pub(crate) fn write_entries(
    conn: &Connection,
    root: &str,
    generation: i64,
    entries: &[FileEntry],
) -> Result<()> {
    write_entry_chunks(conn, root, generation, entries, false)
}

struct EntryValues<'a> {
    entry: &'a FileEntry,
    root: &'a str,
    generation: i64,
    extension: String,
    name_fold: String,
    path_fold: String,
    parent_fold: String,
    path_key: String,
    size: Option<i64>,
}

impl<'a> EntryValues<'a> {
    fn new(entry: &'a FileEntry, root: &'a str, generation: i64) -> Self {
        Self {
            entry,
            root,
            generation,
            extension: entry.extension.to_lowercase(),
            name_fold: entry.name.to_lowercase(),
            path_fold: entry.path.to_lowercase(),
            parent_fold: entry.parent.to_lowercase(),
            path_key: if entry.source.is_empty() {
                path_key(&entry.path)
            } else {
                entry.path.clone()
            },
            size: entry.size.map(|size| size.min(i64::MAX as u64) as i64),
        }
    }

    fn parameters(&self) -> [&dyn ToSql; ENTRY_COLUMN_COUNT] {
        [
            &self.entry.path,
            &self.entry.name,
            &self.entry.parent,
            &self.extension,
            &self.name_fold,
            &self.path_fold,
            &self.parent_fold,
            &self.size,
            &self.entry.modified,
            &self.entry.created,
            &self.entry.is_dir,
            &self.entry.is_symlink,
            &self.entry.source,
            &self.root,
            &self.generation,
            &self.path_key,
        ]
    }
}

fn write_entry_chunks(
    conn: &Connection,
    root: &str,
    generation: i64,
    entries: &[FileEntry],
    scan: bool,
) -> Result<()> {
    for chunk in entries.chunks(WRITE_CHUNK_SIZE) {
        let prepared: Vec<_> = chunk
            .iter()
            .map(|entry| EntryValues::new(entry, root, generation))
            .collect();
        let mut values: Vec<&dyn ToSql> =
            prepared.iter().flat_map(EntryValues::parameters).collect();
        // Several rows per statement avoid flushing FTS pending terms at every
        // individual INSERT's statement savepoint. All existing triggers stay active.
        let sql = upsert_sql(chunk.len(), scan);
        conn.prepare_cached(&sql)?
            .execute(params_from_iter(values.iter().copied()))?;
        if scan {
            // Do this after the complete ordered UPSERT, so a later duplicate
            // still wins even when an earlier occurrence had unchanged metadata.
            values.clear();
            values.push(&root);
            values.push(&generation);
            for entry in &prepared {
                values.push(&entry.entry.source);
                values.push(&entry.path_key);
            }
            values.push(&root);
            values.push(&generation);
            conn.prepare_cached(&touch_scan_sql(chunk.len()))?
                .execute(params_from_iter(values.iter().copied()))?;
        }
    }
    Ok(())
}

fn upsert_sql(rows: usize, scan: bool) -> String {
    let mut sql = String::from(INSERT_ENTRIES);
    for index in 0..rows {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str("(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)");
    }
    sql.push_str(UPDATE_ENTRY);
    if scan {
        sql.push_str(MATERIAL_CHANGE);
    }
    sql
}

fn touch_scan_sql(rows: usize) -> String {
    let mut sql =
        String::from("UPDATE files SET root=?,generation=? WHERE (source,path_key) IN (VALUES ");
    for index in 0..rows {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str("(?,?)");
    }
    sql.push_str(") AND (root IS NOT ? OR generation IS NOT ?)");
    sql
}

pub(crate) fn order_sql(sort: Sort) -> String {
    let column = match sort.column {
        SortColumn::Name => "f.name_fold",
        SortColumn::Path => "f.path_fold",
        SortColumn::Size => "f.size",
        SortColumn::Modified => "f.modified",
        SortColumn::Created => "f.created",
        SortColumn::Extension => "f.extension",
    };
    let direction = if sort.descending { "DESC" } else { "ASC" };
    format!("{column} {direction}, f.id {direction}")
}

fn read_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileEntry> {
    Ok(FileEntry {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        parent: row.get(3)?,
        extension: row.get(4)?,
        size: row.get::<_, Option<i64>>(5)?.map(|n| n.max(0) as u64),
        modified: row.get(6)?,
        created: row.get(7)?,
        is_dir: row.get(8)?,
        is_symlink: row.get(9)?,
        source: row.get(10)?,
    })
}

fn path_key(path: &str) -> String {
    if cfg!(windows) {
        let normal = path.replace('/', "\\").to_lowercase();
        if let Some(unc) = normal.strip_prefix(r"\\?\unc\") {
            format!(r"\\{unc}")
        } else {
            normal.strip_prefix(r"\\?\").unwrap_or(&normal).to_owned()
        }
    } else {
        path.to_owned()
    }
}

fn subtree_bounds(path: &str) -> (String, String) {
    // These mutations affect only live local records. On Unix a backslash is
    // part of a filename, never a directory boundary. Windows keys have already
    // normalized both accepted separators in path_key().
    let sep = std::path::MAIN_SEPARATOR;
    let base = path.trim_end_matches(sep);
    let lower = format!("{base}{sep}");
    let upper = format!("{base}{}", char::from_u32(sep as u32 + 1).unwrap());
    (lower, upper)
}

const INSERT_ENTRIES: &str = "INSERT INTO files(path,name,parent,extension,name_fold,path_fold,parent_fold,size,modified,created,is_dir,is_symlink,source,root,generation,path_key) VALUES ";
const UPDATE_ENTRY: &str = " ON CONFLICT(source,path_key) DO UPDATE SET path=excluded.path,name=excluded.name,parent=excluded.parent,extension=excluded.extension,name_fold=excluded.name_fold,path_fold=excluded.path_fold,parent_fold=excluded.parent_fold,size=excluded.size,modified=excluded.modified,created=excluded.created,is_dir=excluded.is_dir,is_symlink=excluded.is_symlink,root=excluded.root,generation=excluded.generation";
const MATERIAL_CHANGE: &str = " WHERE files.path IS NOT excluded.path OR files.name IS NOT excluded.name OR files.parent IS NOT excluded.parent OR files.extension IS NOT excluded.extension OR files.name_fold IS NOT excluded.name_fold OR files.path_fold IS NOT excluded.path_fold OR files.parent_fold IS NOT excluded.parent_fold OR files.size IS NOT excluded.size OR files.modified IS NOT excluded.modified OR files.created IS NOT excluded.created OR files.is_dir IS NOT excluded.is_dir OR files.is_symlink IS NOT excluded.is_symlink";

const SCHEMA: &str = "BEGIN IMMEDIATE;
CREATE TABLE metadata(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO metadata VALUES('total',0),('generation',0);
CREATE TABLE files(
 id INTEGER PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL, parent TEXT NOT NULL,
 extension TEXT NOT NULL, name_fold TEXT NOT NULL, path_fold TEXT NOT NULL, parent_fold TEXT NOT NULL,
 size INTEGER, modified INTEGER, created INTEGER, is_dir INTEGER NOT NULL, is_symlink INTEGER NOT NULL,
 source TEXT NOT NULL DEFAULT '', root TEXT NOT NULL, generation INTEGER NOT NULL, path_key TEXT NOT NULL,
 UNIQUE(source,path_key)
);
CREATE INDEX files_name ON files(name_fold,id);
CREATE INDEX files_path ON files(path_fold,id);
CREATE INDEX files_size ON files(size,id);
CREATE INDEX files_modified ON files(modified,id);
CREATE INDEX files_created ON files(created,id);
CREATE INDEX files_extension ON files(extension,id);
CREATE INDEX files_root ON files(root,generation) WHERE source='';
CREATE VIRTUAL TABLE file_fts USING fts5(name_fold,path_fold,content='files',content_rowid='id',tokenize='trigram',detail='none');
CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
 INSERT INTO file_fts(rowid,name_fold,path_fold) VALUES(new.id,new.name_fold,new.path_fold);
 UPDATE metadata SET value=value+1 WHERE key='total';
END;
CREATE TRIGGER files_ad AFTER DELETE ON files BEGIN
 INSERT INTO file_fts(file_fts,rowid,name_fold,path_fold) VALUES('delete',old.id,old.name_fold,old.path_fold);
 UPDATE metadata SET value=value-1 WHERE key='total';
END;
CREATE TRIGGER files_au AFTER UPDATE OF name_fold,path_fold ON files WHEN old.name_fold<>new.name_fold OR old.path_fold<>new.path_fold BEGIN
 INSERT INTO file_fts(file_fts,rowid,name_fold,path_fold) VALUES('delete',old.id,old.name_fold,old.path_fold);
 INSERT INTO file_fts(rowid,name_fold,path_fold) VALUES(new.id,new.name_fold,new.path_fold);
END;
PRAGMA user_version=1;
COMMIT;";
