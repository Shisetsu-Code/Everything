#!/usr/bin/env python3
"""Windows-only diagnostic: one executable, identical seed DB, two WAL thresholds."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
import time
import uuid
from datetime import datetime, timezone


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--files", type=int, default=100_000)
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("Run this diagnostic on native Windows.")
    if not 1_000 <= args.files <= 500_000 or not 1 <= args.timeout <= 86_400:
        parser.error("Use 1000..500000 files and 1..86400 timeout seconds.")
    exe, output = args.exe.resolve(strict=True), args.output.resolve()
    if not exe.is_file() or exe == output:
        parser.error("--exe must be a file and --output must be a different path.")
    output.parent.mkdir(parents=True, exist_ok=True)
    directories = 1 + (args.files + 999) // 1000 + (args.files + 99) // 100
    expected_entries = args.files + directories
    report = {
        "kind": "same_executable_identical_seed_wal_checkpoint_control", "status": "running",
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "environment": {"os": platform.platform(), "python": platform.python_version(),
                        "processor": platform.processor(), "logical_cpus": os.cpu_count()},
        "executable": {"path": str(exe), "sha256": sha256(exe)},
        "parameters": {"files": args.files, "timeout_seconds": args.timeout, "repetitions": 2},
        "expected": {"files": args.files, "directories": directories, "entries": expected_entries},
        "cache_note": "One shared fixture; each rescan uses a fresh, hash-verified seed DB copy. No cache eviction.",
        "commands": [], "samples": [], "cleanup": {"removed": False},
    }
    workspace = None

    def save():
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")

    def run(arguments, threshold, label):
        command = [str(exe), *map(str, arguments)]
        env = os.environ.copy()
        env["EVERYTHING_DIAG_WAL_CHECKPOINT_PAGES"] = str(threshold)
        record = {"label": label, "arguments": command, "requested_pages": threshold}
        report["commands"].append(record)
        save()
        started = time.perf_counter()
        try:
            result = subprocess.run(command, capture_output=True, encoding="utf-8", errors="replace",
                                    timeout=args.timeout, env=env, check=False)
        except subprocess.TimeoutExpired as error:
            record.update(seconds=time.perf_counter() - started, timed_out=True)
            for field in ("stdout", "stderr"):
                value = getattr(error, field) or ""
                record[field] = (value.decode("utf-8", "replace") if isinstance(value, bytes) else value)[-8192:]
            save()
            raise RuntimeError(f"{label} exceeded {args.timeout}s; subprocess.run killed and waited for es.exe") from error
        record.update(seconds=time.perf_counter() - started, returncode=result.returncode,
                      stdout=result.stdout[-8192:], stderr=result.stderr[-8192:])
        save()
        if result.returncode:
            raise RuntimeError(f"{label} exited {result.returncode}; see captured diagnostics")
        return record

    def index(data, threshold, label, target):
        result = run(["--data-dir", data, "--root", root, "index", "--profile"], threshold, label)
        target.update(seconds=result["seconds"], command_label=label)
        lines = [line[len("INDEX_PROFILE_JSON "):] for line in result["stderr"].splitlines()
                 if line.startswith("INDEX_PROFILE_JSON ")]
        if len(lines) != 1:
            raise RuntimeError(f"{label}: expected exactly one INDEX_PROFILE_JSON line")
        profile = json.loads(lines[0])
        target["profile"] = profile
        save()
        expected = {"scanned_entries": expected_entries, "submitted_entries": expected_entries,
                    "access_errors": 0, "cancelled": False, "wal_autocheckpoint_pages": threshold}
        for key, value in expected.items():
            if profile.get(key) != value:
                raise RuntimeError(f"{label}: {key}={profile.get(key)!r}; expected {value!r}")
        detail = profile["database_write_profile"]
        if detail["rows"] != expected_entries or profile["page_size_bytes"] <= 0:
            raise RuntimeError(f"{label}: inconsistent write profile")
        for key in ("begin_seconds", "upsert_seconds", "touch_seconds", "commit_seconds"):
            if not math.isfinite(detail[key]) or detail[key] < 0:
                raise RuntimeError(f"{label}: invalid {key}")
        target["checks"] = []
        for query, count in (("file:", args.files), ("folder:", directories)):
            result = run(["--data-dir", data, "search", query, "--json", "--limit", "1"], threshold, f"{label} {query}")
            actual = json.loads(result["stdout"])["total"]
            target["checks"].append({"query": query, "expected": count, "actual": actual})
            save()
            if actual != count:
                raise RuntimeError(f"{label}: {query} returned {actual}, expected {count}")
        stats = json.loads(run(["--data-dir", data, "stats", "--check"], threshold, f"{label} integrity")["stdout"])
        target["stats"] = stats
        save()
        if stats["entries"] != expected_entries or stats["integrity_checked"] is not True:
            raise RuntimeError(f"{label}: integrity or total-entry verification failed")

    try:
        save()
        report["executable"]["version"] = run(["--version"], 1000, "version")["stdout"].strip()
        workspace = Path(tempfile.mkdtemp(prefix="everything-indexing-" + uuid.uuid4().hex[:24]))
        root, seed = workspace / "files", workspace / "seed"
        report["cleanup"]["workspace"] = str(workspace)
        root.mkdir()
        created = time.perf_counter()
        for number in range(args.files):
            bucket = root / f"group_{number // 1000:04}" / f"bucket_{number % 1000 // 100:02}"
            if number % 100 == 0:
                bucket.mkdir(parents=True)
            stem = "informe_á_日本" if number % 10 == 0 else "document"
            (bucket / f"{stem}_{number:08}.{('pdf', 'rs', 'txt', 'png')[number % 4]}").write_bytes(b"Everything filesystem benchmark.\n")
        report["fixture"] = {"root": str(root), "creation_seconds": time.perf_counter() - created}
        report["seed"] = {}
        index(seed, 1000, "seed initial index", report["seed"])
        seed_db = seed / "index.db"
        connection = sqlite3.connect(seed_db.as_uri() + "?mode=ro", uri=True)
        try:
            generation = connection.execute("SELECT value FROM metadata WHERE key='generation'").fetchone()[0]
        finally:
            connection.close()
        wal = seed / "index.db-wal"
        if generation != 1 or (wal.exists() and wal.stat().st_size != 0):
            raise RuntimeError("Seed must be closed, generation 1, with no nonempty WAL before copying")
        seed_hash = sha256(seed_db)
        report["seed"].update(generation=generation, sha256=seed_hash, database_bytes=seed_db.stat().st_size)
        for repetition, order in enumerate(((1000, 16384), (16384, 1000)), 1):
            for position, threshold in enumerate(order, 1):
                data = workspace / f"repeat_{repetition}_{threshold:05}"
                data.mkdir()
                shutil.copyfile(seed_db, data / "index.db")
                if sha256(data / "index.db") != seed_hash:
                    raise RuntimeError("Seed copy checksum mismatch")
                sample = {"repetition": repetition, "order_position": position, "threshold_pages": threshold,
                          "seed_sha256": seed_hash, "data_directory": str(data), "status": "running"}
                report["samples"].append(sample)
                print(f"Repetition {repetition}: wal_autocheckpoint={threshold}", flush=True)
                index(data, threshold, f"repeat {repetition} pages {threshold}", sample)
                sample["status"] = "passed"
                save()
        medians = {str(n): statistics.median(s["seconds"] for s in report["samples"] if s["threshold_pages"] == n)
                   for n in (1000, 16384)}
        report["medians_seconds"] = medians
        report["speedup_16384_over_1000"] = medians["1000"] / medians["16384"]
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}")
        raise
    finally:
        if workspace is not None and report["status"] == "passed":
            try:
                shutil.rmtree(workspace)
                report["cleanup"]["removed"] = True
            except OSError as error:
                report.update(status="failed", failure=f"Cleanup failed: {error}")
        report["completed_utc"] = datetime.now(timezone.utc).isoformat()
        save()
    if report["status"] != "passed":
        raise RuntimeError(report["failure"])
    print(f"Saved verified control to {output}")


if __name__ == "__main__":
    main()
