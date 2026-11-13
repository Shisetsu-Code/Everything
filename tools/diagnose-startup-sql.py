#!/usr/bin/env python3
"""Diagnostic only: exact schema-1 SQLite rows, not filesystem enumeration."""

import argparse
import json
from pathlib import Path
import sqlite3
import statistics
import tempfile
import time


QUERIES = {
    "old_missing_subtree": (
        "DELETE FROM files WHERE source='' AND (path_key=?1 OR (path_key>=?2 AND path_key<?3))",
        (r"c:\users\nahuel\not-indexed", "c:\\users\\nahuel\\not-indexed\\", "c:\\users\\nahuel\\not-indexed]"),
    ),
    "bounded_missing_subtree": (
        "DELETE FROM files WHERE id IN (SELECT id FROM files WHERE source='' AND path_key=?1 UNION ALL SELECT id FROM files WHERE source='' AND path_key>=?2 AND path_key<?3 AND path_key<>?1)",
        (r"c:\users\nahuel\not-indexed", "c:\\users\\nahuel\\not-indexed\\", "c:\\users\\nahuel\\not-indexed]"),
    ),
    "old_distinct_roots": ("SELECT DISTINCT root FROM files WHERE source=''", ()),
    "forced_distinct_roots": ("SELECT DISTINCT root FROM files INDEXED BY files_root WHERE source=''", ()),
    "first_root": ("SELECT root FROM files INDEXED BY files_root WHERE source='' ORDER BY root LIMIT 1", ()),
    "next_root": ("SELECT root FROM files INDEXED BY files_root WHERE source='' AND root>?1 ORDER BY root LIMIT 1", ("C:\\",)),
    "delete_absent_root": ("DELETE FROM files WHERE source='' AND root=?1", ("not-an-indexed-root",)),
}
ROOTS = ["C:\\", "D:\\", "C:\\Users\\Nahuel\\Documents\\álbum_日本", "\\\\server\\archive"]


def entry(index):
    owner = ROOTS[index % len(ROOTS)]
    parent = owner.rstrip("\\") + f"\\projects\\group_{index // 1000:05}\\bucket_{index // 100 % 10:02}"
    name = f"document_álbum_日本_{index:08}.txt"
    path = parent + "\\" + name
    source = "archive.efu" if index % 20 == 0 else ""
    return (path, name, parent, "txt", name.lower(), path.lower(), parent.lower(), 1234,
            1700000000, 1700000000, 0, 0, source, owner, 1, path if source else path.lower())


def add_rows(conn, start, stop):
    prefix = "INSERT INTO files(path,name,parent,extension,name_fold,path_fold,parent_fold,size,modified,created,is_dir,is_symlink,source,root,generation,path_key) VALUES "
    for batch in range(start, stop, 8192):
        conn.execute("BEGIN")
        end = min(batch + 8192, stop)
        for offset in range(batch, end, 256):
            rows = [entry(i) for i in range(offset, min(offset + 256, end))]
            sql = prefix + ",".join(["(" + ",".join(["?"] * 16) + ")"] * len(rows))
            conn.execute(sql, [value for row in rows for value in row])
        conn.execute("COMMIT")


def seek_roots(conn):
    found = []
    row = conn.execute(QUERIES["first_root"][0]).fetchone()
    while row is not None:
        root = row[0]
        found.append(root)
        row = conn.execute(QUERIES["next_root"][0], (root,)).fetchone()
    return sorted(found)


def operation(conn, name):
    if name == "seek_roots":
        return seek_roots(conn)
    sql, parameters = QUERIES[name]
    cursor = conn.execute(sql, parameters)
    return sorted(row[0] for row in cursor.fetchall()) if sql.startswith("SELECT") else cursor.rowcount


def measure(conn, name, expected):
    assert operation(conn, name) == expected
    durations = []
    for _ in range(3):
        started = time.perf_counter()
        actual = operation(conn, name)
        durations.append(time.perf_counter() - started)
        assert actual == expected
    steps = 0

    def progress():
        nonlocal steps
        steps += 1
        return 0

    conn.set_progress_handler(progress, 1)
    try:
        actual = operation(conn, name)
    finally:
        conn.set_progress_handler(None, 0)
    assert actual == expected
    return {"query": name, "seconds_median": statistics.median(durations),
            "seconds_samples": durations, "vm_steps": steps, "result": actual}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source = (args.repo / "src/database.rs").read_text(encoding="utf-8")
    schema = source.split('const SCHEMA: &str = "', 1)[1].split('";', 1)[0]
    report = {"sqlite_version": sqlite3.sqlite_version,
              "kind": "startup_sql_schema_1_diagnostic",
              "methodology": "Full existing schema including live FTS triggers and all indexes; Windows-shaped strings; 5% offline records; no real filesystem enumeration. Wall times are Linux diagnostic samples without progress callbacks. VM steps are measured separately with callback interval 1. No ANALYZE/statistics are installed.",
              "schema_version": 1, "pragmas": "WAL; synchronous=NORMAL; cache_size=-32768; temp_store=FILE; foreign_keys=ON",
              "plans": {}, "sizes": []}
    with tempfile.TemporaryDirectory(prefix="everything-startup-sql-") as directory:
        conn = sqlite3.connect(str(Path(directory) / "index.db"), isolation_level=None)
        conn.executescript("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-32768; PRAGMA temp_store=FILE; PRAGMA foreign_keys=ON;")
        conn.executescript(schema)
        for name, (sql, parameters) in QUERIES.items():
            report["plans"][name] = {"sql": sql, "parameters": parameters,
                                     "plan": [list(row) for row in conn.execute("EXPLAIN QUERY PLAN " + sql, parameters)]}
        start = 0
        for count in [10000, 100000]:
            add_rows(conn, start, count)
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
            known = sorted(ROOTS)
            measurements = []
            for name in ["old_missing_subtree", "bounded_missing_subtree", "old_distinct_roots", "forced_distinct_roots", "seek_roots", "delete_absent_root"]:
                expected = known if "roots" in name else 0
                measurements.append(measure(conn, name, expected))
            assert conn.execute("SELECT value FROM metadata WHERE key='total'").fetchone()[0] == count
            conn.execute("INSERT INTO file_fts(file_fts,rank) VALUES('integrity-check',1)")
            result = {"rows": count, "local_rows": count - (count + 19) // 20, "distinct_local_roots": len(known),
                      "database_bytes": (Path(directory) / "index.db").stat().st_size,
                      "measurements": measurements, "count_and_fts_integrity": "passed"}
            report["sizes"].append(result)
            print(json.dumps(result, ensure_ascii=False), flush=True)
            start = count
        conn.close()
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
