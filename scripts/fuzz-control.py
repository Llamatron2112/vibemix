#!/usr/bin/env python3
"""Fuzzes a running engine's control socket with type-correct but hostile
messages (NaN/infinite floats, huge and negative ids, giant strings), then checks
the engine is still alive and answering.

This covers the message *handlers* (the decoder itself is fuzzed in-process by
`just fuzz`). Run against the sanitizer build (`just asan`) to catch memory
errors, not only panics.

Usage: fuzz-control.py <engine-socket> [messages] [seed]
"""
import importlib.util
import os
import random
import struct
import sys
import time

spec = importlib.util.spec_from_file_location(
    "drive", os.path.join(os.path.dirname(os.path.abspath(__file__)), "rt-guard-drive.py"))
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)

# Argument types per address (i = int, f = float, s = string), from the protocol.
SHAPES = {
    "strip/gain": "if", "strip/mute": "ii", "strip/solo": "ii", "strip/pan": "if",
    "strip/input_gain": "if", "strip/name": "is", "strip/add": "", "strip/remove": "i",
    "strip/move": "ii", "strip/duplicate": "i", "bus/gain": "if", "bus/mute": "ii",
    "bus/name": "is", "bus/move": "ii", "bus/mix_master": "ii", "bus/return": "ii",
    "bus/send_gain": "if", "bus/return_gain": "if", "bus/add": "", "bus/remove": "i",
    "bus/duplicate": "i", "send/gain": "iif", "send/delay": "iif", "send/pre_fader": "iii",
    "plugin/remove": "iii", "plugin/move": "iiii", "plugin/param": "iiiif",
    "plugin/bypass": "iiii", "plugin/sidechain/enabled": "iiii",
    "plugin/sidechain/send": "iiiif", "plugin/sidechain/pre_fader": "iiiii",
    "plugin/ui": "iii", "plugin/ui/close": "iii", "plugin/copy": "iii", "plugin/cut": "iii",
    "plugin/paste": "ii", "edit/undo": "", "edit/redo": "", "state/query": "",
    "session/new": "", "clock/beats_per_bar": "f", "midi/learn_cancel": "",
    "midi/bindings_clear": "", "midi/sync": "i", "midi/key_bind": "iiiiis",
    "midi/key_trigger": "s", "strip/midi_channel": "ii", "bus/midi_channel": "ii",
    "plugin/midi_channel": "iiii", "ports/query": "",
}
INTS = [-2**31, -1, 0, 0, 0, 1, 1, 1, 2, 3, 5, 31, 32, 63, 64, 65, 100, 1000, 2**31 - 1]
FLOATS = [float("nan"), float("inf"), float("-inf"), 0.0, 1.0, -5.0, 1e30, 0.5, -0.0, 2.0, 4.0]
STRINGS = ["", "a", "x" * 3000, "\u00e9\u00e8", "../x", "C-1", "key", "ctrl+k"]


def value(kind: str, rng: random.Random):
    return rng.choice(INTS) if kind == "i" else rng.choice(FLOATS) if kind == "f" else rng.choice(STRINGS)


def main() -> int:
    socket_path = sys.argv[1]
    messages = int(sys.argv[2]) if len(sys.argv) > 2 else 20000
    rng = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 1)
    client = drive.Client(socket_path)
    keys = list(SHAPES)
    # Adding and duplicating are rare, so the session does not just fill up.
    weights = [0.2 if k in ("strip/add", "bus/add", "strip/duplicate", "bus/duplicate",
                            "session/new") else 1 for k in keys]
    for _ in range(messages):
        key = rng.choices(keys, weights)[0]
        shape = SHAPES[key]
        client.send("/vibemix/" + key, *[value(kind, rng) for kind in shape])
        time.sleep(0.002)

    # Still alive and answering?
    client.send("/vibemix/state/query")
    client.sock.settimeout(10)
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            data = client.sock.recv(65536)
        except OSError:
            break
        if data.startswith((b"/vibemix/state", b"/vibemix/chunk")):
            print(f"fuzz-control: {messages} messages sent; the engine is still answering")
            client.close()
            return 0
    print("fuzz-control: the engine did NOT answer after the fuzzing")
    client.close()
    return 1


if __name__ == "__main__":
    sys.exit(main())
