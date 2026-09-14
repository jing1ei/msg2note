# Security Policy

## Reporting a vulnerability

If you find a security issue in Msg2Note, please report it privately rather
than opening a public issue.

- Use GitHub's **"Report a vulnerability"** button under the repository's
  **Security** tab (Private Vulnerability Reporting), or
- Open a regular issue **only** for non-sensitive, low-risk reports.

Please include steps to reproduce, the affected version/commit, and the impact
you observed. You'll get an acknowledgement as soon as practical, and a fix or
mitigation will be coordinated before any public disclosure.

## Scope and threat model

Msg2Note is a local, single-user macOS menu bar app. By default it opens **no
inbound ports** and exposes nothing to the network — it only dials out to
Telegram over HTTPS via long polling. The optional local Bot API server, when
enabled, runs a `telegram-bot-api` child process bound to **loopback only**
(`127.0.0.1`), so it is reachable from this Mac alone and never from the network;
it is terminated when the app quits cleanly. (A force-quit or a crash can leave
the child process running and still listening on loopback — quit from the menu bar
icon, or `pkill telegram-bot-api` if you suspect one was orphaned.) The main assets to protect are:

- **Bot tokens** — stored in the macOS **Keychain** (service
  `com.notekeeper.app`), not in `bots.json`. Tokens are redacted from any error
  messages shown in the UI, tray, or logs.
- **Your notes** — plain markdown/text files at paths you choose, plus any
  files/photos received (saved to a folder you pick). Messages are written and
  attachments downloaded only for the configured Telegram user ID (see the
  `allowed_user_id` caveat below).
- **The local server's `api_hash`** — also in the Keychain, and handed to the
  `telegram-bot-api` child process through its environment rather than its
  command line, so it isn't exposed in `ps` output.

### Things to be aware of

- **`allowed_user_id` gating.** Each bot only saves messages from the numeric
  Telegram user ID you set — *provided you set one*. Leaving it at `0` disables
  the check entirely and allows **anyone** who discovers the bot to write to your
  file. Always set your real ID.
- **One poller per token.** Running the same token in two places triggers
  Telegram's 409 conflict. Don't share a token across instances.
- **Local trust.** The UI is static, fully local content loaded under a CSP that
  blocks remote script and disallows framing. Every dynamic value rendered into
  the table (bot names, file paths, errors, Telegram usernames, ids, counters) is
  HTML-escaped for both text and attribute contexts, and note content is never
  rendered in the app — only written to disk.

## Supported versions

This is a small project; security fixes are applied to the latest release and
`main` only.
