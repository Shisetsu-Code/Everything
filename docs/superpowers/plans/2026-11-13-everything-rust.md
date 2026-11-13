# Everything Rust Implementation Plan

> **For agentic workers:** execute the independent backend/query/UI tasks in parallel, integrate once, then review and verify the complete program.

**Goal:** Deliver a usable Rust desktop file search application to the requested GitHub repository.

**Architecture:** SQLite owns the persistent index. Bounded channels separate the GUI, scanner/watcher, database writer, search reader and export jobs. Trigram candidates narrow substring searches; SQL filters and stable sorting produce bounded pages.

**Tech Stack:** Rust 2024, eframe/egui 0.37, rusqlite 0.42 (bundled SQLite), notify 8, walkdir 2, clap 4, serde, chrono, rfd, trash.

**Spec:** `docs/superpowers/specs/2026-11-13-everything-rust-design.md`

## Global Constraints

- Rust application code only; Windows 11 is the primary platform.
- No filesystem traversal, SQL result scan or shell/file operation on the render thread.
- Preserve old index records on cancellation, inaccessible roots and partial failed scans.
- Parameterize all user query values; never concatenate user content into shell commands.
- Use bounded channels/batches/pages; preserve explicit offline-catalog provenance.

## Review Focus

- Renames/deletions of entire directory trees and notification overflow.
- Quoted Windows paths, drive letters, punctuation, Unicode case and hostile query strings.
- Cancellation during scan or an old search finishing after a newer query.
- Inaccessible/offline roots and changes made between launch and watcher installation.
- Export/import round trips, Windows rename collision behavior, recycle confirmation.

## Tasks

### 1. Shared models and durable database (root)

- [x] Define shared models/config/engine messages and document exact interfaces.
- [x] Write SQLite integration tests for persistence, updates/deletes, safe pruning and paging.
- [x] Implement schema, transactions, search reader, count metadata and exports/import.
- [x] Run `cargo test --no-default-features` and inspect results.

### 2. Query language (query agent)

- [x] Write parser/compiler tests against a real SQLite fixture before implementation.
- [x] Implement bound SQL compilation with boolean grammar, metadata, regex and trigram candidates.
- [x] Test malformed input, paths, Unicode, short queries, precedence and injection.

### 3. Scanner and live monitoring (indexer agent)

- [x] Write temporary-directory tests for exclusions, metadata, cancellation and symlinks.
- [x] Implement bounded batched scanning, root discovery and notify watch lifecycle.
- [x] Surface watcher failure/overflow so engine reconciles; verify event behavior.

### 4. Desktop UI (GUI agent)

- [x] Implement compact menu/search/virtualized results/status UI against shared message types.
- [x] Add settings, filters, bookmarks/history, file actions, dialogs and keyboard navigation.
- [x] Compile GUI, then exercise the app with a real temporary file catalog.

### 5. Integration, CLI and delivery (root)

- [x] Connect asynchronous workers and cancellation, implement CLI and native safe file operations.
- [x] Add Windows/Linux CI, release artifact packaging, README, query and architecture docs.
- [x] Run formatting, clippy, full tests, release build and representative benchmark.
- [x] Have a fresh agent review all runtime paths; fix material findings and reverify.
- [x] Publish to the repository, inspect the actual Windows CI result and artifact.
