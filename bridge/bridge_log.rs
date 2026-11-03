//! Plain-text diagnostics shared by the MCP server and the editor bridge, so one failure can be
//! followed across both processes: `%APPDATA%\RapidRAW-MCP\bridge.log` (times are UTC).
//! Only short operation facts go in; never images, paths' contents or secrets.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE: usize = 400;

fn file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join("AppData").join("Roaming")))?;
    let dir = base.join("RapidRAW-MCP");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("bridge.log"))
}

/// UTC time as `YYYY-MM-DD HH:MM:SS.mmmZ`; the usage reports use it too.
#[allow(dead_code)]
pub fn stamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        now.subsec_millis()
    )
}

fn write(who: &str, text: &str) {
    // Unit tests must not write into the user's real log.
    if cfg!(test) {
        return;
    }
    let Some(path) = file() else {
        return;
    };
    // Keep one previous generation; the log must never grow without bound.
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(&path, path.with_file_name("bridge.old.log"));
    }
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_LINE)
        .collect();
    // One write per line keeps lines from different processes from interleaving.
    let line = format!("{} [{who}] {clean}\n", stamp());
    if let Ok(mut out) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = out.write_all(line.as_bytes());
    }
}

/// An event in the MCP server process (one per agent session). Unused by the app, which shares this file.
#[allow(dead_code)]
pub fn server(text: &str) {
    write(&format!("srv {}", std::process::id()), text);
}

/// An event in the RapidRAW process (bridge listener or editor window). Unused by the server binary,
/// which shares this file with the app.
#[allow(dead_code)]
pub fn editor(text: &str) {
    write("app", text);
}
