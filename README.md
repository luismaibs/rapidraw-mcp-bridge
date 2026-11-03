# RapidRAW MCP Bridge

An [MCP](https://modelcontextprotocol.io) server that lets AI agents work in a **running RapidRAW** the way a
photographer would: open a folder, cull and rate, develop, build local and AI masks, copy the look of a reference
photo, look at every result, and export.

The agent drives the real editor, so what it does is what you see, and what you change by hand is what it reads.
Nothing is a separate pipeline: edits are RapidRAW's own sidecars, undo works, and every change is checked against what
was actually saved.

[RapidRAW](https://github.com/CyberTimon/RapidRAW) is an open source, GPU-accelerated RAW editor. This project is an
independent add-on and is not affiliated with it.

```
agent (MCP client) -> rapidraw-mcp.exe (stdio) -> authenticated named pipe -> bridge inside RapidRAW -> the real editor
```

## What it can do today

47 tools in eight areas. The ones that make it more than a remote control:

| Capability | What happens |
|---|---|
| **Copy the look of a reference photo** | The agent reads how a reference was made (whites, blacks, each colour family), then restyles photos one at a time, comparing each against the reference and judging the result by eye. No single filter is pasted onto everything. |
| **Cull hundreds of photos by measurement** | `assess_photos` ranks sharpness (of the main face when there is one), exposure and clipping, judged against other photos of the same camera, and suggests stars and blur suspects. |
| **Select a subject cleanly** | `select_dress` and `select_skin` turn an AI silhouette into a clean mask using the photo's own pixels, over many photos in one call, with measured local corrections. |
| **Plan multi-step requests** | `plan_task` reads the folder and turns a request written in English or Spanish into an ordered plan with real counts, time estimates and the limits of the tools. |
| **Never lose work to a long job** | Exports, AI masks and batches return a job id instead of timing out. The work continues if the agent stops waiting, and can be followed or cancelled. |
| **Trust what it reports** | A reply says `saved: true` only when the sidecar on disk was written and read back with the same content. Exports and deletions are confirmed on disk. |
| **Diagnose itself** | `rapidraw-mcp.exe --doctor`, a correlated log across all three processes, and one Markdown usage report per agent session. |

### Library and culling

Open folders, list and select photos, set stars, pick/reject flags, colour labels and tags, filter, copy and paste
adjustments, and send photos to the Recycle Bin (explicit lists only, confirmed on disk).

`assess_photos` measures up to 400 photos per call. It reads the sharpness of the largest face (or of the sharpest
region when there is no face), removes each photo's own noise floor from every figure, compares every photo only with
photos of its own file type so that a soft-by-design camera is not blamed, flags suspected blur, and suggests a star
rating from best to worst. It can write the ratings. RAW+JPEG pairs are treated as one shot.

### Develop

Read and set any global adjustment with its slider range (values outside it are kept inside and reported), undo and
redo, rotate, flip, straighten, crop by fractions or aspect ratio, run the automatic adjustment, and apply presets.
`describe_adjustments` lists every control, so an agent can build a patch for tone, colour, HSL per colour family,
colour grading and curves.

### Masks

Linear, radial, brush and AI masks (subject, foreground, sky, text prompt, face parts), each with its own local
adjustments, plus a red-overlay check of coverage.

- **`select_dress`** processes up to 60 photos in one call. It asks the AI for the dress, cleans the silhouette with
  the photo's own colours (dropping skin, hair, other people's clothes and stray pieces), and applies local corrections
  (by default the blue cast is removed). `mode: "neutral_white"` then tunes the mask's temperature and tint until the
  dress measures neutral. Photos without a bride come back `ok: false` and are left untouched.
- **`select_skin`** selects face and body skin with the AI face model and tunes exposure, saturation and warmth in up to
  three measured steps toward a target skin luminance, capped so dark skin is lightened and not bleached. Rows warn
  when skin is still dark, pale or grey.
- Masks are **transactional**: when a segmentation fails the photo keeps the mask it was to replace, and the reply says
  why. Memory errors and "no original image" are retried before a photo is given up on.

### Copy the look of a reference photo

Copying a style is a judgement, not a calculation: averages of a picture follow its subject (a field of grass makes
every midtone green), so a mechanical filter makes a bad style. These tools give an agent objective readings and the
means to apply and re-check one photo at a time.

1. **`analyze_reference`** returns the reference as a picture and its palette. Whites and blacks are measured on
   near-neutral pixels, so what shows is the colour of the light and of the grade, not of the objects. Each colour
   family the editor can adjust (reds, oranges, yellows, greens, aquas, blues, purples, magentas) is reported with its
   share of the picture, saturation, luminance and average hue.
2. **`compare_to_reference`** puts a photo next to the reference and lists what differs, as suggestions in the editor's
   own controls (for example "greens are 62% saturated against the reference's 34%: saturation about -40", or "this
   colour is 40% of the photo and almost absent from the reference: tame it"). It changes nothing.
3. **`style_photo`** applies a patch to one photo (global sliders, `hsl`, `colorGrading`, `curves`), optionally clearing
   its previous look first, saves it, and returns one picture of reference, before and after with both palettes.
4. **`review_set`** lays out the reference and up to 24 photos on one numbered sheet, with whites, blacks, brightness
   and green saturation per photo and the spread across the set, to judge the gallery and find the photos that break it.

The agent decides each photo's settings from what it sees: its light, its whites, its skin, its dominant colours.
A coloured stage light, for example, is fixed with a local mask, not by dragging a global slider that would also grey
the rest of the photo. `match_style` remains as an experimental, mechanical pass over tone only; it is not a way to
copy a style.

The reference has to be a file inside an allowed folder (an image attached to a chat is not a file the editor can read:
copy it into the work folder first). Grain, vignette and halation are not measured.

### Seeing

Previews of the current edit, the original, or both side by side, optionally a zoomed region, and a numeric analysis
for models that cannot see images: colour balance, luminance percentiles, clipping, saturation, a luminance map, and the
same figures inside versus outside a mask.

### Planning

`plan_task` takes a goal in the user's words, reads the folder (counts, RAW+JPEG pairs, edits already present) and
returns an ordered plan made of these tools, with real numbers, time estimates, warnings and the limits of the tools.
It understands English and Spanish. Bulk or hard-to-undo tools mention it when no plan was made.

### Export

JPEG, PNG, TIFF or WebP into a subfolder (never the folder holding the originals), with quality, size, filename
template and metadata options. The reply lists the files that really landed on disk.

### Long jobs

A call still running after about 45 seconds replies with a job (`state: running` or `queued`) instead of a result.
The work carries on inside the editor even if the agent stops waiting or disconnects: `job_status {job, wait_s}` shows
progress, partial results and finally the full result (with `wait_s` it holds the answer until the job ends, so the
agent needs no sleep of its own), and `cancel_job` stops one at its next step. Jobs run one at a time, in order;
`status`, `list_folders`, `job_status`, `cancel_job` and `cancel_export` always answer at once.

## Requirements

- Windows 10 or 11 (the bridge uses a Windows named pipe).
- A RapidRAW source checkout at the commit listed below, and the toolchain to build it
  (see RapidRAW's own instructions: Node.js and Rust).
- A recent Rust stable toolchain (edition 2024) for the server.
- An MCP client with stdio support, such as Claude Code, Codex or OpenCode.

## Install

The bridge is part of the editor, so RapidRAW has to be built with it.

1. **Add the bridge to RapidRAW.** Check out RapidRAW at the supported commit, then apply this repository's changes:

   ```powershell
   git clone https://github.com/CyberTimon/RapidRAW C:\src\RapidRAW
   git -C C:\src\RapidRAW checkout d30a65a803856b1e2f452cff998e0de8f13e7d0f
   .\editor\apply.ps1 -RapidRaw C:\src\RapidRAW
   ```

2. **Build RapidRAW.**

   ```powershell
   cd C:\src\RapidRAW
   npm install
   npx tauri build --debug --no-bundle
   ```

3. **Build the server.**

   ```powershell
   cd server
   cargo build
   ```

4. **Install and register.** This copies both programs to `%LOCALAPPDATA%\Programs\MCP RapidRAW`, creates the
   shared configuration, registers the server with the agent clients it finds, and adds a Start menu shortcut:

   ```powershell
   .\scripts\install.ps1 -BuildDir C:\src\RapidRAW\src-tauri\target\debug -Roots D:\Photos\Copies
   ```

   `-Roots` lists the folders agents may touch. Use **copies** while you evaluate it. Run the script again after a rebuild.

For a client the script does not know, add a stdio server whose command is the installed `rapidraw-mcp.exe`
(no arguments, no environment variables) and give tools a generous timeout, since AI masks can take minutes.

Agents start the editor by themselves on their first call. If the regular RapidRAW is open instead, the call
says so: it has no bridge and shares its settings with the MCP-enabled build, so close it first. If the editor is open
but was started without a bridge, it is never closed behind your back: the call asks you to reopen it when it suits you.

## Using it

Ask the agent for what you would do by hand:

> *Open this folder, give 5 stars to the sharpest portraits, remove the blue cast from the bride's dress, and export
> them at 2000 px.*

> *Here is a reference photo. Make these ten photos feel like it, each one according to its own light.*

A typical loop is `open_folder`, `list_photos`, `open_photo`, `preview_photo`, `set_adjustments` or `add_mask`,
`preview_photo` again, `undo` if it got worse, `export_photos`. For anything with several steps the agent should call
`plan_task` first.

Failures carry a code in brackets that says what to fix: `[editor_not_open]`, `[editor_not_installed]`,
`[stock_rapidraw_open]`, `[bridge_not_started]`, `[editor_without_bridge]`, `[editor_outdated]`, `[ui_not_ready]`,
`[ui_not_responding]`, `[ui_reloaded]`, `[version_mismatch]`, `[unknown_job]`, `[bridge_connection]`,
`[server_timeout]`, `[editor_restarted]`, `[mask_not_created]`.

## Tools

| Area | Tools |
|---|---|
| Library | `status`, `list_folders`, `open_folder`, `list_photos`, `select_photos`, `open_photo`, `get_photo`, `set_rating`, `set_flag`, `set_color`, `set_tags`, `set_filter`, `trash_photos`, `copy_adjustments`, `paste_adjustments`, `set_view` |
| Develop | `describe_adjustments`, `get_adjustments`, `set_adjustments`, `reset_adjustments`, `undo`, `redo`, `transform_photo`, `auto_adjust`, `list_presets`, `apply_preset` |
| Seeing | `preview_photo`, `analyze_photo` |
| Masks | `list_masks`, `add_mask`, `select_dress`, `select_skin`, `update_mask`, `delete_mask`, `mask_overlay` |
| Look | `analyze_reference`, `compare_to_reference`, `style_photo`, `review_set`, `match_style` (experimental, tone only) |
| Output | `export_photos`, `cancel_export` |
| Jobs | `job_status`, `cancel_job` |
| Planning and recovery | `plan_task`, `assess_photos`, `restart_editor` |

`add_mask` accepts `linear`, `radial`, `brush`, `all`, `ai-subject`, `ai-foreground`, `ai-sky`, `ai-text`, `ai-face`
and `ai-dress`. Mask and crop coordinates are fractions of the oriented, uncropped image.

### Selecting a subject

Notes from selecting wedding dresses on an 84-photo session:

| Method | Result |
|---|---|
| Brush strokes placed by eye | Scalloped edges and gaps. Fine for touching up, not for the outline. |
| `ai-text` ("white wedding dress of the bride") | Follows the real outline but often takes more: face, hair, arms, other people's clothes. |
| `ai-text` plus a subtractive `brush` component | Clean when the extra piece is one known spot. Brush edits on an AI component are ignored; the cut has to be a second component of the same mask. |
| `ai-dress` / `select_dress` | The AI outline cleaned with the photo's own colours. The method to use. |

`select_dress` returns one row per photo (`coverage_pct`, `bbox`, `removed_pct`, `masks_total`, warnings) and,
with `preview`, a contact sheet of the masks. Rerunning it replaces the mask of the same name, so there is never a duplicate.

## Safety model

- Only the folders in `config.json` are reachable, and that includes a reference image. `..`, other drives and
  non-image files are refused.
- Exports go to a subfolder and are refused if that folder holds the originals. Originals are never rewritten:
  edits are RapidRAW sidecars.
- `trash_photos` needs an explicit list (at most 200) and reports for each file whether it really left the disk.
- Mutating tools check the saved result (metadata re-read, sidecar compared, export listed from disk) instead of
  trusting the UI. `set_adjustments` accepts `expected_revision` to refuse edits on a photo that changed meanwhile.
- The pipe is local, restricted to the current user, and every connection is authenticated with a secret that
  lives in `%APPDATA%\RapidRAW-MCP\config.json`, so client configurations carry no secrets.
- The editor may be the session you are working in: agents are told never to close, restart or reopen it on their own.

## Diagnosing a problem

```powershell
.\server\target\debug\rapidraw-mcp.exe --doctor
```

`--doctor` checks the configuration, the installation, the processes and the bridge, and prints what to do about
each problem. It never starts or changes anything. Every call is also a line in
`%APPDATA%\RapidRAW-MCP\bridge.log` (UTC), written by the server, the bridge and the editor window with the
same short call id, so one failure can be followed across the three. RapidRAW's own `app.log` keeps the previous
run as `app.previous.log`.

If the editor loses its GPU and stops answering, the server restarts it once (at most every ten minutes) and the
reply says so; `restart_editor` is there for a frozen window. Photos and edits are saved as you go; jobs that were
running are lost, and the reply tells the agent to check what finished.

### Usage reports

Every agent session also leaves a Markdown report in `%APPDATA%\RapidRAW-MCP\reports\`, rewritten after every call:
the verdict (clean, warnings or failures), what needs attention (each failed call with its `[code]` and what to try,
and each call that succeeded but did not do everything, such as photos with `ok: false` or a sidecar that was not
saved), a table per operation (calls, failures, time) and the call log with the same call id as `bridge.log`. Only
names, timings and error text go in: no images or file contents.

While the bridge is on, the editor keeps at most two decoded photos in memory, which is what lets an agent work for
hours on a machine with limited RAM.

## Development

```powershell
cd server
cargo test                    # scheduling of the bridge (queue, jobs, cancel, reload), planner, reports
cargo build
cd ..\tests
python smoke.py               # handshake, tool list, error path
python smoke.py --live        # full suite against a running MCP-enabled editor
python stress.py <folder>     # several parallel clients, no crossed responses
python repro.py verify        # more scenarios: busy, abandon, queue, reload, jobs, verify
```

The live tests need the MCP-enabled editor open and work on throwaway files inside the first allowed folder.
`repro.py jobs` needs the editor started with `RAPIDRAW_MCP_DETACH_S=2`, and the scenarios that export need a folder
with a few sample RAW files (`RAPIDRAW_MCP_RAW_SOURCE`, default `raw-samples` inside the first allowed folder).
`RAPIDRAW_MCP_EXE` points the tests at another server binary. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains
how the pieces fit.

## Repository layout

| Folder | Contents | License |
|---|---|---|
| `server/` | The MCP server (`rapidraw-mcp.exe`): tools, planner, usage reports, doctor | MIT |
| `bridge/` | Rust shared by the server and the editor: pipe protocol, request scheduling, logging | MIT |
| `editor/` | The bridge inside RapidRAW (Rust and TypeScript), the patch for the files it edits, `apply.ps1` | AGPL-3.0 |
| `scripts/` | Setup, install and launch scripts | MIT |
| `tests/` | Smoke, stress and scenario tests | MIT |
| `docs/` | Architecture notes | MIT |

See [NOTICE.md](NOTICE.md) for why `editor/` is not MIT.

## Status and limits

- Built and tested on Windows 11 against RapidRAW commit `d30a65a803856b1e2f452cff998e0de8f13e7d0f` (version 1.6.7).
  Other versions may need the patch adapted.
- Used with Claude Code and OpenCode. The Codex configuration is generated by `setup.ps1` but has not been tested end to end.
- Windows only. Virtual copies and library import are not exposed.
- Copying a look measures brightness, contrast, whites, blacks, hue, saturation and luminance per colour family and
  tonal zone; grain, vignette and halation are not measured. A night flash photo cannot be given golden-hour light, and
  the tools say so instead of forcing it.
- Text-prompt AI masks fall back to the CPU when the GPU provider fails (about two minutes for 24 MP).
- `ai-face` can miss a face (for example under a veil); add a brush component to cover it.
- Reads that depend on editor state (`get_photo`, `list_photos`) queue behind a long job.
- An editor opened before a tool existed answers `[editor_outdated]` for that tool and works as before for the rest
  until it is reopened.
