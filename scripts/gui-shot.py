#!/usr/bin/env python3
"""Visual regression check for the GUI, headless.

Builds a fixed, silent session in a throwaway engine (private JACK client name,
no auto-connect, temporary config), starts `vibemix-gui` on Qt's offscreen
platform, and saves the window contents with the GUI's own `--snapshot-to`
hook. Two modes:

  gui-shot.py baseline DIR [static|edited]
      capture the current GUI twice into DIR and mask the pixels that differ
      between the two identical runs (clocks, load readouts)
  gui-shot.py check DIR [static|edited]
      capture again and compare with DIR's baseline, ignoring the masked
      pixels; exits 1 on a difference (a diff image is written next to it)

`static` shows the fixed session as loaded. `edited` then changes it while the
GUI is running (add, remove and move strips and buses, add and remove plugins,
duplicate), which checks that incremental updates end up looking exactly like a
full rebuild.

Run `baseline` before a GUI change and `check` after it.
"""
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import time

from PIL import Image, ImageChops, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CLIENT = "vibemix-guishot"
AUDIOGAIN = "http://kxstudio.sf.net/carla/plugins/audiogain_s"
# How many pixels (as a fraction) may differ outside the mask before `check` fails.
TOLERANCE = 0.0002

spec = importlib.util.spec_from_file_location("drive", os.path.join(HERE, "rt-guard-drive.py"))
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)


def build_session(client):
    """A small, fixed mix: named strips and buses, varied levels, a few plugins."""

    def send(address, *args, wait=0.08):
        client.send(address, *args)
        time.sleep(wait)

    send("/vibemix/state/query", wait=0.5)
    for _ in range(7):
        send("/vibemix/strip/add")
    for _ in range(3):
        send("/vibemix/bus/add")
    names = ["Vocals", "Guitar", "Bass", "Kick", "Snare", "Keys", "Pads", "FX"]
    for strip, name in enumerate(names):
        send("/vibemix/strip/name", strip, name)
        send("/vibemix/strip/gain", strip, 0.35 + 0.08 * strip)
        send("/vibemix/strip/pan", strip, -0.7 + 0.2 * strip)
    send("/vibemix/strip/mute", 3, 1)
    send("/vibemix/strip/solo", 1, 1)
    for bus, name in zip((1, 2, 3), ("Monitor", "Reverb", "Sub")):
        send("/vibemix/bus/name", bus, name)
        send("/vibemix/bus/gain", bus, 0.6 + 0.1 * bus)
    for strip in range(8):
        send("/vibemix/send/gain", strip, 1, 0.3 + 0.05 * strip)
        send("/vibemix/send/gain", strip, 2, 0.1 * strip % 1.0)
    send("/vibemix/send/delay", 0, 1, 12.5)
    for strip in range(4):
        send("/vibemix/plugin/add", 0, strip, AUDIOGAIN, wait=0.8)
    send("/vibemix/plugin/add", 1, 1, AUDIOGAIN, wait=0.8)
    send("/vibemix/plugin/bypass", 0, 2, 0, 1)
    time.sleep(1.0)


def apply_edits(client) -> None:
    """Live edits made while the GUI is running (the `edited` scenario)."""

    def send(address, *args, wait=1.2):
        client.send(address, *args)
        time.sleep(wait)

    send("/vibemix/strip/add")                      # a new strip appears
    send("/vibemix/strip/name", 8, "Extra")
    send("/vibemix/strip/remove", 2)                # Bass goes away (id hole)
    send("/vibemix/strip/move", 4, 0)               # Snare to the front
    send("/vibemix/strip/duplicate", 1)             # Guitar copied with its plugins
    send("/vibemix/bus/add")
    send("/vibemix/bus/name", 4, "Delay")
    send("/vibemix/bus/remove", 2)                  # Reverb goes away
    send("/vibemix/plugin/add", 0, 6, AUDIOGAIN, wait=2.5)   # Pads gets a plugin
    send("/vibemix/plugin/remove", 0, 0, 0)         # Vocals loses its plugin
    send("/vibemix/strip/gain", 5, 0.9)
    time.sleep(2.0)


def shoot(out: str, scenario: str = "static", seconds: float = 12.0) -> None:
    """Captures the GUI showing the fixed session into `out` (a PNG)."""
    config = tempfile.mkdtemp(prefix="vibemix-guishot-")  # mode 0700: holds the socket
    engine_socket = os.path.join(config, "engine.sock")
    procs = []
    client = None
    try:
        engine = subprocess.Popen(
            ["pw-jack", os.path.join(ROOT, "target/release/vibemix-engine"),
             "--socket", engine_socket, "--auto-connect", "false",
             "--client-name", CLIENT],
            env=dict(os.environ, VIBEMIX_CONFIG_DIR=config),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(engine)
        time.sleep(6)
        client = drive.Client(engine_socket)
        build_session(client)

        env = dict(os.environ, QT_QPA_PLATFORM="offscreen", VIBEMIX_SOCKET=engine_socket,
                   VIBEMIX_CONFIG_DIR=config)
        gui = subprocess.Popen(
            [os.path.join(ROOT, "target/release/vibemix-gui"), f"--snapshot-to={out}",
             f"--snapshot-after={int(seconds * 1000)}"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(gui)
        if scenario == "edited":
            time.sleep(4)  # let the GUI come up before editing under it
            apply_edits(client)
        # The GUI quits itself after saving, but do not rely on that: wait for
        # the image, then stop it.
        deadline = time.time() + seconds + 120
        while time.time() < deadline and not os.path.exists(out) and gui.poll() is None:
            time.sleep(0.5)
        time.sleep(1)
    finally:
        if client is not None:
            client.close()
        for proc in reversed(procs):
            if proc.poll() is None:
                proc.terminate()
        for proc in procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        shutil.rmtree(config, ignore_errors=True)
    if not os.path.exists(out):
        raise SystemExit(f"the GUI did not produce {out}")


def unstable_mask(first: Image.Image, second: Image.Image) -> Image.Image:
    """White where two identical runs disagree, slightly grown to cover text."""
    diff = ImageChops.difference(first.convert("RGB"), second.convert("RGB")).convert("L")
    mask = diff.point(lambda value: 255 if value > 0 else 0)
    return mask.filter(ImageFilter.MaxFilter(9))


def differing_fraction(base: Image.Image, new: Image.Image, mask: Image.Image):
    """(fraction of unmasked pixels that differ noticeably, diff image)."""
    if base.size != new.size:
        return 1.0, None
    diff = ImageChops.difference(base.convert("RGB"), new.convert("RGB")).convert("L")
    marked = diff.point(lambda value: 255 if value > 8 else 0)
    keep = ImageChops.invert(mask)
    marked = ImageChops.multiply(marked, keep.point(lambda value: 1 if value else 0))
    count = sum(1 for value in marked.tobytes() if value)
    return count / (base.size[0] * base.size[1]), marked


def main() -> int:
    if len(sys.argv) not in (3, 4) or sys.argv[1] not in ("baseline", "check"):
        print(__doc__)
        return 2
    mode, directory = sys.argv[1], sys.argv[2]
    scenario = sys.argv[3] if len(sys.argv) == 4 else "static"
    if scenario not in ("static", "edited"):
        print(__doc__)
        return 2
    # The edits run on top of the loaded session, so the capture is taken later.
    seconds = 12.0 if scenario == "static" else 34.0
    os.makedirs(directory, exist_ok=True)
    base_path = os.path.join(directory, f"{scenario}-baseline.png")
    mask_path = os.path.join(directory, f"{scenario}-mask.png")

    if mode == "baseline":
        second_path = os.path.join(directory, f"{scenario}-baseline-second.png")
        shoot(base_path, scenario, seconds)
        shoot(second_path, scenario, seconds)
        first, second = Image.open(base_path), Image.open(second_path)
        mask = unstable_mask(first, second)
        mask.save(mask_path)
        masked = sum(1 for value in mask.tobytes() if value)
        print(f"baseline saved to {base_path} "
              f"({first.size[0]}x{first.size[1]}, {masked} unstable pixels masked)")
        return 0

    new_path = os.path.join(directory, f"{scenario}-new.png")
    shoot(new_path, scenario, seconds)
    base, new, mask = Image.open(base_path), Image.open(new_path), Image.open(mask_path)
    fraction, marked = differing_fraction(base, new, mask)
    if marked is None:
        print(f"FAIL: the window size changed ({base.size} -> {new.size})")
        return 1
    marked.save(os.path.join(directory, f"{scenario}-diff.png"))
    print(f"differing pixels outside the mask: {fraction * 100:.4f}% "
          f"(tolerance {TOLERANCE * 100:.4f}%)")
    if fraction > TOLERANCE:
        print(f"FAIL: see {os.path.join(directory, f'{scenario}-diff.png')} and {new_path}")
        return 1
    print("ok: the GUI looks the same as the baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
