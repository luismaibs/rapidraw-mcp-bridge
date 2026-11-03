//! Local pipe protocol. Each connection carries exactly one request.
use std::io::{Error, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_COMMAND: usize = 512 * 1024;
pub const MAX_REPLY: usize = 4 * 1024 * 1024;

/// Operations the editor bridge accepts. Anything else is rejected before it reaches the UI.
#[allow(dead_code)] // used by the editor side of the bridge, which shares this file
pub const OPS: &[&str] = &[
    "status", "open_folder", "list_photos", "select_photos", "open_photo", "get_photo",
    "set_rating", "set_flag", "set_color", "set_filter", "trash", "copy_adjustments",
    "paste_adjustments", "set_view", "get_adjustments", "set_adjustments", "reset_adjustments",
    "undo", "redo", "preview", "list_masks", "add_mask", "update_mask", "delete_mask",
    "mask_overlay", "export", "cancel_export", "analyze", "transform", "auto_adjust", "set_tags",
    "list_presets", "apply_preset", "describe_adjustments", "list_folders", "select_dress",
    "job_status", "cancel_job", "assess", "inventory", "select_skin", "look", "match_style",
    "style_photo", "review_set",
];

/// Version of the request/reply contract between the server and the editor bridge. A server sending a
/// different number is refused with a message that says which side to update.
pub const PROTOCOL: u32 = 4;

/// Oldest server the editor still serves. Later versions only added operations, so an agent that was already
/// connected keeps working after the editor is updated; it just does not see the new tools until it reconnects.
#[allow(dead_code)] // used by the editor side of the bridge, which shares this file
pub const MIN_PROTOCOL: u32 = 3;

/// Operations that only read state or stop work. They are answered at once, even while a long job holds
/// the editor; everything else is a job that waits its turn.
#[allow(dead_code)] // used by the editor side of the bridge, which shares this file
pub fn is_immediate(op: &str) -> bool {
    matches!(op, "status" | "list_folders" | "cancel_export" | "job_status" | "cancel_job" | "inventory")
}

/// Seconds the editor window may stay silent (no answer and no progress report) on an operation before the
/// job counts as overdue. A job that keeps reporting progress can take as long as it needs.
#[allow(dead_code)] // used by the editor side of the bridge, which shares this file
pub fn op_timeout(op: &str) -> u64 {
    match op {
        "export" | "select_dress" | "select_skin" | "match_style" => 900,
        "assess" => 600,
        "style_photo" | "review_set" => 180,
        "add_mask" | "update_mask" => 600,
        "preview" | "mask_overlay" | "analyze" | "look" => 90,
        "auto_adjust" | "apply_preset" => 90,
        _ => 60,
    }
}

/// Shared settings: environment variables win, otherwise `%APPDATA%\RapidRAW-MCP\config.json`
/// (`{"secret": "...", "roots": ["C:\Photos"]}`) so no client has to carry the secret.
pub struct Config {
    pub secret: String,
    pub roots: Vec<String>,
}

pub fn config() -> Result<Config> {
    // Some agents start servers with a scrubbed environment, so fall back to the profile folder.
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join("AppData").join("Roaming")));
    let file: serde_json::Value = base
        .and_then(|dir| std::fs::read(dir.join("RapidRAW-MCP").join("config.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let secret = std::env::var("RAPIDRAW_MCP_SECRET")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| file["secret"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let roots = match std::env::var("RAPIDRAW_MCP_ROOT").ok().filter(|v| !v.is_empty()) {
        Some(list) => list.split(';').filter(|p| !p.is_empty()).map(str::to_owned).collect(),
        None => file["roots"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
    };
    Ok(Config { secret, roots })
}

pub fn key() -> Result<[u8; 32]> {
    let token = config()?.secret;
    if token.len() < 32 {
        return Err(Error::other(
            "No secret configured: run scripts/setup.ps1 (or set RAPIDRAW_MCP_SECRET with 32+ random characters) for both RapidRAW and the MCP server",
        ));
    }
    Ok(blake3::derive_key(
        "RapidRAW MCP local bridge v1",
        token.as_bytes(),
    ))
}

pub fn pipe_name(key: &[u8; 32]) -> String {
    format!(
        r"\\.\pipe\RapidRAW-MCP-{}",
        &blake3::keyed_hash(key, b"name").to_hex()[..24]
    )
}

pub async fn read_frame(r: &mut (impl AsyncRead + Unpin), limit: usize) -> Result<Vec<u8>> {
    let size = r.read_u32().await? as usize;
    if size > limit {
        return Err(Error::other("Bridge frame too large"));
    }
    let mut data = vec![0; size];
    r.read_exact(&mut data).await?;
    Ok(data)
}

pub async fn write_frame(
    w: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    limit: usize,
) -> Result<()> {
    if data.len() > limit {
        return Err(Error::other("Bridge frame too large"));
    }
    w.write_u32(data.len() as u32).await?;
    w.write_all(data).await
}

// Both peers prove possession before transmitting a path or image.
// The role byte prevents reflection; fresh nonces prevent replay.
pub async fn authenticate(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    key: &[u8; 32],
    server: bool,
) -> Result<()> {
    let nonce = *uuid::Uuid::new_v4().as_bytes();
    let mut peer = [0u8; 16];
    stream.write_all(&nonce).await?;
    stream.read_exact(&mut peer).await?;
    let (a, b) = if server { (peer, nonce) } else { (nonce, peer) };
    let proof = |role: u8| {
        let mut hash = blake3::Hasher::new_keyed(key);
        hash.update(&a).update(&b).update(&[role]).finalize()
    };
    stream.write_all(proof(server as u8).as_bytes()).await?;
    let mut response = [0; 32];
    stream.read_exact(&mut response).await?;
    if proof((!server) as u8) != blake3::Hash::from_bytes(response) {
        return Err(Error::other("Bridge authentication failed"));
    }
    Ok(())
}
