# Architecture

## Pieces

```
MCP client ──stdio──> rapidraw-mcp.exe ──named pipe──> bridge (Rust, in RapidRAW) ──event──> editor window (TypeScript)
                       server/                          editor/src-tauri/src/mcp_bridge.rs    editor/src/hooks/useMcpBridge.ts
                                                        bridge/bridge_core.rs
```

- **Server** (`server/`). A stdio MCP server, one process per agent session. It describes the tools, turns each call
  into one request on the pipe, and turns the reply (text plus an optional JPEG) into an MCP result. It starts the
  installed editor when no bridge is listening.
- **Bridge** (`editor/src-tauri/src/mcp_bridge.rs`, `bridge/`). Listens on the pipe inside RapidRAW. It checks paths
  against the allowed folders, schedules requests, and verifies on disk what the editor reports having done.
- **Editor window** (`editor/src/hooks/useMcpBridge.ts`). Runs each request through the same stores and commands the
  user interface uses (library, adjustments, masks, history, export), so the agent and the person share one state.

## Pipe protocol (`bridge/wire.rs`)

One request per connection. Both sides first prove they know the shared secret (a mutual challenge with fresh
nonces and a role byte), then the server sends one length-prefixed JSON frame and reads one reply frame. The pipe
name is derived from the secret, remote clients are rejected, and frame sizes are bounded. The request carries a
protocol version; a different number is refused with the side to update.

## Scheduling and jobs (`bridge/bridge_core.rs`)

The editor is one window with one selected photo, so two edits must never overlap.

- `status`, `list_folders`, `cancel_export`, `job_status` and `cancel_job` are answered at once.
- Every other operation is a **job**. Jobs take a turn one at a time, in arrival order.
- A caller waits about 45 seconds. If the job is still going it receives the job id and the job carries on by itself.
  A job survives its caller disconnecting.
- A job that goes silent (no progress report, no answer) is marked overdue but keeps its place until the window
  answers or reloads, so nothing ever runs on top of it. A job that keeps reporting progress can take as long as it needs.
- If the caller leaves before its job gets a turn, the job is dropped: nobody could collect its result.
- When the editor page reloads it registers a new session id, and requests that were in flight end with `[ui_reloaded]`.
- Finished jobs are kept for an hour (the last 12). A finishing hook runs on the result before it is stored; the
  bridge uses it to list the files an export wrote and to confirm what a trash really removed.

The scheduler talks to the window through a small trait, so `cargo test` exercises all of the above against a fake
editor without starting RapidRAW.

## Saving and verification (`useMcpBridge.ts`)

`saved: true` means the sidecar on disk was written and read back with the same masks and the requested values.
The write is made directly and awaited, instead of waiting for the editor's autosave timer. AI bitmaps are stored
apart from the sidecar, so masks are compared by id. `open_photo` waits until the editor and the saved sidecar agree
before it answers, and `paste_adjustments` / `reset_adjustments` confirm each photo from its sidecar because those
commands return before they have written.

## Dress selection

`ai-dress` asks the AI for a silhouette and cleans it with the photo's own pixels at 800 px: the dress color is read
from the calm, lit half of the silhouette, then skin (by hue), dark hair, color outliers and anything the caller
excluded are dropped, thin bridges are cut, and only the biggest connected piece plus small neighbours are kept.
The result is multiplied into the AI bitmap at its original size so the edge keeps the AI's sharpness. The reply
reports what was removed and where the mask landed.

## Planning and assessment

`plan_task` runs in the server. The editor bridge answers an `inventory` request from disk only (counts, RAW+JPEG
pairs, edits already present), and `server/src/plan.rs` turns the goal into an ordered plan of tools with real
numbers. Goals are read in English and Spanish; the intent words are folded to lowercase ASCII before matching.

`assess_photos` renders each photo at a fixed size with its own edits removed, measures a Laplacian energy per tile,
takes the flattest tiles as that photo's noise floor and subtracts it, and (with faces) measures the largest face from
the AI face mask. Photos are ranked against others of the same file type, so one camera's character is not mistaken
for blur.

## Looks from a reference (`mcpPalette.ts`, `mcpLook.ts`)

A look is read as a palette, not as averages. Whites and blacks are measured on near-neutral pixels (the brightest and
darkest 8%, low saturation), so they carry the colour of the light and of the grade and not the colour of objects.
Coloured pixels are bucketed into the editor's eight HSL bands (nearest band centre) and each band reports its share,
saturation, luminance and circular mean hue. `compare_to_reference` turns the difference between a photo's palette
and the reference's into suggestions in the editor's own controls; `style_photo` applies a patch to one photo and
re-measures it. The decision about what to apply stays with the agent, which sees the picture.

`mcpLook.ts` holds tonal and colour-cast statistics, targets and small closed-loop controllers used by the
experimental `match_style` pass (tone only by default). It is pure and has no editor state.

## Reports

`server/src/report.rs` writes one Markdown report per agent session after every call: verdict, what needs attention
with the advice for each error code, a table per operation and the call log. A connection that never calls a tool
leaves nothing behind.

## Diagnostics

`%APPDATA%\RapidRAW-MCP\bridge.log` collects short UTC lines from the server, the bridge and the editor window, all
tagged with the same eight-character call id. `rapidraw-mcp.exe --doctor` reads it, checks the configuration, the
installation, the processes and a live `status` round trip, and prints the fix for each problem.

## Files outside the repository

| Path | Purpose |
|---|---|
| `%APPDATA%\RapidRAW-MCP\config.json` | Shared secret and allowed folders |
| `%APPDATA%\RapidRAW-MCP\bridge.log` | Call log (2 MB, one older generation) |
| `%LOCALAPPDATA%\Programs\MCP RapidRAW\` | Installed editor and server |
