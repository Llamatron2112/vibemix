#!/usr/bin/env python3
"""Runs the engine's isolated smoke test for every installed plugin using a
sanitizer-instrumented engine (see `just asan`), and reports memory errors.

Each plugin is loaded, run for a few blocks and torn down in its own process
(`vibemix-engine --smoke*`). AddressSanitizer or UBSan reports are classified:

  ours      a stack frame in the vibemix shims or host crates: a bug to fix
  plugin    only the plugin's own frames: not ours (the plugin is buggy)

Usage: asan-smoke.py <sanitizer-engine-binary> [--jobs N] [--timeout SECONDS]
Exit status 1 if any report implicates vibemix code.
"""
import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import sys

OURS = re.compile(r"(vibemix[-_](vst3|clap|lv2|engine|core|proto)|/shim\.cpp|/ui_host\.c|crates/vibemix)")
REPORT = re.compile(r"(ERROR: AddressSanitizer|runtime error:|SUMMARY: (Address|Undefined)Sanitizer)")


def sanitizer_env() -> dict:
    """The environment for running the instrumented engine. The ASan runtime has
    to be loaded *first*, or its allocator is not used and heap errors go
    unnoticed (it is preloaded rather than linked for exactly that reason)."""
    runtime = subprocess.run(["gcc", "-print-file-name=libasan.so"], capture_output=True,
                             text=True).stdout.strip()
    if not os.path.isabs(runtime):
        raise SystemExit("libasan.so not found; install gcc's sanitizer runtime")
    return dict(os.environ, LD_PRELOAD=runtime,
                ASAN_OPTIONS="detect_leaks=0:abort_on_error=0",  # plugins leak by design
                UBSAN_OPTIONS="print_stacktrace=1")


def command(binary: str, entry: dict):
    fmt = entry.get("format", "lv2")
    if fmt == "lv2":
        return [binary, "--smoke", entry["id"]]
    flag = "--smoke-vst3" if fmt == "vst3" else "--smoke-clap"
    bundle = entry.get("bundle")
    return [binary, flag, bundle, entry["id"]] if bundle else None


def run(binary: str, entry: dict, timeout: float):
    args = command(binary, entry)
    if args is None:
        return entry, "skipped", ""
    env = sanitizer_env()
    try:
        done = subprocess.run(args, capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        return entry, "timeout", ""
    text = done.stderr
    if REPORT.search(text):
        return entry, "ours" if OURS.search(text) else "plugin", text
    return entry, ("ok" if done.returncode == 0 else f"exit {done.returncode}"), ""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--timeout", type=float, default=40.0)
    parser.add_argument("--catalog", default=os.path.expanduser("~/.config/vibemix/catalog.json"))
    args = parser.parse_args()

    entries = json.load(open(args.catalog))
    counts, findings = {}, []
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        futures = [pool.submit(run, args.binary, entry, args.timeout) for entry in entries]
        for number, future in enumerate(concurrent.futures.as_completed(futures), 1):
            entry, status, text = future.result()
            key = (entry.get("format", "lv2"), "exit" if status.startswith("exit") else status)
            counts[key] = counts.get(key, 0) + 1
            if status in ("ours", "plugin"):
                findings.append((status, entry, text))
            if number % 50 == 0:
                print(f"  {number}/{len(entries)} plugins", flush=True)

    print("\nresults (format, outcome): count")
    for (fmt, status), count in sorted(counts.items()):
        print(f"  {fmt:5s} {status:8s} {count}")
    ours = [item for item in findings if item[0] == "ours"]
    for status, entry, text in findings:
        print(f"\n--- {status}: {entry.get('name')} ({entry.get('format')})")
        print("\n".join(text.strip().splitlines()[:18]))
    print(f"\n{len(ours)} report(s) implicate vibemix code, "
          f"{len(findings) - len(ours)} only the plugins' own code")
    return 1 if ours else 0


if __name__ == "__main__":
    sys.exit(main())
