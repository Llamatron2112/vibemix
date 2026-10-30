#!/usr/bin/env python3
"""Measures how much CPU the GUI uses at different session sizes, headless.

Starts the release engine (no auto-connect, so nothing is audible), a tone
client linked to many strips so meters move, and `vibemix-gui` on Qt's
`offscreen` platform. The GUI talks to the engine through a small UDP proxy
owned by this script, which lets it (a) inject fader moves that arrive at the
engine looking like they came from the GUI, so the engine answers the GUI with
state snapshots exactly as during a real drag, and (b) count the traffic.

For each session size it reports per-thread CPU (percent of one core) for the
GUI process while the meters run and while a fader is dragged. Offscreen mode
has no GPU, so the render-thread figure is not representative of real
hardware; the main-thread figure (JSON parsing, binding re-evaluation) is.

Usage: gui-load.py [--budget PERCENT] [--seconds N]
"""
import argparse
import collections
import importlib.util
import math
import os
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CLK_TCK = os.sysconf("SC_CLK_TCK")

spec = importlib.util.spec_from_file_location("drive", os.path.join(HERE, "rt-guard-drive.py"))
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)

AUDIOGAIN = "http://kxstudio.sf.net/carla/plugins/audiogain_s"
# Private JACK client name, so the test engine and its tone can never be linked
# into (or renamed by) a real vibemix session that is running at the same time.
CLIENT = "vibemix-guiload"


class Proxy(threading.Thread):
    """GUI <-> engine control-socket relay that can also inject messages as the GUI."""

    def __init__(self, gui_socket: str, engine_socket: str):
        super().__init__(daemon=True)
        self.engine = engine_socket
        self.gui_sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.gui_sock.bind(gui_socket)  # the GUI is pointed here instead of the engine
        self.up = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.up.bind(gui_socket + ".up")
        self.gui_addr = None
        self.engine_up = False
        self.drop = set()  # engine->GUI address prefixes to discard
        self.running = True
        self.lock = threading.Lock()
        self.stats = collections.defaultdict(lambda: [0, 0])  # address -> [count, bytes]

    @staticmethod
    def address_of(data: bytes) -> str:
        end = data.find(b"\0")
        return data[:end].decode("ascii", "replace") if end > 0 else "?"

    def run(self):
        while self.running:
            ready, _, _ = select.select([self.gui_sock, self.up], [], [], 0.2)
            for sock in ready:
                data, source = sock.recvfrom(65536)
                if sock is self.gui_sock:
                    self.gui_addr = source
                    if self.engine_up:
                        self.forward(self.up, data, self.engine)
                elif self.gui_addr:
                    name = self.address_of(data)
                    if any(name.startswith(prefix) for prefix in self.drop):
                        continue
                    with self.lock:
                        entry = self.stats[name]
                        entry[0] += 1
                        entry[1] += len(data)
                    self.forward(self.gui_sock, data, self.gui_addr)

    @staticmethod
    def forward(sock, data, destination):
        try:
            sock.sendto(data, destination)
        except OSError:
            pass  # peer gone or its queue is full: drop, as UDP would

    def send(self, address, *args):
        self.forward(self.up, drive.message(address, *args), self.engine)

    def snapshot(self):
        with self.lock:
            return {k: list(v) for k, v in self.stats.items()}


def thread_cpu(pid: int):
    """{tid: (name, cpu ticks)} for every thread of `pid`."""
    out = {}
    for tid in os.listdir(f"/proc/{pid}/task"):
        try:
            with open(f"/proc/{pid}/task/{tid}/stat") as handle:
                text = handle.read()
        except OSError:
            continue
        name = text[text.index("(") + 1 : text.rindex(")")]
        fields = text[text.rindex(")") + 2 :].split()
        out[int(tid)] = (name, int(fields[11]) + int(fields[12]))
    return out


def measure(pid: int, seconds: float, during=None):
    before = thread_cpu(pid)
    start = time.time()
    if during:
        during(seconds)
    else:
        time.sleep(seconds)
    elapsed = time.time() - start
    after = thread_cpu(pid)
    # The main thread is the one whose id is the pid; threads the Rust side
    # spawns inherit the process name, so label them by id to keep them apart.
    per_name = collections.defaultdict(float)
    for tid, (name, ticks) in after.items():
        base = before.get(tid, (name, 0))[1]
        label = "main" if tid == pid else (f"{name}#{tid}" if name == "vibemix-gui" else name)
        per_name[label] += (ticks - base) / CLK_TCK / elapsed * 100
    return dict(per_name), elapsed


def busy_seconds(pid: int, quiet_percent: float = 60.0, timeout: float = 240.0):
    """Waits until the GUI's busiest thread stays under `quiet_percent` for a
    second; returns how long that took (the time the GUI was tied up). Rebuilding
    the cards keeps a thread near 100%; the steady-state cost stays well below."""
    started = time.time()
    previous = thread_cpu(pid)
    quiet_since = None
    while time.time() - started < timeout:
        time.sleep(0.25)
        now = thread_cpu(pid)
        busiest = max(
            ((ticks - previous.get(tid, (name, 0))[1]) / CLK_TCK / 0.25 * 100
             for tid, (name, ticks) in now.items()),
            default=0.0,
        )
        previous = now
        if busiest < quiet_percent:
            quiet_since = quiet_since or time.time()
            if time.time() - quiet_since >= 1.0:
                return time.time() - started - 1.0
        else:
            quiet_since = None
    return timeout


def drag(proxy: Proxy, seconds: float):
    """Sweeps strip 0's fader at about 120 messages per second."""
    end = time.time() + seconds
    step = 0
    while time.time() < end:
        value = 0.5 + 0.4 * math.sin(step / 20.0)
        proxy.send("/vibemix/strip/gain", 0, float(value))
        step += 1
        time.sleep(1 / 120)


def link_tone(strips: int):
    """Fans the tone client out to the first `strips` strips (best effort)."""
    for strip in range(2, strips + 1):
        for side in ("l", "r"):
            subprocess.run(
                ["pw-link", f"vibemix-tone:out_{side}", f"{CLIENT}:ch_{strip}_in_{side}"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=float, default=15.0)
    parser.add_argument("--budget", type=float, default=None,
                        help="fail if the GUI main thread exceeds this percent of a core "
                             "at the largest size while meters run")
    parser.add_argument("--quick", action="store_true",
                        help="skip the 64-strip size (its structural edits take minutes)")
    args = parser.parse_args()

    engine_bin = os.path.join(ROOT, "target/release/vibemix-engine")
    gui_bin = os.path.join(ROOT, "target/release/vibemix-gui")
    tone_bin = os.path.join(ROOT, "target/release/examples/tone")
    for binary in (engine_bin, gui_bin, tone_bin):
        if not os.path.exists(binary):
            print(f"missing {binary}; run `cargo build --release --workspace --bins --examples`")
            return 2

    config = tempfile.mkdtemp(prefix="vibemix-guiload-")  # mode 0700: holds the sockets
    engine_socket = os.path.join(config, "engine.sock")
    gui_socket = os.path.join(config, "gui-proxy.sock")
    procs = []
    proxy = Proxy(gui_socket, engine_socket)
    proxy.start()
    results = []
    edits = []
    failed = False
    try:
        env = dict(os.environ, QT_QPA_PLATFORM="offscreen", VIBEMIX_SOCKET=gui_socket,
                   VIBEMIX_CONFIG_DIR=config)
        gui = subprocess.Popen([gui_bin], env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
        procs.append(gui)
        time.sleep(4)
        if gui.poll() is not None:
            print("the GUI exited at startup (offscreen platform unavailable?)")
            return 2

        cpu, _ = measure(gui.pid, 10)
        results.append(("no engine", "idle", cpu, {}))

        engine = subprocess.Popen(
            ["pw-jack", engine_bin, "--socket", engine_socket, "--auto-connect", "false",
             "--client-name", CLIENT],
            env=dict(os.environ, VIBEMIX_CONFIG_DIR=config),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(engine)
        time.sleep(6)
        proxy.engine_up = True
        tone = subprocess.Popen(["pw-jack", tone_bin, CLIENT], stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL)
        procs.append(tone)
        time.sleep(2)

        strips, buses, plugin_strips = 1, 1, 0
        sizes = ((1, 1, 0), (16, 4, 8)) if args.quick else ((1, 1, 0), (16, 4, 8), (64, 32, 16))
        for want_strips, want_buses, want_plugins in sizes:
            for _ in range(want_strips - strips):
                proxy.send("/vibemix/strip/add")
                time.sleep(0.03)
            for _ in range(want_buses - buses):
                proxy.send("/vibemix/bus/add")
                time.sleep(0.03)
            for strip in range(plugin_strips, want_plugins):
                proxy.send("/vibemix/plugin/add", 0, strip, AUDIOGAIN)
                time.sleep(0.6)
            strips, buses, plugin_strips = want_strips, want_buses, want_plugins
            link_tone(min(strips, 32))
            settle = busy_seconds(gui.pid)
            label = f"{strips} strips, {buses} buses, {plugin_strips} plugins"

            # Same session with no meter/clock traffic: what the cards cost on
            # their own, so the meter cost is the difference.
            proxy.drop = {"/vibemix/meters", "/vibemix/clock"}
            cpu, elapsed = measure(gui.pid, args.seconds)
            results.append((label, "no meters", cpu, {}))
            proxy.drop = set()

            before = proxy.snapshot()
            cpu, elapsed = measure(gui.pid, args.seconds)
            after = proxy.snapshot()
            results.append((label, "meters", cpu, delta(before, after, elapsed)))

            before = proxy.snapshot()
            cpu, elapsed = measure(gui.pid, args.seconds, lambda s: drag(proxy, s))
            after = proxy.snapshot()
            results.append((label, "fader drag", cpu, delta(before, after, elapsed)))

            # How long one structural edit ties up the GUI: remove the last
            # strip, then add one back.
            proxy.send("/vibemix/strip/remove", strips - 1)
            removed = busy_seconds(gui.pid)
            proxy.send("/vibemix/strip/add")
            added = busy_seconds(gui.pid)
            edits.append((label, settle, removed, added))
    finally:
        proxy.running = False
        for proc in reversed(procs):
            proc.terminate()
        for proc in procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        shutil.rmtree(config, ignore_errors=True)

    print(f"\nGUI CPU (percent of one core; offscreen, so render-thread time is not "
          f"representative)\n")
    for label, phase, cpu, traffic in results:
        total = sum(cpu.values())
        top = [item for item in sorted(cpu.items(), key=lambda item: -item[1]) if item[1] >= 0.5][:4]
        threads = ", ".join(f"{name} {value:.1f}" for name, value in top)
        print(f"{label:38s} {phase:10s} total {total:5.1f}   [{threads}]")
        if traffic:
            busiest = sorted(traffic.items(), key=lambda item: -item[1][1])[:3]
            parts = ", ".join(f"{name.replace('/vibemix', '')} {c:.0f}/s {b / 1024:.0f} KB/s"
                              for name, (c, b) in busiest)
            print(f"{'':38s} {'':10s} engine -> GUI: {parts}")
    print("\nGUI busy time after structural changes (seconds the GUI was tied up)\n")
    for label, settle, removed, added in edits:
        print(f"{label:38s} building the session {settle:6.1f}s   remove a strip {removed:5.1f}s"
              f"   add a strip {added:5.1f}s")
    if args.budget is not None:
        main_cpu = next(cpu for label, phase, cpu, _ in reversed(results) if phase == "meters")
        main_thread = max(main_cpu.values()) if main_cpu else 0.0
        if main_thread > args.budget:
            print(f"\nmain thread {main_thread:.1f}% exceeds the budget of {args.budget}%")
            failed = True
    return 1 if failed else 0


def delta(before, after, elapsed):
    out = {}
    for name, (count, size) in after.items():
        base = before.get(name, [0, 0])
        out[name] = ((count - base[0]) / elapsed, (size - base[1]) / elapsed)
    return out


if __name__ == "__main__":
    sys.exit(main())
