//! `restart_editor`: get out of a frozen editor. A window that stopped answering (a GPU out-of-memory
//! error is the usual cause) can only be recovered by closing it; every edit is already saved in the
//! sidecars, so what is lost is the work that was running, not the photos.
use std::time::{Duration, Instant};

/// Ends the editor process and its web view. Ok(false) when no editor was running.
#[cfg(windows)]
pub fn kill_editor() -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/IM", "MCP RapidRAW.exe"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("could not run taskkill: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(128) => Ok(false), // nothing to kill
        other => Err(format!("taskkill ended with {other:?}")),
    }
}

#[cfg(not(windows))]
pub fn kill_editor() -> Result<bool, String> {
    Err("Windows only".into())
}

/// After a GPU crash RapidRAW switches itself to its OpenGL backend for the next run, and on some cards that
/// backend cannot compile the shaders and closes at the first use. Put the backend back to automatic and drop the
/// crash flag. Returns what was changed.
pub fn reset_gpu_backend() -> Vec<String> {
    let mut changed = Vec::new();
    let Some(dir) = std::env::var_os("APPDATA").map(std::path::PathBuf::from).map(|d| d.join("io.github.CyberTimon.RapidRAW")) else {
        return changed;
    };
    let settings = dir.join("settings.json");
    if let Ok(text) = std::fs::read_to_string(&settings) {
        for spelling in ["\"processingBackend\": \"gl\"", "\"processingBackend\":\"gl\""] {
            if text.contains(spelling) {
                let fixed = text.replace(spelling, "\"processingBackend\": \"auto\"");
                if std::fs::write(&settings, fixed).is_ok() {
                    changed.push("processingBackend gl -> auto".to_string());
                }
                break;
            }
        }
    }
    if std::fs::remove_file(dir.join(".gpu_init_crash_flag")).is_ok() {
        changed.push("removed the GPU crash flag".to_string());
    }
    changed
}

/// The editor's own log says it lost the GPU or panicked in this run. A window that then stops answering is
/// not busy, it is dead.
pub fn gpu_trouble_logged() -> bool {
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) else {
        return false;
    };
    std::fs::read(base.join("io.github.CyberTimon.RapidRAW").join("logs").join("app.log"))
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            text.contains("GPU device lost") || text.contains("PANIC!")
        })
        .unwrap_or(false)
}

/// The launcher skips starting an editor while a fresh lock file says another agent is doing it; after a
/// deliberate kill that lock is stale.
pub fn clear_launch_lock() {
    let _ = std::fs::remove_file(std::env::temp_dir().join("rapidraw-mcp-launch.lock"));
}

/// Waits until `gone()` is true, up to `limit`.
pub async fn wait_until(limit: Duration, mut gone: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if gone() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    gone()
}
