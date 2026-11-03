"""Concurrency check: several MCP server processes hit one editor at the same time.

Every response must name the photo that was asked about, and the final ratings must be exactly
what each worker wrote last. Run with a MCP-enabled RapidRAW open on a folder with 3+ photos:
    python stress.py <folder inside the allowed folders>
"""
import json
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

folder = sys.argv[1] if len(sys.argv) > 1 else "."
client = Path(__file__).parent / "client.py"


def call(tool, **arguments):
    out = subprocess.run([sys.executable, str(client), tool, json.dumps(arguments)],
                         capture_output=True, text=True, encoding="utf-8").stdout.strip().splitlines()
    status = out[-1] if out else "no output"
    body = json.loads(out[0]) if out and out[0].startswith("{") else {}
    return status == "ok", body, "\n".join(out)


good, listing, raw = call("open_folder", folder=folder)
assert good, raw
good, listing, raw = call("list_photos", scope="all")
photos = [i["path"] for i in listing["items"]][:4]
assert len(photos) >= 3, photos
print("photos:", photos)

errors = []


def worker(index):
    path = photos[index % len(photos)]
    last = None
    for step in range(6):
        stars = (index + step) % 6
        kind = step % 3
        started = time.time()
        if kind == 0:
            ok, body, raw = call("set_rating", paths=[path], stars=stars)
            if ok and body["files"][0]["path"] == path and body["files"][0]["rating"] == stars:
                last = stars
            else:
                errors.append(("set_rating", path, raw[:200]))
        elif kind == 1:
            ok, body, raw = call("get_photo", path=path)
            if not ok or body.get("path") != path:
                errors.append(("get_photo", path, raw[:200]))
        else:
            ok, body, raw = call("analyze_photo", path=path, max_dimension=300)
            if not ok or body.get("path") != path:
                errors.append(("analyze_photo", path, raw[:200]))
        print(f"worker {index} {path.split('/')[-1][:22]:22} step {step} {time.time() - started:4.1f}s")
    return path, last


begin = time.time()
with ThreadPoolExecutor(max_workers=len(photos)) as pool:
    results = list(pool.map(worker, range(len(photos))))
print(f"{len(photos) * 6} calls from {len(photos)} parallel clients in {time.time() - begin:.1f}s")

ok, final, raw = call("list_photos", scope="all")
ratings = {i["path"]: i["rating"] for i in final["items"]}
for path, last in results:
    if last is not None and ratings.get(path) != last:
        errors.append(("final rating", path, f"expected {last}, found {ratings.get(path)}"))
for path in photos:
    call("set_rating", paths=[path], stars=0)
print("ERRORS:" if errors else "CONCURRENCY OK: no crossed responses, ratings consistent")
for e in errors:
    print(" ", e)
sys.exit(1 if errors else 0)
