use crate::config::atomic_write;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex};

/// getFile over the public Bot API only serves files up to 20 MB. A self-hosted
/// local server lifts this, so we only pre-empt the limit when talking to the
/// public API.
///
/// Decimal, as Telegram documents it (20,000,000 bytes, not 20 MiB), so this
/// pre-emptive check lines up with the server's real cutoff.
const PUBLIC_DOWNLOAD_LIMIT: i64 = 20_000_000;

/// How many recently-written `update_id`s each bot remembers (see
/// [`BotStatus::saved`]). Telegram keeps an update in its queue for 24 hours,
/// so this only has to cover the messages that could plausibly be replayed —
/// a few hundred is far more than a personal note-taking bot ever sees in a
/// day, and the list costs a few KB in `status.json`.
const RECENT_SAVED_MAX: usize = 500;

/// Live status for one bot, surfaced to the UI and the tray. The message
/// counters and the long-poll `offset` are persisted across restarts; the
/// transient fields (`running`, `username`, `last_error`) are not.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct BotStatus {
    #[serde(default, skip_deserializing)]
    pub running: bool,
    #[serde(default, skip_deserializing)]
    pub username: Option<String>,
    #[serde(default, skip_deserializing)]
    pub last_error: Option<String>,
    /// Error from the daily send-back, kept separate from `last_error` so the
    /// poll loop (which clears `last_error` on every successful getUpdates) can't
    /// wipe it within seconds and hide a failed daily send. Transient.
    #[serde(default, skip_deserializing)]
    pub last_daily_error: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub message_count: u64,
    /// Next Telegram getUpdates offset. Persisted so a restart doesn't
    /// re-deliver (and re-append) already-saved messages.
    #[serde(default)]
    pub offset: i64,
    /// `update_id`s already written to the markdown file, oldest first, capped
    /// at [`RECENT_SAVED_MAX`].
    ///
    /// The offset alone was the only thing standing between a replayed update
    /// and a duplicate line, and it is not enough: an update stays in
    /// Telegram's queue for 24 hours after it is confirmed, so anything that
    /// moves the offset backwards — a second copy of the app, a clobbered
    /// `status.json`, a restore of an old one — makes every message since come
    /// back and be appended again. This list is the record that survives that.
    #[serde(default)]
    pub saved: Vec<i64>,
}

pub type StatusMap = Arc<Mutex<HashMap<String, BotStatus>>>;

/// Remove the bot token from a message before it is shown or stored.
///
/// `reqwest` includes the token-bearing request URL in its error `Display`, so
/// without this a network error would leak the token into the UI, tray, and logs.
fn scrub(msg: String, token: &str) -> String {
    if token.is_empty() {
        msg
    } else {
        msg.replace(token, "<redacted>")
    }
}

/// Load persisted per-bot status (counters + offset) from disk.
///
/// An unparseable file is moved aside rather than silently discarded (mirroring
/// `Config::load`): dropping it resets every bot's counters and long-poll offset,
/// which is worth a backup and a line in the log.
pub fn load_status(path: &Path) -> HashMap<String, BotStatus> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    match serde_json::from_str(&text) {
        Ok(map) => map,
        Err(e) => {
            let backup = crate::config::sibling(
                path,
                "corrupt",
                &Local::now().format("%Y%m%d-%H%M%S").to_string(),
            );
            let _ = std::fs::rename(path, &backup);
            eprintln!(
                "msg2note: failed to parse {} ({e}); moved aside to {}",
                path.display(),
                backup.display()
            );
            HashMap::new()
        }
    }
}

/// Persist the current status map atomically, merged with what is already on
/// disk rather than overwriting it.
///
/// The lock is held across the whole read-merge-write: snapshotting and then
/// writing outside it lets two concurrent persisters interleave, so an older
/// snapshot can land on disk after a newer one and roll a bot's long-poll
/// offset backwards — re-delivering, and re-appending, already-saved messages.
///
/// The merge covers the same hazard between *processes*, which the lock cannot
/// reach: offsets and counters only ever move forwards, and the recent-saved
/// lists are unioned, so a writer that knows less than the file does can no
/// longer erase what the file knows.
pub async fn persist_status(path: &Path, status: &StatusMap) {
    let guard = status.lock().await;
    let mut merged = load_status(path);
    for (id, mine) in guard.iter() {
        let on_disk = merged.remove(id).unwrap_or_default();
        let mut next = mine.clone();
        next.offset = next.offset.max(on_disk.offset);
        next.message_count = next.message_count.max(on_disk.message_count);
        for uid in on_disk.saved {
            if !next.saved.contains(&uid) {
                next.saved.push(uid);
            }
        }
        // `update_id`s only ever increase, so sorting keeps the newest at the
        // end and the trim below drops the oldest.
        next.saved.sort_unstable();
        trim_saved(&mut next.saved);
        merged.insert(id.clone(), next);
    }
    if let Ok(text) = serde_json::to_string_pretty(&merged) {
        let _ = atomic_write(path, text.as_bytes());
    }
}

async fn set_status<F: FnOnce(&mut BotStatus)>(status: &StatusMap, id: &str, f: F) {
    let mut map = status.lock().await;
    let entry = map.entry(id.to_string()).or_default();
    f(entry);
}

/// Drop the oldest entries so a recent-saved list stays bounded.
fn trim_saved(saved: &mut Vec<i64>) {
    let len = saved.len();
    if len > RECENT_SAVED_MAX {
        saved.drain(..len - RECENT_SAVED_MAX);
    }
}

/// Record that `uid` has been written to the markdown file. Call this from
/// inside a [`set_status`] closure, in the same critical section as the
/// counters, so the note and the record of it can't be separated.
fn remember_saved(s: &mut BotStatus, uid: i64) {
    if s.saved.contains(&uid) {
        return;
    }
    s.saved.push(uid);
    trim_saved(&mut s.saved);
}

/// Whether this update has already been written to the markdown file.
///
/// Checked in addition to the long-poll offset, which cannot be trusted on its
/// own: Telegram will re-deliver a confirmed update for 24 hours if it is ever
/// asked with a lower offset.
async fn already_saved(status: &StatusMap, id: &str, uid: i64) -> bool {
    status
        .lock()
        .await
        .get(id)
        .is_some_and(|s| s.saved.contains(&uid))
}

/// Sleep for `secs`, waking early if the bot is asked to stop.
///
/// Returns `true` when it was told to stop. Every backoff in this module goes
/// through here so a stop request is never left waiting out a retry delay.
///
/// Any completion of `changed()` means stop: `true` is the only value ever sent,
/// and an `Err` (the sender was dropped) means the owning bot is gone.
async fn sleep_or_stop(stop_rx: &mut watch::Receiver<bool>, secs: u64) -> bool {
    if *stop_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = stop_rx.changed() => true,
        _ = tokio::time::sleep(Duration::from_secs(secs)) => false,
    }
}

/// A file download that's been acknowledged to Telegram but not yet saved.
///
/// Downloads run on a background worker so a large (slow) file can't block the
/// bot's poll loop from receiving later messages or honouring a stop request.
/// Because the update is acked to Telegram as soon as it's enqueued (so the
/// long-poll keeps flowing), the job is also written to an on-disk journal —
/// otherwise a crash mid-download would lose the file with no way to re-fetch
/// it. The journal is replayed on the next start.
#[derive(Clone, Serialize, Deserialize)]
struct PendingDownload {
    update_id: i64,
    file_id: String,
    file_name: Option<String>,
    caption: String,
    chat_id: i64,
    message_id: i64,
}

/// The pending-download journal for one bot, shared between the poll loop (which
/// appends jobs) and the worker (which removes them once saved).
type Journal = Arc<Mutex<Vec<PendingDownload>>>;

/// Path to a bot's pending-download journal, kept beside `status.json`.
fn journal_path(status_path: &Path, id: &str) -> PathBuf {
    let dir = status_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("pending-{}.json", id))
}

/// Load a bot's pending-download journal; an absent or unreadable file is an
/// empty journal.
fn load_journal(path: &Path) -> Vec<PendingDownload> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Delete a bot's pending-download journal — called when the bot is removed so
/// a stale journal can't be replayed against a bot that no longer exists.
pub fn remove_journal(status_path: &Path, id: &str) {
    let _ = std::fs::remove_file(journal_path(status_path, id));
}

/// Persist the journal atomically.
///
/// An empty journal removes the file rather than leaving an empty `[]` behind.
/// Besides keeping things tidy, this closes a race with bot removal: if the
/// worker finishes its last job (emptying the journal) just after `remove_bot`
/// has deleted the file, this removes it again instead of recreating it as an
/// orphan that lingers for a bot that no longer exists.
async fn persist_journal(path: &Path, journal: &Journal) {
    // Held across the write for the same reason as `persist_status`: an older
    // snapshot landing last would drop a job that is still queued, and that job
    // has already been acked to Telegram — it can never be re-fetched.
    let guard = journal.lock().await;
    if guard.is_empty() {
        let _ = tokio::fs::remove_file(path).await;
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(&*guard) {
        let _ = atomic_write(path, text.as_bytes());
    }
}

/// Format a byte count for human-readable status/ack messages.
///
/// Decimal (SI) units — 1 MB = 1,000,000 bytes — matching
/// `PUBLIC_DOWNLOAD_LIMIT` and macOS Finder, so a file just over the cap never
/// reads as "19.1 MB — over the 20 MB limit".
fn human_size(bytes: i64) -> String {
    if bytes <= 0 {
        return "unknown size".to_string();
    }
    const KB: f64 = 1000.0;
    const MB: f64 = KB * 1000.0;
    const GB: f64 = MB * 1000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// The public Telegram Bot API. Used when a bot has no custom `api_base`.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Normalize a configured API base into a usable origin. Falls back to the
/// public API when unset/blank, and trims any trailing slash so URL building
/// can always join with a single `/`.
pub fn resolve_api_base(configured: Option<&str>) -> String {
    let trimmed = configured.map(str::trim).unwrap_or("");
    let base = if trimmed.is_empty() {
        DEFAULT_API_BASE
    } else {
        trimmed
    };
    base.trim_end_matches('/').to_string()
}

/// Call getMe to validate a token and return the bot's @username.
pub async fn get_me(client: &reqwest::Client, base: &str, token: &str) -> Result<String, String> {
    let url = format!("{}/bot{}/getMe", base, token);
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| scrub(e.to_string(), token))?;
    let json: Value = resp.json().await.map_err(|e| scrub(e.to_string(), token))?;
    if json["ok"].as_bool() != Some(true) {
        return Err(json["description"]
            .as_str()
            .unwrap_or("invalid token")
            .to_string());
    }
    Ok(json["result"]["username"]
        .as_str()
        .unwrap_or("unknown")
        .to_string())
}

pub async fn append_timestamped(path: &str, text: &str) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let ts = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let line = format!("- [{}] {}\n", ts, text);
    let path = path.to_string();
    // The actual write runs on a blocking thread because the advisory lock
    // (and the short retry/backoff) are blocking syscalls; doing them on the
    // async runtime would stall other bots' poll loops.
    tokio::task::spawn_blocking(move || append_locked(&path, line.as_bytes()))
        .await
        .map_err(std::io::Error::other)?
}

/// Path of the sidecar lock file that processes coordinate on for a given data
/// file. For `dir/notes.md` this is `dir/.notes.md.lock` — matching the
/// convention the companion Python scripts use with `fcntl.flock`. The lock is
/// taken on this sidecar, never on the markdown file itself.
fn sidecar_lock_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("msg2note");
    let lock_name = format!(".{name}.lock");
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(lock_name),
        _ => PathBuf::from(lock_name),
    }
}

/// Append `bytes` to `path` while holding an exclusive **advisory** (flock-style)
/// lock on the sidecar lock file (see [`sidecar_lock_path`]). Because `flock(2)`
/// is OS-level, this interoperates with other processes — e.g. Python scripts
/// using `fcntl.flock` on the same sidecar — so writers take turns instead of
/// interleaving. We lock the sidecar, not the markdown file, so all writers
/// must agree on the same lock target. The lock covers only this one append and
/// is released as soon as the sidecar handle is dropped.
///
/// In the rare case the lock is already held, we retry a few times with a short
/// backoff rather than blocking the poll loop indefinitely. If it's still locked
/// after several attempts we return an error; the caller then leaves the Telegram
/// update unconfirmed so the message is retried on the next poll (no data lost).
fn append_locked(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // Acquire the advisory lock on the sidecar that other writers coordinate on.
    let lock_path = sidecar_lock_path(path);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Never truncate: the sidecar holds no data, and truncating it would be
        // a write to a file another process may already be holding the lock on.
        .truncate(false)
        .open(&lock_path)?;

    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt: u32 = 0;
    loop {
        // Fully-qualified call so this resolves to fs2's trait method even on
        // newer Rust std, which has its own inherent `try_lock_exclusive`.
        match fs2::FileExt::try_lock_exclusive(&lock_file) {
            Ok(()) => break,
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_ATTEMPTS {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!("file locked by another process after {attempt} attempts: {e}"),
                    ));
                }
                // Linear backoff: 100ms, 200ms, 300ms, 400ms.
                std::thread::sleep(Duration::from_millis(100 * attempt as u64));
            }
        }
    }

    // Lock held on the sidecar: append to the actual markdown file and flush.
    // The sidecar lock is released when `lock_file` drops, including on the
    // early-return paths below.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Strip anything that isn't a safe filename character. Telegram-supplied names
/// can contain path separators or other surprises, so we keep only a basename
/// and a conservative character set.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed
    }
}

/// A downloadable attachment pulled from a message.
struct Attachment {
    file_id: String,
    file_name: Option<String>,
    /// Telegram-reported size in bytes, or 0 when the message omits it.
    file_size: i64,
}

/// Pull the first downloadable attachment out of a message, returning its
/// Telegram `file_id`, a suggested filename (when the message provides one) and
/// its reported size. Checks the common attachment kinds in priority order.
fn extract_attachment(msg: &Value) -> Option<Attachment> {
    for key in [
        "document",
        "video",
        "audio",
        "voice",
        "animation",
        "video_note",
    ] {
        if let Some(obj) = msg.get(key) {
            if let Some(id) = obj["file_id"].as_str() {
                return Some(Attachment {
                    file_id: id.to_string(),
                    file_name: obj["file_name"].as_str().map(|s| s.to_string()),
                    file_size: obj["file_size"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    // Photos arrive as an array of sizes; the last entry is the largest.
    if let Some(sizes) = msg["photo"].as_array() {
        if let Some(largest) = sizes.last() {
            if let Some(id) = largest["file_id"].as_str() {
                return Some(Attachment {
                    file_id: id.to_string(),
                    file_name: None,
                    file_size: largest["file_size"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    // Stickers are .webp (or .tgs/.webm for animated) with no file_name.
    if let Some(id) = msg["sticker"]["file_id"].as_str() {
        return Some(Attachment {
            file_id: id.to_string(),
            file_name: None,
            file_size: msg["sticker"]["file_size"].as_i64().unwrap_or(0),
        });
    }
    None
}

/// Resolve where a bot's received/saved files go: the configured `files_dir`,
/// or an `attachments` folder next to the markdown file when unset. Shared by
/// the Telegram download path and the quick-window's local file drops so both
/// land in the same place.
pub fn resolve_save_dir(file: &str, files_dir: Option<&str>) -> PathBuf {
    match files_dir.map(str::trim) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => Path::new(file)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("attachments"))
            .unwrap_or_else(|| PathBuf::from("attachments")),
    }
}

/// Pick a unique destination path inside `dir`, prefixing the name with a
/// timestamp and appending a counter if a same-named file already exists.
pub fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let base = format!("{}_{}", stamp, sanitize_filename(name));
    let mut candidate = dir.join(&base);
    // Split once, outside the loop: the counter goes before the extension.
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{}", e)),
        None => (base.as_str(), String::new()),
    };
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{}-{}{}", stem, n, ext));
        n += 1;
    }
    candidate
}

/// A failed download. `permanent` means Telegram itself rejected the file (e.g.
/// it exceeds getFile's ~20 MB download limit), so retrying can't help and the
/// caller should skip past the message instead of blocking on it.
struct DownloadError {
    permanent: bool,
    msg: String,
}

impl DownloadError {
    fn transient(msg: String) -> Self {
        Self {
            permanent: false,
            msg,
        }
    }
    fn permanent(msg: String) -> Self {
        Self {
            permanent: true,
            msg,
        }
    }
}

/// Download a Telegram file by `file_id` into `dir`, returning the saved path.
async fn download_attachment(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
    file_name: Option<&str>,
    dir: &Path,
) -> Result<PathBuf, DownloadError> {
    // getFile resolves a file_id to a temporary download path on Telegram's servers.
    let get_url = format!("{}/bot{}/getFile", base, token);
    let resp = client
        .get(&get_url)
        .query(&[("file_id", file_id)])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    let json: Value = resp
        .json()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    if json["ok"].as_bool() != Some(true) {
        return Err(DownloadError::permanent(
            json["description"]
                .as_str()
                .unwrap_or("getFile failed")
                .to_string(),
        ));
    }
    let file_path = json["result"]["file_path"]
        .as_str()
        .ok_or_else(|| DownloadError::permanent("getFile returned no file_path".to_string()))?
        .to_string();

    // Fall back to the basename Telegram reports when the message had no file_name.
    let name = file_name.map(|s| s.to_string()).unwrap_or_else(|| {
        file_path
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("file")
            .to_string()
    });

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    let dest = unique_dest(dir, &name);

    // A self-hosted local Bot API server (run with `--local`) returns `file_path`
    // as an absolute path to the already-downloaded file on disk rather than a
    // relative URL. In that case copy it straight off disk — there's no 20 MB cap
    // and no second HTTP round trip. Otherwise fetch it over HTTP as usual.
    // Gated on the base actually being loopback: without this, any api_base that
    // returns an absolute `file_path` — a typo'd host, a hostile one — makes the
    // app copy an arbitrary file off this Mac, echo its path into the notes, and
    // then delete the original.
    let is_loopback_server = base.starts_with("http://127.0.0.1:")
        || base.starts_with("http://localhost:")
        || base.starts_with("http://[::1]:");
    if is_loopback_server && Path::new(&file_path).is_absolute() {
        tokio::fs::copy(&file_path, &dest)
            .await
            .map_err(|e| DownloadError::permanent(format!("could not read local file: {}", e)))?;
        // The local server never deletes what it downloads, so its data dir
        // would grow unbounded. We have our copy; drop its one (best-effort).
        let _ = tokio::fs::remove_file(&file_path).await;
        return Ok(dest);
    }

    let dl_url = format!("{}/file/bot{}/{}", base, token, file_path);
    let mut resp = client
        .get(&dl_url)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?;
    let status = resp.status();
    if !status.is_success() {
        // 4xx means Telegram won't serve this path (expired/invalid) — retrying
        // can't help, so treat it as permanent and skip past the message. 5xx and
        // the like are transient and worth retrying.
        let msg = format!("file download returned HTTP {}", status.as_u16());
        return Err(if status.is_client_error() {
            DownloadError::permanent(msg)
        } else {
            DownloadError::transient(msg)
        });
    }
    // A part-written file is removed, so a retry can never find a stub.
    if let Err(e) = write_body(&mut resp, &dest, token).await {
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(e);
    }
    Ok(dest)
}

/// Copy a response body to `dest` chunk by chunk, so a 20 MB attachment is never
/// held in memory in one piece.
async fn write_body(
    resp: &mut reqwest::Response,
    dest: &Path,
    token: &str,
) -> Result<(), DownloadError> {
    use tokio::io::AsyncWriteExt;
    let mut out = tokio::fs::File::create(dest)
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))?;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| DownloadError::transient(scrub(e.to_string(), token)))?
    {
        out.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::transient(e.to_string()))?;
    }
    out.flush()
        .await
        .map_err(|e| DownloadError::transient(e.to_string()))
}

async fn react(client: &reqwest::Client, base: &str, token: &str, chat_id: i64, message_id: i64) {
    let url = format!("{}/bot{}/setMessageReaction", base, token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction": [{ "type": "emoji", "emoji": "👍" }]
    });
    let _ = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
}

/// Telegram caps a single text message at 4096 UTF-16 code units. We split a
/// little under that; since a string's byte length is always >= its UTF-16
/// length, staying under this many bytes guarantees we're under the real cap.
const TELEGRAM_MSG_LIMIT: usize = 4000;

/// Split text into Telegram-sized pieces, preferring line boundaries so the
/// content stays readable. A single line longer than the limit is hard-split on
/// char boundaries.
///
/// Returns byte ranges rather than owned strings: the caller sends `&text[a..b]`,
/// so a long file is never held twice over. The ranges are contiguous and cover
/// the whole input.
fn chunk_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut end = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = end;
        let line_end = line_start + line.len();
        if line.len() > TELEGRAM_MSG_LIMIT {
            if end > start {
                out.push((start, end));
            }
            let mut pos = line_start;
            while line_end - pos > TELEGRAM_MSG_LIMIT {
                let mut idx = pos + TELEGRAM_MSG_LIMIT;
                while !text.is_char_boundary(idx) {
                    idx -= 1;
                }
                out.push((pos, idx));
                pos = idx;
            }
            start = pos;
            end = line_end;
            continue;
        }
        if end - start + line.len() > TELEGRAM_MSG_LIMIT {
            out.push((start, end));
            start = line_start;
        }
        end = line_end;
    }
    if end > start {
        out.push((start, end));
    }
    out
}

/// Send one text message, returning whether Telegram accepted it.
///
/// Callers that only want a best-effort ack discard the result; the daily digest
/// checks it so it can surface a failure and stop mid-file rather than send a
/// partial digest silently.
async fn send_message(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    text: &str,
) -> bool {
    let url = format!("{}/bot{}/sendMessage", base, token);
    let body = serde_json::json!({ "chat_id": chat_id, "text": text });
    match client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<Value>()
            .await
            .map(|j| j["ok"].as_bool() == Some(true))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Parse an "HH:MM" string into (hour, minute), clamped to valid ranges.
/// Falls back to 08:00 on anything unparseable.
fn parse_hhmm(s: &str) -> (u32, u32) {
    let mut parts = s.trim().splitn(2, ':');
    let h = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(8);
    let m = parts
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    (h.min(23), m.min(59))
}

/// Once a day at `daily_time` (local), send the exact contents of `file` back to
/// the owner (`chat_id`). Runs until `stop_rx` flips to true.
///
/// `file` here is the bot's *read* file, which may be a different markdown file
/// from the one the bot appends messages to. It is only ever read.
///
/// Rather than sleeping for the whole interval (which a laptop sleep or a clock
/// change would throw off), it wakes every 30s, checks the wall clock, and fires
/// once per calendar day when the scheduled minute has arrived. If the bot
/// starts after today's send time, today is treated as already done so there's
/// no surprise catch-up send on launch.
#[allow(clippy::too_many_arguments)]
async fn daily_send_loop(
    id: String,
    base: String,
    token: String,
    file: String,
    chat_id: i64,
    daily_time: String,
    status: StatusMap,
    client: reqwest::Client,
    mut stop_rx: watch::Receiver<bool>,
) {
    use chrono::Timelike;
    let (hh, mm) = parse_hhmm(&daily_time);

    let now = Local::now();
    let past_today = now.hour() > hh || (now.hour() == hh && now.minute() >= mm);
    let mut last_sent: Option<chrono::NaiveDate> = if past_today {
        Some(now.date_naive())
    } else {
        None
    };

    loop {
        if sleep_or_stop(&mut stop_rx, 30).await {
            break;
        }

        let now = Local::now();
        let today = now.date_naive();
        if last_sent == Some(today) {
            continue;
        }
        let due = now.hour() > hh || (now.hour() == hh && now.minute() >= mm);
        if !due {
            continue;
        }

        // Mark before sending so a failure can't spin-resend in a tight loop;
        // it'll simply try again tomorrow.
        last_sent = Some(today);

        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                if content.trim().is_empty() {
                    // An empty file isn't a failure — today's send is a no-op, so
                    // clear any stale error from a previous day rather than letting
                    // it linger until the next non-empty send.
                    set_status(&status, &id, |s| s.last_daily_error = None).await;
                    continue; // nothing to send today
                }
                let mut ok = true;
                for (a, b) in chunk_ranges(&content) {
                    // A long file is many sequential sends; honour a stop
                    // request between them so shutting the bot down isn't held
                    // up by a digest that spans dozens of messages.
                    if *stop_rx.borrow() {
                        return;
                    }
                    if !send_message(&client, &base, &token, chat_id, &content[a..b]).await {
                        ok = false;
                        break;
                    }
                }
                set_status(&status, &id, |s| {
                    s.last_daily_error = if ok {
                        None
                    } else {
                        Some("daily send failed (Telegram rejected message)".to_string())
                    };
                })
                .await;
            }
            Err(e) => {
                set_status(&status, &id, |s| {
                    s.last_daily_error = Some(format!("daily send: cannot read file: {}", e));
                })
                .await;
            }
        }
    }
}

/// Background worker that drains a bot's download queue. Runs one download at a
/// time (preserving order), retrying transient failures until they succeed, the
/// failure turns out permanent, or the bot stops. Each finished or permanently
/// failed job is removed from the journal so it isn't replayed next start.
#[allow(clippy::too_many_arguments)]
async fn download_worker(
    id: String,
    base: String,
    token: String,
    file: String,
    save_dir: PathBuf,
    status: StatusMap,
    status_path: PathBuf,
    journal: Journal,
    journal_file: PathBuf,
    client: reqwest::Client,
    mut rx: mpsc::UnboundedReceiver<PendingDownload>,
    mut stop_rx: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            // `true` is the only value ever sent, and `changed()` erroring means
            // the sender was dropped — so any completion of this arm means stop.
            _ = stop_rx.changed() => break,
            j = rx.recv() => match j {
                Some(j) => j,
                None => break, // sender dropped: bot is shutting down
            },
        };

        // A duplicate (e.g. replayed from the journal *and* re-delivered by
        // Telegram after a crash) may already be gone — skip if so.
        if !journal
            .lock()
            .await
            .iter()
            .any(|p| p.update_id == job.update_id)
        {
            continue;
        }

        // Retry transient failures with backoff; stop promptly if asked.
        loop {
            if *stop_rx.borrow() {
                return; // leave the job in the journal for the next start
            }
            // Race the transfer against a stop request. A large file can take
            // minutes; without this, stopping the bot would wait out the whole
            // download and let the restarted instance run alongside this one,
            // saving the same file twice. Bailing here leaves the job in the
            // journal, so the next start resumes it.
            let outcome = tokio::select! {
                _ = stop_rx.changed() => return,
                r = download_attachment(
                    &client,
                    &base,
                    &token,
                    &job.file_id,
                    job.file_name.as_deref(),
                    &save_dir,
                ) => r,
            };
            match outcome {
                Ok(dest) => {
                    let name = dest
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".to_string());
                    // The size comes from the saved file itself, so it's accurate
                    // even when Telegram didn't report one.
                    let size = tokio::fs::metadata(&dest)
                        .await
                        .map(|m| m.len() as i64)
                        .unwrap_or(0);
                    let mut note = format!("saved file: {} → {}", name, dest.display());
                    if !job.caption.is_empty() {
                        note.push_str(&format!(" — {}", job.caption));
                    }
                    match append_timestamped(&file, &note).await {
                        Ok(()) => {
                            set_status(&status, &id, |s| {
                                remember_saved(s, job.update_id);
                                s.message_count += 1;
                                s.last_message_at = Some(Local::now().format("%H:%M").to_string());
                                s.last_error = None;
                            })
                            .await;
                            persist_status(&status_path, &status).await;
                            react(&client, &base, &token, job.chat_id, job.message_id).await;
                            // Confirm completion. The ack is sent only once the file
                            // is actually on disk (there's no "Saving…" message
                            // beforehand), so a slow, large download no longer looks
                            // stuck mid-save.
                            let _ = send_message(
                                &client,
                                &base,
                                &token,
                                job.chat_id,
                                &format!("✅ Saved {} ({})", name, human_size(size)),
                            )
                            .await;
                        }
                        Err(e) => {
                            // The file downloaded fine but its note couldn't be
                            // written. Record the error and say so, rather than
                            // sending a misleading "✅ Saved" with no record of it
                            // in the markdown file. The file is on disk, so the job
                            // is still considered done (retrying would re-download).
                            set_status(&status, &id, |s| {
                                s.last_error =
                                    Some(format!("file saved but note write failed: {}", e));
                            })
                            .await;
                            let _ = send_message(
                                &client,
                                &base,
                                &token,
                                job.chat_id,
                                &format!(
                                    "⚠️ Downloaded {} ({}) but couldn't write its note to the file: {}",
                                    name,
                                    human_size(size),
                                    e
                                ),
                            )
                            .await;
                        }
                    }
                    break;
                }
                Err(e) if e.permanent => {
                    let mut note = format!("could not save file: {}", e.msg);
                    if !job.caption.is_empty() {
                        note.push_str(&format!(" — {}", job.caption));
                    }
                    let _ = append_timestamped(&file, &note).await;
                    set_status(&status, &id, |s| remember_saved(s, job.update_id)).await;
                    let _ = send_message(
                        &client,
                        &base,
                        &token,
                        job.chat_id,
                        &format!("⚠️ {}", e.msg),
                    )
                    .await;
                    break;
                }
                Err(e) => {
                    set_status(&status, &id, |s| {
                        s.last_error = Some(format!("file save failed: {}", e.msg));
                    })
                    .await;
                    // Back off, but wake immediately on stop so shutdown isn't
                    // delayed.
                    if sleep_or_stop(&mut stop_rx, 10).await {
                        return;
                    }
                    continue;
                }
            }
        }

        // Done (saved or permanently failed): drop it from the journal.
        journal
            .lock()
            .await
            .retain(|p| p.update_id != job.update_id);
        persist_journal(&journal_file, &journal).await;
    }
}

/// Long-poll loop for a single bot. Runs until `stop_rx` flips to true.
#[allow(clippy::too_many_arguments)]
pub async fn run_bot(
    id: String,
    token: String,
    file: String,
    // File the daily send-back reads. Usually the same as `file`, but can be a
    // different markdown file — see `BotConfig::read_path`.
    read_file: String,
    files_dir: Option<String>,
    allowed_user_id: i64,
    api_base: Option<String>,
    daily_send: bool,
    daily_time: String,
    status: StatusMap,
    status_path: PathBuf,
    client: reqwest::Client,
    mut stop_rx: watch::Receiver<bool>,
) {
    let base = resolve_api_base(api_base.as_deref());

    // If a daily digest is configured, spawn it alongside the poll loop. It needs
    // a concrete chat to send to: in a private chat the owner's user id is also
    // the chat id, so we require `allowed_user_id` to be set.
    let daily_task = if daily_send && allowed_user_id != 0 {
        Some(tokio::spawn(daily_send_loop(
            id.clone(),
            base.clone(),
            token.clone(),
            read_file,
            allowed_user_id,
            daily_time,
            status.clone(),
            client.clone(),
            stop_rx.clone(),
        )))
    } else if daily_send {
        // Configured but unusable: the digest is sent to the owner's private
        // chat, whose id *is* the Telegram user id, so there is nowhere to send
        // it. Say so instead of silently never firing.
        set_status(&status, &id, |s| {
            s.last_daily_error = Some(
                "daily send needs your Telegram user ID — it's the chat to send to".to_string(),
            );
        })
        .await;
        None
    } else {
        // Not active for this (re)start: clear any stale daily error so the tray
        // and UI stop flagging a send that is no longer scheduled. Nothing else
        // clears it once the loop is gone.
        set_status(&status, &id, |s| s.last_daily_error = None).await;
        None
    };

    // Validate token / fetch username up front. If this fails (e.g. a transient
    // network blip at launch) `have_username` stays false and the poll loop
    // re-fetches the handle on its first successful getUpdates, so the dashboard
    // isn't left without an @username until the next restart.
    let mut have_username = false;
    match get_me(&client, &base, &token).await {
        Ok(name) => {
            set_status(&status, &id, |s| {
                s.username = Some(name);
                s.last_error = None;
                s.running = true;
            })
            .await;
            have_username = true;
        }
        Err(e) => {
            set_status(&status, &id, |s| {
                s.running = true;
                s.last_error = Some(format!("getMe failed: {}", e));
            })
            .await;
        }
    }

    // Resume from the persisted offset so a restart doesn't replay old messages.
    let mut offset: i64 = { status.lock().await.get(&id).map(|s| s.offset).unwrap_or(0) };
    let updates_url = format!("{}/bot{}/getUpdates", base, token);

    // Resolve where received files are saved: the configured folder, or an
    // `attachments` folder next to the markdown file when unset.
    let save_dir: PathBuf = resolve_save_dir(&file, files_dir.as_deref());

    // Whether file downloads are subject to the public API's 20 MB cap.
    let capped = base == DEFAULT_API_BASE;

    // Per-bot download queue + crash-safe journal. Files are saved on a
    // background worker so a slow download can't stall the poll loop; the
    // journal lets a restart resume downloads that were acked to Telegram but
    // not yet written to disk.
    let journal_file = journal_path(&status_path, &id);
    let journal: Journal = Arc::new(Mutex::new(load_journal(&journal_file)));
    let (tx, rx) = mpsc::unbounded_channel::<PendingDownload>();
    // Re-enqueue anything left over from a previous run.
    for job in journal.lock().await.iter().cloned() {
        let _ = tx.send(job);
    }
    let worker_task = tokio::spawn(download_worker(
        id.clone(),
        base.clone(),
        token.clone(),
        file.clone(),
        save_dir.clone(),
        status.clone(),
        status_path.clone(),
        journal.clone(),
        journal_file.clone(),
        client.clone(),
        rx,
        stop_rx.clone(),
    ));

    loop {
        if *stop_rx.borrow() {
            break;
        }

        let params: Vec<(&str, String)> = vec![
            ("offset", offset.to_string()),
            ("timeout", "30".to_string()),
            ("allowed_updates", "[\"message\"]".to_string()),
        ];

        let request = client
            .get(&updates_url)
            .query(&params)
            .timeout(Duration::from_secs(45))
            .send();

        let result = tokio::select! {
            _ = stop_rx.changed() => { break; }
            r = request => r,
        };

        match result {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(json) => {
                    if json["ok"].as_bool() != Some(true) {
                        let code = json["error_code"].as_i64().unwrap_or(0);
                        let desc = json["description"]
                            .as_str()
                            .unwrap_or("getUpdates error")
                            .to_string();
                        let msg = if code == 409 {
                            format!(
                                "conflict: this token is already being polled elsewhere ({desc})"
                            )
                        } else {
                            desc
                        };
                        set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                        // 409s won't clear on their own; back off a little longer.
                        let wait = if code == 409 { 15 } else { 5 };
                        if sleep_or_stop(&mut stop_rx, wait).await {
                            break;
                        }
                        continue;
                    }
                    set_status(&status, &id, |s| {
                        s.last_error = None;
                        s.running = true;
                    })
                    .await;

                    // The initial getMe failed but the network is clearly back
                    // now (this getUpdates succeeded), so re-fetch the @username
                    // once instead of leaving the dashboard handle blank for the
                    // rest of the session.
                    if !have_username {
                        if let Ok(name) = get_me(&client, &base, &token).await {
                            set_status(&status, &id, |s| s.username = Some(name)).await;
                            have_username = true;
                        }
                    }

                    if let Some(updates) = json["result"].as_array() {
                        let start_offset = offset;
                        let mut write_failed = false;
                        for update in updates {
                            // Stop between messages rather than working through the
                            // whole batch: a restart spawns the replacement bot
                            // immediately, and two instances appending from the same
                            // offset would duplicate every remaining message.
                            if *stop_rx.borrow() {
                                break;
                            }
                            let update_id = update["update_id"].as_i64();
                            let msg = &update["message"];

                            let from_id = msg["from"]["id"].as_i64().unwrap_or(0);
                            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(from_id);
                            let message_id = msg["message_id"].as_i64().unwrap_or(0);

                            if allowed_user_id != 0 && from_id != allowed_user_id {
                                // Not the owner — ignore, but confirm so it isn't replayed.
                                if let Some(uid) = update_id {
                                    offset = offset.max(uid + 1);
                                }
                                continue;
                            }

                            // Decide what to do. A file/photo/etc is queued for a
                            // background download (so a slow transfer can't stall this
                            // loop); a plain text message is appended inline; anything
                            // else is acknowledged but not saved.
                            let caption = msg["caption"].as_str().unwrap_or("").trim().to_string();

                            if let Some(att) = extract_attachment(msg) {
                                let Some(uid) = update_id else { continue };
                                let display_name =
                                    att.file_name.clone().unwrap_or_else(|| "file".to_string());

                                // Already queued (re-delivered after a crash, or replayed
                                // from the journal) — just confirm it and move on.
                                if journal.lock().await.iter().any(|p| p.update_id == uid) {
                                    offset = offset.max(uid + 1);
                                    continue;
                                }

                                // Already saved on an earlier pass. The journal
                                // entry is gone the moment a download lands, so
                                // it can't answer this — only the recent-saved
                                // list can, and without it a replayed update
                                // would download and note the file a second time.
                                if already_saved(&status, &id, uid).await {
                                    offset = offset.max(uid + 1);
                                    continue;
                                }

                                // Pre-empt the public API's 20 MB cap: a getFile we know
                                // will fail is worth skipping. Record the note now and tell
                                // the user why instead of attempting a doomed download.
                                if capped && att.file_size > PUBLIC_DOWNLOAD_LIMIT {
                                    let mut note = format!(
                                        "could not save file: {} is {} — over the 20 MB Bot API limit; enable the local server to receive it",
                                        display_name,
                                        human_size(att.file_size)
                                    );
                                    if !caption.is_empty() {
                                        note.push_str(&format!(" — {}", caption));
                                    }
                                    match append_timestamped(&file, &note).await {
                                        Ok(()) => {
                                            offset = offset.max(uid + 1);
                                            // The file was *not* saved, so don't bump
                                            // the "saved" counter — just record the
                                            // activity timestamp for the dashboard.
                                            set_status(&status, &id, |s| {
                                                remember_saved(s, uid);
                                                s.last_message_at =
                                                    Some(Local::now().format("%H:%M").to_string());
                                            })
                                            .await;
                                            let _ = send_message(
                                                &client,
                                                &base,
                                                &token,
                                                chat_id,
                                                &format!(
                                                    "⚠️ {} ({}) is over the 20 MB limit and was not saved.",
                                                    display_name,
                                                    human_size(att.file_size)
                                                ),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            set_status(&status, &id, |s| {
                                                s.last_error = Some(format!("write failed: {}", e));
                                            })
                                            .await;
                                            write_failed = true;
                                            break;
                                        }
                                    }
                                    continue;
                                }

                                // Journal the job *before* advancing the offset so a crash
                                // can't ack the update to Telegram with no record of it,
                                // then hand it to the background worker.
                                let job = PendingDownload {
                                    update_id: uid,
                                    file_id: att.file_id,
                                    file_name: att.file_name,
                                    caption: caption.clone(),
                                    chat_id,
                                    message_id,
                                };
                                journal.lock().await.push(job.clone());
                                persist_journal(&journal_file, &journal).await;
                                // Persist the advanced offset *now*, not with the
                                // rest of the batch. The journal is the only record
                                // that this file still needs fetching, and the
                                // worker deletes its entry the moment the download
                                // succeeds — so a quit between "journal entry gone"
                                // and "offset written" would let Telegram re-deliver
                                // the update and save the file a second time.
                                offset = offset.max(uid + 1);
                                set_status(&status, &id, |s| s.offset = s.offset.max(offset)).await;
                                persist_status(&status_path, &status).await;
                                let _ = tx.send(job);
                                // No "Saving…" ack here on purpose: the worker sends
                                // a single "✅ Saved …" message once the download
                                // actually finishes, so a long transfer doesn't look
                                // perpetually stuck on a "Saving…" note.
                                continue;
                            }

                            // Plain text: append inline (fast).
                            let Some(text) = msg["text"].as_str().map(|s| s.to_string()) else {
                                // Non-text, non-file update: nothing to save, but confirm
                                // it so Telegram doesn't keep re-delivering it.
                                if let Some(uid) = update_id {
                                    offset = offset.max(uid + 1);
                                }
                                continue;
                            };

                            // The offset says this message is new; the saved
                            // list is what actually knows. They disagree
                            // whenever the offset has been rolled back, and
                            // that disagreement is what fills a file with
                            // thousands of copies of one line.
                            if let Some(uid) = update_id {
                                if already_saved(&status, &id, uid).await {
                                    offset = offset.max(uid + 1);
                                    continue;
                                }
                            }

                            match append_timestamped(&file, &text).await {
                                Ok(()) => {
                                    // Only advance past a message once it's safely written.
                                    if let Some(uid) = update_id {
                                        offset = offset.max(uid + 1);
                                    }
                                    set_status(&status, &id, |s| {
                                        if let Some(uid) = update_id {
                                            remember_saved(s, uid);
                                        }
                                        s.message_count += 1;
                                        s.last_message_at =
                                            Some(Local::now().format("%H:%M").to_string());
                                        s.last_error = None;
                                    })
                                    .await;
                                    react(&client, &base, &token, chat_id, message_id).await;
                                }
                                Err(e) => {
                                    // Do NOT advance the offset: leaving this update
                                    // unconfirmed makes Telegram re-deliver it next poll
                                    // so a transient write failure can't silently drop a note.
                                    set_status(&status, &id, |s| {
                                        s.last_error = Some(format!("write failed: {}", e));
                                    })
                                    .await;
                                    write_failed = true;
                                    break;
                                }
                            }
                        }
                        // Record the advanced offset + counters so a restart resumes
                        // cleanly without re-delivering already-saved messages.
                        if offset != start_offset {
                            set_status(&status, &id, |s| s.offset = s.offset.max(offset)).await;
                            persist_status(&status_path, &status).await;
                        }
                        // Back off briefly on a write failure so we don't hammer Telegram
                        // re-fetching the same un-writable message in a tight loop.
                        if write_failed && sleep_or_stop(&mut stop_rx, 5).await {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let msg = scrub(e.to_string(), &token);
                    set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                    if sleep_or_stop(&mut stop_rx, 5).await {
                        break;
                    }
                }
            },
            Err(e) => {
                // Network/timeout. Long-poll timeouts are normal; only record real errors.
                if !e.is_timeout() {
                    let msg = scrub(e.to_string(), &token);
                    set_status(&status, &id, |s| s.last_error = Some(msg)).await;
                    if sleep_or_stop(&mut stop_rx, 5).await {
                        break;
                    }
                }
            }
        }
    }

    // Wait for the download worker and the daily digest to finish before
    // returning. `stop_bot` awaits *this* task, so once it completes the caller
    // knows every task belonging to this bot is gone and it is safe to start a
    // replacement — the guarantee that keeps a restart from briefly running two
    // instances against the same file, offset and journal.
    //
    // Dropping `tx` first closes the worker's queue, so it finishes even if it
    // is parked on `recv()`.
    drop(tx);
    let _ = worker_task.await;
    if let Some(t) = daily_task {
        let _ = t.await;
    }

    // Deliberately don't clear `running` here: `stop_bot` already did so before
    // signalling stop, and clearing it again would race a freshly started task
    // during a restart, flipping its `running = true` back to false.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_saved_is_idempotent_and_bounded() {
        let mut st = BotStatus::default();
        for uid in 0..(RECENT_SAVED_MAX as i64 + 50) {
            remember_saved(&mut st, uid);
            remember_saved(&mut st, uid); // a replay must not grow the list
        }
        assert_eq!(st.saved.len(), RECENT_SAVED_MAX);
        // The oldest are dropped, the newest kept.
        assert!(!st.saved.contains(&0));
        assert!(st.saved.contains(&(RECENT_SAVED_MAX as i64 + 49)));
    }

    #[tokio::test]
    async fn persist_status_never_moves_a_bot_backwards() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "msg2note-status-{}-{}.json",
            std::process::id(),
            stamp
        ));

        // What another process already recorded.
        let ahead: StatusMap = Arc::new(Mutex::new(HashMap::from([(
            "bot".to_string(),
            BotStatus {
                offset: 200,
                message_count: 20,
                saved: vec![199],
                ..Default::default()
            },
        )])));
        persist_status(&path, &ahead).await;

        // A writer that knows less must not erase what the file knows.
        let behind: StatusMap = Arc::new(Mutex::new(HashMap::from([(
            "bot".to_string(),
            BotStatus {
                offset: 100,
                message_count: 10,
                saved: vec![99],
                ..Default::default()
            },
        )])));
        persist_status(&path, &behind).await;

        let on_disk = load_status(&path);
        let bot = on_disk.get("bot").expect("bot survived the merge");
        assert_eq!(bot.offset, 200);
        assert_eq!(bot.message_count, 20);
        assert_eq!(bot.saved, vec![99, 199]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chunk_ranges_tile_the_input_within_the_limit() {
        for text in [
            "",
            "one line",
            "a\nb\nc\n",
            &"x".repeat(TELEGRAM_MSG_LIMIT * 3 + 7),
            &format!("short\n{}\nshort\n", "y".repeat(TELEGRAM_MSG_LIMIT + 1)),
            &"line of text\n".repeat(900),
            "héllo ünicode ✅ 中文\n",
            &"✅".repeat(TELEGRAM_MSG_LIMIT),
        ] {
            let mut cursor = 0;
            for (a, b) in chunk_ranges(text) {
                assert_eq!(a, cursor, "chunks must be contiguous");
                assert!(b > a, "empty chunk");
                assert!(b - a <= TELEGRAM_MSG_LIMIT, "chunk over the limit");
                cursor = b;
            }
            assert_eq!(cursor, text.len(), "chunks must cover the whole input");
        }
    }

    #[test]
    fn filenames_are_reduced_to_a_safe_basename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a/b\\c.txt"), "c.txt");
        assert_eq!(sanitize_filename("re;po rt.pdf"), "re_po rt.pdf");
        assert_eq!(sanitize_filename("  ...  "), "file");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("/"), "file");
        // Non-ASCII letters are alphanumeric and are kept.
        assert_eq!(sanitize_filename("報告.pdf"), "報告.pdf");
    }

    #[test]
    fn sizes_use_decimal_units_matching_the_api_cap() {
        assert_eq!(human_size(0), "unknown size");
        assert_eq!(human_size(-1), "unknown size");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2_600), "3 KB");
        assert_eq!(human_size(20_000_000), "20.0 MB");
        assert_eq!(human_size(3_400_000_000), "3.4 GB");
        // A file just over the cap must not read as "19.x MB".
        assert_eq!(human_size(PUBLIC_DOWNLOAD_LIMIT + 1), "20.0 MB");
    }

    #[test]
    fn daily_time_parsing_clamps_and_defaults() {
        assert_eq!(parse_hhmm("07:30"), (7, 30));
        assert_eq!(parse_hhmm(" 23:59 "), (23, 59));
        assert_eq!(parse_hhmm("25:70"), (23, 59));
        assert_eq!(parse_hhmm("nonsense"), (8, 0));
        assert_eq!(parse_hhmm(""), (8, 0));
        assert_eq!(parse_hhmm("9"), (9, 0));
    }

    #[test]
    fn api_base_normalizes_to_a_bare_origin() {
        assert_eq!(resolve_api_base(None), DEFAULT_API_BASE);
        assert_eq!(resolve_api_base(Some("   ")), DEFAULT_API_BASE);
        assert_eq!(
            resolve_api_base(Some("https://api.telegram.org/")),
            DEFAULT_API_BASE
        );
        assert_eq!(
            resolve_api_base(Some("  http://127.0.0.1:8081//  ")),
            "http://127.0.0.1:8081"
        );
    }

    #[test]
    fn save_dir_falls_back_to_attachments_beside_the_note() {
        assert_eq!(
            resolve_save_dir("/notes/ideas.md", Some("/inbox")),
            PathBuf::from("/inbox")
        );
        assert_eq!(
            resolve_save_dir("/notes/ideas.md", Some("  ")),
            PathBuf::from("/notes/attachments")
        );
        assert_eq!(
            resolve_save_dir("/notes/ideas.md", None),
            PathBuf::from("/notes/attachments")
        );
        assert_eq!(
            resolve_save_dir("ideas.md", None),
            PathBuf::from("attachments")
        );
    }

    #[test]
    fn lock_file_is_a_hidden_sidecar_next_to_the_note() {
        assert_eq!(
            sidecar_lock_path("/notes/ideas.md"),
            PathBuf::from("/notes/.ideas.md.lock")
        );
        assert_eq!(
            sidecar_lock_path("ideas.md"),
            PathBuf::from(".ideas.md.lock")
        );
    }

    #[test]
    fn unique_dest_never_collides() {
        let dir = std::env::temp_dir().join(format!("m2n-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = unique_dest(&dir, "report.pdf");
        std::fs::write(&a, b"x").unwrap();
        let b = unique_dest(&dir, "report.pdf");
        assert_ne!(a, b);
        // The counter goes before the extension, not after it.
        assert_eq!(b.extension().and_then(|e| e.to_str()), Some("pdf"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn errors_never_carry_the_token() {
        let token = "123456:AA-secret";
        let msg =
            format!("error sending request for url (https://api.telegram.org/bot{token}/getMe)");
        let scrubbed = scrub(msg, token);
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("<redacted>"));
        // An empty token must not turn every message into redaction noise.
        assert_eq!(scrub("plain".to_string(), ""), "plain");
    }

    #[test]
    fn attachment_extraction_prefers_documents_and_largest_photo() {
        let doc = serde_json::json!({ "document": { "file_id": "D", "file_name": "a.pdf", "file_size": 10 } });
        let got = extract_attachment(&doc).unwrap();
        assert_eq!(got.file_id, "D");
        assert_eq!(got.file_name.as_deref(), Some("a.pdf"));
        assert_eq!(got.file_size, 10);

        let photo = serde_json::json!({
            "photo": [
                { "file_id": "small", "file_size": 1 },
                { "file_id": "large", "file_size": 99 }
            ]
        });
        assert_eq!(extract_attachment(&photo).unwrap().file_id, "large");

        let sticker = serde_json::json!({ "sticker": { "file_id": "S" } });
        let got = extract_attachment(&sticker).unwrap();
        assert_eq!(got.file_id, "S");
        assert_eq!(got.file_size, 0);

        assert!(extract_attachment(&serde_json::json!({ "text": "hi" })).is_none());
    }
}
