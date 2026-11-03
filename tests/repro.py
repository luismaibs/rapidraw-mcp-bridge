"""Reproduction and regression scenarios for the bridge, run on disposable copies only.

    python repro.py reopen      # editor state vs saved sidecar after reopening cached photos
    python repro.py busy        # does a quick call wait behind a long export?
    python repro.py abandon     # does the editor stay busy after the agent that asked disappears?
    python repro.py minimized   # do calls slow down or stall while the window is minimized?
    python repro.py verify      # does `saved` mean the sidecar on disk holds the edit?
    python repro.py queue       # do two long jobs asked at once run one after the other?
    python repro.py reload      # does a caller hear about a window reload (F5) instead of timing out?
    python repro.py jobs        # do long calls become followable, cancellable jobs? (editor started with RAPIDRAW_MCP_DETACH_S=2)
    python repro.py transaction # does a failed select_dress keep the mask it was to replace?
    python repro.py skin        # does select_skin fail cleanly on a photo without people?
    python repro.py all

Needs the MCP-enabled RapidRAW open. Everything happens in a throwaway folder `_repro-<id>` inside the
first allowed folder (fresh PNGs plus copies of the RAW files in `raw-samples`, or in the folder named by RAPIDRAW_MCP_RAW_SOURCE); it is deleted at the end.
The signals worth reading are printed at the end and kept in %APPDATA%\\RapidRAW-MCP\\bridge.log.
"""
import ctypes
import json
import os
import queue
import re
import secrets
import shutil
import struct
import subprocess
import sys
import threading
import time
import zlib
from pathlib import Path

EXE = os.environ.get("RAPIDRAW_MCP_EXE") or str(Path(__file__).resolve().parent.parent / "server/target/debug/rapidraw-mcp.exe")
APPDATA = Path(os.environ["APPDATA"])
LOG = APPDATA / "RapidRAW-MCP" / "bridge.log"


class Session:
    """One MCP server process, like one agent session."""

    def __init__(self):
        self.process = subprocess.Popen([EXE], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.lock = threading.Lock()
        self.counter = 0
        self.waiting = {}
        threading.Thread(target=self._read, daemon=True).start()
        self._send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                  "clientInfo": {"name": "repro", "version": "1"}})
        self._send("notifications/initialized", {}, notify=True)

    def _read(self):
        for line in self.process.stdout:
            try:
                message = json.loads(line)
            except ValueError:
                continue
            slot = self.waiting.pop(message.get("id"), None)
            if slot:
                slot.put(message)

    def _send(self, method, params, notify=False, timeout=1200):
        with self.lock:
            self.counter += 1
            number = self.counter
            body = {"jsonrpc": "2.0", "method": method, "params": params}
            slot = queue.Queue()
            if not notify:
                body["id"] = number
                self.waiting[number] = slot
            self.process.stdin.write(json.dumps(body) + "\n")
            self.process.stdin.flush()
        return None if notify else slot.get(timeout=timeout)

    def call(self, tool, **arguments):
        """Returns (ok, parsed json or text, seconds)."""
        started = time.time()
        reply = self._send("tools/call", {"name": tool, "arguments": arguments})["result"]
        text = next((c["text"] for c in reversed(reply["content"]) if c["type"] == "text"), "")
        try:
            body = json.loads(text)
        except ValueError:
            body = text
        return (not reply.get("isError")), body, time.time() - started

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(timeout=5)
        except Exception:
            self.process.kill()

    def kill(self):
        self.process.kill()


def png_chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))


def make_png(path, w=640, h=480, ellipse=True):
    rows = []
    for y in range(h):
        row = bytearray([0])
        for x in range(w):
            dx, dy = (x - w / 2) / (w * 0.22), (y - h * 0.55) / (h * 0.38)
            r, g, b = (205, 215, 255) if ellipse and dx * dx + dy * dy < 1 else (40 + x // 8, 90 + y // 8, 60)
            row += bytes((r, g, b))
        rows.append(bytes(row))
    with path.open("xb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + png_chunk(b"IDAT", zlib.compress(b"".join(rows))) + png_chunk(b"IEND", b""))


def first_root():
    config = json.loads((APPDATA / "RapidRAW-MCP" / "config.json").read_text(encoding="utf-8"))
    return Path(config["roots"][0]).resolve(strict=True)


def sidecar_masks(photo):
    """Mask count stored on disk for a photo, or None when there is no sidecar yet."""
    file = Path(str(photo) + ".rrdata")
    if not file.exists():
        return None
    try:
        data = json.loads(file.read_text(encoding="utf-8"))
        return len((data.get("adjustments") or {}).get("masks") or [])
    except (ValueError, OSError):
        return None


class Workspace:
    def __init__(self, raws):
        self.root = first_root()
        self.name = f"_repro-{secrets.token_hex(3)}"
        self.dir = self.root / self.name
        self.dir.mkdir()
        self.pngs = []
        for i in range(2):
            make_png(self.dir / f"p{i}.png")
            self.pngs.append(f"{self.name}/p{i}.png")
        self.raws = []
        source = Path(os.environ.get("RAPIDRAW_MCP_RAW_SOURCE") or self.root / "raw-samples")
        picked = [p for p in sorted(source.glob("*")) if p.suffix.lower() in (".arw", ".cr2", ".dng")][:raws]
        for p in picked:
            shutil.copy2(p, self.dir / p.name)
            self.raws.append(f"{self.name}/{p.name}")
        self.log_offset = LOG.stat().st_size if LOG.exists() else 0

    def cleanup(self):
        # The work folder itself, plus the export folders this run asked for (named after it).
        mine = [self.dir, *self.root.glob(f"{self.name}-out*")]
        for target in mine:
            for attempt in range(5):
                try:
                    shutil.rmtree(target)
                    break
                except OSError:
                    time.sleep(1)
            else:
                print(f"! could not delete {target}; remove it by hand")

    def new_log_lines(self, pattern):
        if not LOG.exists():
            return []
        with LOG.open("rb") as f:
            f.seek(self.log_offset)
            text = f.read().decode("utf-8", "replace")
        return [line for line in text.splitlines() if re.search(pattern, line)]


def scenario_reopen(ws):
    """Reopening a cached photo must show the saved state, not an older one."""
    s = Session()
    print("== reopen: add a mask, switch photos, come back, compare editor with the sidecar on disk")
    a, b = ws.pngs
    assert s.call("open_folder", folder=ws.name)[0]
    mismatches = 0
    for i in range(8):
        ok, body, _ = s.call("open_photo", path=a)
        assert ok, body
        ok, made, _ = s.call("add_mask", path=a, name=f"r{i}", sub_masks=[{"type": "radial", "params": {
            "centerX": 0.5, "centerY": 0.5, "radiusX": 0.2, "radiusY": 0.2, "feather": 0.3}}],
            adjustments={"exposure": 0.2})
        assert ok, made
        s.call("open_photo", path=b)
        ok, body, _ = s.call("open_photo", path=a)
        ok, info, _ = s.call("get_photo", path=a)
        in_editor = info.get("masks")
        time.sleep(0.8)
        on_disk = sidecar_masks(ws.root / a)
        flag = "" if in_editor == on_disk else "   <-- MISMATCH"
        mismatches += bool(flag)
        print(f"  round {i}: masks in editor={in_editor} on disk={on_disk} (add_mask said total={made.get('masks_total')}, saved={made.get('saved')}){flag}")
    s.close()
    return {"reopen_mismatches": mismatches}


def scenario_busy(ws):
    """A quick read must not wait behind a long export."""
    print("== busy: export all RAW copies while a second agent asks for status")
    if not ws.raws:
        print("  no RAW files found; set RAPIDRAW_MCP_RAW_SOURCE to a folder with sample RAWs; skipped")
        return {}
    long_run, quick = Session(), Session()
    assert long_run.call("open_folder", folder=ws.name)[0]
    result = {}

    def export():
        result["export"] = long_run.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out", format="jpeg", quality=80)

    t = threading.Thread(target=export)
    t.start()
    time.sleep(2)
    ok, body, took = quick.call("status")
    print(f"  status during export: ok={ok}, answered after {took:.1f}s")
    shows = isinstance(body, dict) and (body.get("bridge") or {}).get("running")
    t.join()
    ok_export, body_export, took_export = result["export"]
    print(f"  export: ok={ok_export}, took {took_export:.1f}s")
    long_run.close()
    quick.close()
    verdict = "FIXED" if took < 2 and took < took_export - 2 else "STILL BLOCKED"
    print(f"  -> {verdict} (status {'shows' if shows else 'does not show'} what the editor is running)")
    return {"status_wait_s": round(took, 1), "export_s": round(took_export, 1), "verdict": verdict, "status_shows_running_job": bool(shows)}


def scenario_abandon(ws):
    """The agent that asked disappears (client timeout, crash): the editor must stay usable for the next one."""
    print("== abandon: start an export, kill the asking server, then ask for status")
    if not ws.raws:
        print("  no RAW files found; set RAPIDRAW_MCP_RAW_SOURCE to a folder with sample RAWs; skipped")
        return {}
    gone, next_agent = Session(), Session()
    assert gone.call("open_folder", folder=ws.name)[0]
    threading.Thread(target=lambda: gone.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out2"), daemon=True).start()
    time.sleep(2)
    gone.kill()
    ok, body, took = next_agent.call("status")
    print(f"  status after the asker vanished: ok={ok}, answered after {took:.1f}s")
    # A queued call must wait for the abandoned export (never run on top of it) and then work.
    began = time.time()
    ok2, body2, took2 = next_agent.call("get_photo", path=ws.pngs[0])
    print(f"  next queued call: ok={ok2}, waited {time.time() - began:.1f}s for the abandoned export to finish")
    next_agent.close()
    verdict = "FIXED" if took < 2 and ok2 else "STILL BLOCKED"
    print(f"  -> {verdict}")
    return {"status_after_abandon_s": round(took, 1), "next_call_ok": ok2, "verdict": verdict}


def scenario_queue(ws):
    """Two agents ask for long jobs at once; the editor must run them one after the other."""
    print("== queue: two exports asked at the same moment")
    if not ws.raws:
        print("  no RAW files found; set RAPIDRAW_MCP_RAW_SOURCE to a folder with sample RAWs; skipped")
        return {}
    a, b = Session(), Session()
    assert a.call("open_folder", folder=ws.name)[0]
    results = {}

    def go(name, session):
        results[name] = session.call("export_photos", paths=ws.raws[:2], subfolder=f"{ws.name}-out-{name}", format="jpeg", quality=70)

    threads = [threading.Thread(target=go, args=("a", a)), threading.Thread(target=go, args=("b", b))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    time.sleep(1)
    marks = [("start" if "window start" in line else "done") for line in ws.new_log_lines(r"export window (start|done)")]
    print(f"  exports: a ok={results['a'][0]} {results['a'][2]:.1f}s, b ok={results['b'][0]} {results['b'][2]:.1f}s")
    print(f"  editor saw: {marks}")
    overlap = "start" in marks and any(marks[i] == "start" and marks[i + 1] == "start" for i in range(len(marks) - 1))
    verdict = "NO OVERLAP" if marks and not overlap and all(r[0] for r in results.values()) else "CHECK"
    print(f"  -> {verdict}")
    a.close()
    b.close()
    return {"verdict": verdict, "sequence": marks}


def wait_job(session, job, seconds=90):
    """Poll job_status until the job leaves queued/running. Returns (view, list of progress snapshots)."""
    seen = []
    end = time.time() + seconds
    view = {}
    while time.time() < end:
        ok, view, _ = session.call("job_status", job=job)
        if isinstance(view, dict) and view.get("progress"):
            seen.append(view["progress"])
        if not ok or (isinstance(view, dict) and view.get("state") not in ("queued", "running", "cancelling")):
            break
        time.sleep(0.5)
    return view, seen


def scenario_jobs(ws):
    """Long work hands back a job id and keeps going. Start the editor with RAPIDRAW_MCP_DETACH_S=2 for this."""
    print("== jobs: an export that outlasts the wait becomes a job that can be followed, cancelled and recovered")
    if not ws.raws:
        print("  no RAW files found; set RAPIDRAW_MCP_RAW_SOURCE to a folder with sample RAWs; skipped")
        return {}
    s = Session()
    assert s.call("open_folder", folder=ws.name)[0]
    checks = []

    def check(name, passed, detail=""):
        checks.append((name, passed))
        print(f"  [{'ok' if passed else 'FAIL'}] {name} {detail}")

    ok, env, took = s.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out-j1", format="jpeg", quality=80)
    if not (ok and isinstance(env, dict) and env.get("job")):
        print(f"  the export finished in {took:.1f}s without handing back a job id; start the editor with RAPIDRAW_MCP_DETACH_S=2")
        s.close()
        return {"verdict": "NOT TESTED"}
    check("a long call returns a job id instead of waiting", env.get("state") in ("running", "queued"), f"state={env.get('state')} after {took:.1f}s")
    view, seen = wait_job(s, env["job"])
    check("the job finishes and job_status carries the result", view.get("state") == "done" and view.get("exported") == len(ws.raws), f"state={view.get('state')} exported={view.get('exported')}")
    check("the result was checked on disk (written files listed)", bool(view.get("written")), f"{len(view.get('written') or [])} files")
    check("progress was visible while it ran", len(seen) > 0, f"{seen[:2]}")

    ok, env2, _ = s.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out-j2", format="jpeg", quality=80)
    ok_c, cancelled, _ = s.call("cancel_job", job=env2["job"])
    check("cancel_job is accepted", ok_c and cancelled.get("state") in ("cancelling", "cancelled", "done"), f"state={cancelled.get('state')}")
    view2, _ = wait_job(s, env2["job"])
    check("a cancelled job ends cancelled (or had already finished)", view2.get("state") in ("cancelled", "done"), f"state={view2.get('state')}")

    ok, env3, _ = s.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out-j3", format="jpeg", quality=80)
    s.kill()
    other = Session()
    view3, _ = wait_job(other, env3["job"])
    check("a job outlives the agent that asked for it", view3.get("state") == "done", f"state={view3.get('state')}")
    other.close()
    failed = [n for n, passed in checks if not passed]
    return {"verdict": "OK" if not failed else "FAILED", "failed": failed}


def scenario_reload(ws):
    """The window is reloaded (F5) in the middle of a long job: the caller must hear about it, not time out."""
    print("== reload: press F5 while an export runs")
    if not ws.raws:
        print("  no RAW files found; set RAPIDRAW_MCP_RAW_SOURCE to a folder with sample RAWs; skipped")
        return {}
    hwnd = find_window()
    if not hwnd:
        print("  could not find the RapidRAW window; skipped")
        return {}
    user32 = ctypes.windll.user32
    s = Session()
    assert s.call("open_folder", folder=ws.name)[0]
    outcome = {}
    threading.Thread(target=lambda: outcome.update(r=s.call("export_photos", paths=ws.raws, subfolder=f"{ws.name}-out-r", format="jpeg"))).start()
    time.sleep(2)
    user32.ShowWindow(hwnd, 9)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.5)
    user32.keybd_event(0x74, 0, 0, 0)  # F5 down
    user32.keybd_event(0x74, 0, 2, 0)  # F5 up
    waited = time.time()
    while "r" not in outcome and time.time() - waited < 60:
        time.sleep(0.5)
    if "r" not in outcome:
        print("  the export call did not return within 60 s after the reload")
        return {"verdict": "HUNG"}
    ok, body, took = outcome["r"]
    text = body if isinstance(body, str) else json.dumps(body)[:200]
    print(f"  export returned after {took:.1f}s: ok={ok} {text[:160]}")
    verdict = "TOLD" if (not ok and "ui_reloaded" in text) or took < 15 else "CHECK"
    time.sleep(8)  # let the new page register
    ok2, body2, _ = s.call("status")
    print(f"  status after the reload: ok={ok2}")
    s.close()
    return {"verdict": verdict, "status_after_reload_ok": ok2}


def find_window():
    user32 = ctypes.windll.user32
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)
    def visit(hwnd, _):
        if user32.IsWindowVisible(hwnd) or user32.IsIconic(hwnd):
            buffer = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(hwnd, buffer, 256)
            if buffer.value.strip() == "RapidRAW":
                found.append(hwnd)
        return True

    user32.EnumWindows(visit, 0)
    return found[0] if found else None


def scenario_minimized(ws):
    """Calls must not slow down or stall while the window is minimized."""
    print("== minimized: same short sequence with the window shown, then minimized")
    hwnd = find_window()
    if not hwnd:
        print("  could not find the RapidRAW window; skipped")
        return {}
    user32 = ctypes.windll.user32
    s = Session()
    a, _ = ws.pngs
    assert s.call("open_folder", folder=ws.name)[0]

    def sequence():
        timings = {}
        for name, tool, args in [
            ("open_photo", "open_photo", {"path": a}),
            ("set_adjustments", "set_adjustments", {"path": a, "patch": {"exposure": 0.3}}),
            ("preview_photo", "preview_photo", {"path": a, "max_dimension": 800}),
            ("add_mask", "add_mask", {"path": a, "name": "m", "sub_masks": [{"type": "radial", "params": {
                "centerX": 0.5, "centerY": 0.5, "radiusX": 0.2, "radiusY": 0.2}}], "adjustments": {"exposure": 0.1}}),
        ]:
            try:
                ok, body, took = s.call(*[tool], **args)
            except queue.Empty:
                ok, took = False, float("inf")
            timings[name] = (ok, round(took, 1))
        return timings

    shown = sequence()
    user32.ShowWindow(hwnd, 6)  # SW_MINIMIZE
    time.sleep(1.5)
    hidden = sequence()
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    print("  shown    :", shown)
    print("  minimized:", hidden)
    s.close()
    return {"shown": shown, "minimized": hidden}


def hold_open(file):
    """Open a file with no sharing, so the editor can neither write nor read it. Returns the handle."""
    kernel32 = ctypes.windll.kernel32
    kernel32.CreateFileW.restype = ctypes.c_void_p
    handle = kernel32.CreateFileW(str(file), 0x80000000, 0, None, 3, 0x80, None)
    if handle in (None, ctypes.c_void_p(-1).value):
        return None
    return handle


def release(handle):
    ctypes.windll.kernel32.CloseHandle(ctypes.c_void_p(handle))


def scenario_verify(ws):
    """`saved` must mean the sidecar on disk holds the edit; batch jobs must be confirmed."""
    print("== verify: saved is real, ranges are kept, paste and reset are confirmed")
    s = Session()
    a, b = ws.pngs
    checks = []

    def check(name, passed, detail=""):
        checks.append((name, passed))
        print(f"  [{'ok' if passed else 'FAIL'}] {name} {detail}")

    assert s.call("open_folder", folder=ws.name)[0]
    ok, opened, _ = s.call("open_photo", path=a)
    check("open_photo", ok, str(opened)[:80] if not ok else "")

    ok, r, _ = s.call("set_adjustments", path=a, patch={"exposure": 9})
    check("value outside the range is clamped and reported", ok and r.get("clamped", {}).get("exposure", {}).get("used") == 5, str(r.get("clamped")))
    check("clamped edit is saved", ok and r.get("saved") is True, str(r.get("save_problem", "")))

    ok, made, _ = s.call("add_mask", path=a, name="v", sub_masks=[{"type": "radial", "params": {
        "centerX": 0.5, "centerY": 0.5, "radiusX": 0.2, "radiusY": 0.2}}], adjustments={"exposure": 0.2})
    time.sleep(0.5)
    check("add_mask is saved and the sidecar has the mask", ok and made.get("saved") is True and sidecar_masks(ws.root / a) == 1,
          f"disk masks={sidecar_masks(ws.root / a)}")

    handle = hold_open(str(ws.root / a) + ".rrdata")
    if handle is None:
        print("  [skip] could not lock the sidecar")
    else:
        try:
            ok, r, _ = s.call("set_adjustments", path=a, patch={"contrast": 10})
            check("a sidecar that cannot be written is reported, not hidden", ok and r.get("saved") is False and bool(r.get("save_problem")), str(r.get("save_problem", ""))[:90])
        finally:
            release(handle)
        ok, r, _ = s.call("set_adjustments", path=a, patch={"contrast": 11})
        check("saving works again once the file is free", ok and r.get("saved") is True, str(r.get("save_problem", "")))

    ok, c, _ = s.call("copy_adjustments", path=a)
    ok, p, _ = s.call("paste_adjustments", paths=[b])
    check("paste is confirmed from the saved sidecar", ok and p.get("ok") is True, str(p)[:120])
    ok, z, _ = s.call("reset_adjustments", paths=[b])
    check("reset is confirmed from the saved sidecar", ok and z.get("ok") is True, str(z)[:120])

    for i in range(4):
        s.call("open_photo", path=b)
        ok, r, _ = s.call("open_photo", path=a)
        if not ok:
            check(f"reopen round {i}", False, str(r)[:100])
            break
        ok, info, _ = s.call("get_photo", path=a)
        time.sleep(0.5)
        check(f"reopen round {i}: editor masks match the sidecar", info.get("masks") == sidecar_masks(ws.root / a),
              f"editor={info.get('masks')} disk={sidecar_masks(ws.root / a)}")
    s.close()
    failed = [n for n, passed in checks if not passed]
    return {"verify_failed": failed, "verify_checks": len(checks)}


def scenario_transaction(ws):
    """A mask that cannot be made must not cost the photo the mask it was to replace."""
    print("== transaction: select_dress on a photo with no dress, over an existing mask of the same name")
    make_png(ws.dir / "plain.png", ellipse=False)
    plain = f"{ws.name}/plain.png"
    s = Session()
    checks = []

    def check(name, passed, detail=""):
        checks.append((name, passed))
        print(f"  [{'ok' if passed else 'FAIL'}] {name} {detail}")

    assert s.call("open_folder", folder=ws.name)[0]
    assert s.call("open_photo", path=plain)[0]
    ok, made, _ = s.call("add_mask", path=plain, name="Dress test", sub_masks=[{"type": "radial", "params": {
        "centerX": 0.5, "centerY": 0.5, "radiusX": 0.2, "radiusY": 0.2}}], adjustments={"exposure": 0.2})
    original = made.get("mask_id") if ok else None
    check("the first mask is created", bool(original), str(made)[:80])

    ok, rows, took = s.call("select_dress", paths=[plain], name="Dress test")
    result = rows
    if ok and isinstance(rows, dict) and rows.get("job"):
        result, _ = wait_job(s, rows["job"], 240)
    row = (result.get("results") or [{}])[0] if isinstance(result, dict) else {}
    check("select_dress reports the photo as not ok", ok and row.get("ok") is False, str(row)[:160])
    ok, listing, _ = s.call("list_masks", path=plain)
    ids = [m["id"] for m in (listing.get("masks") or [])] if ok and isinstance(listing, dict) else []
    check("the previous mask is still there, untouched", ids == [original], f"masks now {ids}, expected [{original}]")

    ok, again, _ = s.call("add_mask", path=plain, name="Dress test", replace_name="Dress test",
                          sub_masks=[{"type": "ai-dress", "params": {}}])
    check("add_mask with a dress that is not there answers [mask_not_created]", (not ok) and "mask_not_created" in str(again), str(again)[:140])
    ok, listing, _ = s.call("list_masks", path=plain)
    ids = [m["id"] for m in (listing.get("masks") or [])] if ok and isinstance(listing, dict) else []
    check("and still keeps the previous mask", ids == [original], f"masks now {ids}")
    s.close()
    failed = [n for n, p in checks if not p]
    return {"verdict": "OK" if not failed else "FAILED", "failed": failed}


def scenario_skin(ws):
    """select_skin measures the skin and reports it; a photo without people keeps its previous skin mask."""
    print("== skin: select_skin on a synthetic photo (no faces) must fail cleanly")
    s = Session()
    a, _ = ws.pngs
    assert s.call("open_folder", folder=ws.name)[0]
    ok, res, took = s.call("select_skin", paths=[a])
    result = res
    if ok and isinstance(res, dict) and res.get("job"):
        result, _ = wait_job(s, res["job"], 240)
    row = (result.get("results") or [{}])[0] if isinstance(result, dict) else {}
    passed = ok and row.get("ok") is False
    print(f"  [{'ok' if passed else 'FAIL'}] a photo without faces comes back not ok ({str(row)[:140]})")
    s.close()
    return {"verdict": "OK" if passed else "FAILED"}


SCENARIOS = {"reopen": scenario_reopen, "busy": scenario_busy, "abandon": scenario_abandon, "minimized": scenario_minimized,
             "verify": scenario_verify, "queue": scenario_queue, "reload": scenario_reload,
             "jobs": scenario_jobs, "transaction": scenario_transaction, "skin": scenario_skin}


def main():
    choice = sys.argv[1] if len(sys.argv) > 1 else ""
    names = list(SCENARIOS) if choice == "all" else [choice]
    if any(n not in SCENARIOS for n in names):
        print(__doc__)
        sys.exit(2)
    ws = Workspace(raws=3 if any(n in ("busy", "abandon", "queue", "reload", "jobs") for n in names) else 0)
    results = {}
    try:
        probe = Session()
        ok, body, _ = probe.call("status")
        probe.close()
        if not ok:
            print("The editor did not answer status:", body)
            return
        for name in names:
            results[name] = SCENARIOS[name](ws)
    finally:
        # Let the editor finish whatever is left before deleting its files.
        time.sleep(3)
        signals = ws.new_log_lines(r"MISMATCH|late|TIMEOUT|NOT delivered|STOPPED|rejected|refused|gave up|failed")
        ws.cleanup()
    print("\n== summary")
    print(json.dumps(results, indent=2))
    print(f"\n== warning lines in bridge.log during this run ({len(signals)})")
    for line in signals[:40]:
        print(" ", line)


if __name__ == "__main__":
    main()
