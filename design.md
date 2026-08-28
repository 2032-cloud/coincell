# coincell — design outline

Working notes for the sync daemon. Companion to 2032's `infra.md` / `design.md`
(the backend this talks to). Nothing here is committed; `<!-- -->` marks
rejected ideas and undecided forks.

## What coincell is

- Standalone Rust daemon + tray app. The first real client of 2032's
  `cr.2032.cloud` device-auth API.
- Watches user-configured local save files (per game), pushes changes up, pulls
  newer versions down.
- Lives in the tray. Two windows, both frameless, bottom-right, auto-hide on
  focus loss: **Home** (left-click, the per-game list) and **Config**
  (right-click, this doc's subject).
- Distributed as GitHub Release binaries, pointed to from the website. Self-updates
  from those same releases.

## Backend surface we actually have (`cr.2032.cloud`, Bearer `<session_id>`)

- `GET /auth/device/config` — public bootstrap (Auth0 domain/client_id/audience/scope + `api_base`)
- `POST /auth/device/token` — mint session (one-time, from the Auth0 JWT)
- `POST /auth/device/logout` — revoke *this* session, no `:id`
- `GET /api/me` — `sub` + `theme` only (no name/email/picture on `cr.`)
- list / create game instances — list takes `?since=<ts>` (instances whose latest
  save is newer than `ts`) and embeds a `latestSave` object per row:
  `{ id, content_hash, size_bytes, uploaded_at, starred, note }`
- `GET /api/game-instances/:id/saves` — list; `POST` same path — push raw
  `application/octet-stream`, returns `{ id, duplicate }`; `GET …/:saveId` —
  download raw bytes
- `GET /api/events` — WebSocket upgrade (sessionBearer), the realtime save-updated
  stream
- `GET /api/consoles` — `{ slug, name, description, validSaveSizes: number[],
  iconUrl, boxArtUrl }[]`. `validSaveSizes` is the only save-size check we need.

<!-- casing: instance top-level fields are camelCase (consoleSlug, lastSavedAt),
     but the nested latestSave object is snake_case (size_bytes, uploaded_at,
     content_hash) with starred as a number. Confirmed against /openapi.json.
     The deserialize struct needs rename_all="camelCase" on the outer type and a
     nested type that opts out, plus a number->bool read for `starred`. -->

- realtime save-updated stream — WebSocket to a per-user Durable Object (see
  Download direction)
- `GET /api/consoles`, `GET /api/consoles/:slug/games` — public catalog + art
- Nothing that edits existing data (instance rename, star, note, retention limit,
  account/theme) is reachable here — that's browser-only on `2032.cloud`.
- Retention: backend keeps `save_retention_limit` (default 5) non-starred saves
  per instance, cycles oldest out. Starred saves never cycle. `(game_instance_id,
  content_hash)` is unique — re-uploading identical bytes just bumps `uploaded_at`,
  so redundant uploads are cheap and safe.

## Config split: preferences vs state

Two files, different lifecycles:

- **`config.toml`** (exists) — user-set preferences + identity. Hand-editable.
  Already preserves unknown keys. Keep it small.
- **`data.sqlite`** (new, in `DATA_DIR` — `ProjectDirs::data_dir()`, separate from
  the config dir on purpose) — operational state the daemon owns. Not meant to be
  edited. Holds the path ↔ `game_instance_id`
  mappings and per-instance pause flags, per-instance sync bookkeeping
  (last-synced hash / save id, last uploaded hash), the offline upload queue,
  last-poll / last-stream-position timestamps, conflict markers, and a launch-time
  cache of each console's `validSaveSizes`. Rebuildable from the backend without
  losing user intent.

<!-- putting watch bookkeeping in config.toml would mean the user's editor fights
     the daemon's writes, and a corrupt state file would nuke their preferences -->

**DECIDED: SQLite, not a JSON blob.** Adds a dep, but the offline queue + save
history + per-instance reconciliation are relational and will only grow; JSON
would be rewritten whole on every change and get awkward fast.

- `rusqlite` with the `bundled` feature — compiles SQLite in, no system libsqlite
  dependency, which matters for the standalone Release binaries.
- WAL mode. Single writer behind a `Mutex`, mirroring the existing `Config`
  pattern (`OnceLock<Mutex<_>>`, closures over a guard) so call sites never see
  SQL or connection handling — a `Store::get(|s| …)` / `Store::write(|s| …)`
  shaped module (`src/store/` or `src/state.rs`).
- Schema versioned from day one (`PRAGMA user_version` + an ordered migration
  list, or `rusqlite_migration`) — this schema will move.
- Same robustness manners as `Config`: on open failure, back up the bad file and
  start fresh rather than crashing the daemon.

## `config.toml` schema (proposed)

```toml
session_id = "..."            # exists

[startup]
launch_on_login = false       # OS autostart: Run key / .desktop / LaunchAgent
start_hidden = true

[sync]
enabled = true                # global pause switch
poll = "auto"                 # "auto" | "30s" | "5m" | "off"
upload_trigger = "on-change"  # "on-change" (debounced) | "on-emulator-exit" | "manual"
conflict = "ask"              # "ask" | "prefer-local" | "prefer-remote" | "prefer-newest"
pause_on_metered = true       # Windows metered-connection awareness

[notifications]
enabled = true
on_pull = true
on_conflict = true
on_error = true
on_session_expired = true

[appearance]
theme = "account"             # "account" (follow /api/me) | "auto" | "light" | "dark"

[window]
hide_on_focus_loss = true     # minimize button + Esc always work regardless

[updates]
channel = "stable"            # "stable" | "prerelease"
auto_check = true
check_interval = "24h"
on_update = "notify"          # "notify" | "download" | "install"

[advanced]
api_base = "https://cr.2032.cloud"   # override for dev / self-host
log_level = "info"
crash_reports = false                # opt-in Sentry. KEY ABSENT = not yet asked;
                                     # written once the user answers the first-run prompt
```

<!-- no max_upload_bytes: the real check is the console's `validSaveSizes`
     (raw byte counts) from GET /api/consoles, cached in data.sqlite at launch.
     They rarely change; a client restart re-reads them. Backend may switch to
     size ranges later -- treat the cached shape as "list or range", not "list". -->

<!-- crash_reports is tri-state: absent = ask, present = answered. Absence, not a
     `false` default, is what the first-run prompt keys off -- see Logging below. -->

Per-game watch config (path ↔ `game_instance_id`, per-instance pause) is
**Home-menu territory**, not this file — games are added/removed there. `[sync]`
here holds the defaults a newly-added game inherits.

## Config window

Sidebar + working area (Discord / 2032-Settings style), not one long scroll.
Sections:

1. **Account** — "signed in as `<sub>`" (that's all `cr./api/me` gives us),
   short session id, **Log out**, and an "Open account settings ↗" link to
   `2032.cloud` settings for anything we can't do from here (rename/revoke other
   sessions, theme, delete account, retention limit).
   - Log out: confirm ("stops syncing on this device") → `POST /auth/device/logout`
     → clear local session even if that call fails (offline) → back to login screen.
2. **Sync** — global pause toggle, poll interval, upload trigger, conflict policy,
   **Sync now** button, last-sync status line, most-recent error.
3. **Startup** — launch on login, start hidden.
4. **Notifications** — the toggles above.
5. **Appearance** — theme (follow account / override).
6. **Updates** — current version, channel, auto-check toggle, **Check for
   updates** → becomes **Install update `vX.Y.Z`** when one exists, changelog link.
7. **Advanced** — buttons to open config/data/log folders, log level, API base
   override, **Copy diagnostics** (version, OS/arch, redacted session, last errors),
   **Reset config**.

Always-visible footer: **Quit coincell** (fully exits the process — distinct from
the window's close-to-tray) and maybe **Restart**.

## Home window

The per-game list and everything instance-scoped. **DECIDED: coincell owns the
full instance lifecycle natively — no bouncing to the website for a first bind.**

- **Add a game** — pick a console from `GET /api/consoles`, then a game from
  `GET /api/consoles/:slug/games`, both public and art-enriched. eframe renders
  the box art / icons in a scrollable grid cheaply. Then bind a local save path
  (native picker via `rfd`) and `POST` the new instance. Also supports an
  unlinked instance (`game_slug` null + a `game_name`) for romhacks / anything
  not in the catalog.
- **Per-instance row** — display name, last-synced time, sync status, per-instance
  pause, the watched path, and a manual "sync now / restore" affordance.
- **Save history** — on demand, `GET /api/game-instances/:id/saves`, listed and
  sorted client-side; lets the user restore an older or starred save to disk.
- **Conflict resolution** — the "changed here and on another device" picker
  (see Conflict policy) lives here.
- Catalog responses can be cached in `data.sqlite` with a short TTL so reopening
  the add-game flow isn't a fresh fetch every time.

## Session identity

**DECIDED.** The `client_name` sent to `POST /auth/device/token` (currently just
`APP_NAME` in `auth.rs`) becomes:

```
{APP_NAME} - {username}@{hostname} - {platform}
  e.g.  CoinCell - ethan@tower - Windows
```

- `whoami` crate for the pieces: `whoami::username()` (login name — the
  `~` / `C:\Users\<name>` folder name), `whoami::devicename()` (hostname),
  `whoami::platform()` → `Windows` / `Linux` / `macOS` / …. `user@host` is
  well-understood by Linux users and readable enough for everyone else; it's what
  actually distinguishes two machines where the login name and location are the
  same.
- Use the `whoami::fallible::*` variants. If hostname is missing, fall back to
  bare `{username}`; if username is also missing, bare `{platform}` — degrade
  piece by piece rather than emitting the crate's `"Unknown"` placeholder.
- The backend appends its own `request.cf` location. The name is editable on the
  website (infra.md), so this is only the default.

<!-- the OS username leaves the machine and shows in the user's own Sessions list.
     own account, own data -- acceptable, noting it. -->

## Tray context menu (new)

Right now the tray is click-only (`Menu::new()` is empty). Add a real menu:
**Open**, **Sync now**, **Pause sync** (toggle), separator, **Quit coincell**.
The "fully shut down" action wants to be here as well as in the Config footer.

## Sync engine

### Upload direction (local → backend)

- `notify` crate (`ReadDirectoryChangesW` / inotify / FSEvents) on each watched
  path. Debounce bursts — emulators write in chunks, or write-temp-then-rename —
  wait for ~2–5 s of quiet, then act.
- On quiet: hash the file, compare to the last-synced `content_hash` in `state`.
  Unchanged → nothing. Changed → upload as a new save.
- Handle Windows file locks: retry with backoff if the emulator still holds the
  handle. `upload_trigger = "on-emulator-exit"` sidesteps this entirely for users
  who want it — needs process-watch (see launcher idea below).
- Backend dedupe means an over-eager upload is harmless.

<!-- REJECTED: blocking/intercepting read() as the change signal. Needs a
     filesystem filter driver (Windows) or FUSE (Linux), elevated install,
     per-OS. notify covers the write side; the read side is the launcher's job. -->

### Download direction (backend → local)

Realtime stream first, timestamp-filtered poll as catch-up. No tight clock, no
read-intercept.

**Realtime — WebSocket at `GET /api/events`** (Bearer session; one per-user
Durable Object behind it). Pushes a "save updated" event (instance id + latest
save metadata) whenever any other device uploads for that user. Primary
mechanism, not a v2 nicety.

**Catch-up — `GET /api/game-instances?since=<ts>`** (already built). Filters to
instances whose latest save is newer than `since`. Each row carries its
`latestSave` (`content_hash`, `size_bytes`, `uploaded_at`, `starred`, `note`), so
the daemon settles staleness for every instance from this one call — no follow-up
`/saves` request per instance. `GET /api/game-instances/:id/saves` is only hit
when the user opens save history for one instance in Home.

**Ordering is the whole point — connect the WebSocket *before* the first poll.**
Any upload that lands in the gap between "poll response serialized" and "stream
is live" would otherwise be lost. Stream up first, then poll, so that window
never exists.

- **First run / no trustworthy local state:** poll with **no `since`** — full
  instance list — and walk every instance's saves comparing `uploaded_at` *and*
  `content_hash` against local, so an older file that never synced isn't missed.
- **Steady state:** `since = <last successful catch-up timestamp>`. Keep only the
  most recent save per instance for the "is my local stale" decision; the full
  hash compare on first run is what guarantees completeness.
- **On WebSocket disconnect:** reconnect, and once the stream is live again, run
  the `since=` poll (since = last confirmed-good position) to backfill anything
  missed while down. Same connect-then-poll order as startup.

Also poll on: **Sync now**, network-regained / wake-from-sleep (which will also
have dropped the socket, so this falls out of the reconnect path), and a lazy
`[sync].poll` interval as a last-resort backstop.

**Launcher model (later, the clean endgame for "right before you play").** Let
the user register their emulator command with coincell. Launching through
coincell does: pull latest for that instance → exec emulator → on exit, push. No
races. Falls back to process-watch (poll the process list for a configured
emulator exe) for users who launch the emulator directly.

### Conflict policy

Conflict = local file hash ≠ last-synced hash **and** remote latest hash ≠
last-synced hash **and** the two differ.

- Default (`conflict = "ask"`): never silently overwrite a local file with
  unsynced changes. Upload the local bytes as a new save (history + retention
  protect it), download the remote bytes alongside, surface it in Home: "changed
  here and on another device — pick which stays on disk." Star the kept one so
  cycling can't evict it.
- `prefer-newest` uses `uploaded_at` vs local mtime; `prefer-local` /
  `prefer-remote` are explicit.
- Never delete a local save file. Never discard bytes that weren't uploaded.

### Offline / resilience

- Failed uploads go to a queue in `state`, retried with backoff; survive restart.
- A `401` from `cr.` mid-sync flips the app into the logged-out state and fires
  the session-expired notification — don't silently spin forever.
- `pause_on_metered` (Windows) and a manual global pause both stop all network
  work but keep watching the filesystem, so the queue is ready when sync resumes.

## Versioning

One version string, resolved at build time, used everywhere — Config > Updates,
the diagnostics copy, Sentry `release`, the API `User-Agent`.

- **Release build** — HEAD sits exactly on a version tag and the tree is clean.
  Version is that tag's semver (`0.2.0`); channel `stable`, or `prerelease` if
  the tag says so. This is the only kind of build ever distributed.
- **Development build** — anything else. Version is
  `{CARGO_PKG_VERSION}+dev.{short_hash}` (`-dirty` suffix if the tree isn't
  clean), channel **always `development`** regardless of `[updates].channel`.
- A `build.rs` (or `vergen` / `shadow-rs`) resolves this from
  `git describe --tags --dirty` / `git rev-parse`, emitted as a `rustc-env` var;
  `CARGO_PKG_VERSION` is the fallback when git isn't available (source tarball).
- The updater treats a `development` build as "never has an update to offer" —
  it can still check and *show* the latest release, but auto-install stays off.
  Only `stable` / `prerelease` builds self-update.

## Updater

Built **last** — after everything else in this doc is realised. Nothing is
distributed to anyone until then, so there's no installed base to migrate and the
signing key can be generated right before the first real release.

- Source: GitHub Releases for this repo (binaries attached per release). No mirror
  through the Worker.
- Check: `GET /repos/<owner>/<repo>/releases/latest` (or `/releases` filtered for
  the prerelease channel), semver-compare `tag_name` to the running version.
  Unauthenticated GitHub API is 60 req/hr/IP — a 24 h check is nowhere near that;
  back off on `403`.
- Apply: download the asset for this OS/arch, **verify a detached signature
  (minisign or cosign) against a public key baked into the binary** — planned
  from the first distributed build, not optional — then self-replace. Windows
  can't overwrite a running exe: rename self → drop new exe → spawn new → exit.
  `self_update` / `self-replace` crates do most of this against GH Releases.
- Coordinate with the single-instance socket lock: the new process must wait for
  the old one to drop it. Sequence the handoff in `ipc`.
- UI: `[updates].on_update` decides notify / auto-download / auto-install. The
  Config button walks **Check → Downloading → Install `vX.Y.Z` (restart)**.

## Logging & observability

- **Logging** — `tracing` + `tracing-subscriber`. Rolling file in the data dir
  (`tracing-appender`), level from `[advanced].log_level`, plus stderr in debug
  builds. Config > Advanced's "open logs folder" / "copy diagnostics" read from
  here.
- **Crash / error reporting** — `sentry` crate at a private DSN, `release` set to
  the version string above, `environment` = channel. **Deferred** — put the
  `tracing` layer in now so switching it on later is just initialising the
  client; don't block the rest of the build on it.
- Opt-in, not silent. `[advanced].crash_reports` is tri-state: **absent** until
  answered. After the first successful sign-in, if it's absent, ask once
  ("upload anonymous error reports?") and write the answer. Not re-asked on later
  sign-ins — a fresh `config.toml` (or Config > Advanced's "Reset config") is what
  brings the prompt back. Always changeable in Config > Advanced regardless.
  Reports ship stack traces and local paths off the machine, hence opt-in.

## Window behavior

- egui's own popups (combo boxes, context menus, in-window confirm modals) keep
  focus inside the window, so auto-hide (`app/mod.rs`) is not a threat to any of
  the Config UI. The only thing that hands focus away is spawning a native dialog
  (`rfd` file picker, etc.); the handful of spots that do must suppress auto-hide
  for its duration, same as `Connecting` / `Validating` already do.
- Add regardless:
  - an egui minimize button in each window
  - minimize on `Esc`
- Add as an option — `[window].hide_on_focus_loss` (default `true`). When off, the
  window only leaves via the minimize button, `Esc`, or the tray toggle.

## Open questions

None blocking. Deferred to implementation time (sequencing, not open design):

- auto-updater + signing key — built last, key generated just before first release
- Sentry client init — `tracing` layer goes in now, DSN wired later
- launcher / process-watch model for "pull right before you play"
