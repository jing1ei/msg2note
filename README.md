# Msg2Note

**Nothing sits between you and your own disk.** No notes service to sign up for, no
cloud account holding your data, no sync backend, no server to host, no web API to
build or call. You message your own Telegram bot; the text lands as a timestamped line
in a plain markdown file on your Mac, and any file you send is saved into a folder you
chose. Telegram is only the pipe — what it carries stops at your disk, as plain files
any editor can open and any backup can copy.

The app itself is a macOS **menu bar app**. Each Telegram bot is bound 1-to-1 to a local
markdown file. Manage the bindings — add, edit, enable/disable, remove — from a small
table UI, and watch each bot's live status from the menu bar.

```
You ──Telegram──▶ Bot "Ideas"  ──▶ /Users/you/notes/ideas.md
You ──Telegram──▶ Bot "Tasks"  ──▶ /Users/you/notes/tasks.md
```

The app uses **long polling**, so there is no inbound server, no public port, and no
domain to configure — it just dials out to Telegram. Perfect for a Mac mini at home.
(Optionally it can run a *local* Telegram Bot API server bound to loopback to lift the
20 MB download cap — see "Local Bot API server" below.)

---

## 1. One-time setup on the Mac mini

Install the toolchain (only needed to build the app):

```bash
# Xcode command line tools
xcode-select --install

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Tauri CLI (used to build/run)
cargo install tauri-cli --version "^2"
```

The macOS icon set in `src-tauri/icons/` is committed, so there's nothing to generate.
Regenerate it only if you swap in your own 1024×1024 PNG:

```bash
cargo tauri icon app-icon.png
```

## 2. Create your bots in Telegram

For **each** markdown file you want to feed:

1. In Telegram, open a chat with **@BotFather** → `/newbot` → follow the prompts.
2. Copy the **token** it gives you (looks like `123456:ABC-DEF...`).
3. Open a chat with **@userinfobot** once and note **your** numeric user ID — this is the
   only account allowed to write to your files.

## 3. Run it

From the repo root:

```bash
cargo tauri dev     # development / first try
cargo tauri build   # build a real app bundle
```

`cargo tauri dev` is the one to start with: it runs the app from the current
source, with no install and no stale `/Applications` copy in the way. It's a dev
build, so you can right-click the window → **Inspect Element** for a console.

The finished app is at
`src-tauri/target/release/bundle/macos/Msg2Note.app`
(a `.dmg` usually appears under `bundle/dmg/` too, but the `.app` is all you need).
Drag `Msg2Note.app` into `/Applications`.

> The app is built on your own Mac and isn't code-signed or notarized, which is fine
> when you build it yourself. If you copy the `.dmg` or the `.app` to a *different*
> Mac, Gatekeeper will refuse to open it ("Msg2Note is damaged and can't be opened").
> Clear the quarantine flag there once with:
>
> ```bash
> xattr -dr com.apple.quarantine /Applications/Msg2Note.app
> ```

When it launches there's **no dock icon** — look for the Msg2Note icon in the **menu
bar**. Click it → **Open Manager** to add bots.

## 4. Add a bot in the app

Click **Add bot**, then:

**Essentials**

- **Name** — anything, e.g. "Ideas"
- **Bot token** — paste it, click **Validate token** to confirm it connects
- **Markdown file** — where the bot *writes*. **Browse…** to pick an existing file, or
  type a full path (it's created automatically on the first message if it doesn't exist)
- **Your Telegram user ID** — from @userinfobot. Leave it blank (or 0) to allow anyone,
  which isn't recommended. A value that isn't a number is rejected rather than quietly
  treated as "allow anyone"
- **Enabled** — leave checked to start immediately

**Files & shortcut**

- **Files folder** *(optional)* — **Browse…** to pick where received files (documents,
  photos, etc.) are saved. Leave blank to use an `attachments` folder next to the
  markdown file
- **Local shortcut** *(optional)* — click **Record** and press a hotkey (see below)

**Daily send-back**

- **Send a file to me every day at** *(optional)* — tick it and pick a time to have the
  bot send a file's contents back to you once a day. Needs your Telegram user ID, and
  only runs while the bot is enabled. See "Daily send-back" below
- **Send-back file** *(optional)* — the file the daily send *reads*. Leave blank to send
  back the same file the bot writes to. Set it to a different markdown file when you want
  one bot to read one file and write another; this file is only ever read

**Advanced**

- **Bot API server** *(optional)* — leave blank for Telegram's public API. Set it only
  to point this one bot at a specific server; the **Local server** button configures
  the app-managed one for every bot at once

The menu bar **dropdown** lists each bot with its status — 🟢 running, 🔴 error,
🟡 starting, ⚪ disabled — and a count of messages saved. Hovering the menu bar icon
shows a one-line summary instead: `Msg2Note — 2 running, 0 error(s)`.

Dialogs scroll internally: the title and the **Cancel** / **Save** row stay pinned, so a
long form never pushes **Save** past the bottom of the window. **Esc**, the **✕**, or a
click on the dimmed backdrop closes without saving.

In the manager table, click a bot's **markdown file path** to open it in the default
text editor (it's created first if it doesn't exist yet) so you can read or edit notes.

## 5. Keep it running 24/7

Because it's a menu bar app, it runs as long as you're logged in:

1. **System Settings → General → Login Items** → add **Msg2Note** so it starts on boot.
2. **System Settings → Users & Groups → Automatically log in as** → set to your account,
   so a reboot lands you logged in (required for menu bar apps to run unattended).

Each bot also auto-reconnects after network blips on its own.

---

## Local quick-capture (no Telegram needed)

Each file can have a **global hotkey**. Press it anywhere on the Mac — even when
Msg2Note isn't focused — and a small input box pops up over whatever you're doing,
already pointed at that file. Type, press **Enter**, and it's appended (same timestamped
format as the bot); **Esc** dismisses it. Clicking away also dismisses it, unless
you've typed something or queued files — that work is kept, so use **Esc** to
discard it deliberately. You can also **drag files onto
the box** to copy them into the bot's files folder (the same place Telegram attachments go),
with a note added to the markdown file — exactly like sending the bot a file.

Set one per bot in the manager: edit the bot, click **Record**, and press the keys (e.g.
⌘⇧I). Pick combinations that include ⌘/⌥/⌃ so they don't clash with normal typing. The
hotkey works whether or not the bot's Telegram side is enabled.

> Hotkeys are registered with the system directly, so macOS asks for no extra
> permission and Msg2Note won't appear under Accessibility or Input Monitoring.
> If a hotkey does nothing, another app has almost certainly claimed the same
> combination — pick a different one.

## Daily send-back (file → you, every morning)

Each bot can also push **the other way**: tick **Send a file to me every day at**
in the bot's edit form and pick a time. Once a day at that local time the bot sends
a markdown file's **exact contents** back to you in Telegram (split across multiple
messages if it's longer than Telegram's per-message limit).

- Which file it sends is the **Send-back file** field. Leave it blank and the bot sends
  back the file it writes to (`Markdown file`). Point it at a different path and the
  bot reads one file and writes another:

  ```
  You ──Telegram──▶ Bot "Daily"  ──writes──▶ /Users/you/notes/inbox.md
                              └──reads/sends──  /Users/you/notes/plan.md
  ```

  The send-back file is **never written to** by the bot — it only reads it. Point it
  at a file that exists (**Browse…** only offers existing ones); if it's missing on
  send day the bot records a "cannot read file" error instead. Clicking the path in
  the manager table opens it in your editor, creating an empty file if it's missing —
  which turns that error into a silent "nothing to send".
- It sends to the account in **Your Telegram user ID**, so that field must be set
  (it's also the chat the bot replies in). Leaving it at `0` disables the daily send.
- Only sends when the bot is **enabled** — it rides along with the poll loop.
- If the file is empty that day, nothing is sent.
- Starting the app *after* the scheduled time won't trigger a catch-up send; it waits
  until the next day's slot.

## How messages are written

Each message becomes one timestamped line, appended to the file:

```
- [2026-06-25 14:30] pick up dry cleaning
- [2026-06-25 14:31] idea: a bot that writes to markdown
```

Send a file, photo, or other attachment and it's downloaded to the bot's **Files folder**;
a note recording the saved filename and path (plus any caption) is appended to the file:

```
- [2026-06-25 14:32] saved file: 20260625-143200_report.pdf → /Users/you/notes/attachments/20260625-143200_report.pdf
- [2026-06-25 14:33] saved file: 20260625-143300_photo.jpg → /Users/you/notes/attachments/20260625-143300_photo.jpg — vacation pic
```

Saved files are prefixed with a timestamp so names never collide. Only messages from your
allowed user ID are saved; anything else is ignored. The bot reacts with 👍 on each saved
message or file so you get confirmation in Telegram.

Only text and downloadable attachments (documents, photos, videos, audio, voice, stickers,
etc.) are saved. Other message types — locations, contacts, polls, and the like — are
acknowledged (so Telegram stops re-delivering them) but nothing is written to the file.

Attachments also get a `✅ Saved <name> (<size>)` reply once the download actually
finishes, so a large transfer confirms on completion rather than up front and never
looks stuck mid-save.

## Where settings live

Your bot list (names, file paths, send-back paths, user IDs, shortcuts) is stored at:

```
~/Library/Application Support/com.notekeeper.desktop/bots.json
```

**Bot tokens are not kept in that file.** They live in the macOS **Keychain**, one
entry per bot, under the service `com.notekeeper.app`.

> Both `com.notekeeper.*` identifiers are held fixed on purpose. The bundle id keys the
> Application Support folder and the Keychain service keys your tokens, so renaming
> either would strand existing settings and tokens under the old name.

## Project layout

```
msg2note/
├─ app-icon.png              # source icon (run: cargo tauri icon app-icon.png)
├─ .github/workflows/        # CI on every PR; a v* tag builds the release .dmg
├─ README.md  SECURITY.md  LICENSE  .gitignore
├─ src/                      # the UI (static HTML/CSS/JS — no build step)
│  ├─ index.html             # bot manager table
│  └─ quick.html             # the pop-up quick-capture box
└─ src-tauri/                # Rust backend
   ├─ Cargo.toml  Cargo.lock
   ├─ tauri.conf.json
   ├─ build.rs
   ├─ capabilities/default.json
   ├─ icons/                 # generated by `cargo tauri icon` (macOS set committed)
   └─ src/
      ├─ main.rs             # Tauri commands + tray + bot lifecycle
      ├─ bots.rs             # Telegram long-poll engine, timestamped append
      ├─ server.rs           # optional app-managed local Bot API server
      ├─ secrets.rs          # bot tokens in the macOS Keychain
      └─ config.rs           # load/save bots.json (no tokens)
```

`src-tauri/gen/` and the `icons/android` + `icons/ios` sets are regenerated by the
build and are not tracked.

## Notes & limits

- A bot token can only run **one** long-poll consumer at a time. Don't run the same
  token elsewhere (e.g. a second copy of the app) or Telegram returns a 409 conflict.
- Leaving the user ID at `0` lets **anyone** who finds the bot write to your file — set
  your real ID unless you have a reason not to.
- Telegram's **public** Bot API caps file downloads at ~20 MB. Over the public API a
  larger file can't be fetched; instead of saving it, the bot appends a
  `could not save file:` note and moves on. Run the optional **local Bot API server**
  (below) to raise this to 2 GB.

## Local Bot API server (optional, lifts the 20 MB cap)

To receive files larger than 20 MB (most videos), the app can spawn and manage a local
[`telegram-bot-api`](https://github.com/tdlib/telegram-bot-api) server in `--local` mode,
which raises the download limit to 2 GB and writes files straight to disk.

1. Build `telegram-bot-api` from source once — there is **no Homebrew formula** for it.
   The easiest path is the official build-instructions generator at
   <https://tdlib.github.io/telegram-bot-api/build.html> (pick macOS). In short:

   ```bash
   xcode-select --install
   brew install gperf cmake openssl zlib
   git clone --recursive https://github.com/tdlib/telegram-bot-api.git
   cd telegram-bot-api && mkdir build && cd build
   cmake -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX:PATH=.. \
         -DOPENSSL_ROOT_DIR=$(brew --prefix openssl) ..
   cmake --build . --target install
   ```

   The binary lands at `telegram-bot-api/bin/telegram-bot-api`. Either copy it into
   `/opt/homebrew/bin` (Apple Silicon) or `/usr/local/bin` (Intel) so the app
   auto-detects it, or paste its full path into the **Server binary path** field.
2. Get an `api_id` and `api_hash` from **my.telegram.org → API development tools**.
3. In the app, click **Local server**, tick "Run the local server…", enter the
   `api_id`/`api_hash`, and save. The `api_hash` is stored in the macOS Keychain.

The server binds to **loopback only** (`127.0.0.1`), so it isn't reachable from other
machines, and it's killed when the app quits. The app waits for it to actually start
accepting connections before routing anything to it — if it exits immediately (wrong
`api_id`/`api_hash`, or the port already in use) the failure is shown in the **Local
server** dialog and every bot stays on the public API. Its own output is written to
`bot-api/server.log` next to `bots.json`, which is where to look for the reason.

The `api_id`/`api_hash` are passed to the server through its environment rather than
its command line, so they don't show up in `ps`.

> A bot already used with the public API must be logged out of it before a local server
> can take over — send `https://api.telegram.org/bot<token>/logOut` once per bot. It logs
> back in automatically if you later disable the local server.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).
