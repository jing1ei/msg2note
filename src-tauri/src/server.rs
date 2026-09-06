//! App-managed local Telegram Bot API server.
//!
//! The public Bot API at api.telegram.org caps file downloads at 20 MB, which
//! is smaller than most videos. A self-hosted `telegram-bot-api` server run in
//! `--local` mode lifts that to 2 GB and writes downloaded files straight to
//! disk, so the app can read them without a second HTTP round trip.
//!
//! This module locates the binary and supervises a single child process. The
//! process is killed when its [`ServerHandle`] is dropped, so it never outlives
//! the app.

use crate::config::ServerConfig;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a freshly spawned server to start accepting connections
/// before declaring it failed.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Install locations probed when no explicit binary path is configured.
/// Covers Homebrew on Apple Silicon and Intel plus the usual system prefixes.
const CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/telegram-bot-api",
    "/usr/local/bin/telegram-bot-api",
    "/usr/bin/telegram-bot-api",
];

/// Resolve the `telegram-bot-api` binary: an explicit configured path first,
/// then the common install locations, then a `PATH` lookup via `which`.
/// Returns `None` if nothing usable is found.
pub fn locate_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        let pb = PathBuf::from(p);
        return pb.exists().then_some(pb);
    }
    for c in CANDIDATES {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(out) = Command::new("which").arg("telegram-bot-api").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

/// The origin a bot should talk to when the local server is running.
pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// A running local Bot API server. Dropping the handle kills the child process
/// (and reaps it), so the server can't be left orphaned when the app exits or
/// the settings change.
pub struct ServerHandle {
    child: Child,
    /// The port this process is listening on. Bots are routed using *this*, not
    /// the configured port, so a config edit can never point them somewhere the
    /// running server isn't.
    pub port: u16,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the local `telegram-bot-api` server in `--local` mode.
///
/// `data_dir` is where the server stores downloaded files; the app reads them
/// from there directly (see `bots::download_attachment`). Fails fast with a
/// human-readable message when credentials are missing or the binary can't be
/// found, so the error can be surfaced in the settings UI.
pub fn start(cfg: &ServerConfig, data_dir: &Path) -> Result<ServerHandle, String> {
    if cfg.api_id == 0 || cfg.api_hash.trim().is_empty() {
        return Err("api_id and api_hash are required to run the local server".into());
    }
    let bin = locate_binary(cfg.bin_path.as_deref()).ok_or_else(|| {
        "telegram-bot-api not found — build it from source \
         (https://tdlib.github.io/telegram-bot-api/build.html) and set its path in server settings"
            .to_string()
    })?;

    let temp_dir = data_dir.join("temp");
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let port = cfg.effective_port();
    // Refuse to start if something else already owns the port. The readiness
    // probe below only checks that *something* accepts on 127.0.0.1:port, so a
    // foreign listener would be mistaken for our server — and every bot's token
    // would then be sent to it.
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(probe) => drop(probe),
        Err(e) => {
            return Err(format!(
                "port {port} is already in use ({e}) - stop whatever is listening on it, \
                 or choose another port in server settings"
            ))
        }
    }

    // Keep the server's own diagnostics: without them a refused connection or an
    // instant exit is indistinguishable from a wrong port, and there is nowhere
    // to look. Truncated each start so it can't grow without bound.
    let log_path = data_dir.join("server.log");
    let log = std::fs::File::create(&log_path).map_err(|e| {
        format!(
            "could not open the server log at {}: {}",
            log_path.display(),
            e
        )
    })?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("could not open the server log: {}", e))?;

    // Credentials go through the environment, not argv: `telegram-bot-api` reads
    // TELEGRAM_API_ID / TELEGRAM_API_HASH, and argv is world-readable via `ps`,
    // which would undo the point of keeping the api_hash in the Keychain.
    //
    // `--local` writes downloads straight to disk; binding to 127.0.0.1 keeps the
    // server off the network entirely.
    let mut child = Command::new(&bin)
        .arg("--local")
        .arg("--http-ip-address=127.0.0.1")
        .arg(format!("--http-port={}", port))
        .arg(format!("--dir={}", data_dir.display()))
        .arg(format!("--temp-dir={}", temp_dir.display()))
        .env("TELEGRAM_API_ID", cfg.api_id.to_string())
        .env("TELEGRAM_API_HASH", cfg.api_hash.trim())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("could not start telegram-bot-api: {}", e))?;

    // Spawning only proves the binary was executable: `telegram-bot-api` exits
    // within milliseconds on bad credentials or an already-bound port. Wait for
    // the socket to actually accept before calling the server healthy.
    match wait_until_listening(&mut child, port) {
        Ok(()) => Ok(ServerHandle { child, port }),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{} (details in {})", e, log_path.display()))
        }
    }
}

/// Block until the server accepts a TCP connection on `port`, it exits, or
/// [`READY_TIMEOUT`] elapses.
fn wait_until_listening(child: &mut Child, port: u16) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        // A server that has already exited will never listen; report why.
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "telegram-bot-api exited immediately ({status}) — usually a wrong \
                     api_id/api_hash, or port {port} is already in use"
                ))
            }
            Ok(None) => {}
            Err(e) => return Err(format!("could not check on telegram-bot-api: {e}")),
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "telegram-bot-api did not start listening on port {port} within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}
