//! Msg2Note — a macOS menu bar app binding Telegram bots to local markdown
//! files. macOS-only by construction: Keychain-backed secrets, an `Accessory`
//! activation policy, and `open -t` for revealing notes.

mod bots;
mod config;
mod secrets;
mod server;

use bots::{run_bot, BotStatus, StatusMap};
use chrono::Local;
use config::{BotConfig, Config, ServerConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

/// Handle to a running bot: the stop signal plus the task itself, so a stop can
/// be awaited rather than merely requested.
pub struct BotHandle {
    stop: watch::Sender<bool>,
    task: tauri::async_runtime::JoinHandle<()>,
}

pub struct AppState {
    pub config_path: PathBuf,
    /// Where persisted per-bot status (counters + long-poll offset) lives.
    pub status_path: PathBuf,
    pub config: Mutex<Config>,
    /// Running bots, by id. Each handle owns the task so `stop_bot` can wait for
    /// it to actually finish before a replacement is started.
    pub handles: Mutex<HashMap<String, BotHandle>>,
    pub status: StatusMap,
    /// Shared HTTP client, reused across all bots and token validation.
    pub http: reqwest::Client,
    /// Registered hotkey -> bot id, for local quick-capture.
    pub shortcuts: Mutex<HashMap<Shortcut, String>>,
    /// Which bot the quick-capture window should write to right now.
    pub quick_target: Mutex<Option<(String, String)>>,
    /// The app-managed local Bot API server, when running. A std Mutex (not the
    /// async one) so it can be locked and the child killed synchronously from
    /// the app's exit handler.
    pub server: std::sync::Mutex<Option<server::ServerHandle>>,
    /// The last error from trying to start the managed local server (cleared once
    /// it starts, or when it's disabled). Surfaced in the server settings UI so a
    /// startup failure stays visible instead of being overwritten by per-bot
    /// status once the bots fall back to the public API.
    pub server_error: std::sync::Mutex<Option<String>>,
}

/// The port the managed local server is actually listening on, or `None` when
/// no server is running.
///
/// This is the single source of truth for "is the local server usable": it comes
/// from the live handle, not from the config, so a bot can never be routed at a
/// port nothing is bound to. Routing a bot at a dead loopback port would break it
/// outright instead of letting it degrade to the public API.
fn running_server_port(state: &AppState) -> Option<u16> {
    state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.port)
}

/// Resolve the Bot API base a given bot should use: an explicit per-bot override
/// wins; otherwise the managed local server when it's enabled and listening;
/// otherwise `None`, meaning the public Telegram API.
fn effective_api_base(
    bot_override: Option<&str>,
    server: &ServerConfig,
    server_port: Option<u16>,
) -> Option<String> {
    if let Some(explicit) = bot_override.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(explicit.to_string());
    }
    match server_port {
        Some(port) if server.enabled => Some(server::local_url(port)),
        _ => None,
    }
}

#[derive(Serialize)]
struct BotView {
    id: String,
    name: String,
    /// Whether a token is stored, so the edit form can show "leave blank to
    /// keep". The token itself is never sent to the webview.
    has_token: bool,
    file: String,
    /// Path the daily send-back reads. `None`/empty means "same as `file`".
    read_file: Option<String>,
    files_dir: Option<String>,
    /// Sent to the webview as a string so a 64-bit Telegram user id can't lose
    /// precision when JS reads it (JSON numbers are IEEE-754 doubles, exact only
    /// up to 2^53). The form posts it back as a string too; see `add_bot`.
    allowed_user_id: String,
    api_base: Option<String>,
    enabled: bool,
    shortcut: Option<String>,
    daily_send: bool,
    daily_time: String,
    status: BotStatus,
}

#[derive(Serialize, Clone)]
struct QuickTarget {
    id: String,
    name: String,
}

/// The bot fields the manager form posts, in one payload.
///
/// `add_bot` and `update_bot` take this rather than a dozen positional
/// parameters each, so the two commands cannot drift apart and the form's
/// normalization lives in exactly one place ([`BotForm::into_config`]).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotForm {
    name: String,
    /// Blank on edit means "keep the token already in the Keychain".
    token: String,
    file: String,
    read_file: Option<String>,
    files_dir: Option<String>,
    /// Arrives as a string so a 64-bit Telegram user id can't lose precision in
    /// JS (see `BotView::allowed_user_id`); parsed back to i64 here.
    allowed_user_id: String,
    api_base: Option<String>,
    enabled: bool,
    shortcut: Option<String>,
    daily_send: bool,
    daily_time: String,
}

/// Trim an optional field, treating an all-whitespace value as absent, so a
/// blank form field is stored as "unset" rather than as an empty string every
/// later reader has to re-check.
fn norm(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Shared validation for the add/edit forms.
///
/// A *blank* user id legitimately means "allow anyone". An *unparseable* one is
/// a typo (a pasted @handle, a letter O for a zero), and letting `into_config`
/// fold it to the same 0 sentinel would switch off the bot's only access check
/// without saying so.
fn validate_form(form: &BotForm) -> Result<(), String> {
    if form.file.trim().is_empty() {
        return Err("a markdown file path is required".into());
    }
    let uid = form.allowed_user_id.trim();
    if !uid.is_empty() && uid.parse::<i64>().is_err() {
        return Err(
            "the Telegram user ID must be a number (leave it blank to allow anyone)".into(),
        );
    }
    Ok(())
}

/// Reject a token another bot entry is already bound to.
///
/// Two entries sharing one token means two long-poll loops draining one update
/// queue with independent offsets: both receive the same update and both append
/// it, so every message lands in the markdown file twice (and keeps landing,
/// each time the lower offset re-requests it).
fn ensure_token_unused(
    config: &Config,
    token: &str,
    except_id: Option<&str>,
) -> Result<(), String> {
    match config
        .bots
        .iter()
        .find(|b| b.token == token && Some(b.id.as_str()) != except_id)
    {
        Some(other) => Err(format!(
            "that token already belongs to \"{}\" — two bots on one token save every message twice",
            other.name
        )),
        None => Ok(()),
    }
}

impl BotForm {
    /// The submitted token, trimmed, or `None` when the field was left blank.
    fn token(&self) -> Option<&str> {
        let t = self.token.trim();
        (!t.is_empty()).then_some(t)
    }

    /// Build a complete bot record from the form. All trimming and defaulting
    /// happens here, so the rest of the app never sees a stray-whitespace path
    /// or a present-but-blank override.
    fn into_config(self, id: String, token: String) -> BotConfig {
        let name = self.name.trim();
        let daily_time = self.daily_time.trim();
        BotConfig {
            id,
            name: if name.is_empty() {
                "Untitled".to_string()
            } else {
                name.to_string()
            },
            token,
            file: self.file.trim().to_string(),
            read_file: norm(self.read_file),
            files_dir: norm(self.files_dir),
            // Blank or unparseable means 0 — "allow anyone".
            allowed_user_id: self.allowed_user_id.trim().parse::<i64>().unwrap_or(0),
            api_base: norm(self.api_base),
            enabled: self.enabled,
            shortcut: norm(self.shortcut),
            daily_send: self.daily_send,
            daily_time: if daily_time.is_empty() {
                "08:00".to_string()
            } else {
                daily_time.to_string()
            },
        }
    }
}

// ---- bot lifecycle helpers ----

async fn start_bot(bot: &BotConfig, state: &AppState) {
    stop_bot(&bot.id, state).await;

    let (tx, rx) = watch::channel(false);
    {
        let mut s = state.status.lock().await;
        s.entry(bot.id.clone()).or_default().running = true;
    }

    let id = bot.id.clone();
    let token = bot.token.clone();
    let file = bot.file.clone();
    let read_file = bot.read_path();
    let files_dir = bot.files_dir.clone();
    let allowed = bot.allowed_user_id;
    let daily_send = bot.daily_send;
    let daily_time = bot.daily_time.clone();
    // Use the managed local server (or a per-bot override) when configured.
    // Only route to the local server when its process is actually up, so a bot
    // falls back to the public API instead of a dead port if it failed to start.
    let api_base = {
        let server_port = running_server_port(state);
        let cfg = state.config.lock().await;
        effective_api_base(bot.api_base.as_deref(), &cfg.server, server_port)
    };
    let status = state.status.clone();
    let status_path = state.status_path.clone();
    let client = state.http.clone();
    let task = tauri::async_runtime::spawn(async move {
        run_bot(
            id,
            token,
            file,
            read_file,
            files_dir,
            allowed,
            api_base,
            daily_send,
            daily_time,
            status,
            status_path,
            client,
            rx,
        )
        .await;
    });

    // Two overlapping `start_bot` calls for the same bot (e.g. a double-clicked
    // enable toggle) would otherwise have the second `insert` drop the first
    // handle's sender without ever signalling stop, orphaning that instance.
    if let Some(previous) = state
        .handles
        .lock()
        .await
        .insert(bot.id.clone(), BotHandle { stop: tx, task })
    {
        let _ = previous.stop.send(true);
        // Signalling isn't enough: until the old task actually returns it shares
        // this bot's offset, journal and markdown file with the instance just
        // spawned. Wait it out (aborting if it wedges) while still holding the
        // handles lock, so a third concurrent start can't slip in between.
        let mut previous_task = previous.task;
        if tokio::time::timeout(STOP_TIMEOUT, &mut previous_task)
            .await
            .is_err()
        {
            previous_task.abort();
        }
    }
}

/// How long `stop_bot` waits for a bot's tasks to wind down before giving up.
/// Every loop in `bots.rs` checks the stop flag between steps and races its
/// network calls against it, so this is generous; the cap exists only so a wedged
/// task can never freeze the UI.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Signal a bot to stop and wait for its tasks to finish.
///
/// Waiting matters: `start_bot` calls this first, and a bot that is still draining
/// its update batch shares the persisted long-poll offset, the pending-download
/// journal and the markdown file with its replacement. Two instances running at
/// once duplicate saved messages and downloaded files, and race each other's
/// writes to the journal.
async fn stop_bot(id: &str, state: &AppState) {
    let handle = state.handles.lock().await.remove(id);
    if let Some(h) = handle {
        let _ = h.stop.send(true);
        // A dropped JoinHandle *detaches* the task rather than cancelling it, so
        // letting the timeout consume it would leave a wedged bot appending to
        // the same file, offset and journal as its replacement. Abort instead.
        let mut task = h.task;
        if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
            task.abort();
        }
    }
    if let Some(s) = state.status.lock().await.get_mut(id) {
        s.running = false;
    }
}

// ---- local Bot API server lifecycle ----

/// Where the managed server stores its data (downloaded files live here).
fn server_data_dir(state: &AppState) -> PathBuf {
    state
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bot-api")
}

/// Tear down any running managed server, then start a fresh one if the current
/// config enables it. Returns an error (with the reason) when an enabled server
/// fails to start, leaving no server running.
async fn restart_local_server(state: &AppState) -> Result<(), String> {
    // Dropping the old handle kills its child process. Take it out under the
    // lock and drop it *after* releasing, so nothing else blocks on the mutex
    // while kill()/waitpid() runs.
    {
        let taken = state
            .server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(taken);
    }
    let cfg = { state.config.lock().await.server.clone() };
    if !cfg.enabled {
        *state.server_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Ok(());
    }
    let data_dir = server_data_dir(state);
    // `server::start` waits for the child to accept connections, which blocks for
    // up to ten seconds — off the async runtime, so it can't stall the UI or the
    // other bots' poll loops.
    let started = tokio::task::spawn_blocking(move || server::start(&cfg, &data_dir))
        .await
        .unwrap_or_else(|e| Err(format!("could not start the local server: {e}")));

    // Each lock is taken and released on its own statement — never two at once —
    // so these can't deadlock against the reverse order used elsewhere.
    match started {
        Ok(handle) => {
            *state.server.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            *state.server_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Ok(())
        }
        Err(e) => {
            // Keep the reason so the settings UI can show why the server is down.
            *state.server_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
            Err(e)
        }
    }
}

/// Restart every bot so they pick up a changed API base (e.g. after the local
/// server is toggled). Enabled bots are (re)started; disabled ones stopped.
async fn restart_all_bots(state: &AppState) {
    let bots = { state.config.lock().await.bots.clone() };
    for b in &bots {
        if b.enabled {
            start_bot(b, state).await;
        } else {
            stop_bot(&b.id, state).await;
        }
    }
}

// ---- global shortcuts ----

/// Re-register all configured hotkeys from the current config.
async fn sync_shortcuts(app: &tauri::AppHandle) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let bots = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().await;
        cfg.bots.clone()
    };

    let mut map: HashMap<Shortcut, String> = HashMap::new();
    for b in bots {
        let Some(sc) = b.shortcut.as_ref() else {
            continue;
        };
        if sc.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = sc.parse::<Shortcut>() {
            if gs.register(parsed).is_ok() {
                map.insert(parsed, b.id.clone());
            }
        }
    }

    let state = app.state::<AppState>();
    *state.shortcuts.lock().await = map;
}

/// When a hotkey fires, show the quick-capture window targeting its bot.
async fn open_quick_for_shortcut(app: &tauri::AppHandle, sc: &Shortcut) {
    let state = app.state::<AppState>();
    let id = { state.shortcuts.lock().await.get(sc).cloned() };
    let Some(id) = id else { return };
    let name = {
        let cfg = state.config.lock().await;
        cfg.bots
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.name.clone())
            .unwrap_or_default()
    };
    *state.quick_target.lock().await = Some((id.clone(), name.clone()));

    if let Some(w) = app.get_webview_window("quick") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.emit("quick-open", QuickTarget { id, name });
    }
}

// ---- commands ----

#[tauri::command]
async fn get_bots(state: State<'_, AppState>) -> Result<Vec<BotView>, String> {
    let config = state.config.lock().await;
    let status = state.status.lock().await;
    let out = config
        .bots
        .iter()
        .map(|b| BotView {
            id: b.id.clone(),
            name: b.name.clone(),
            has_token: !b.token.is_empty(),
            file: b.file.clone(),
            read_file: b.read_file.clone(),
            files_dir: b.files_dir.clone(),
            allowed_user_id: b.allowed_user_id.to_string(),
            api_base: b.api_base.clone(),
            enabled: b.enabled,
            shortcut: b.shortcut.clone(),
            daily_send: b.daily_send,
            daily_time: b.daily_time.clone(),
            status: status.get(&b.id).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(out)
}

#[tauri::command]
async fn add_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    form: BotForm,
) -> Result<(), String> {
    // A bot with no file has nowhere to write and a bot with no token could
    // never poll. Reject both before touching the Keychain, rather than storing
    // an empty entry and surfacing it later as a puzzling "getMe failed".
    validate_form(&form)?;
    let token = form.token().ok_or("a bot token is required")?.to_string();
    // Checked before the Keychain write so a rejected duplicate leaves nothing
    // behind. Short scope: `start_bot` below takes this lock too.
    {
        let config = state.config.lock().await;
        ensure_token_unused(&config, &token, None)?;
    }
    let bot = form.into_config(Uuid::new_v4().to_string(), token);
    // Store the token in the Keychain before persisting the (token-less) config.
    secrets::set_token(&bot.id, &bot.token)?;
    {
        let mut config = state.config.lock().await;
        config.bots.push(bot.clone());
        if let Err(e) = config.save(&state.config_path) {
            // Roll back so a failed save doesn't leave an orphan Keychain entry
            // or an in-memory bot that was never persisted.
            config.bots.pop();
            secrets::delete_token(&bot.id);
            return Err(e.to_string());
        }
    }
    if bot.enabled {
        start_bot(&bot, state.inner()).await;
    }
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn update_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    form: BotForm,
) -> Result<(), String> {
    validate_form(&form)?;
    // A blank token means "keep the existing one".
    let new_token = form.token().map(str::to_string);
    // Before the Keychain write below: returning here must not strand a new
    // token under this bot's id with the old one already gone.
    if let Some(t) = &new_token {
        let config = state.config.lock().await;
        ensure_token_unused(&config, t, Some(&id))?;
    }
    // Write the replacement to the Keychain *before* taking the config lock: it
    // is a blocking Security-framework call that can raise a system prompt, and
    // holding the async config mutex across it stalls every other command, the
    // tray refresh, and every bot's start_bot.
    if let Some(t) = &new_token {
        secrets::set_token(&id, t)?;
    }
    let updated;
    {
        let mut config = state.config.lock().await;
        let Some(idx) = config.bots.iter().position(|b| b.id == id) else {
            // Nothing was updated — don't leave a token behind for a bot that
            // isn't in the config.
            if new_token.is_some() {
                secrets::delete_token(&id);
            }
            return Err("bot not found".into());
        };
        // Keep the previous state so a failed save can be rolled back.
        let prev = config.bots[idx].clone();
        let token = match &new_token {
            Some(t) => t.clone(),
            None => prev.token.clone(),
        };
        updated = form.into_config(id.clone(), token);
        config.bots[idx] = updated.clone();
        if let Err(e) = config.save(&state.config_path) {
            // Restore in-memory state, and the Keychain too if we'd replaced the
            // token, so the saved token can't drift ahead of the persisted config.
            if new_token.is_some() {
                let _ = secrets::set_token(&id, &prev.token);
            }
            config.bots[idx] = prev;
            return Err(e.to_string());
        }
    }
    // start_bot stops any existing task first, so only stop explicitly here when
    // the bot is being disabled.
    if updated.enabled {
        start_bot(&updated, state.inner()).await;
    } else {
        stop_bot(&id, state.inner()).await;
    }
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn remove_bot(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    {
        let mut config = state.config.lock().await;
        let idx = config
            .bots
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| "bot not found".to_string())?;
        let removed = config.bots.remove(idx);
        if let Err(e) = config.save(&state.config_path) {
            // Re-insert on a failed save so the bot isn't dropped from memory
            // only. Nothing destructive (stop/Keychain) has run yet, so the
            // still-running bot stays consistent.
            config.bots.insert(idx, removed);
            return Err(e.to_string());
        }
    }
    // The removal is durable now — tear down the running bot and its secrets.
    stop_bot(&id, state.inner()).await;
    // Remove the bot's token from the Keychain so it doesn't linger.
    secrets::delete_token(&id);
    state.status.lock().await.remove(&id);
    // Don't leave the quick-note window aimed at a bot that no longer exists —
    // it would keep showing the old name and fail every append with "bot not found".
    {
        let mut target = state.quick_target.lock().await;
        if target.as_ref().is_some_and(|(t, _)| t == &id) {
            *target = None;
        }
    }
    // Persist so the removed bot's counters/offset don't linger in status.json
    // and get reloaded as an orphan entry on the next launch.
    bots::persist_status(&state.status_path, &state.status).await;
    // Drop any leftover pending-download journal for the removed bot.
    bots::remove_journal(&state.status_path, &id);
    sync_shortcuts(&app).await;
    Ok(())
}

#[tauri::command]
async fn set_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> Result<(), String> {
    let bot;
    {
        let mut config = state.config.lock().await;
        let idx = config
            .bots
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| "bot not found".to_string())?;
        let prev = config.bots[idx].enabled;
        config.bots[idx].enabled = enabled;
        bot = config.bots[idx].clone();
        if let Err(e) = config.save(&state.config_path) {
            config.bots[idx].enabled = prev;
            return Err(e.to_string());
        }
    }
    if enabled {
        start_bot(&bot, state.inner()).await;
    } else {
        stop_bot(&id, state.inner()).await;
    }
    Ok(())
}

#[tauri::command]
async fn validate_token(
    state: State<'_, AppState>,
    token: String,
    api_base: Option<String>,
) -> Result<String, String> {
    // Validate against the same base a bot would actually use: an explicit
    // per-bot override wins; otherwise the managed local server when it's
    // enabled *and* running; otherwise the public API.
    let chosen = {
        let server_port = running_server_port(state.inner());
        let cfg = state.config.lock().await;
        effective_api_base(api_base.as_deref(), &cfg.server, server_port)
    };
    let base = bots::resolve_api_base(chosen.as_deref());
    bots::get_me(&state.http, &base, &token).await
}

#[derive(Serialize)]
struct ServerView {
    enabled: bool,
    bin_path: Option<String>,
    port: u16,
    api_id: i64,
    /// Whether an api_hash is stored, so the form can show "leave blank to keep".
    has_api_hash: bool,
    /// The binary path the app would actually use (auto-detected when unset), or
    /// null if none was found — surfaced as a hint in the UI.
    detected_bin: Option<String>,
    /// Whether the managed server process is currently running.
    running: bool,
    /// Why the managed server isn't running, if it was enabled but failed to
    /// start. Null when it's running or disabled.
    last_error: Option<String>,
}

#[tauri::command]
async fn get_server_config(state: State<'_, AppState>) -> Result<ServerView, String> {
    let cfg = { state.config.lock().await.server.clone() };
    let detected = server::locate_binary(cfg.bin_path.as_deref()).map(|p| p.display().to_string());
    let running = running_server_port(state.inner()).is_some();
    let last_error = state
        .server_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Ok(ServerView {
        enabled: cfg.enabled,
        bin_path: cfg.bin_path.clone(),
        // The port the server would actually bind, not the raw stored value: a
        // hand-edited `0` would otherwise be shown as 0 and saved straight back,
        // while the server quietly used the default.
        port: cfg.effective_port(),
        api_id: cfg.api_id,
        has_api_hash: !cfg.api_hash.is_empty(),
        detected_bin: detected,
        running,
        last_error,
    })
}

#[tauri::command]
async fn update_server_config(
    state: State<'_, AppState>,
    enabled: bool,
    bin_path: Option<String>,
    port: u16,
    api_id: i64,
    api_hash: Option<String>,
) -> Result<(), String> {
    // A blank api_hash means "keep the stored one". Write to the Keychain before
    // taking the config lock: it is a blocking call that can raise a system
    // prompt, and a failure here must leave the in-memory config untouched.
    let new_hash = api_hash
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(h) = &new_hash {
        secrets::set_server_api_hash(h)?;
    }
    {
        let mut config = state.config.lock().await;
        let prev = config.server.clone();
        let mut hash_changed = false;
        if let Some(h) = &new_hash {
            config.server.api_hash = h.clone();
            hash_changed = true;
        }
        config.server.enabled = enabled;
        config.server.bin_path = bin_path
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        config.server.port = port;
        config.server.api_id = api_id;
        if let Err(e) = config.save(&state.config_path) {
            // Restore the Keychain too, so the stored secret can't drift ahead
            // of the persisted config (mirrors the rollback in `update_bot`).
            // With no previous hash, delete the entry rather than storing "" —
            // an empty entry reads back as a stored secret.
            if hash_changed {
                if prev.api_hash.is_empty() {
                    secrets::clear_server_api_hash();
                } else {
                    let _ = secrets::set_server_api_hash(&prev.api_hash);
                }
            }
            config.server = prev;
            return Err(e.to_string());
        }
    }
    // Apply the new settings: (re)start or stop the server, then point bots at
    // it. The bots restart *regardless* of whether the server came up — the old
    // one is already torn down, so every bot must re-resolve its API base (and
    // fall back to the public API) rather than stay aimed at a dead local port.
    let server_result = restart_local_server(state.inner()).await;
    restart_all_bots(state.inner()).await;
    server_result
}

/// Async so the blocking native dialog runs off the main thread (calling the
/// blocking picker on the main thread can freeze the UI).
#[tauri::command]
async fn pick_markdown_file(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Markdown / text", &["md", "markdown", "txt"])
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    rx.await
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|pb| pb.to_string_lossy().to_string())
}

/// Pick a folder for saving received files. Async for the same reason as
/// `pick_markdown_file` — the native dialog must run off the main thread.
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    rx.await
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|pb| pb.to_string_lossy().to_string())
}

/// Open a bot's markdown file in the system's default text editor.
/// The file is created (empty) if it doesn't exist yet, so the dashboard link
/// always works even before the first message has been received.
#[tauri::command]
async fn open_note_file(file: String) -> Result<(), String> {
    if file.trim().is_empty() {
        return Err("no file path set for this bot".into());
    }
    let path = PathBuf::from(&file);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| e.to_string())?;
    }
    // `open -t` opens with the default *text* editor (e.g. TextEdit) rather than
    // a Markdown previewer, so the file is immediately editable. Awaited on a
    // blocking thread: an un-awaited child would linger as a zombie (one per
    // click), and its exit status is the only way a failure surfaces at all.
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("open")
            .arg("-t")
            .arg(&path)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
    .and_then(|s| {
        if s.success() {
            Ok(())
        } else {
            Err(format!("`open` could not open the file ({s})"))
        }
    })
}

/// Returns the bot the quick-capture window should currently write to.
#[tauri::command]
async fn get_quick_target(state: State<'_, AppState>) -> Result<Option<QuickTarget>, String> {
    Ok(state
        .quick_target
        .lock()
        .await
        .clone()
        .map(|(id, name)| QuickTarget { id, name }))
}

/// Append text to a bot's file locally (used by the quick-capture window).
#[tauri::command]
async fn append_note(state: State<'_, AppState>, id: String, text: String) -> Result<(), String> {
    let file = {
        let cfg = state.config.lock().await;
        cfg.bots.iter().find(|b| b.id == id).map(|b| b.file.clone())
    };
    let Some(file) = file else {
        return Err("bot not found".into());
    };
    bots::append_timestamped(&file, &text)
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut map = state.status.lock().await;
        let e = map.entry(id).or_default();
        e.message_count += 1;
        e.last_message_at = Some(Local::now().format("%H:%M").to_string());
    }
    bots::persist_status(&state.status_path, &state.status).await;
    Ok(())
}

/// Copy one or more local files into a bot's destination folder and log each in
/// its markdown file (used by the quick-capture window's drag-and-drop). Mirrors
/// the Telegram file-receive path: files land in the same `files_dir` (or the
/// `attachments` folder beside the note file) with a unique timestamped name,
/// and each gets a `saved file: name → dest` note. The original is left in
/// place — this copies, it doesn't move.
#[tauri::command]
async fn save_files(
    state: State<'_, AppState>,
    id: String,
    paths: Vec<String>,
) -> Result<usize, String> {
    let (file, files_dir) = {
        let cfg = state.config.lock().await;
        let bot = cfg
            .bots
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| "bot not found".to_string())?;
        (bot.file.clone(), bot.files_dir.clone())
    };

    let save_dir = bots::resolve_save_dir(&file, files_dir.as_deref());
    tokio::fs::create_dir_all(&save_dir)
        .await
        .map_err(|e| format!("could not create destination folder: {}", e))?;

    // Bump the saved counter immediately after each file lands (not once at the
    // end) so a failure partway through still reflects the files already copied
    // and noted — otherwise an early return would undercount them.
    async fn record_saved(state: &AppState, id: &str, n: u64) {
        if n == 0 {
            return;
        }
        {
            let mut map = state.status.lock().await;
            let e = map.entry(id.to_string()).or_default();
            e.message_count += n;
            e.last_message_at = Some(Local::now().format("%H:%M").to_string());
        }
        bots::persist_status(&state.status_path, &state.status).await;
    }

    let mut saved = 0usize;
    for src in &paths {
        let src_path = Path::new(src);
        // Only files are saved; reject folders (and surface unreadable paths)
        // rather than silently doing nothing.
        match tokio::fs::metadata(src_path).await {
            Ok(m) if m.is_dir() => {
                record_saved(state.inner(), &id, saved as u64).await;
                return Err(format!(
                    "{} is a folder — drop individual files, not folders",
                    src_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| src.clone())
                ));
            }
            Ok(_) => {}
            Err(e) => {
                record_saved(state.inner(), &id, saved as u64).await;
                return Err(format!("can't read {}: {}", src, e));
            }
        }
        let name = src_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let dest = bots::unique_dest(&save_dir, &name);
        if let Err(e) = tokio::fs::copy(src_path, &dest).await {
            record_saved(state.inner(), &id, saved as u64).await;
            return Err(format!("could not copy {}: {}", name, e));
        }
        let note = format!("saved file: {} → {}", name, dest.display());
        if let Err(e) = bots::append_timestamped(&file, &note).await {
            record_saved(state.inner(), &id, saved as u64).await;
            return Err(format!("{} copied but note write failed: {}", name, e));
        }
        saved += 1;
    }

    record_saved(state.inner(), &id, saved as u64).await;
    Ok(saved)
}

// ---- tray ----

/// A cheap fingerprint of everything the tray displays, so we only rebuild the
/// menu when something actually changed (rebuilding every tick causes flicker
/// and can dismiss the menu while it's open).
async fn tray_signature(app: &tauri::AppHandle) -> String {
    let state = app.state::<AppState>();
    let config = state.config.lock().await;
    let status = state.status.lock().await;
    let mut sig = String::new();
    for b in &config.bots {
        let st = status.get(&b.id).cloned().unwrap_or_default();
        sig.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{};",
            b.id,
            b.name,
            b.enabled,
            st.running,
            st.last_error.is_some(),
            st.last_daily_error.is_some(),
            st.message_count,
        ));
    }
    sig
}

async fn update_tray(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let (bots, statuses) = {
        let config = state.config.lock().await;
        let status = state.status.lock().await;
        (config.bots.clone(), status.clone())
    };

    let open_i = match MenuItem::with_id(app, "open", "Open Manager", true, None::<&str>) {
        Ok(i) => i,
        Err(_) => return,
    };
    let sep1 = PredefinedMenuItem::separator(app).ok();
    let quit_i = match MenuItem::with_id(app, "quit", "Quit Msg2Note", true, None::<&str>) {
        Ok(i) => i,
        Err(_) => return,
    };

    let mut running = 0usize;
    let mut errors = 0usize;
    let mut status_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
    for b in &bots {
        let st = statuses.get(&b.id).cloned().unwrap_or_default();
        let has_error = st.last_error.is_some() || st.last_daily_error.is_some();
        let icon = if !b.enabled {
            "⚪"
        } else if has_error {
            "🔴"
        } else if st.running {
            "🟢"
        } else {
            "🟡"
        };
        if b.enabled && st.running && !has_error {
            running += 1;
        }
        if b.enabled && has_error {
            errors += 1;
        }
        let label = format!("{} {}  ·  {} saved", icon, b.name, st.message_count);
        if let Ok(item) =
            MenuItem::with_id(app, format!("bot_{}", b.id), label, false, None::<&str>)
        {
            status_items.push(item);
        }
    }

    let sep2 = PredefinedMenuItem::separator(app).ok();

    let mut refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open_i];
    if let Some(s) = sep1.as_ref() {
        refs.push(s);
    }
    for it in &status_items {
        refs.push(it);
    }
    if let Some(s) = sep2.as_ref() {
        refs.push(s);
    }
    refs.push(&quit_i);

    if let Ok(menu) = Menu::with_items(app, refs.as_slice()) {
        if let Some(tray) = app.tray_by_id("tray") {
            let _ = tray.set_menu(Some(menu));
            let tip = format!("Msg2Note — {} running, {} error(s)", running, errors);
            let _ = tray.set_tooltip(Some(tip.as_str()));
        }
    }
}

fn main() {
    tauri::Builder::default()
        // Registered first, as the plugin requires. Without it a second launch
        // (the installed app alongside a dev build, most often) runs its own
        // poll loop against the same token and the same status.json, and the
        // two roll each other's long-poll offset backwards — which makes
        // Telegram re-deliver, and this app re-append, messages already saved.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let app = app.clone();
                        let sc = *shortcut;
                        tauri::async_runtime::spawn(async move {
                            open_quick_for_shortcut(&app, &sc).await;
                        });
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_bots,
            add_bot,
            update_bot,
            remove_bot,
            set_enabled,
            validate_token,
            get_server_config,
            update_server_config,
            pick_markdown_file,
            pick_folder,
            open_note_file,
            get_quick_target,
            append_note,
            save_files
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let config_dir = app
                .path()
                .app_config_dir()
                .expect("msg2note: macOS did not provide an application config directory");
            std::fs::create_dir_all(&config_dir).ok();
            let config_path = config_dir.join("bots.json");
            let status_path = config_dir.join("status.json");

            // Restore persisted counters + offsets from the last run.
            let status = bots::load_status(&status_path);

            // Load config, then pull each bot's token from the Keychain. If an
            // older plaintext config still held tokens inline, they're migrated
            // into the Keychain and the config is re-saved without them.
            let mut config = Config::load(&config_path);
            if secrets::hydrate_tokens(&mut config) {
                let _ = config.save(&config_path);
            }
            // The server api_hash lives in the Keychain, not bots.json.
            config.server.api_hash = secrets::get_server_api_hash().unwrap_or_default();

            app.manage(AppState {
                config_path: config_path.clone(),
                status_path,
                config: Mutex::new(config),
                handles: Mutex::new(HashMap::new()),
                status: Arc::new(Mutex::new(status)),
                http: reqwest::Client::new(),
                shortcuts: Mutex::new(HashMap::new()),
                quick_target: Mutex::new(None),
                server: std::sync::Mutex::new(None),
                server_error: std::sync::Mutex::new(None),
            });

            // Tray icon with an initial menu.
            let open_i = MenuItem::with_id(app, "open", "Open Manager", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Msg2Note", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &quit_i])?;
            let mut tray = TrayIconBuilder::with_id("tray")
                .tooltip("Msg2Note")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            // A missing icon is cosmetic, not fatal: unwrapping here would kill
            // the app at launch, with no window and no dock icon to show for it.
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;

            // Closing a window hides it instead of quitting the app.
            for label in ["main", "quick"] {
                if let Some(window) = app.get_webview_window(label) {
                    let w = window.clone();
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = w.hide();
                        }
                    });
                }
            }

            // Start enabled bots, register hotkeys, then refresh the tray on a timer.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                {
                    let state = handle.state::<AppState>();
                    // Bring up the managed local server first so bots can reach
                    // it. If it fails, the reason is stored in `server_error`
                    // (shown in the server settings UI) and bots transparently
                    // fall back to the public API rather than a dead local port.
                    if let Err(e) = restart_local_server(state.inner()).await {
                        eprintln!("local Bot API server: {}", e);
                    }
                    let bots: Vec<BotConfig> = {
                        let c = state.config.lock().await;
                        c.bots.clone()
                    };
                    for b in bots.iter().filter(|b| b.enabled) {
                        start_bot(b, state.inner()).await;
                    }
                }
                sync_shortcuts(&handle).await;
                let mut last_sig: Option<String> = None;
                loop {
                    let sig = tray_signature(&handle).await;
                    if last_sig.as_ref() != Some(&sig) {
                        update_tray(&handle).await;
                        last_sig = Some(sig);
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running msg2note")
        .run(|app_handle, event| {
            // Kill the managed server when the app exits so it isn't orphaned.
            if matches!(event, tauri::RunEvent::Exit) {
                let state = app_handle.state::<AppState>();
                // Named local (declared after `state`) so the guard drops before
                // `state`: a temporary in the `if let` scrutinee would outlive
                // what it borrows (E0597). A poisoned lock is recovered rather
                // than skipped — skipping would orphan the child server.
                let lock = state.server.lock();
                let mut guard = lock.unwrap_or_else(|e| e.into_inner());
                *guard = None;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(json: serde_json::Value) -> BotForm {
        serde_json::from_value(json).unwrap()
    }

    fn full_form() -> serde_json::Value {
        serde_json::json!({
            "name": "  Ideas  ",
            "token": "  123456:ABC  ",
            "file": "  /notes/ideas.md  ",
            "readFile": "   ",
            "filesDir": "  /inbox  ",
            "allowedUserId": " 7000000000 ",
            "apiBase": "",
            "enabled": true,
            "shortcut": "  ",
            "dailySend": true,
            "dailyTime": "  07:30  "
        })
    }

    #[test]
    fn form_trims_every_field_and_drops_blank_overrides() {
        let cfg = form(full_form()).into_config("id-1".into(), "tok".into());
        assert_eq!(cfg.id, "id-1");
        assert_eq!(cfg.name, "Ideas");
        assert_eq!(cfg.token, "tok");
        assert_eq!(cfg.file, "/notes/ideas.md");
        // Blank overrides become `None`, not `Some("")`.
        assert_eq!(cfg.read_file, None);
        assert_eq!(cfg.api_base, None);
        assert_eq!(cfg.shortcut, None);
        assert_eq!(cfg.files_dir.as_deref(), Some("/inbox"));
        assert_eq!(cfg.daily_time, "07:30");
        // A 64-bit user id survives the string round trip intact.
        assert_eq!(cfg.allowed_user_id, 7_000_000_000);
    }

    #[test]
    fn form_defaults_a_blank_name_and_time() {
        let mut v = full_form();
        v["name"] = serde_json::json!("   ");
        v["dailyTime"] = serde_json::json!("");
        let cfg = form(v).into_config("i".into(), "t".into());
        assert_eq!(cfg.name, "Untitled");
        assert_eq!(cfg.daily_time, "08:00");
    }

    #[test]
    fn an_unparseable_user_id_is_rejected() {
        let mut v = full_form();
        v["allowedUserId"] = serde_json::json!("not a number");
        // Rejected at the command boundary rather than silently folded to the
        // 0 = "allow anyone" sentinel.
        assert!(validate_form(&form(v)).is_err());
        let mut v = full_form();
        v["allowedUserId"] = serde_json::json!("");
        assert_eq!(
            form(v).into_config("i".into(), "t".into()).allowed_user_id,
            0
        );
    }

    #[test]
    fn a_blank_token_field_means_keep_the_stored_one() {
        let mut v = full_form();
        v["token"] = serde_json::json!("   ");
        assert!(form(v).token().is_none());
        assert_eq!(form(full_form()).token(), Some("123456:ABC"));
    }

    #[test]
    fn bots_only_route_to_a_local_server_that_is_actually_listening() {
        let mut server = ServerConfig::default();

        // Disabled server, no override: the public API.
        assert_eq!(effective_api_base(None, &server, None), None);

        // Enabled but not listening: still the public API, never a dead port.
        server.enabled = true;
        assert_eq!(effective_api_base(None, &server, None), None);

        // Enabled and listening: the port the process actually bound.
        assert_eq!(
            effective_api_base(None, &server, Some(9099)),
            Some("http://127.0.0.1:9099".to_string())
        );

        // An explicit per-bot override always wins…
        assert_eq!(
            effective_api_base(Some(" https://example.test "), &server, Some(9099)),
            Some("https://example.test".to_string())
        );
        // …but a blank override is not an override.
        assert_eq!(
            effective_api_base(Some("   "), &server, Some(9099)),
            Some("http://127.0.0.1:9099".to_string())
        );
    }

    #[test]
    fn norm_maps_blank_to_absent() {
        assert_eq!(norm(None), None);
        assert_eq!(norm(Some(String::new())), None);
        assert_eq!(norm(Some("  \t ".into())), None);
        assert_eq!(norm(Some("  x  ".into())), Some("x".to_string()));
    }
}
