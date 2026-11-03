//! One Markdown report per agent session, so every use of the MCP is documented: what was called, what worked,
//! what failed and what to try. It lives in `%APPDATA%\RapidRAW-MCP\reports\` and is rewritten after every call,
//! so it is complete even when the agent just closes the connection.
//! Only operation facts go in (names, timings, error text); never images or file contents.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_ROWS: usize = 400;
const MAX_DETAIL: usize = 300;
const SLOW_S: u64 = 45;
/// Bulk or hard-to-undo operations that should follow plan_task.
const BULK: &[&str] = &["trash", "select_dress", "select_skin", "export", "assess", "match_style"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "OK",
            Verdict::Warn => "WARN",
            Verdict::Fail => "FAIL",
        }
    }
}

struct Entry {
    at: String,
    op: String,
    call: String,
    ms: u128,
    verdict: Verdict,
    detail: String,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    notes: Vec<String>,
    client: String,
    planned: bool,
    path: Option<PathBuf>,
}

pub struct Report {
    started: String,
    clock: Instant,
    inner: Mutex<Inner>,
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

fn clip(text: &str, max: usize) -> String {
    let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let clean = clean.replace('|', "/");
    if clean.chars().count() > max {
        format!("{}...", clean.chars().take(max).collect::<String>())
    } else {
        clean
    }
}

/// The `[code]` an error starts with, when it has one.
fn code_of(message: &str) -> Option<&str> {
    let rest = message.trim_start().strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(&rest[..end])
}

/// What to try for an error code: the same advice the README gives, so the report is useful on its own.
fn hint(code: &str) -> &'static str {
    match code {
        "editor_not_open" => "Open MCP RapidRAW (the Desktop shortcut) and try again. --doctor shows whether it is installed.",
        "editor_not_installed" => "MCP RapidRAW.exe is missing next to the server: run scripts/install.ps1 or open the build by hand.",
        "stock_rapidraw_open" => "The regular RapidRAW is open and has no bridge: close it; MCP RapidRAW opens by itself.",
        "bridge_not_started" => "The editor opened but its bridge did not come up: check that config.json lists folders (setup.ps1) and read bridge.log.",
        "editor_without_bridge" => "The editor is open without the bridge (it was opened before the bridge could start, or it is an old build). It was left alone so the session is not interrupted: close and reopen MCP RapidRAW when convenient; the bridge starts by itself and bridge.log says why it did not.",
        "ui_not_ready" => "The window is still loading: wait a few seconds and try again.",
        "ui_not_responding" => "The window is frozen (usually GPU memory): use restart_editor and read bridge.log.",
        "ui_reloaded" => "The window reloaded during the call: check with get_photo/list_photos that it finished and repeat what is missing.",
        "version_mismatch" => "Server and editor are different versions: reinstall with install.ps1 or copy both executables from the same build.",
        "unknown_job" => "The job no longer exists (they are kept for 1 h, the last 12) or the editor restarted.",
        "bridge_connection" => "The connection to the editor was lost: the operation may have finished; check status before repeating.",
        "server_timeout" => "The editor did not answer in 300 s: the operation may have finished; check status and bridge.log.",
        "editor_restarted" => "The server restarted the editor after it lost the GPU: what was saved is safe; repeat what did not finish.",
        "restart_failed" => "The editor could not be restarted: close it from Task Manager and open MCP RapidRAW.",
        "mask_not_created" => "The mask was not created and the photo keeps the previous one: see the cause in the message; retry with region/exclude or a different prompt.",
        _ => "",
    }
}

/// Signs that a call that came back as a success still did not do everything: per-photo failures, an unsaved
/// sidecar, warnings, a job that failed. Returns the severity and a short description.
pub fn assess_reply(reply: &Value) -> (Verdict, String) {
    let mut found: Vec<String> = Vec::new();
    let mut verdict = Verdict::Ok;
    let bump = |to: Verdict, text: String, found: &mut Vec<String>, v: &mut Verdict| {
        if to == Verdict::Fail || *v == Verdict::Ok {
            *v = to;
        }
        found.push(text);
    };
    if let Some(state) = reply["state"].as_str() {
        match state {
            "failed" => bump(Verdict::Fail, format!("job failed: {}", reply["error"].as_str().unwrap_or("no detail")), &mut found, &mut verdict),
            "cancelled" => bump(Verdict::Warn, "job cancelled".into(), &mut found, &mut verdict),
            "running" | "queued" | "cancelling" => found.push(format!("job {} ({state})", reply["job"].as_str().unwrap_or("?"))),
            _ => {}
        }
    }
    if reply["ok"] == Value::Bool(false) {
        let why = reply["error"].as_str().or_else(|| reply["reason"].as_str()).unwrap_or("ok=false");
        bump(Verdict::Warn, why.to_string(), &mut found, &mut verdict);
    }
    if reply["saved"] == Value::Bool(false) {
        let why = reply["save_problem"].as_str().unwrap_or("saved=false");
        bump(Verdict::Warn, format!("not saved: {why}"), &mut found, &mut verdict);
    }
    let failed = match &reply["failed"] {
        Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
        Value::Array(a) => a.len(),
        _ => 0,
    };
    if failed > 0 {
        let total = reply["photos"].as_u64().or_else(|| reply["count"].as_u64());
        let of = total.map(|t| format!(" of {t}")).unwrap_or_default();
        bump(Verdict::Warn, format!("{failed}{of} photos failed"), &mut found, &mut verdict);
    }
    for key in ["results", "files", "rows", "photos"] {
        if let Some(rows) = reply[key].as_array() {
            let bad: Vec<&Value> = rows.iter().filter(|r| r["ok"] == Value::Bool(false)).collect();
            if !bad.is_empty() && failed == 0 {
                let first = bad[0]["error"].as_str().or_else(|| bad[0]["reason"].as_str()).unwrap_or("");
                bump(Verdict::Warn, format!("{} of {} photos with ok=false ({first})", bad.len(), rows.len()), &mut found, &mut verdict);
            }
        }
    }
    for key in ["warning", "warnings"] {
        match &reply[key] {
            Value::String(s) if !s.is_empty() => bump(Verdict::Warn, format!("warning: {s}"), &mut found, &mut verdict),
            Value::Array(a) if !a.is_empty() => {
                let first = a[0].as_str().map(str::to_owned).unwrap_or_else(|| a[0].to_string());
                bump(Verdict::Warn, format!("{} warnings, the first: {first}", a.len()), &mut found, &mut verdict);
            }
            _ => {}
        }
    }
    (verdict, found.join("; "))
}

impl Report {
    pub fn new() -> Self {
        let started = crate::bridge_log::stamp();
        let path = dir().map(|d| d.join(format!("{}_pid{}.md", started.replace([' ', ':'], "-").trim_end_matches('Z').split('.').next().unwrap_or("session"), std::process::id())));
        Report {
            started,
            clock: Instant::now(),
            inner: Mutex::new(Inner { path, ..Inner::default() }),
        }
    }

    pub fn client(&self, name: String) {
        self.inner.lock().unwrap().client = name;
        self.flush();
    }

    pub fn planned(&self) {
        self.inner.lock().unwrap().planned = true;
    }

    /// A fact worth reading later that is not a call (an automatic restart, for instance).
    pub fn note(&self, text: &str) {
        let line = format!("{} {}", crate::bridge_log::stamp(), clip(text, MAX_DETAIL));
        self.inner.lock().unwrap().notes.push(line);
        self.flush();
    }

    /// One call to the editor. `result` is the reply on success or the error text on failure.
    pub fn call(&self, op: &str, call: &str, took: Duration, result: Result<&Value, &str>) {
        let (verdict, detail) = match result {
            Ok(reply) => assess_reply(reply),
            Err(message) => (Verdict::Fail, message.to_string()),
        };
        {
            let mut inner = self.inner.lock().unwrap();
            if BULK.contains(&op) && !inner.planned && !inner.notes.iter().any(|n| n.contains(&format!("`{op}` without a previous plan_task"))) {
                let line = format!("{} `{op}` without a previous plan_task in this session (bulk or hard-to-undo operation)", crate::bridge_log::stamp());
                inner.notes.push(line);
            }
            inner.entries.push(Entry {
                at: crate::bridge_log::stamp(),
                op: op.to_string(),
                call: call.to_string(),
                ms: took.as_millis(),
                verdict,
                detail: clip(&detail, MAX_DETAIL),
            });
        }
        self.flush();
    }

    fn flush(&self) {
        if cfg!(test) {
            return;
        }
        let (path, text) = {
            let inner = self.inner.lock().unwrap();
            // A connection that never calls a tool (most of them) leaves nothing behind.
            if inner.entries.is_empty() && inner.notes.is_empty() {
                return;
            }
            let Some(path) = inner.path.clone() else {
                return;
            };
            (path, render(&self.started, self.clock.elapsed(), &inner))
        };
        // Write beside the target and swap, so a reader never sees half a report; fall back to a direct write
        // when the file is held open by an editor.
        let tmp = path.with_extension("md.tmp");
        if fs::write(&tmp, &text).is_ok() && fs::rename(&tmp, &path).is_ok() {
            return;
        }
        let _ = fs::write(&path, text);
    }
}

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join("AppData").join("Roaming")))?;
    let dir = base.join("RapidRAW-MCP").join("reports");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[derive(Default)]
struct Totals {
    calls: usize,
    ok: usize,
    warn: usize,
    fail: usize,
    ms: u128,
    slowest: u128,
}

fn render(started: &str, uptime: Duration, inner: &Inner) -> String {
    let mut by_op: BTreeMap<&str, Totals> = BTreeMap::new();
    let (mut ok, mut warn, mut fail) = (0, 0, 0);
    for e in &inner.entries {
        let t = by_op.entry(&e.op).or_default();
        t.calls += 1;
        t.ms += e.ms;
        t.slowest = t.slowest.max(e.ms);
        match e.verdict {
            Verdict::Ok => {
                t.ok += 1;
                ok += 1;
            }
            Verdict::Warn => {
                t.warn += 1;
                warn += 1;
            }
            Verdict::Fail => {
                t.fail += 1;
                fail += 1;
            }
        }
    }
    let verdict = if inner.entries.is_empty() {
        "No calls yet".to_string()
    } else if fail > 0 {
        format!("WITH FAILURES: {fail} failure(s), {warn} warning(s), {ok} successful")
    } else if warn > 0 {
        format!("WITH WARNINGS: {warn} warning(s), {ok} successful, no failures")
    } else {
        format!("CLEAN: {ok} calls, no failures or warnings")
    };
    let mut out = String::new();
    out.push_str("# MCP RapidRAW usage report\n\n");
    out.push_str(&format!("- **Result:** {verdict}\n"));
    out.push_str(&format!("- **Started (UTC):** {started}\n"));
    out.push_str(&format!("- **Session length:** {} s\n", uptime.as_secs()));
    let client = if inner.client.is_empty() { "unknown" } else { &inner.client };
    out.push_str(&format!("- **Agent (MCP client):** {client}\n"));
    out.push_str(&format!(
        "- **Server:** rapidraw-mcp {} (PID {}, protocol {})\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        crate::wire::PROTOCOL
    ));
    out.push_str("- Rewritten after every call. Technical details are in `bridge.log` under the same call id.\n\n");

    out.push_str("## What needs attention\n\n");
    let problems: Vec<&Entry> = inner.entries.iter().filter(|e| e.verdict != Verdict::Ok).collect();
    if problems.is_empty() && inner.notes.is_empty() {
        out.push_str("Nothing: every call finished well.\n\n");
    }
    for (i, e) in problems.iter().enumerate() {
        out.push_str(&format!(
            "{}. **{}** `{}` at {} (call `{}`, {} ms): {}\n",
            i + 1,
            e.verdict.label(),
            e.op,
            e.at,
            e.call,
            e.ms,
            e.detail
        ));
        if let Some(code) = code_of(&e.detail) {
            let advice = hint(code);
            if !advice.is_empty() {
                out.push_str(&format!("   - What to try: {advice}\n"));
            }
        }
    }
    if !problems.is_empty() {
        out.push('\n');
    }
    if !inner.notes.is_empty() {
        out.push_str("### Automatic observations\n\n");
        for n in &inner.notes {
            out.push_str(&format!("- {n}\n"));
        }
        out.push('\n');
    }
    let slow: Vec<&Entry> = inner.entries.iter().filter(|e| e.ms > u128::from(SLOW_S) * 1000).collect();
    if !slow.is_empty() {
        out.push_str(&format!("### Calls longer than {SLOW_S} s\n\n"));
        for e in slow {
            out.push_str(&format!("- `{}` {} ms (call `{}`)\n", e.op, e.ms, e.call));
        }
        out.push('\n');
    }

    out.push_str("## Summary by operation\n\n");
    out.push_str("| Operation | Calls | OK | Warnings | Failures | Total time (s) | Slowest (ms) |\n|---|---|---|---|---|---|---|\n");
    for (op, t) in &by_op {
        out.push_str(&format!(
            "| `{op}` | {} | {} | {} | {} | {:.1} | {} |\n",
            t.calls,
            t.ok,
            t.warn,
            t.fail,
            t.ms as f64 / 1000.0,
            t.slowest
        ));
    }

    out.push_str("\n## Call log\n\n");
    let total = inner.entries.len();
    // Past the cap, every problem is kept and the plain successes in the middle are the ones left out.
    let keep: Vec<&Entry> = if total > MAX_ROWS {
        let tail_from = total - MAX_ROWS / 2;
        inner
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| *i < MAX_ROWS / 2 || *i >= tail_from || e.verdict != Verdict::Ok)
            .map(|(_, e)| e)
            .collect()
    } else {
        inner.entries.iter().collect()
    };
    if keep.len() < total {
        out.push_str(&format!("Showing {} of {total} calls (every call that failed or warned, plus the first and the last).\n\n", keep.len()));
    }
    out.push_str("| Time (UTC) | Operation | Call | ms | Result | Detail |\n|---|---|---|---|---|---|\n");
    for e in keep {
        out.push_str(&format!(
            "| {} | `{}` | `{}` | {} | {} | {} |\n",
            e.at.get(11..23).unwrap_or(&e.at),
            e.op,
            e.call,
            e.ms,
            e.verdict.label(),
            e.detail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Report {
        Report::new()
    }

    #[test]
    fn plain_success_is_ok() {
        let (v, d) = assess_reply(&json!({"photos": 1, "done": 1, "failed": 0, "results": [{"ok": true}]}));
        assert_eq!(v, Verdict::Ok);
        assert!(d.is_empty());
    }

    #[test]
    fn per_photo_failures_are_warnings() {
        let (v, d) = assess_reply(&json!({"photos": 3, "failed": 1, "results": [{"ok": true}, {"ok": false, "error": "no bride"}]}));
        assert_eq!(v, Verdict::Warn);
        assert!(d.contains("1 of 3 photos failed"), "{d}");
    }

    #[test]
    fn rows_without_a_failed_count_are_still_noticed() {
        let (v, d) = assess_reply(&json!({"files": [{"ok": true}, {"ok": false, "error": "disk"}]}));
        assert_eq!(v, Verdict::Warn);
        assert!(d.contains("1 of 2") && d.contains("disk"), "{d}");
    }

    #[test]
    fn unsaved_and_failed_jobs_are_reported() {
        assert_eq!(assess_reply(&json!({"saved": false, "save_problem": "masks differ"})).0, Verdict::Warn);
        let (v, d) = assess_reply(&json!({"job": "j1", "state": "failed", "error": "[ui_reloaded] gone"}));
        assert_eq!(v, Verdict::Fail);
        assert!(d.contains("ui_reloaded"));
        assert_eq!(assess_reply(&json!({"job": "j2", "state": "running"})).0, Verdict::Ok);
    }

    #[test]
    fn warnings_array_is_a_warning() {
        let (v, d) = assess_reply(&json!({"warnings": ["file looks damaged"]}));
        assert_eq!(v, Verdict::Warn);
        assert!(d.contains("damaged"));
    }

    #[test]
    fn code_is_read_from_the_message() {
        assert_eq!(code_of("[ui_not_responding] The window"), Some("ui_not_responding"));
        assert_eq!(code_of("plain"), None);
        assert!(!hint("ui_not_responding").is_empty());
    }

    #[test]
    fn report_lists_problems_with_advice_and_a_summary() {
        let r = session();
        r.call("status", "aaaa1111", Duration::from_millis(40), Ok(&json!({"ok": true})));
        r.call("select_dress", "bbbb2222", Duration::from_millis(61_000), Err("[ui_not_responding] frozen"));
        r.call("add_mask", "cccc3333", Duration::from_millis(900), Ok(&json!({"failed": 2, "photos": 5})));
        let text = render(&r.started, Duration::from_secs(3), &r.inner.lock().unwrap());
        assert!(text.contains("WITH FAILURES: 1 failure(s), 1 warning(s), 1 successful"), "{text}");
        assert!(text.contains("restart_editor"), "advice for the code is missing:\n{text}");
        assert!(text.contains("without a previous plan_task"), "bulk call without a plan is not noted");
        assert!(text.contains("| `select_dress` | 1 | 0 | 0 | 1 |"));
        assert!(text.contains("Calls longer than 45 s"));
    }

    #[test]
    fn a_plan_silences_the_note_and_a_clean_session_says_so() {
        let r = session();
        r.planned();
        r.call("export", "dddd4444", Duration::from_millis(10), Ok(&json!({"files": []})));
        let text = render(&r.started, Duration::from_secs(1), &r.inner.lock().unwrap());
        assert!(text.contains("CLEAN"), "{text}");
        assert!(text.contains("Nothing: every call finished well."));
        assert!(!text.contains("without a previous plan_task"));
    }

    #[test]
    fn long_sessions_keep_every_problem() {
        let r = session();
        let empty = json!({});
        for i in 0..1000 {
            let result = if i == 500 { Err("[server_timeout] x") } else { Ok(&empty) };
            r.call("status", &format!("{i:08}"), Duration::from_millis(1), result);
        }
        let text = render(&r.started, Duration::from_secs(1), &r.inner.lock().unwrap());
        assert!(text.contains("00000500"), "the failing call was dropped");
        assert!(text.contains("Showing"));
        assert!(text.len() < 200_000);
    }
}
