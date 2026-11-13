# Indexing throughput correction

## Intent and evidence

The user reports that the first Windows build takes too long to index their catalogue. The reference catalogue has about 8.5 million objects. Improve the existing indexing pipeline and publish a verified Windows executable without asking the user to delete their index.

A release benchmark of the exact previous scanner/database/engine, plus an isolated benchmark harness, enumerated 100,000 real Linux files in 0.195 seconds but needed 5.650 seconds for the initial GUI-engine index and 1.638 seconds for an unchanged rescan. The metadata-only pass warms caches; these timings are not measurements of the user's disk.

Diagnostic SQLite experiments identified per-row outer SQL statements, repeated ordinary-index maintenance, and small transactions as substantial costs. In a controlled 200,000-row experiment with 8,192-row transactions, grouping 256 rows per SQL statement took 4.251 seconds versus 10.490 seconds for one-row statements, while keeping all indexes and triggers active.

## Approaches considered

1. **Selected: bounded multi-row SQL and a scan-specific unchanged-record path.** Preserve the existing schema, live FTS consistency, partial results, existing data and permissions. Increase scanner transaction-sized batches while flushing slow partial batches promptly.
2. **Defer FTS/sort indexes until the end.** This needs persistent incomplete-index state, fallback queries, recovery and possibly migration. Multi-row SQL measured faster than deferred FTS including rebuild time, so that complexity is unnecessary for this correction.
3. **Native MFT/USN or a new Windows directory walker.** USN enumeration alone lacks size and file timestamps and changes privilege/coverage assumptions. Larger directory-fetch buffers remain an opportunity, but database work dominates the measured pipeline. Do not silently remove metadata or alter permissions.

## Design

- `write_entries` keeps its transaction ownership contract and issues parameterized SQL statements of at most 256 rows. Normal live updates and atomic EFU import continue to use it.
- `Database::upsert_scan_batch(root, generation, entries)` performs conditional metadata upserts followed by root/generation updates in one transaction. Unchanged metadata does not update its secondary indexes. Duplicate keys retain input-order semantics, including across SQL chunks.
- Scanner batches contain at most 8,192 records. The existing approximately 200 ms progress checkpoint flushes a nonempty partial batch, so slow directories and small roots remain visible without waiting for a full batch.
- Both CLI indexing and the GUI engine use the scan-specific database path. The GUI writer discards queued scan work after cancellation; previously committed rows remain available.
- Scanner exclusion checks reuse normalized traversal paths, preserving Windows case, Unicode, UNC/verbatim and unusual trailing-dot/space semantics.
- Add `es benchmark-filesystem` to measure real metadata enumeration, the actual GUI pipeline and an unchanged rescan, with correctness checks and bounded cleanup.

## Constraints and verification

Keep Rust 1.100.0 and locked dependencies. Do not change schema version, durability settings, query semantics, offline-catalog identity, symlink handling or permission-error pruning. Readers must see complete committed transactions and valid FTS results throughout indexing.

Verify duplicate records, NULL metadata, changed fields, failures partway through a multi-statement transaction, live reader snapshots, cancellation, slow partial batches and Windows path exclusions. Compare old/new release builds on the same host. Native Windows tests and a real-filesystem benchmark must pass before publishing the executable. Document actual improvements and their limits; do not extrapolate synthetic measurements to the user's 8.5-million-object catalogue.
