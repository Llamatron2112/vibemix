#!/usr/bin/env python3
"""Desktop-behaviour checks for the GUI and plugin editors, on a private
headless compositor (`kwin_wayland --virtual` with its own Xwayland and window
manager), so nothing appears on the real screen and no real session is touched.

  shutdown   open a LV2 (out-of-process host), a CLAP and a VST3 editor, then
             stop the engine with SIGTERM like the launcher does: it must exit
             quickly and take every editor window with it (it used to hang and
             leave unresponsive windows behind)
  autofit    with auto-fit on, adding and removing a strip must resize the
             window once, straight to the final size (it used to flash an
             intermediate geometry)

Needs kwin_wayland, python-xlib, pw-jack and the release binaries.
Usage: gui-desktop-check.py [shutdown] [autofit]   (default: both)

With VIBEMIX_ASAN=1 the engine is the AddressSanitizer build from `just asan`
(target/asan/...), and any sanitizer report in its log fails the check.
"""
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from Xlib import X, display

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
SOCKET = "wl-vibemix-check"
CLIENT = "vibemix-desktop"
LV2_EDITOR = "https://github.com/jatinchowdhury18/AnalogTapeModel"
CLAP_EDITOR = "com.MusicalEntropy.SpaceshipDelayx1"
VST3_EDITOR = "56535455634631756862696b2d660000"
AUDIOGAIN = "http://kxstudio.sf.net/carla/plugins/audiogain_s"

ASAN = os.environ.get("VIBEMIX_ASAN") == "1"
ENGINE_BIN = os.path.join(
    ROOT, "target/asan/x86_64-unknown-linux-gnu/debug/vibemix-engine" if ASAN
    else "target/release/vibemix-engine")
REPORT = ("ERROR: AddressSanitizer", "runtime error:")

spec = importlib.util.spec_from_file_location("drive", os.path.join(HERE, "rt-guard-drive.py"))
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)


class Compositor:
    """A throwaway KWin on a virtual output, with its own X display."""

    def __init__(self):
        self.tmp = tempfile.mkdtemp(prefix="vibemix-kwin-")
        self.marker = os.path.join(self.tmp, "display")
        self.proc = subprocess.Popen(
            ["kwin_wayland", "--virtual", "--width", "2560", "--height", "1440", "--xwayland",
             "--socket", SOCKET, "--no-lockscreen", "--exit-with-session",
             f"bash -c 'echo $DISPLAY > {self.marker}; sleep 3000'"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        for _ in range(60):
            if os.path.exists(self.marker) and open(self.marker).read().strip():
                break
            time.sleep(0.25)
        else:
            raise SystemExit("the private compositor did not start")
        self.display = open(self.marker).read().strip()
        time.sleep(1)

    def close(self):
        # Only the process we started: the real desktop's kwin has the same name.
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        shutil.rmtree(self.tmp, ignore_errors=True)


class Session:
    """A throwaway engine (+ optional GUI) on the private compositor."""

    def __init__(self, compositor, gui=False, platform="xcb"):
        self.display = compositor.display
        self.config = tempfile.mkdtemp(prefix="vibemix-desktop-")
        for name in ("catalog.json", "plugin_health.json"):
            source = os.path.expanduser(f"~/.config/vibemix/{name}")
            if os.path.exists(source):
                shutil.copy(source, self.config)
        self.env = dict(os.environ, DISPLAY=self.display, WAYLAND_DISPLAY=SOCKET,
                        QT_QPA_PLATFORM=platform,
                        VIBEMIX_SOCKET=os.path.join(self.config, "engine.sock"),
                        VIBEMIX_CONFIG_DIR=self.config)
        self.procs = []
        self.log_path = os.path.join(self.config, "engine.log")
        prefix = []
        if ASAN:
            # Preload the runtime for the engine only (it must come first for
            # its allocator to be used); plugins leak by design.
            runtime = subprocess.run(["gcc", "-print-file-name=libasan.so"], capture_output=True,
                                     text=True).stdout.strip()
            prefix = ["env", f"LD_PRELOAD={runtime}", "ASAN_OPTIONS=detect_leaks=0",
                      "UBSAN_OPTIONS=print_stacktrace=1"]
        with open(self.log_path, "wb") as log:
            self.engine = subprocess.Popen(
                ["pw-jack", *prefix, ENGINE_BIN,
                 "--socket", self.env["VIBEMIX_SOCKET"], "--auto-connect", "false",
                 "--client-name", CLIENT],
                env=self.env, stdout=log, stderr=subprocess.STDOUT)
        self.procs.append(self.engine)
        for _ in range(240):  # the sanitizer build starts slowly
            if b"control ready" in open(self.log_path, "rb").read():
                break
            time.sleep(0.25)
        time.sleep(1)
        self.client = drive.Client(self.env["VIBEMIX_SOCKET"])
        self.gui = None
        self.want_gui = gui

    def send(self, address, *args, wait=0.4):
        self.client.send(address, *args)
        time.sleep(wait)

    def start_gui(self):
        self.gui = subprocess.Popen([os.path.join(ROOT, "target/release/vibemix-gui")],
                                    env=self.env, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL)
        self.procs.append(self.gui)

    def sanitizer_reports(self) -> int:
        text = open(self.log_path, "rb").read().decode("utf8", "replace")
        return sum(text.count(marker) for marker in REPORT)

    def close(self):
        self.client.close()
        for proc in reversed(self.procs):
            if proc.poll() is None:
                proc.terminate()
        for proc in self.procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        shutil.rmtree(self.config, ignore_errors=True)


def walk(window):
    try:
        for child in window.query_tree().children:
            yield child
            yield from walk(child)
    except Exception:
        return


def editor_windows(dpy):
    found = []
    for window in walk(dpy.screen().root):
        try:
            attrs, geometry = window.get_attributes(), window.get_geometry()
            name = window.get_wm_name()
            if attrs.map_state == X.IsViewable and name and geometry.width > 50:
                found.append(name if isinstance(name, str) else name.decode("utf8", "replace"))
        except Exception:
            continue
    return found


def check_shutdown(compositor) -> bool:
    session = Session(compositor)
    try:
        session.send("/vibemix/state/query", wait=1)
        session.send("/vibemix/plugin/add", 0, 0, LV2_EDITOR, wait=3)
        session.send("/vibemix/plugin/add", 0, 0, CLAP_EDITOR, wait=3)
        session.send("/vibemix/plugin/add", 0, 0, VST3_EDITOR, wait=4)
        for slot in range(3):
            session.send("/vibemix/plugin/ui", 0, 0, slot, wait=6)
        dpy = display.Display(compositor.display)
        before = editor_windows(dpy)
        started = time.time()
        os.kill(session.engine.pid, signal.SIGTERM)
        while session.engine.poll() is None and time.time() - started < 20:
            time.sleep(0.1)
        took = time.time() - started
        time.sleep(1)
        after = editor_windows(dpy)
        exited = session.engine.poll() is not None
        status = session.engine.returncode
        reports = session.sanitizer_reports()
        print(f"shutdown: {len(before)} editor windows open; engine "
              f"{'exited in %.1fs with status %s' % (took, status) if exited else 'DID NOT EXIT'}; "
              f"{len(after)} windows left" + (f"; {reports} sanitizer report(s)" if ASAN else ""))
        # A crash during teardown is a failure too, even if it closes the windows.
        return (exited and status == 0 and took < (12 if ASAN else 4) and not after
                and reports == 0)
    finally:
        session.close()


def check_autofit(compositor) -> bool:
    session = Session(compositor)
    try:
        session.send("/vibemix/state/query", wait=1)
        for _ in range(5):
            session.send("/vibemix/strip/add", wait=0.1)
        for _ in range(2):
            session.send("/vibemix/bus/add", wait=0.1)
        for strip in range(4):
            session.send("/vibemix/plugin/add", 0, strip, AUDIOGAIN, wait=1.0)
        session.send("/vibemix/transport/ui", json.dumps({
            "beats": True, "bpm": True, "stability": True, "time": False,
            "loop_indicator": False, "loop_bars": False, "loop_length": 8, "autosize": True}))
        session.start_gui()
        time.sleep(9)
        dpy = display.Display(compositor.display)
        root = dpy.screen().root
        target, best = None, 0
        for window in walk(root):
            try:
                wm_class = window.get_wm_class()
                geometry = window.get_geometry()
                if (wm_class and "vibemix" in " ".join(wm_class)
                        and window.get_attributes().map_state == X.IsViewable
                        and geometry.width * geometry.height > best):
                    target, best = window, geometry.width * geometry.height
            except Exception:
                continue
        if target is None:
            print("autofit: could not find the GUI window")
            return False
        target.change_attributes(event_mask=X.StructureNotifyMask)
        dpy.flush()

        def sizes_after(action) -> list:
            action()
            end, seen = time.time() + 5, []
            while time.time() < end:
                while dpy.pending_events():
                    event = dpy.next_event()
                    if event.type == X.ConfigureNotify:
                        size = (event.width, event.height)
                        if not seen or seen[-1] != size:
                            seen.append(size)
                time.sleep(0.01)
            return seen

        removed = sizes_after(lambda: session.send("/vibemix/strip/remove", 5, wait=0))
        added = sizes_after(lambda: session.send("/vibemix/strip/add", wait=0))
        print(f"autofit: window sizes after removing a strip {removed}, after adding one {added}")
        # One resize straight to the final size, never an intermediate geometry.
        return len(removed) <= 1 and len(added) <= 1
    finally:
        session.close()


def main() -> int:
    wanted = sys.argv[1:] or ["shutdown", "autofit"]
    for binary in ("vibemix-engine", "vibemix-gui"):
        if not os.path.exists(os.path.join(ROOT, "target/release", binary)):
            print(f"missing target/release/{binary}; run `cargo build --release --workspace`")
            return 2
    compositor = Compositor()
    try:
        results = {}
        if "shutdown" in wanted:
            results["shutdown"] = check_shutdown(compositor)
        if "autofit" in wanted:
            results["autofit"] = check_autofit(compositor)
    finally:
        compositor.close()
    for name, ok in results.items():
        print(f"{name}: {'ok' if ok else 'FAIL'}")
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
