# Existing-index startup fix

Goal: eliminate full-catalog work before Ready, preserve cleanup correctness and show fatal writer failures without an endless startup spinner.

## Evidence

- Ready follows root cleanup, exclusions and native watcher installation.
- The original exclusion OR predicate searches only source in the composite index; it visits all local paths even for an absent exclusion.
- DISTINCT root likewise chooses the source index, reads each wide table row and builds a distinct set.
- SQLite progress budgets reproduce both regressions on the bundled SQLite before changing production queries.
- The initial empty search uses metadata.total and existing sort indexes. Windows notify 8.2.2 registers recursive ReadDirectoryChangesW; it does not enumerate millions of paths at watch creation.
- The first Windows run passed 106 tests, but fixture generation exceeded its shared 300-second budget before startup could be measured. Separate fixture creation (900 seconds) from each startup (30 seconds in CI); retain all correctness and indexing regression gates.

## Work

- [x] Reproduce query plans and growth in VM instructions.
- [x] Replace subtree cleanup with bounded exact/range lookups, preserving statement atomicity and offline records.
- [x] Enumerate distinct roots with successive seeks in the existing partial files_root index; keep schema 1.
- [x] Report preparation phases and fatal writer failure; preserve Ready after cleanup and invalidate stale first-page requests.
- [x] Verify partial/whole root semantics, rollback, snapshots, GUI failure state and bounded SQL work.
- [x] Measure three opens of a million-record synthetic index through the actual engine, separately from fixture creation and reindexing.
- [ ] Run native Windows/Linux gates, publish 0.1.3, and verify the release artifact.

The existing indexing regression gate and safety guarantees remain mandatory. No MFT/USN work or dependency changes are included. Startup measurements use warm caches and synthetic metadata; they do not represent the user's real 8.5-million-entry catalog or native window creation time.
