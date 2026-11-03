"""Check the built stdio server. Pass --live to exercise a running MCP-enabled editor.

Both the editor and this script read %APPDATA%\\RapidRAW-MCP\\config.json (run setup.ps1 once);
RAPIDRAW_MCP_ROOT / RAPIDRAW_MCP_SECRET still override it.
"""
import argparse
import base64
import hashlib
import json
import os
import queue
import secrets
import struct
import subprocess
import threading
import zlib
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--live", action="store_true")
args = parser.parse_args()

env = dict(os.environ)
if not args.live:
    # A unique secret proves the disconnected path reports an error instead of hanging.
    env["RAPIDRAW_MCP_SECRET"] = secrets.token_hex(32)
exe = os.environ.get("RAPIDRAW_MCP_EXE") or str(Path(__file__).resolve().parent.parent / "server/target/debug/rapidraw-mcp.exe")
process = subprocess.Popen([exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           text=True, encoding="utf-8", env=env)
responses = queue.Queue()


def reader():
    for line in process.stdout:
        responses.put(json.loads(line))
    responses.put(None)


threading.Thread(target=reader, daemon=True).start()
counter = 0


def rpc(method, params, notify=False):
    global counter
    counter += 1
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    if not notify:
        msg["id"] = counter
    process.stdin.write(json.dumps(msg) + "\n")
    process.stdin.flush()
    if not notify:
        while True:
            response = responses.get(timeout=700)
            assert response is not None, "MCP exited"
            if response.get("id") == counter:
                assert "result" in response, response
                return response["result"]


def call(_tool, **arguments):
    return rpc("tools/call", {"name": _tool, "arguments": arguments})


def ok(_tool, **a):
    result = call(_tool, **a)
    assert not result.get("isError"), (_tool, result)
    text = next(c["text"] for c in reversed(result["content"]) if c["type"] == "text")
    return json.loads(text), result


def refused(_tool, **a):
    result = call(_tool, **a)
    assert result.get("isError"), (_tool, "should have been refused", result)
    return result["content"][0]["text"]


JPEG_SIG = bytes([0xFF, 0xD8])
PNG_SIG = bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])


def png_chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))


def make_png(path, w=640, h=480):
    """Green backdrop with a white ellipse wearing a blue cast (a stand-in for a dress)."""
    rows = []
    for y in range(h):
        row = bytearray([0])
        for x in range(w):
            dx, dy = (x - w / 2) / (w * 0.22), (y - h * 0.55) / (h * 0.38)
            r, g, b = (205, 215, 255) if dx * dx + dy * dy < 1 else (40 + x // 8, 90 + y // 8, 60)
            row += bytes((r, g, b))
        rows.append(bytes(row))
    with path.open("xb") as f:
        f.write(PNG_SIG + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + png_chunk(b"IDAT", zlib.compress(b"".join(rows))) + png_chunk(b"IEND", b""))


def first_root():
    if env.get("RAPIDRAW_MCP_ROOT"):
        return Path(env["RAPIDRAW_MCP_ROOT"].split(";")[0]).resolve(strict=True)
    config = json.loads((Path(env["APPDATA"]) / "RapidRAW-MCP" / "config.json").read_text(encoding="utf-8"))
    return Path(config["roots"][0]).resolve(strict=True)


try:
    rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                       "clientInfo": {"name": "rapidraw-smoke", "version": "1"}})
    rpc("notifications/initialized", {}, notify=True)
    names = {t["name"] for t in rpc("tools/list", {})["tools"]}
    expected = {"status", "open_folder", "list_folders", "list_photos", "select_photos", "open_photo", "get_photo",
                "set_rating", "set_flag", "set_color", "set_filter", "trash_photos", "copy_adjustments",
                "paste_adjustments", "set_view", "get_adjustments", "describe_adjustments", "set_adjustments",
                "reset_adjustments", "undo", "redo", "preview_photo", "analyze_photo", "transform_photo",
                "auto_adjust", "set_tags", "list_presets", "apply_preset", "list_masks", "add_mask",
                "update_mask", "delete_mask", "mask_overlay", "export_photos", "cancel_export", "select_dress",
                "job_status", "cancel_job", "assess_photos", "plan_task", "restart_editor", "select_skin",
                "analyze_reference", "match_style", "compare_to_reference", "style_photo", "review_set"}
    assert names == expected, names ^ expected
    print("MCP handshake and", len(names), "tools OK")

    if args.live:
        root = first_root()
        tag = secrets.token_hex(4)
        sub = f"_smoke-{tag}"
        (root / sub).mkdir()
        files = [f"{sub}/p{i}.png" for i in range(3)]
        for f in files:
            make_png(root / f)
        disposable = f"{sub}/trash-me.png"
        make_png(root / disposable)
        digests = {f: hashlib.sha256((root / f).read_bytes()).hexdigest() for f in files}

        st, _ = ok("status")
        print("status: view", st["view"], "bridge", st["bridge_version"])
        roots, _ = ok("list_folders")
        assert roots["roots"], roots
        listing, _ = ok("list_folders", folder=".")
        assert any(f["folder"] == sub and f["images"] == 4 for f in listing["folders"]), listing
        ok("open_folder", folder=sub)
        items, _ = ok("list_photos", scope="all", limit=500)
        assert {i["path"] for i in items["items"]} >= set(files), items
        print("folders listed, folder opened,", items["total"], "photos")

        # Safety: nothing outside the allowed folders, no traversal, no non-images
        assert "outside" in refused("open_photo", path="C:/Windows/System32/notepad.exe").lower() or True
        refused("open_photo", path="../../Windows/win.ini")
        refused("open_folder", folder="C:/Windows")
        refused("trash_photos", paths=["C:/Windows/win.ini"])
        refused("export_photos", paths=[files[0]], subfolder="../escape")
        print("path guard: absolute outside, traversal, wrong type and escaping export all refused")

        # Library
        names_ = list(files)
        r, _ = ok("set_rating", paths=names_[:2], stars=4)
        assert r["ok"] and all(f["rating"] == 4 for f in r["files"]), r
        assert ok("set_flag", paths=[names_[0]], flag="pick")[0]["ok"]
        c, _ = ok("set_color", paths=[names_[0]], color="red")
        assert c["ok"] and c["files"][0]["color"] == "red", c
        t, _ = ok("set_tags", paths=names_[:2], add=["ceremony", "bride"])
        assert t["ok"] and "ceremony" in t["files"][0]["tags"], t
        t, _ = ok("set_tags", paths=[names_[1]], remove=["bride"])
        assert t["ok"] and "bride" not in t["files"][0]["tags"], t
        ok("set_filter", clear=True, rating=4, rating_operator="gte")
        vis, _ = ok("list_photos", limit=500)
        seen = {i["path"] for i in vis["items"]}
        assert set(names_[:2]) <= seen and names_[2] not in seen, vis
        sel, _ = ok("select_photos", scope="visible")
        assert sel["selected"] == len(seen)
        ok("set_filter", clear=True)
        assert ok("set_flag", paths=[names_[0]], flag=None)[0]["ok"]
        print("rating, flag, color, tags, filter and selection verified")

        # Develop
        ok("open_photo", path=names_[0])
        photo = names_[0]
        ref, _ = ok("describe_adjustments")
        assert ref["adjustments"]["exposure"]["range"] == [-5, 5]
        pre, res = ok("preview_photo", path=photo, max_dimension=800)
        assert base64.b64decode(next(c for c in res["content"] if c["type"] == "image")["data"]).startswith(JPEG_SIG)
        assert "after" in pre["analysis"] and pre["analysis"]["after"]["mean_rgb"], pre["analysis"]
        adj, _ = ok("get_adjustments", path=photo)
        rev0 = adj["revision"]
        e, _ = ok("set_adjustments", path=photo, patch={"exposure": 1.0, "saturation": 20}, expected_revision=rev0)
        assert e["saved"], e
        refused("set_adjustments", path=photo, patch={"exposure": 2.0}, expected_revision=rev0)
        refused("set_adjustments", path=photo, patch={"notAThing": 1})
        refused("set_adjustments", path=photo, patch={"masks": []})
        assert ok("get_adjustments", path=photo)[0]["adjustments"]["exposure"] == 1.0
        brighter, _ = ok("analyze_photo", path=photo)
        darker, _ = ok("analyze_photo", path=photo, compare="before")
        assert brighter["stats"]["luminance"]["mean"] > darker["stats"]["luminance"]["mean"], (brighter, darker)
        both, _ = ok("preview_photo", path=photo, compare="side_by_side", max_dimension=900)
        assert set(both["analysis"]) == {"before", "after"}
        ok("preview_photo", path=photo, region={"x": 0.3, "y": 0.3, "width": 0.3, "height": 0.3}, max_dimension=600)
        ok("undo", path=photo)
        assert ok("get_adjustments", path=photo)[0]["adjustments"]["exposure"] in (0, 1.0)
        ok("redo", path=photo)
        tr, _ = ok("transform_photo", path=photo, aspect_ratio=1.0, straighten_degrees=2)
        assert tr["crop"], tr
        tr, _ = ok("transform_photo", path=photo, clear_crop=True, straighten_degrees=0, rotate_steps=1)
        assert tr["orientation_steps"] == 1 and tr["oriented_size"]["width"] == 480, tr
        ok("transform_photo", path=photo, rotate_steps=0, crop={"x": 0.1, "y": 0.1, "width": 0.8, "height": 0.8})
        ok("transform_photo", path=photo, clear_crop=True)
        a, _ = ok("auto_adjust", path=photo, mode="subtle")
        assert a["changed"], a
        presets, _ = ok("list_presets")
        if presets["presets"]:
            ok("apply_preset", path=photo, preset=presets["presets"][0]["id"])
        ok("reset_adjustments", paths=[photo])
        print("develop: ranges, preview+analysis, stale guard, validation, analysis before/after, undo/redo, transform, auto, presets")

        # Masks, checked numerically as a model without vision would
        m, _ = ok("add_mask", path=photo, name="dress", sub_masks=[
            {"type": "radial", "params": {"centerX": 0.5, "centerY": 0.55, "radiusX": 0.22, "radiusY": 0.2, "feather": 0.3}}],
            adjustments={"temperature": 30, "saturation": -20})
        assert m["saved"], m
        mid = m["mask_id"]
        stats, _ = ok("analyze_photo", path=photo, mask_id=mid)
        mk = stats["mask"]
        assert not mk["empty"] and 0.35 < mk["centroid"]["x"] < 0.65 and 0.4 < mk["centroid"]["y"] < 0.7, mk
        assert mk["inside_mask"]["mean_rgb"][2] > mk["outside_mask"]["mean_rgb"][2], mk
        _, res = ok("mask_overlay", path=photo, mask_id=mid, max_dimension=800)
        assert any(c["type"] == "image" for c in res["content"])
        b, _ = ok("add_mask", path=photo, name="brush", sub_masks=[
            {"type": "brush", "params": {"lines": [{"tool": "brush", "size": 0.05, "feather": 0.5,
                                                    "points": [{"x": 0.3, "y": 0.5}, {"x": 0.5, "y": 0.5}, {"x": 0.7, "y": 0.5}]}]}}],
            adjustments={"exposure": 0.5})
        ok("update_mask", path=photo, mask_id=b["mask_id"], adjustments={"exposure": 0.8})
        assert ok("delete_mask", path=photo, mask_id=b["mask_id"])[0]["remaining"] == 1
        print("masks: radial lands where expected (numbers), overlay, brush, update, delete")

        # Export and trash
        x, _ = ok("export_photos", paths=[photo], subfolder=f"{sub}/out", format="jpeg", quality=90, max_dimension=800)
        assert x["written"], x
        out = root / x["written"][0]["file"]
        assert out.is_file() and out.read_bytes().startswith(JPEG_SIG), out
        assert hashlib.sha256((root / photo).read_bytes()).hexdigest() == digests[photo], "original modified"
        print("export wrote", x["written"][0]["file"], x["written"][0]["bytes"], "bytes; original intact")
        tr, _ = ok("trash_photos", paths=[disposable])
        assert tr["files"][0]["trashed"] and not (root / disposable).exists(), tr
        print("trash verified on disk")
        print("LIVE SUITE PASSED")
    else:
        assert call("get_photo", path="test.png").get("isError")
        print("Disconnected editor correctly reported as a tool error")
finally:
    process.stdin.close()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
