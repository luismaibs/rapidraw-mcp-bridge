use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

static BRIDGE_ON: AtomicBool = AtomicBool::new(false);

/// Decoded photos kept in memory. An agent that assesses or edits hundreds of photos in a row keeps decoding
/// big RAW files, and the default of five (about 300 MB each) ended in an out-of-memory abort after ~50 minutes
/// on a 7 GB machine, so while the bridge is on the cache holds at most two.
pub fn decoded_cache_cap(requested: usize) -> usize {
    if BRIDGE_ON.load(Ordering::Relaxed) { requested.min(2) } else { requested }
}


#[cfg(windows)]
#[path = "mcp_shared/wire.rs"]
mod wire;

#[cfg(windows)]
#[path = "mcp_shared/bridge_log.rs"]
mod bridge_log;

/// The editor window writes into the same bridge.log (timings, window visibility, state mismatches).
#[tauri::command]
pub fn mcp_log(window: tauri::Window, text: String) {
    if window.label() == "main" {
        #[cfg(windows)]
        bridge_log::editor(&text);
        #[cfg(not(windows))]
        let _ = text;
    }
}

/// Path rules for what an external agent may touch: only inside the configured roots.
#[cfg(windows)]
mod guard {
    use std::{
        io::{Error, Result},
        path::{Component, Path, PathBuf},
    };

    /// Strip the verbatim prefix so the editor sees the same path form it lists.
    pub fn display(path: &Path) -> Result<String> {
        let raw = path
            .to_str()
            .ok_or_else(|| Error::other("Invalid path encoding"))?;
        Ok(if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            raw.trim_start_matches(r"\\?\").to_string()
        })
    }

    fn inside(roots: &[PathBuf], path: &Path) -> bool {
        roots.iter().any(|r| path.starts_with(r))
    }

    /// Resolve a user-supplied path: absolute inside a root, or relative to the first root
    /// that contains it. Plain components only, so `..` can never escape.
    fn resolve(roots: &[PathBuf], value: &str) -> Result<PathBuf> {
        let given = Path::new(value);
        if given.is_absolute() {
            let canonical = given
                .canonicalize()
                .map_err(|e| Error::other(format!("Not found ({value}): {e}")))?;
            return if inside(roots, &canonical) {
                Ok(canonical)
            } else {
                Err(Error::other(format!("Path outside the allowed folders: {value}")))
            };
        }
        if !given
            .components()
            .all(|p| matches!(p, Component::Normal(_) | Component::CurDir))
        {
            return Err(Error::other("Expected a plain relative path or an absolute path inside an allowed folder"));
        }
        for root in roots {
            if let Ok(found) = root.join(given).canonicalize()
                && inside(roots, &found)
            {
                return Ok(found);
            }
        }
        Err(Error::other(format!("Not found in the allowed folders: {value}")))
    }

    pub fn file(roots: &[PathBuf], value: &str) -> Result<PathBuf> {
        if value.is_empty() {
            return Err(Error::other("Expected an image path"));
        }
        let file = resolve(roots, value)?;
        if !file.is_file() || !crate::formats::is_supported_image_file(&file) {
            return Err(Error::other(format!("Not a supported image: {value}")));
        }
        Ok(file)
    }

    pub fn folder(roots: &[PathBuf], value: &str) -> Result<PathBuf> {
        let dir = resolve(roots, if value.is_empty() { "." } else { value })?;
        if !dir.is_dir() {
            return Err(Error::other(format!("Not a folder: {value}")));
        }
        Ok(dir)
    }

    /// Export destination inside an allowed root; created on demand, never holding originals.
    pub fn output(roots: &[PathBuf], value: &str, sources: &[PathBuf]) -> Result<PathBuf> {
        let given = Path::new(value);
        if value.is_empty() || value == "." {
            return Err(Error::other("Export needs a subfolder, not the work folder itself"));
        }
        let target = if given.is_absolute() {
            given.to_path_buf()
        } else {
            if !given.components().all(|p| matches!(p, Component::Normal(_))) {
                return Err(Error::other("Export subfolder must be a plain relative name"));
            }
            let base = sources
                .first()
                .and_then(|s| roots.iter().find(|r| s.starts_with(r)))
                .or_else(|| roots.first())
                .ok_or_else(|| Error::other("No folders configured"))?;
            base.join(given)
        };
        // Check the nearest existing ancestor before creating anything.
        let mut probe = target.as_path();
        while !probe.exists() {
            probe = probe.parent().ok_or_else(|| Error::other("Invalid export folder"))?;
        }
        if !inside(roots, &probe.canonicalize()?) {
            return Err(Error::other("Export folder outside the allowed folders"));
        }
        std::fs::create_dir_all(&target)?;
        let target = target.canonicalize()?;
        if !inside(roots, &target) {
            return Err(Error::other("Export folder outside the allowed folders"));
        }
        if sources.iter().any(|s| s.parent() == Some(target.as_path())) {
            return Err(Error::other(
                "Export folder contains the originals; choose a different subfolder",
            ));
        }
        Ok(target)
    }

    /// Relative to the first root that contains it, otherwise absolute; always forward slashes.
    pub fn shown(roots: &[PathBuf], path: &Path) -> String {
        let text = roots
            .first()
            .filter(|r| path.starts_with(r))
            .and_then(|r| path.strip_prefix(r).ok())
            .unwrap_or(path);
        let text = text.to_string_lossy().replace('\\', "/");
        text.trim_start_matches("//?/").to_string()
    }
}

#[cfg(windows)]
fn snapshot(dir: &std::path::Path) -> std::collections::HashMap<std::ffi::OsString, (u64, std::time::SystemTime)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((e.file_name(), (m.len(), m.modified().ok()?)))
        })
        .collect()
}

/// Folder overview without touching the UI: subfolders and how many supported images each holds.
/// What a folder holds, read from the disk only: counts by type, RAW+JPEG pairs, edits already there.
/// It is what `plan_task` builds its plan from.
#[cfg(windows)]
fn inventory(roots: &[std::path::PathBuf], args: &Map<String, Value>) -> std::io::Result<Value> {
    use std::collections::{BTreeMap, HashMap};
    let dir = guard::folder(roots, args.get("folder").and_then(Value::as_str).unwrap_or("."))?;
    let mut by_extension: BTreeMap<String, u64> = BTreeMap::new();
    let mut shots: HashMap<String, (bool, bool)> = HashMap::new(); // stem -> (has a RAW, has another format)
    let (mut raw, mut other, mut sidecars, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if name.ends_with(".rrdata") {
            sidecars += 1;
            continue;
        }
        if !crate::formats::is_supported_image_file(&path) {
            continue;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        *by_extension.entry(ext).or_default() += 1;
        bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        let slot = shots.entry(stem).or_default();
        if crate::formats::is_raw_file(&path) {
            raw += 1;
            slot.0 = true;
        } else {
            other += 1;
            slot.1 = true;
        }
    }
    let pairs = shots.values().filter(|(r, o)| *r && *o).count();
    Ok(json!({
        "folder": guard::shown(roots, &dir),
        "images": raw + other,
        "raw": raw,
        "other": other,
        "by_extension": by_extension,
        "distinct_shots": shots.len(),
        "raw_plus_jpeg_pairs": pairs,
        "already_edited": sidecars,
        "size_mb": bytes / (1024 * 1024),
    }))
}

#[cfg(windows)]
fn list_folders(roots: &[std::path::PathBuf], args: &Map<String, Value>) -> std::io::Result<Value> {
    let dir = match args.get("folder").and_then(Value::as_str) {
        Some(v) => guard::folder(roots, v)?,
        None => {
            let shown: Vec<Value> = roots
                .iter()
                .map(|r| json!({"folder": guard::display(r).unwrap_or_default().replace('\\', "/"), "root": true}))
                .collect();
            return Ok(json!({"roots": shown}));
        }
    };
    let count = |d: &std::path::Path| {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file() && crate::formats::is_supported_image_file(&e.path()))
            .count()
    };
    let mut folders: Vec<Value> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| json!({"folder": guard::shown(roots, &e.path()), "images": count(&e.path())}))
        .collect();
    folders.sort_by_key(|v| v["folder"].as_str().unwrap_or("").to_lowercase());
    Ok(json!({"folder": guard::shown(roots, &dir), "images": count(&dir), "folders": folders}))
}

#[cfg(windows)]
#[path = "mcp_shared/bridge_core.rs"]
mod bridge_core;

/// Requests reach the editor as an event on its main window.
#[cfg(windows)]
struct MainWindow(tauri::AppHandle);

#[cfg(windows)]
impl bridge_core::Window for MainWindow {
    fn deliver(&self, request: Value) -> std::io::Result<()> {
        use tauri::Emitter;
        self.0
            .emit_to("main", "rapidraw-mcp-request", request)
            .map_err(std::io::Error::other)
    }
}

#[cfg(windows)]
static CORE: std::sync::OnceLock<std::sync::Arc<bridge_core::Core<MainWindow>>> = std::sync::OnceLock::new();

/// The editor window registers once its listener exists. `session` changes whenever the page is reloaded,
/// which tells the bridge that whatever was in flight is gone.
#[tauri::command]
pub fn mcp_ready(window: tauri::Window, session: String) {
    #[cfg(windows)]
    if window.label() == "main"
        && let Some(core) = CORE.get()
    {
        core.register(&session);
    }
    #[cfg(not(windows))]
    let _ = (window, session);
}

/// How far the request `id` has got, with the results so far. Counts as a sign of life for the watchdog.
#[tauri::command]
pub fn mcp_progress(window: tauri::Window, id: String, progress: Value, partial: Option<Value>) {
    #[cfg(windows)]
    if window.label() == "main"
        && let Some(core) = CORE.get()
    {
        core.progress(&id, progress, partial);
    }
    #[cfg(not(windows))]
    let _ = (window, id, progress, partial);
}

#[tauri::command]
pub fn mcp_reply(window: tauri::Window, id: String, result: Value) {
    #[cfg(windows)]
    if window.label() == "main"
        && let Some(core) = CORE.get()
    {
        core.reply(&id, result);
    }
    #[cfg(not(windows))]
    let _ = (window, id, result);
}

/// Validate one request against the allowed folders, hand it to the editor and check the outcome on disk.
#[cfg(windows)]
async fn dispatch(
    cmd: &Value,
    call: &str,
    roots: &[std::path::PathBuf],
    core: &std::sync::Arc<bridge_core::Core<MainWindow>>,
) -> std::io::Result<Value> {
    use std::{io::Error, path::PathBuf};
    let op = cmd["op"].as_str().unwrap_or("").to_string();
    if !wire::OPS.contains(&op.as_str()) {
        return Err(Error::other("Unknown operation"));
    }
    // A server that predates the version field is let through, and so is any version the editor still speaks.
    if let Some(proto) = cmd["proto"].as_u64()
        && !(u64::from(wire::MIN_PROTOCOL)..=u64::from(wire::PROTOCOL)).contains(&proto)
    {
        return Err(Error::other(format!(
            "[version_mismatch] The MCP server speaks protocol {proto} but this editor speaks {} to {}. Update the older one: \
             the server is rapidraw-mcp.exe and the editor is MCP RapidRAW.exe (scripts/install.ps1 refreshes both).",
            wire::MIN_PROTOCOL,
            wire::PROTOCOL
        )));
    }
    let mut args: Map<String, Value> = cmd.as_object().cloned().unwrap_or_default();
    for key in ["op", "call", "proto"] {
        args.remove(key);
    }

    if op == "list_folders" {
        return list_folders(roots, &args);
    }
    if op == "inventory" {
        return inventory(roots, &args);
    }

    let mut sources: Vec<PathBuf> = Vec::new();
    if let Some(v) = args.get("path").and_then(Value::as_str) {
        let file = guard::file(roots, v)?;
        args.insert("path".into(), json!(guard::display(&file)?));
        sources.push(file);
    }
    // The image whose look is copied: read only, but it must sit in an allowed folder like everything else.
    if let Some(v) = args.get("reference").and_then(Value::as_str) {
        let file = guard::file(roots, v)?;
        args.insert("reference".into(), json!(guard::display(&file)?));
    }
    if let Some(list) = args.get("paths") {
        let list = list
            .as_array()
            .ok_or_else(|| Error::other("paths must be a list"))?;
        if list.len() > 500 {
            return Err(Error::other("Too many photos in one request (max 500)"));
        }
        let mut resolved = Vec::new();
        for item in list {
            let item = item
                .as_str()
                .ok_or_else(|| Error::other("paths must be strings"))?;
            let file = guard::file(roots, item)?;
            resolved.push(json!(guard::display(&file)?));
            sources.push(file);
        }
        args.insert("paths".into(), Value::Array(resolved));
    }
    if let Some(v) = args.get("folder").and_then(Value::as_str) {
        let dir = guard::folder(roots, v)?;
        args.insert("folder".into(), json!(guard::display(&dir)?));
    }
    let mut export_dir: Option<PathBuf> = None;
    if op == "export" {
        let sub = args.get("subfolder").and_then(Value::as_str).unwrap_or("exports");
        let dir = guard::output(roots, sub, &sources)?;
        args.insert("output".into(), json!(guard::display(&dir)?));
        export_dir = Some(dir);
    }
    let before = export_dir.as_deref().map(snapshot);
    let shown_roots: Vec<String> = roots
        .iter()
        .map(|r| guard::display(r).unwrap_or_default())
        .collect();
    args.insert("root".into(), json!(shown_roots[0]));
    args.insert("roots".into(), json!(shown_roots));

    // The editor reports intent; the disk decides what actually happened. The check runs when the job
    // finishes, so it is part of the stored result even if the caller stopped waiting long ago.
    let finalize: Option<bridge_core::Finalize> = if op == "trash" || export_dir.is_some() {
        let roots = roots.to_vec();
        let trashing = op == "trash";
        Some(Box::new(move |reply: &mut Value| {
            if trashing {
                let files: Vec<Value> = sources
                    .iter()
                    .map(|f| json!({"path": guard::shown(&roots, f), "trashed": !f.exists()}))
                    .collect();
                reply["files"] = Value::Array(files);
            }
            if let (Some(dir), Some(before)) = (export_dir.as_deref(), before) {
                let after = snapshot(dir);
                let written: Vec<Value> = after
                    .iter()
                    .filter(|(n, v)| before.get(*n) != Some(v))
                    .map(|(n, (len, _))| {
                        json!({"file": format!("{}/{}", guard::shown(&roots, dir), n.to_string_lossy()), "bytes": len})
                    })
                    .collect();
                reply["written"] = Value::Array(written);
            }
        }))
    } else {
        None
    };
    core.run(&op, call, args, finalize).await
}

/// One connected agent: authenticate, read its request, answer it. Runs on its own task, so a slow
/// operation for one agent never keeps another from being heard.
#[cfg(windows)]
async fn serve(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    key: [u8; 32],
    roots: std::sync::Arc<Vec<std::path::PathBuf>>,
    core: std::sync::Arc<bridge_core::Core<MainWindow>>,
) {
    use std::{io::Error, time::Duration};
    use tokio::time::timeout;
    let received = timeout(Duration::from_secs(4), async {
        wire::authenticate(&mut pipe, &key, true).await?;
        let data = wire::read_frame(&mut pipe, wire::MAX_COMMAND).await?;
        Ok::<_, Error>(serde_json::from_slice::<Value>(&data)?)
    })
    .await;
    match &received {
        Err(_) => bridge_log::editor("client rejected: no valid request within 4 s (authentication or read)"),
        Ok(Err(e)) => bridge_log::editor(&format!("client rejected: {e}")),
        Ok(Ok(_)) => {}
    }
    if let Ok(Ok(cmd)) = received {
        let call = cmd["call"].as_str().unwrap_or("-").to_string();
        let op_name = cmd["op"].as_str().unwrap_or("?").to_string();
        let began = std::time::Instant::now();
        bridge_log::editor(&format!("{call} {op_name} received"));
        let result = dispatch(&cmd, &call, &roots, &core).await;
        let summary = match &result {
            Ok(v) if v["error"].is_string() => format!("error: {}", v["error"].as_str().unwrap_or("")),
            Ok(_) => "ok".to_string(),
            Err(e) => format!("bridge error: {e}"),
        };
        bridge_log::editor(&format!(
            "{call} {op_name} replied in {} ms: {summary}",
            began.elapsed().as_millis()
        ));
        let value = result.unwrap_or_else(|e| json!({"error": e.to_string()}));
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        let payload: &[u8] = if bytes.len() > wire::MAX_REPLY {
            bridge_log::editor(&format!("{call} {op_name} reply of {} bytes exceeds the bridge limit", bytes.len()));
            br#"{"error":"Reply too large for the bridge; request a smaller result"}"#
        } else {
            &bytes
        };
        match timeout(Duration::from_secs(5), wire::write_frame(&mut pipe, payload, wire::MAX_REPLY)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bridge_log::editor(&format!("{call} {op_name} reply NOT delivered: {e}")),
            Err(_) => bridge_log::editor(&format!("{call} {op_name} reply NOT delivered: client did not read it in 5 s")),
        }
    }
    // Drain the response before disconnecting (DisconnectNamedPipe discards unread bytes).
    let mut ack = [0; 1];
    let _ = timeout(Duration::from_secs(1), tokio::io::AsyncReadExt::read_exact(&mut pipe, &mut ack)).await;
    let _ = pipe.disconnect();
}

/// A new pipe instance for the next agent. Never gives up: the bridge must keep listening.
#[cfg(windows)]
async fn fresh_instance(name: &str) -> tokio::net::windows::named_pipe::NamedPipeServer {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        match ServerOptions::new().reject_remote_clients(true).create(name) {
            Ok(pipe) => return pipe,
            Err(e) => {
                log::error!("MCP pipe: {e}");
                bridge_log::editor(&format!("cannot open a pipe instance, retrying: {e}"));
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Start the bridge the moment the editor opens, however it was opened (taskbar, shortcut, an agent). When the
/// configuration is not usable yet (no config.json, no allowed folder that exists, a drive that is still
/// connecting) the editor stays a normal RapidRAW and tries again every 10 seconds, so the bridge comes up by
/// itself and nobody has to close the editor and open it again. Why it did not start goes to bridge.log.
#[cfg(windows)]
pub fn start(app: tauri::AppHandle) {
    let Err(first) = try_start(&app) else {
        return;
    };
    bridge_log::editor(&format!("bridge not started yet: {first}; trying again every 10 s"));
    std::thread::spawn(move || {
        let mut last = first;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            match try_start(&app) {
                Ok(()) => return,
                Err(why) if why != last => {
                    bridge_log::editor(&format!("bridge still not started: {why}"));
                    last = why;
                }
                Err(_) => {}
            }
        }
    });
}

/// Ok once the bridge listens; Err says what is missing.
#[cfg(windows)]
fn try_start(app: &tauri::AppHandle) -> Result<(), String> {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::net::windows::named_pipe::ServerOptions;
    let app = app.clone();
    let config = wire::config().map_err(|e| format!("cannot read the configuration: {e}"))?;
    if config.roots.is_empty() {
        // Not configured for the bridge: behave exactly like stock RapidRAW (and look again later).
        return Err("no allowed folders in RapidRAW-MCP/config.json (run scripts/setup.ps1)".into());
    }
    let roots: Vec<PathBuf> = config
        .roots
        .iter()
        .filter_map(|r| Path::new(r).canonicalize().ok())
        .filter(|r| r.is_dir())
        .collect();
    if roots.is_empty() {
        return Err(format!("none of the {} allowed folder(s) exists right now", config.roots.len()));
    }
    let key = wire::key().map_err(|e| format!("invalid secret: {e}"))?;
    BRIDGE_ON.store(true, Ordering::Relaxed);
    {
        use tauri::Manager;
        let size = crate::app_settings::load_settings(app.clone()).ok().and_then(|s| s.image_cache_size).unwrap_or(5) as usize;
        let state = app.state::<crate::app_state::AppState>();
        state.decoded_image_cache.lock().unwrap().set_capacity(decoded_cache_cap(size));
    }
    let name = wire::pipe_name(&key);
    let core = bridge_core::Core::new(MainWindow(app), bridge_core::Limits::production());
    let _ = CORE.set(core.clone());
    let roots = Arc::new(roots);
    bridge_log::editor(&format!(
        "bridge starting, {} allowed folder(s), protocol {}",
        roots.len(),
        wire::PROTOCOL
    ));
    tauri::async_runtime::spawn(async move {
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name);
        let mut listening = match first {
            Ok(p) => p,
            Err(e) => {
                log::error!("MCP pipe: {e}");
                bridge_log::editor(&format!("bridge NOT listening, cannot create the pipe: {e}"));
                return;
            }
        };
        bridge_log::editor("bridge listening");
        loop {
            if let Err(e) = listening.connect().await {
                bridge_log::editor(&format!("pipe connect failed, listening again: {e}"));
                tokio::time::sleep(Duration::from_millis(500)).await;
                listening = fresh_instance(&name).await;
                continue;
            }
            // A fresh instance is listening before this agent is served, and every agent has its own task.
            let client = listening;
            listening = fresh_instance(&name).await;
            tauri::async_runtime::spawn(serve(client, key, roots.clone(), core.clone()));
        }
    });
    Ok(())
}
