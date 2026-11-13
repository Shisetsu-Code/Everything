# Internal contracts

`src/model.rs` is the authoritative set of shared types. Timestamp values are UTC Unix seconds. `None` means unknown. `source == ""` means a live local filesystem record; imported catalogs use a nonempty source. Paths retain their native separator and case; `name_fold`, `path_fold`, `parent_fold` are Unicode `.to_lowercase()` copies for searching. Never interpolate a user value into SQL or a shell command.

## Query module (query agent owns `src/query.rs`)

```rust
pub struct CompiledQuery { pub where_sql: String, pub params: Vec<rusqlite::types::Value> }
pub fn compile(text: &str, options: &crate::model::SearchOptions) -> anyhow::Result<CompiledQuery>;
pub fn register_functions(conn: &rusqlite::Connection) -> anyhow::Result<()>;
```

SQL alias is `f` for the `files` table. Columns: `id INTEGER PRIMARY KEY`, `path`, `name`, `parent`, `extension`, `name_fold`, `path_fold`, `parent_fold` TEXT, `size`, `modified`, `created` INTEGER nullable, `is_dir`, `is_symlink` INTEGER, `source` TEXT. Internal `root` TEXT and `generation` INTEGER are not query functions. FTS table `file_fts(name_fold, path_fold, content='files', content_rowid='id', tokenize='trigram', detail='none')` stays synchronized through triggers. Accelerate literal candidates using bounded three-Unicode-character MATCH terms on that table; always retain exact predicates so candidates cannot create false positives. Never use a trigram candidate that creates false negatives. Values are `?` parameters in their SQL occurrence order. Functions available to user: `ext`, `file`, `folder`, `path`, `parent`, `size`, `dm`/`date-modified`, `dc`/`date-created`, `regex`, `case`, `audio`, `video`, `pic`, `doc`, `exe`, `compressed`. For `content:` do not silently fake support; either implement correctly or return explicit unsupported diagnostic.

## Indexer module (indexer agent owns `src/indexer.rs`)

```rust
pub fn default_roots() -> Vec<std::path::PathBuf>;
pub fn entry_from_path(path: &std::path::Path) -> std::io::Result<crate::model::FileEntry>;
pub struct ScanSummary { pub scanned: u64, pub errors: u64, pub cancelled: bool, pub complete_roots: Vec<std::path::PathBuf>, pub incomplete_roots: Vec<(std::path::PathBuf, Vec<std::path::PathBuf>)> }
pub enum ScanEvent { Batch { root: std::path::PathBuf, entries: Vec<crate::model::FileEntry> }, Progress { scanned: u64, errors: u64, current: String }, Warning(String) }
pub fn scan(roots: &[std::path::PathBuf], excludes: &[std::path::PathBuf], cancel: &std::sync::atomic::AtomicBool, emit: impl FnMut(ScanEvent) -> anyhow::Result<()>) -> anyhow::Result<ScanSummary>;
pub struct WatchBatch { pub paths: Vec<std::path::PathBuf>, pub rescan_required: bool, pub warnings: Vec<String> }
pub struct WatchService { /* private */ }
impl WatchService {
    pub fn new(roots: &[std::path::PathBuf], excludes: &[std::path::PathBuf]) -> anyhow::Result<Self>;
    pub fn drain(&mut self) -> WatchBatch;
}
```

Emit bounded batches (up to 8192 records), including nonempty partial batches at progress checkpoints. `complete_roots` contains only roots entirely traversed without access/errors/cancellation; these can be generation-pruned directly. Incomplete roots include every protected error subtree; prune only outside those paths. Any cancellation clears both pruning lists. Follow no symlinks. Excludes use normalized component boundaries, not naive string prefixes. Watch callback queue is bounded; overflow/need_rescan signals a full reconciliation. Failed watches must be surfaced and fall back to reconciliation, not silently turn off monitoring.

## Database scan writes

`Database::upsert_scan_batch(&mut self, root: &str, generation: i64, entries: &[FileEntry]) -> anyhow::Result<()>` atomically writes one scan batch. Its SQL statements contain at most 256 rows. Unchanged records retain their metadata and stable IDs while root/generation are refreshed; changed rows keep FTS synchronized in the same transaction. The engine checks cancellation before accepting queued batches. `upsert_batch` remains available for ordinary updates, and `write_entries` retains the caller's transaction for atomic imports. Source/path identity, NULL handling and last-occurrence-wins duplicate order are shared by both write paths.

## Configuration (root owns `src/config.rs`)

```rust
pub struct Bookmark { pub name: String, pub query: String, pub options: SearchOptions }
pub enum Theme { System, Light, Dark }
pub struct AppConfig { pub roots: Vec<PathBuf>, pub excludes: Vec<PathBuf>, pub bookmarks: Vec<Bookmark>, pub history: Vec<String>, pub theme: Theme, pub auto_scan: bool, pub watch: bool, pub page_size: usize, pub search_options: SearchOptions, pub sort: Sort }
impl AppConfig { pub fn load(path: &Path) -> anyhow::Result<Self>; pub fn save(&self, path: &Path) -> anyhow::Result<()>; }
```

## Engine (root owns `src/engine.rs`)

```rust
pub struct Engine { pub events: crossbeam_channel::Receiver<Event>, /* private */ }
impl Engine {
    pub fn start(db_path: PathBuf, config_path: PathBuf, config: AppConfig) -> anyhow::Result<Self>;
    pub fn send(&self, command: Command) -> anyhow::Result<()>;
}
pub enum Command {
    Search(SearchRequest), Rebuild, CancelIndex, Configure(AppConfig),
    Export { path: PathBuf, request: SearchRequest, format: ExportFormat },
    Import { path: PathBuf }, FileAction(FileAction), Shutdown,
}
pub enum FileAction { Open(PathBuf), Reveal(PathBuf), Rename { path: PathBuf, new_name: String }, Recycle(Vec<PathBuf>) }
pub enum Event {
    Ready { total: u64 }, IndexProgress { scanned: u64, errors: u64, current: String },
    IndexFinished { total: u64, cancelled: bool, errors: u64 },
    Results { request_id: u64, page: SearchPage }, SearchError { request_id: u64, message: String }, Configured { config: AppConfig }, ConfigurationError { message: String, config: AppConfig }, Changed { total: u64 },
    Error(String), Warning(String), OperationComplete(String),
}
```

Engine `send(Search)` immediately invalidates the old search before enqueueing. Shutdown and cancel set atomics immediately. Results carry IDs so the UI discards obsolete generations. Scanning, SQL, export/import and file actions run outside the render thread. Database-directory exclusions are added automatically by the engine.

## GUI (GUI agent owns `src/gui.rs` and may add `src/gui/`)

```rust
pub fn run(engine: crate::engine::Engine, config: crate::config::AppConfig, initial_query: Option<String>) -> anyhow::Result<()>;
```

Compact menu, query, type filters, virtual table of bounded pages and status. File actions act only on explicit selection; recycle needs a concrete UI confirmation. Offline rows must be visibly marked and must not silently operate on a same-named local file. Remember geometry optionally. Bookmarks include search mode. Send `Configure` to persist options; it triggers scan only if roots/exclusions/watch changed. Use `.try_recv()` and a short per-frame time budget for event draining. Query debounce approximately 120 ms. Pagination remains explicit and correct, and the UI must never imply it is displaying all results when showing a page. Disable destructive actions until selection/confirmation is valid.
