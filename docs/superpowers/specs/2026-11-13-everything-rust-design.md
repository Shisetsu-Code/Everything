# Everything Rust: desktop file search

## Intent

Build a real Windows desktop application in Rust, inspired by the supplied Everything screenshot. Keep the compact menu/search/results/status layout. The screenshot shows 8,525,854 objects and a nonresponding window: bounded memory and a responsive UI are primary requirements. The user authorized implementation and delivery to `Shisetsu-Code/Everything`.

## Decisions

- Rust 2024; native egui/eframe desktop UI. No JavaScript, browser shell, cloud account, or telemetry.
- Persistent SQLite index with WAL, B-tree metadata indexes, and an external-content FTS5 trigram index. Search work, indexing, filesystem operations, and exports run off the UI thread. Render only visible rows and request bounded pages.
- Filesystem enumeration covers configured drives/folders, files and folders, hidden items, Unicode names, timestamps and sizes. Symbolic links are indexed but never traversed. Permission failures are counted and surfaced. A cancelled/failed scan must not prune unseen files.
- OS change notifications update the database. Start monitoring before enumeration; debounce bursts; overflow requires reconciliation. Refresh on launch reconciles changes made while the application was closed.
- Queries: substrings, quoted text, AND/OR/NOT, grouping, wildcards, regex, case/word/path modes, `ext:`, `file:`, `folder:`, `path:`, `parent:`, `size:`, `dm:`, `dc:` and type presets. All query values are bound parameters. Malformed input yields an error, never a panic.
- Sort by name, path, size or modification date, with stable tie-breaking. Open files, reveal parents, copy paths/names, rename, recycle after confirmation, export CSV/EFU, import EFU as an explicitly separate offline catalog, bookmarks/history, exclusions and root settings, keyboard shortcuts and CLI.
- Automated tests exercise actual SQLite and temporary files; CI builds and tests Linux and Windows and produces a portable Windows executable.

## Approaches considered

1. Store the entire path index and all result IDs in RAM. Good simple scan throughput, but duplicates strings and scales memory with the screenshot's millions of entries.
2. Persistent indexed SQLite with bounded pages (selected implementation decision). More disk work, but bounded application memory, durable incremental updates, query indexes and precise metadata.
3. Read raw NTFS MFT/USN data. Fast initial name enumeration, but special access, parent reconstruction, hard links and journal recovery need a separate native backend. USN records do not supply file sizes or actual last-write times. Do not label ordinary traversal as MFT indexing.

## Limits to state honestly

The first release implements the local desktop search workflow. It does not claim byte-for-byte parity with voidtools Everything, its proprietary SDK/protocols, HTTP/FTP servers, Windows service, IFilter document extraction, or raw NTFS MFT/USN indexing. Initial traversal of millions of files can take time; the application remains usable and indexes persist between launches. Folder size means the directory entry's size is unknown, not a recursively calculated total. Network/external roots require configuration. No performance claim on 8.5 million real user files without measuring them.

## Acceptance

Both target OS builds pass; query parser/injection/Unicode tests pass; scan/create/update/rename/delete/cancel tests pass; paging and exporting use bounded memory; no unsafe/destructive action is performed just by indexing or searching; all deliverables and usage instructions are committed to the requested repository.
