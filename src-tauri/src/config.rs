use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    8081
}

fn default_daily_time() -> String {
    "08:00".to_string()
}

/// Settings for an app-managed local Telegram Bot API server. Running one lifts
/// the public API's 20 MB download cap to 2 GB, so large videos can be saved.
/// A single server instance serves every bot.
#[derive(Serialize, Deserialize, Clone)]
pub struct ServerConfig {
    /// Whether the app should spawn and manage a local server.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the `telegram-bot-api` binary. Unset/empty = auto-detect from
    /// common install locations and `PATH`.
    #[serde(default)]
    pub bin_path: Option<String>,
    /// HTTP port the local server listens on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Telegram `api_id` from my.telegram.org. Not secret.
    #[serde(default)]
    pub api_id: i64,
    /// Telegram `api_hash`. Secret: stored in the Keychain, never serialized to
    /// `bots.json`. Hydrated into memory at startup.
    #[serde(default, skip_serializing)]
    pub api_hash: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bin_path: None,
            port: default_port(),
            api_id: 0,
            api_hash: String::new(),
        }
    }
}

impl ServerConfig {
    /// The port the server actually listens on. A `0` (e.g. a hand-edited
    /// config) falls back to the default so the spawned server and the URL bots
    /// are routed to can never disagree.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            default_port()
        } else {
            self.port
        }
    }
}

/// One bot bound to a markdown file it writes to, and optionally a second one
/// it reads from for the daily send-back.
#[derive(Serialize, Deserialize, Clone)]
pub struct BotConfig {
    pub id: String,
    pub name: String,
    /// The Telegram bot token. In-memory only: stored in the macOS Keychain (see
    /// `secrets.rs`), never serialized to `bots.json`. `default` lets older
    /// plaintext configs still deserialize so their token can be migrated out.
    #[serde(default, skip_serializing)]
    pub token: String,
    pub file: String,
    /// Markdown file the daily send-back reads. Unset or empty means "same as
    /// `file`", which is the historical behaviour: the bot sends back whatever
    /// it writes to. Set it to a different path to read one file and write
    /// another. Only ever read, never written to.
    #[serde(default)]
    pub read_file: Option<String>,
    /// Folder where received files (documents, photos, etc.) are saved. If unset
    /// or empty, files go to an `attachments` folder next to the markdown file.
    #[serde(default)]
    pub files_dir: Option<String>,
    /// Telegram numeric user id allowed to write. 0 = allow anyone (not recommended).
    #[serde(default)]
    pub allowed_user_id: i64,
    /// Base URL of the Telegram Bot API. Unset/empty uses the public
    /// `https://api.telegram.org`, which caps file downloads at 20 MB. Point this
    /// at a self-hosted local Bot API server (e.g. `http://127.0.0.1:8081` — the
    /// app binds the managed server to `127.0.0.1`, so prefer that over
    /// `localhost`, which can resolve to IPv6 `::1` and miss it) to
    /// raise the limit to 2 GB so large videos can be saved.
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional global hotkey for local quick-capture, e.g. "CmdOrCtrl+Shift+KeyI".
    #[serde(default)]
    pub shortcut: Option<String>,
    /// When true, the bot sends the markdown file's exact contents back to the
    /// owner once a day at `daily_time`. Requires `allowed_user_id` to be set (in
    /// a private chat the user id is also the chat id we send to).
    #[serde(default)]
    pub daily_send: bool,
    /// Local time of day for the daily send, "HH:MM" (24-hour). Defaults to 08:00.
    #[serde(default = "default_daily_time")]
    pub daily_time: String,
}

impl BotConfig {
    /// The file the daily send-back reads. `read_file` when it is set to a
    /// non-blank path, otherwise the file the bot writes to — so an existing
    /// config with no `read_file` behaves exactly as before.
    pub fn read_path(&self) -> String {
        match self.read_file.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => self.file.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub bots: Vec<BotConfig>,
    /// Global settings for the app-managed local Bot API server.
    #[serde(default)]
    pub server: ServerConfig,
}

impl Config {
    /// Load config from disk. Returns an empty config if the file does not exist.
    ///
    /// If the file exists but cannot be parsed, the corrupt file is moved aside
    /// (to `<name>.corrupt-<timestamp>`) rather than silently discarded, so a
    /// subsequent `save()` can never overwrite recoverable data.
    pub fn load(path: &Path) -> Config {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        match serde_json::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                // Built from the *whole* file name so the backup keeps the
                // original `.json` extension.
                let backup = sibling(
                    path,
                    "corrupt",
                    &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
                );
                let _ = std::fs::rename(path, &backup);
                eprintln!(
                    "msg2note: failed to parse {} ({e}); moved aside to {}",
                    path.display(),
                    backup.display()
                );
                Config::default()
            }
        }
    }

    /// Persist config to disk (pretty JSON), written atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(path, text.as_bytes())
    }
}

/// A sibling of `path` named `<file name>.<tag>-<suffix>`, keeping the original
/// extension so the file it came from stays recognisable. Both tags are covered
/// by the repo's `.gitignore`.
pub(crate) fn sibling(path: &Path, tag: &str, suffix: &str) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "msg2note".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    path.with_file_name(format!("{name}.{tag}-{suffix}"))
}

/// Write `bytes` to `path` atomically: write to a sibling temp file, fsync, then
/// rename over the target. A crash mid-write leaves the original file intact.
///
/// Every failure path removes the temp file, so a full disk or a permissions
/// change can't leave `.tmp-<uuid>` orphans accumulating in the config dir.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Unique temp name so concurrent writers (e.g. several bots persisting
    // status at once) don't clobber each other's in-flight temp file.
    let tmp = sibling(path, "tmp", &Uuid::new_v4().to_string());
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("m2n-cfg-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = scratch();
        let path = dir.join("bots.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "bots.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn temp_and_backup_names_keep_the_original_extension() {
        let p = Path::new("/cfg/bots.json");
        assert_eq!(
            sibling(p, "tmp", "abc"),
            PathBuf::from("/cfg/bots.json.tmp-abc")
        );
        assert_eq!(
            sibling(p, "corrupt", "20260101-000000"),
            PathBuf::from("/cfg/bots.json.corrupt-20260101-000000")
        );
    }

    #[test]
    fn a_corrupt_config_is_moved_aside_not_overwritten() {
        let dir = scratch();
        let path = dir.join("bots.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let cfg = Config::load(&path);
        assert!(cfg.bots.is_empty());
        // The unreadable original must still exist under a backup name.
        assert!(!path.exists(), "corrupt file was left in place");
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1, "expected exactly one backup: {backups:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_config_is_an_empty_config() {
        let cfg = Config::load(Path::new("/definitely/not/here/bots.json"));
        assert!(cfg.bots.is_empty());
        assert!(!cfg.server.enabled);
        assert_eq!(cfg.server.port, default_port());
    }

    #[test]
    fn tokens_are_never_serialized_to_disk() {
        let dir = scratch();
        let path = dir.join("bots.json");
        let mut cfg = Config::default();
        cfg.bots.push(BotConfig {
            id: "id-1".into(),
            name: "Ideas".into(),
            token: "123456:SUPER-SECRET".into(),
            file: "/notes/ideas.md".into(),
            read_file: None,
            files_dir: None,
            allowed_user_id: 42,
            api_base: None,
            enabled: true,
            shortcut: None,
            daily_send: false,
            daily_time: "08:00".into(),
        });
        cfg.server.api_hash = "hash-secret".into();
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SUPER-SECRET"), "bot token hit the disk");
        assert!(!text.contains("hash-secret"), "api_hash hit the disk");

        // …and a saved config round-trips everything else.
        let back = Config::load(&path);
        assert_eq!(back.bots.len(), 1);
        assert_eq!(back.bots[0].allowed_user_id, 42);
        assert!(back.bots[0].token.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_path_falls_back_to_the_written_file() {
        let mut b = BotConfig {
            id: "i".into(),
            name: "n".into(),
            token: String::new(),
            file: "/notes/inbox.md".into(),
            read_file: None,
            files_dir: None,
            allowed_user_id: 0,
            api_base: None,
            enabled: true,
            shortcut: None,
            daily_send: false,
            daily_time: "08:00".into(),
        };
        assert_eq!(b.read_path(), "/notes/inbox.md");
        b.read_file = Some("   ".into());
        assert_eq!(b.read_path(), "/notes/inbox.md");
        b.read_file = Some(" /notes/plan.md ".into());
        assert_eq!(b.read_path(), "/notes/plan.md");
    }

    #[test]
    fn a_zero_port_falls_back_to_the_default() {
        let mut s = ServerConfig::default();
        assert_eq!(s.effective_port(), default_port());
        s.port = 0;
        assert_eq!(s.effective_port(), default_port());
        s.port = 9000;
        assert_eq!(s.effective_port(), 9000);
    }
}
