//! `rapidraw-mcp --doctor`: one command that says why the connector is not working and what to do about
//! it. It never starts the editor and never changes anything; run it yourself or ask an agent to.
use super::wire;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};
use tokio::time::timeout;

struct Report {
    problems: usize,
}

impl Report {
    fn ok(&self, what: &str, detail: impl std::fmt::Display) {
        println!("  ok    {what}: {detail}");
    }
    fn warn(&self, what: &str, detail: impl std::fmt::Display) {
        println!("  note  {what}: {detail}");
    }
    fn fail(&mut self, what: &str, detail: impl std::fmt::Display, fix: &str) {
        self.problems += 1;
        println!("  FAIL  {what}: {detail}");
        println!("        -> {fix}");
    }
}

fn age(path: &std::path::Path) -> String {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    match modified.and_then(|m| SystemTime::now().duration_since(m).ok()) {
        Some(d) if d.as_secs() < 3600 => format!("{} min ago", d.as_secs() / 60),
        Some(d) if d.as_secs() < 172_800 => format!("{} h ago", d.as_secs() / 3600),
        Some(d) => format!("{} days ago", d.as_secs() / 86_400),
        None => "unknown date".to_string(),
    }
}

#[cfg(windows)]
fn processes() -> String {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default()
}

/// One `status` request, without starting anything. Err is a ready-to-print reason.
#[cfg(windows)]
async fn status() -> Result<(Value, u128), (Option<i32>, String)> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let key = wire::key().map_err(|e| (None, e.to_string()))?;
    let name = wire::pipe_name(&key);
    let began = Instant::now();
    let mut pipe = ClientOptions::new().open(&name).map_err(|e| (e.raw_os_error(), e.to_string()))?;
    let exchange = async {
        wire::authenticate(&mut pipe, &key, false).await?;
        let cmd = json!({"op": "status", "proto": wire::PROTOCOL, "call": "doctor00"});
        wire::write_frame(&mut pipe, &serde_json::to_vec(&cmd)?, wire::MAX_COMMAND).await?;
        let reply = wire::read_frame(&mut pipe, wire::MAX_REPLY).await?;
        let _ = tokio::io::AsyncWriteExt::write_all(&mut pipe, &[0]).await;
        Ok::<_, std::io::Error>(serde_json::from_slice::<Value>(&reply)?)
    };
    match timeout(Duration::from_secs(15), exchange).await {
        Ok(Ok(value)) => Ok((value, began.elapsed().as_millis())),
        Ok(Err(e)) => Err((None, format!("the bridge closed the connection: {e}"))),
        Err(_) => Err((None, "the bridge did not answer within 15 s".to_string())),
    }
}

fn gl_backend() -> bool {
    let Some(base) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return false;
    };
    std::fs::read_to_string(base.join("io.github.CyberTimon.RapidRAW").join("settings.json"))
        .map(|t| t.contains("\"processingBackend\": \"gl\"") || t.contains("\"processingBackend\":\"gl\""))
        .unwrap_or(false)
}

/// The last line in the editor's own log that says it ran out of GPU memory or panicked.
fn editor_crash() -> Option<String> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let text = std::fs::read(base.join("io.github.CyberTimon.RapidRAW").join("logs").join("app.log")).ok()?;
    let text = String::from_utf8_lossy(&text).to_string();
    text.lines()
        .rev()
        .find(|l| l.contains("GPU device lost") || l.contains("PANIC!"))
        .map(|l| l.chars().take(180).collect())
}

fn log_tail() -> Vec<String> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    let Some(text) = base.and_then(|b| std::fs::read(b.join("RapidRAW-MCP").join("bridge.log")).ok()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&text).to_string();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(300);
    lines[start..]
        .iter()
        .filter(|l| {
            ["TIMEOUT", "NOT delivered", "STOPPED", "NOT listening", "rejected", "OUT OF SYNC", "failed", "reloaded", "gave up"]
                .iter()
                .any(|k| l.contains(k))
        })
        .map(|l| l.to_string())
        .collect()
}

pub async fn run() -> i32 {
    let mut report = Report { problems: 0 };
    println!("RapidRAW MCP doctor (server pid {}, protocol {})\n", std::process::id(), wire::PROTOCOL);

    println!("Configuration");
    let config = wire::config().ok();
    match &config {
        Some(c) if c.secret.len() >= 32 => report.ok("secret", "set"),
        _ => report.fail("secret", "missing or shorter than 32 characters", "run scripts/setup.ps1 once (it creates %APPDATA%\\RapidRAW-MCP\\config.json)"),
    }
    match &config {
        Some(c) if !c.roots.is_empty() => {
            for root in &c.roots {
                if std::path::Path::new(root).is_dir() {
                    report.ok("allowed folder", root);
                } else {
                    report.fail("allowed folder", format!("{root} does not exist"), "restore the folder or run scripts/setup.ps1 -Roots <folder>");
                }
            }
        }
        _ => report.fail("allowed folders", "none configured", "run scripts/setup.ps1 -Roots <folder with the photo copies>"),
    }

    println!("\nInstallation");
    let exe = std::env::current_exe().ok();
    if let Some(exe) = &exe {
        report.ok("server", format!("{} (built {})", exe.display(), age(exe)));
        let editor = exe.with_file_name("MCP RapidRAW.exe");
        if editor.is_file() {
            report.ok("editor", format!("{} (built {})", editor.display(), age(&editor)));
        } else {
            report.fail("editor", "MCP RapidRAW.exe is not next to the server", "run scripts/install.ps1, or open the MCP-enabled build by hand");
        }
    }

    #[cfg(windows)]
    {
        println!("\nProcesses");
        let running = processes();
        let editor_up = running.contains("\"mcp rapidraw.exe\"");
        let stock_up = running.contains("\"rapidraw.exe\"");
        if editor_up {
            report.ok("MCP RapidRAW", "running");
        } else {
            report.warn("MCP RapidRAW", "not running (an agent's first call will start it)");
        }
        if stock_up {
            report.fail("regular RapidRAW", "is open and has no agent bridge", "close it; it shares settings with MCP RapidRAW");
        }

        println!("\nBridge");
        match status().await {
            Ok((reply, ms)) => {
                if let Some(error) = reply["error"].as_str() {
                    let fix = if error.contains("[version_mismatch]") {
                        "the server and the editor are different versions: run scripts/install.ps1 (or update the older one)"
                    } else if error.contains("[ui_not_ready]") {
                        "the editor is open but its window never registered: wait a moment, then close and reopen MCP RapidRAW"
                    } else if error.contains("[ui_not_responding]") {
                        "the editor window is frozen or still loading: wait, then close and reopen MCP RapidRAW"
                    } else {
                        "see bridge.log below"
                    };
                    report.fail("status", error, fix);
                } else {
                    report.ok("status", format!("answered in {ms} ms"));
                    let bridge = &reply["bridge"];
                    match bridge["protocol"].as_u64() {
                        Some(p) if p == u64::from(wire::PROTOCOL) => report.ok("protocol", p),
                        Some(p) => report.fail("protocol", format!("editor speaks {p}, this server speaks {}", wire::PROTOCOL), "run scripts/install.ps1 so both are the same build"),
                        None => report.warn("protocol", "the editor did not report one (older build)"),
                    }
                    println!("        view {}, folder {}, {} photo(s), active {}",
                        reply["view"], reply["folder"], reply["photos_total"], reply["active_photo"]);
                    match bridge["running"].as_object() {
                        Some(job) => println!("        busy with {} for {} s{}", job["op"], job["for_s"],
                            if job["overdue"] == true { " (overdue: no sign of progress)" } else { "" }),
                        None => println!("        idle, {} queued", bridge["queued"]),
                    }
                }
            }
            Err((Some(2), _)) => {
                if editor_up {
                    report.fail("pipe", "the editor is running but the bridge is not listening", "check that folders are configured (scripts/setup.ps1) and read bridge.log; close and reopen MCP RapidRAW");
                } else {
                    report.warn("pipe", "not available because the editor is not running");
                }
            }
            Err((Some(231), _)) => report.warn("pipe", "busy serving other agents; try again"),
            Err((_, why)) => report.fail("bridge", why, "close and reopen MCP RapidRAW, then run this again"),
        }
    }
    #[cfg(not(windows))]
    report.warn("bridge", "Windows only");

    println!("\nGPU settings");
    match gl_backend() {
        true => report.fail(
            "processingBackend",
            "is gl, which RapidRAW sets by itself after a GPU crash",
            "on cards where OpenGL cannot compile the shaders the editor closes at the first use; call restart_editor (it puts the backend back to auto) or set Processing backend to Auto in RapidRAW's settings",
        ),
        false => report.ok("processingBackend", "not forced to gl"),
    }

    println!("\nEditor log");
    match editor_crash() {
        Some(line) => report.fail(
            "app.log",
            line,
            "the editor lost its GPU or crashed; close and reopen MCP RapidRAW (an agent can call restart_editor). Fewer large previews at once, or closing other GPU users, avoids it",
        ),
        None => println!("  none"),
    }

    let recent = log_tail();
    println!("\nRecent problems in bridge.log (last 300 lines)");
    if recent.is_empty() {
        println!("  none");
    } else {
        for line in recent.iter().rev().take(6).rev() {
            println!("  {line}");
        }
    }

    if report.problems == 0 {
        println!("\nNo problems found.");
        0
    } else {
        println!("\n{} problem(s) found.", report.problems);
        1
    }
}
