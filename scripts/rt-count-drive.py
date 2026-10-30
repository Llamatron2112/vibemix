#!/usr/bin/env python3
"""Adds the first installed LV2 (preferring one that accepts MIDI), VST3 and CLAP
plugin to strip 0, arms nothing else, and lets the engine run while a MIDI clock
is playing into it, so `just rt-count` can measure steady-state audio-thread
allocations (the OSC helpers are shared with rt-guard-drive.py).

Usage: rt-count-drive.py <engine-socket> <config-dir> <seconds>
"""
import importlib.util
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("drive", os.path.join(here, "rt-guard-drive.py"))
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)


def main() -> None:
    engine_socket, config_dir, seconds = sys.argv[1], sys.argv[2], float(sys.argv[3])
    client = drive.Client(engine_socket)

    def send(address, *args, wait=0.3):
        client.send(address, *args)
        time.sleep(wait)

    send("/vibemix/state/query", wait=0.5)
    uris = drive.first_uris(config_dir, prefer_midi=True)
    for fmt in ("lv2", "vst3", "clap"):
        if fmt in uris:
            send("/vibemix/plugin/add", 0, 0, uris[fmt], wait=3.0)
    print(f"rt-count-drive: measuring {seconds:.0f}s with {', '.join(sorted(uris))}")
    time.sleep(seconds)


if __name__ == "__main__":
    main()
