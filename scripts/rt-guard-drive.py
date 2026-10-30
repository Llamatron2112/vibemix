#!/usr/bin/env python3
"""Drives a running engine over OSC so `just rt-guard` exercises the audio
thread beyond idling: strip/bus add and remove, fader and mute edits, and (when
an LV2 plugin is installed) plugin add/remove on a recycled strip slot.

Usage: rt-guard-drive.py <engine-socket> <config-dir>

Also provides the shared helpers of the other test scripts: OSC `message()`
encoding and `Client`, a Unix-datagram client for the engine's control socket.
"""
import json
import os
import shutil
import socket
import struct
import sys
import tempfile
import time


class Client:
    """Talks to an engine's control socket. A datagram client must bind its own
    socket to receive replies, so it gets one in a private temporary directory."""

    def __init__(self, engine_socket: str):
        self.engine = engine_socket
        self.dir = tempfile.mkdtemp(prefix="vibemix-client-")  # mode 0700
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.sock.bind(os.path.join(self.dir, "client.sock"))
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)

    def send(self, address, *args):
        self.send_raw(message(address, *args))

    def send_raw(self, data: bytes):
        try:
            self.sock.sendto(data, self.engine)
        except OSError:
            pass  # engine not (yet) listening, or its queue is full: like UDP

    def close(self):
        self.sock.close()
        shutil.rmtree(self.dir, ignore_errors=True)


def pad(data: bytes) -> bytes:
    return data + b"\0" * (4 - len(data) % 4)


def message(address: str, *args) -> bytes:
    tags = ","
    payload = b""
    for arg in args:
        if isinstance(arg, bool):
            tags += "i"
            payload += struct.pack(">i", int(arg))
        elif isinstance(arg, int):
            tags += "i"
            payload += struct.pack(">i", arg)
        elif isinstance(arg, float):
            tags += "f"
            payload += struct.pack(">f", arg)
        else:
            tags += "s"
            payload += pad(str(arg).encode())
    return pad(address.encode()) + pad(tags.encode()) + payload


def first_uris(config_dir: str, prefer_midi: bool = False):
    """The first installed plugin of each format: {"lv2": id, "vst3": id, ...}.
    With `prefer_midi`, a plugin that accepts MIDI is chosen when one exists."""
    try:
        with open(f"{config_dir}/catalog.json") as handle:
            entries = json.load(handle)
    except (OSError, ValueError):
        return {}
    found = {}
    for entry in entries:
        found.setdefault(entry.get("format", "lv2").lower(), entry.get("id"))
    if prefer_midi:
        for entry in entries:
            fmt = entry.get("format", "lv2").lower()
            if entry.get("midi"):
                found[fmt] = entry.get("id")
                break
    return found


def main() -> None:
    engine_socket, config_dir = sys.argv[1], sys.argv[2]
    client = Client(engine_socket)

    def send(address, *args, wait=0.15):
        client.send(address, *args)
        time.sleep(wait)

    send("/vibemix/state/query", wait=0.5)
    uris = first_uris(config_dir)
    uri = uris.get("lv2")

    for _ in range(3):
        send("/vibemix/strip/add")
        send("/vibemix/strip/gain", 1, 0.3)
        send("/vibemix/strip/mute", 1, 1)
        send("/vibemix/strip/remove", 1)
        # Re-adds the freed slot: this is where a cleared plugin chain used to
        # lose its capacity and allocate on the audio thread.
        send("/vibemix/strip/add")
        if uri:
            send("/vibemix/plugin/add", 0, 1, uri, wait=1.0)
            send("/vibemix/plugin/add", 1, 1, uri, wait=1.0)
            send("/vibemix/plugin/remove", 0, 1, 0)
        send("/vibemix/bus/add")
        send("/vibemix/bus/remove", 1)
        send("/vibemix/bus/add")
        if uri:
            send("/vibemix/plugin/add", 2, 1, uri, wait=1.0)
        # The pre-fader chain is empty here (the LV2 plugin above was removed),
        # so each added plugin lands in the next slot.
        slot = 0
        for fmt in ("vst3", "clap"):
            if fmt in uris:
                send("/vibemix/plugin/add", 0, 1, uris[fmt], wait=3.0)
                send("/vibemix/plugin/bypass", 0, 1, slot, 1)
                send("/vibemix/plugin/bypass", 0, 1, slot, 0)
                slot += 1
        send("/vibemix/strip/remove", 1)
        send("/vibemix/bus/remove", 1)
    send("/vibemix/edit/undo")
    send("/vibemix/edit/redo")
    print(f"rt-guard-drive: done (formats: {', '.join(sorted(uris)) or 'none installed'})")


if __name__ == "__main__":
    main()
