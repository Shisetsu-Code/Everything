# Indexing Throughput Implementation Plan

> **For agentic workers:** use the executing-plans workflow; independent scanner and database tasks are delegated with separate file ownership. Tests and measurements precede completion claims.

**Goal:** Correct excessive initial-index and rescan work and deliver a verified Windows update.

**Architecture:** Keep the existing SQLite WAL index and bounded worker channels. Group parameterized SQL into 256-row statements, use scan-specific conditional writes, and feed transactions with promptly flushed batches of at most 8,192 entries.

**Tech Stack:** Existing Rust 1.100.0, rusqlite/bundled SQLite, walkdir, notify and eframe; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-13-indexing-throughput-design.md`

## Global constraints and review focus

- Retain schema/version compatibility and all search functions during indexing.
- Preserve source/path identity, ordered duplicates, NULLs and transaction rollback.
- Never prune incomplete/cancelled/error-protected areas or modify offline catalogues as local files.
- Keep SQL bindings, memory, queued work and cancellation latency bounded.
- Measure real filesystem/engine work as well as synthetic insertion; identify cache conditions and platform.

## Tasks

### 1. Reproduce and measure

- [x] Inspect scanner, engine, database and exact pinned Windows backend.
- [x] Add isolated real-filesystem benchmark using the production engine; test file counts, Unicode, integrity and cleanup.
- [x] Capture previous production-code release baseline and compare SQLite statement/transaction strategies.

### 2. Database writes — database agent

Files: `src/database.rs`, `tests/database.rs`.

- [x] Add failing tests for unchanged metadata writes, bounded statements, duplicates, rollback and reader consistency.
- [x] Implement 256-row parameterized `write_entries` while retaining caller-owned transactions.
- [x] Implement `Database::upsert_scan_batch(&mut self, root: &str, generation: i64, entries: &[FileEntry]) -> Result<()>` with conditional metadata writes and root/generation touch.
- [x] Run database/operation tests and inspect live FTS integrity.

### 3. Scanner batching — scanner agent

File: `src/indexer.rs`.

- [x] Test and implement normalized-path exclusion fast path with Windows edge cases.
- [x] Test and implement 8,192-record maximum batches plus partial flush at progress checkpoints.
- [x] Verify bounded batches, no lost records, cancellation and error protection.

### 4. Integration and delivery — root

Files: `src/engine.rs`, `src/cli.rs`, release workflow and documentation.

- [x] Use scan-specific writes in CLI/GUI; prevent queued batches from writing after cancellation.
- [x] Run paired release benchmarks, format, Clippy and full tests.
- [x] Obtain a fresh focused review and resolve material findings.
- [ ] Run native Windows/Linux CI and Windows real-filesystem comparison.
- [ ] Publish the verified executable, preserve existing index compatibility, and document measurements.

### 5. Resolve the Windows rescan regression before final delivery

- [x] Preserve all original Windows measurements, including the 18% unchanged-rescan regression of 0.1.1.
- [x] Profile native Windows scan, database writes, pruning, checkpoint and CPU time.
- [x] Use a VFS observer linked to the exact bundled SQLite to isolate statement subjournal creation.
- [x] Match the existing all-or-nothing scan transaction with `UPDATE OR ROLLBACK`; retain schema and durability settings.
- [x] Verify late-touch ABORT/ROLLBACK errors undo prior chunks and leave the connection usable; run 96 local tests including opt-in diagnostics and independent review.
- [x] Confirm the isolated rollback change on native Windows against 0.1.1; only 4.95% better, so it did not explain the main delay.
- [x] Isolate checkpoint frequency on identical seed copies and one Windows executable: total median 28.407 → 14.434 s including final checkpoint and close.
- [x] Apply the measured threshold only during scans, restore the original watcher/import policy, and emit completion after the final checkpoint; 98 local tests, format, Clippy and no-GUI check pass.
- [ ] Publish 0.1.2 only after native correctness, compatibility and relative performance checks against 0.1.0.
