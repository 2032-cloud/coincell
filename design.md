# coincell — design outline

Working notes for the sync daemon. Companion to 2032's `infra.md` / `design.md`
(the backend this talks to). Sections tagged **BUILT** are implemented and the
prose describes what the code does; everything else is design intent. `<!-- -->`
marks rejected ideas and undecided forks.

Built so far: the `config.toml` layer + config window, the shared window chrome
(header / minimise / Esc / UI scale), session identity, the whole `src/api/`
module (REST client, device auth, WebSocket event stream, sync orchestrator),
the `data.sqlite` store (`src/store.rs`), and branding-driven theming
(`src/theme.rs` off `GET /api/branding`). Not started: the sync **engine** that
consumes the events and drives the store, the Home window, the tray menu,
versioning, logging, the updater.

## What coincell is

- Standalone Rust daemon + tray app. The first real client of 2032's
  `cr.2032.cloud` device-auth API.
- Watches user-configured local save files (per game), pushes changes up, pulls
  newer versions down.
- Lives in the tray. Two frameless windows, bottom-right: **Home** (left-click,
  the per-game list) and **Config** (right-click). They auto-hide on focus loss
  (unless `[window].hide_on_focus_loss = false`), and always via the header's
  **—** button or `Esc`.
- Distributed as GitHub Release binaries, pointed to from the website. Self-updates
  from those same releases.

## Backend surface we actually have (`cr.2032.cloud`, Bearer `<session_id>`)

- `GET /auth/device/config` — public bootstrap (Auth0 domain/client_id/audience/scope + `api_base`)
- `POST /auth/device/token` — mint session (one-time, from the Auth0 JWT)
- `POST /auth/device/logout` — revoke _this_ session, no `:id`
- `GET /api/me` — `sub` + `theme` only (no name/email/picture on `cr.`)
- list / create game instances — list takes `?since=<ts>` (instances whose latest
  save is newer than `ts`) and embeds a `latestSave` object per row:
  `{ id, content_hash, size_bytes, uploaded_at, starred, note }`
- `GET /api/game-instances/:id/saves` — list; `POST` same path — push raw
  `application/octet-stream`, returns `{ id, duplicate }`; `GET …/:saveId` —
  download raw bytes
- `GET /api/events` — WebSocket upgrade (sessionBearer); the realtime
  save-updated stream, one per-user Durable Object behind it (see Download
  direction)
- `GET /api/consoles`, `GET /api/consoles/:slug/games` — public catalog + art.
  Consoles carry `validSaveSizes: number[]`, the only save-size check we need.
- `GET /api/branding` — public, snake_case. The service's presentation layer
  (name, tagline, homepage / docs URLs, attribution line, a light + dark colour
  palette, `default_scheme`, typography hints). Client theme comes from here (see
  Theme). Payload example under Theme.
- Nothing that edits existing data (instance rename, star, note, retention limit,
  account/theme) is reachable here — that's browser-only on `2032.cloud`.
- Retention: backend keeps `save_retention_limit` (default 5) non-starred saves
  per instance, cycles oldest out. Starred saves never cycle.
  `(game_instance_id, content_hash)` is unique — re-uploading identical bytes
  just bumps `uploaded_at`, so redundant uploads are cheap and safe. `POST …/saves`
  returns `duplicate: true` in that case.

<!-- casing: the backend is being standardised to all-snake_case responses.
     Until that job lands, `api::models` names every field snake_case and adds a
     `#[serde(alias = "camelCaseName")]` per field so it decodes both; `starred`
     reads via a bool-or-int helper. Strip the aliases once the backend is done. -->

## API module (`src/api/`)

**BUILT.** The whole device API as a self-contained module that depends on
nothing else in the crate (no `config`, no `constants`, no `egui`) — everything
it needs is passed in, so it can be lifted into its own crate later. The old
`src/auth.rs` is gone, folded in here. New deps: `thiserror`, `tungstenite`
(rustls, webpki roots), `serde_json`.

- `error` — `Error` (`thiserror`) with `is_unauthorized()`; `401`/`403` from
  anywhere collapse to `Error::Unauthorized`, the one variant the UI reacts to.
- `models` — every wire type, casing-tolerant (see above). `Timestamp` is a
  string newtype whose byte order is chronological, so it doubles as the cursor.
  `Branding` + `Brand*` sub-structs decode `/api/branding` (snake_case, no
  aliases needed).
- `client` — blocking `Client { base, session }`, `Clone`, `with_session()`, one
  method per REST endpoint (`me`, `consoles`, `games`, `game_instances(since)`,
  `create_game_instance`, `saves`, `upload_save`, `download_save`, `logout`).
  Plus free `fetch_device_config(api_base)` and `fetch_branding(api_base)` (both
  used before a `Client` exists).
- `device` — the Auth0 device flow, now egui-free: `DeviceFlow` (channel of
  `DeviceEvent`), `SessionCheck` (`Valid(Me)` / `Invalid` / `Unknown`) — the
  `Me` body rides along so the app gets `theme` without a second request —
  `revoke_in_background`, `open_in_browser`. Each spawner takes a
  `wake: impl Fn()` — the app passes `ctx.request_repaint`, a library `|| {}`.
- `events` — `EventStream`: a `tungstenite` (blocking, rustls) WebSocket to
  `/api/events`. Parses `{"type":"save","instanceId":…}` into
  `StreamEvent::{Connected, SaveChanged, Disconnected}`, self-reconnects with
  backoff, uses a 500 ms socket read-timeout to stay responsive to `stop()` and
  to flush keepalive pongs.
- `sync` — `SyncStream`: the stateful orchestration. Opens the `EventStream`
  **before** the first poll (no gap), does a full hydrate (emits `Synced`), then
  on each WS ping / `poll_now()` / fallback tick re-polls `?since=cursor` and
  emits `Changed { instance_id, latest }`, advancing the cursor by max
  `last_saved_at`. Same-second boundary is covered by a targeted `saves()` fetch
  when the stream names an instance the poll didn't return. On reconnect it
  re-polls from the cursor. Also emits `Connected` / `Disconnected` / `Error`. It
  does **not** touch the filesystem or a store — that's the sync engine.

App wiring: `App` holds the `DeviceConfig` and the `Branding` (both fetched once
in `main` from `advanced.api_base`), the account theme from
`SessionStatus::Valid(Me)`, and an `Option<SyncStream>` (started on `Ready`,
dropped on logout / when `[sync].enabled` is false). `drain_sync()` currently just
`eprintln!`s the events — the consumer that applies them isn't built yet, and an
`Error` carrying an auth failure is logged, not acted on.

## Config split: preferences vs state

Two files, different lifecycles:

- **`config.toml`** (exists) — user-set preferences + identity. Hand-editable.
  Already preserves unknown keys. Keep it small.
- **`data.sqlite`** (**BUILT** — `src/store.rs`; goes in `DATA_DIR` —
  `ProjectDirs::data_dir()`, deliberately separate from the config dir) —
  operational state the daemon owns, not meant to be hand-edited. Holds: path ↔
  `game_instance_id` mappings and per-instance pause flags; per-instance sync
  bookkeeping (last-synced hash / save id, last uploaded hash); the offline
  upload queue; the last-stream-position timestamp (`SyncStream`'s cursor);
  conflict markers; and a launch-time cache of each console's `validSaveSizes`.
  Rebuildable from the backend without losing user intent.

<!-- putting watch bookkeeping in config.toml would mean the user's editor fights
     the daemon's writes, and a corrupt state file would nuke their preferences -->

**DECIDED: SQLite, not a JSON blob.** Adds a dep, but the offline queue + save
history + per-instance reconciliation are relational and will only grow; JSON
would be rewritten whole on every change and get awkward fast.

**BUILT** (`src/store.rs`), matching every point below:

- `rusqlite` with the `bundled` feature — compiles SQLite in, no system libsqlite
  dependency, which matters for the standalone Release binaries.
- WAL mode (`synchronous = NORMAL`, `foreign_keys = ON`, 5 s `busy_timeout`).
  Single writer behind a `Mutex`, mirroring the existing `Config` pattern
  (`OnceLock<Mutex<_>>`, closures over a guard) so call sites never see SQL or
  connection handling — `Store::get(|s| …)` for reads, `Store::write(|s| …)`
  running the whole closure as one transaction (with a `ROLLBACK` guard so a
  panicked writer can't wedge the next one). Typed methods on `Store` per
  operation; the SQL lives only there.
- Schema versioned from day one — `PRAGMA user_version` + an append-only
  `MIGRATIONS` list, each entry applied (schema change + version bump) inside a
  transaction.
- Same robustness manners as `Config`: on open/migrate failure the file (and its
  `-wal` / `-shm` siblings) is moved to `data.bak[.N].sqlite` and rebuilt; if
  even that fails the daemon runs against an in-memory database rather than
  crashing.
- Tables: `instances` (path ↔ id, pause, sync bookkeeping + conflict columns),
  `upload_queue` (bytes snapshotted at enqueue, `UNIQUE(instance, hash)`,
  `ON DELETE CASCADE` from `instances`), `console_save_sizes` (JSON `validSaveSizes`
  cache), `meta` (k/v — holds the `sync_cursor`).

## `config.toml` schema

**BUILT** (`src/config.rs`). Section structs each carry `#[serde(default)]` +
a `Default` impl, so a missing or partial `[table]` falls back per-field. The
enum values are kebab-case; `poll` is a `PollInterval` (`"auto"` / `"off"` /
`"30s"` / `"5m"` / `"1h"` / `"1d"`), `check_interval` an `Interval` (same
duration syntax, no auto/off). Unknown keys survive a round-trip.

```toml
session_id = "..."

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
ui_scale = 1.0                 # egui zoom factor, clamped 0.5..=3.0

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

**BUILT** (`src/app/config.rs`). The window is tall and narrow (~9:16), so it's a
**left icon rail** (48 px, Phosphor glyphs, section name on hover) + a working
area, not a text sidebar. There is always exactly one section selected; `reset()`
(called every time the window is shown as Config, and after a logout) returns it
to **Account**.

Icons: `egui-phosphor` has no release for eframe/egui 0.36 yet, so `Phosphor.ttf`
plus the ~10 codepoints in use are vendored under `assets/` and
`src/app/icons.rs` (MIT). `icons::install()` runs once in `App::new`.

Chrome: a **shared title bar** (`App::header`, a `Panel::top`) sits above every
screen — Config, Home, sign-in — with the screen name on the left and the
minimise-to-tray **—** button on the right. Not per-screen; `App` owns it.

Rail sections:

1. **Account** — "Signed in on this device", short session id, an
   "Manage account & sessions ↗" link to `2032.cloud/settings` for what `cr.`
   can't do (rename/revoke other sessions, theme, retention, deletion). Sign-out
   is the rail's footer button, not here. _(No `sub` shown yet — would need a
   `GET /api/me` fetch the app doesn't do outside validation.)_
2. **Sync** — `enabled`, poll interval, upload trigger, conflict policy,
   pause-on-metered. _(No "Sync now" / status line — no sync engine yet.)_
3. **Startup** — launch on login, start hidden. _(Records the pref only; OS
   autostart registration not wired.)_
4. **Notifications** — master toggle + the four per-event toggles (disabled while
   the master is off).
5. **Appearance** — theme (follow account / follow system / light / dark;
   applied live via `src/theme.rs`, see Theme) and **UI scale**
   (`window.ui_scale`, a preset % combo). Scale is applied live too: `App::logic`
   pushes it to `ctx.set_zoom_factor` whenever it drifts from the stored value.
6. **Updates** — current version (`CARGO_PKG_VERSION` for now), channel,
   check-automatically, on-update action. _(No "Check now" — no updater yet.)_
7. **Advanced** — log level, crash-reports checkbox (shows "not answered yet"
   when the pref is absent), API base URL (commits on focus-loss, blank reverts),
   open-config-folder / open-data-folder / copy-diagnostics buttons.

**Rail footer** (below a `bottom_up` gap): **Log out** and **Quit CoinCell**.
Each switches the working area to a confirmation "section" (`ConfirmLogout` /
`ConfirmQuit`) with the warning text + Cancel / affirm buttons. **Cancel returns
to Account**; the affirm bubbles a `ConfigOutcome` up to `App`:

- `LogOut` → `api::revoke_in_background` (fire-and-forget
  `POST /auth/device/logout`), `Config::clear_session()` regardless of that call,
  drop the `SyncStream`, back to the sign-in screen.
- `Quit` → `App` sets `quitting = true` (suppresses the focus-loss auto-hide for
  that frame) and sends `ViewportCommand::Close`, ending `run_native` cleanly.

Not built: **Restart**, **Reset config**, **open-logs-folder** (no logs yet).

## Home window

Not built (the stub renders "Home"). **DECIDED: coincell owns the full instance
lifecycle natively — no bouncing to the website for a first bind.** All the API
calls it needs already exist as `Client` methods.

- **Add a game** — `client.consoles()`, then `client.games(slug)`, both
  art-enriched. eframe renders the box art / icons in a scrollable grid cheaply.
  Then bind a local save path (native picker via `rfd`, not yet a dep) and
  `client.create_game_instance(..)`. Also supports an unlinked instance
  (`game_slug` null + `game_name`) for romhacks / anything not in the catalog.
- **Per-instance row** — display name, last-synced time, sync status, per-instance
  pause, the watched path, and a manual "sync now / restore" affordance.
- **Save history** — on demand, `client.saves(id)`, listed and sorted
  client-side; lets the user restore an older or starred save to disk.
- **Conflict resolution** — the "changed here and on another device" picker
  (see Conflict policy) lives here.
- Catalog responses can be cached in `data.sqlite` with a short TTL so reopening
  the add-game flow isn't a fresh fetch every time.

## Session identity

**BUILT** (`constants.rs`). The `client_name` sent at bootstrap is:

```
{APP_NAME} - {username}@{device_name} - {platform}
  e.g.  CoinCell - ethan@tower - Windows
```

- `whoami` (2.x — the whole API is fallible now, no `fallible` submodule):
  `username()`, `devicename()`, `platform()`. `user@host` is what actually
  distinguishes two machines where the login name and location match.
- Degrades a piece at a time: no device name → `CoinCell - ethan - Windows`; no
  username either → `CoinCell - Windows`. Never the crate's `"Unknown"` filler.
- `constants` exposes the parts too — `USERNAME`, `DEVICE_NAME`, `PLATFORM`
  (empty string = unavailable) — and the assembled `CLIENT_NAME`.
- The backend appends its own `request.cf` location. The name is editable on the
  website (infra.md), so this is only the default.

<!-- the OS username leaves the machine and shows in the user's own Sessions list.
     own account, own data -- acceptable, noting it. -->

## Tray context menu

Not built — the tray is click-only (`Menu::new()` is empty). Add a real menu:
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

**The network state machine for all of this is BUILT — `api::SyncStream`** (see
the API module section). It emits `Synced` / `Changed { instance_id, latest }` /
`Connected` / `Disconnected` / `Error`. What's left for the engine is everything
below the network line: diffing `latest.content_hash` against the on-disk file,
downloading, writing, the offline upload queue, and conflict resolution.

**Realtime — WebSocket at `GET /api/events`** (Bearer session; one per-user
Durable Object behind it). The server message is only `{type:"save",instanceId}`
— no payload — so `SyncStream` reacts by polling `?since=` for the details
(with a targeted `saves()` fallback for the same-second boundary).

**Catch-up — `GET /api/game-instances?since=<ts>`.** Filters to instances whose
latest save is newer than `since`. Each row carries its `latest_save`
(`content_hash`, `size_bytes`, `uploaded_at`, `starred`, `note`), so the daemon
settles staleness for every instance from this one call — no follow-up `/saves`
per instance. `client.saves(id)` is only hit for the same-second backfill and
for save history in Home.

Done by `SyncStream` (see API module):

- **Connect the WebSocket _before_ the first poll**, so nothing that lands
  between "poll returned" and "stream live" is missed.
- **Initial hydrate:** poll with **no `since`** → `Synced { instances }` with
  every instance's `latest_save`.
- **Steady state:** cursor = max `last_saved_at` seen; each ping / poke / fallback
  re-polls `?since=cursor` → `Changed` per instance.
- **On reconnect:** re-poll from the cursor. Wake-from-sleep folds into this (it
  drops the socket too).

Still owed by the **engine** (not `SyncStream`):

- The first-run completeness pass — walk every instance's `latest_save` comparing
  `content_hash` against the on-disk file, so an older file that never synced
  isn't missed.
- Acting on it: download, write to disk, update `data.sqlite` bookkeeping.
- A "Sync now" button wired to `SyncStream::poll_now()`. (`[sync].poll` is
  already mapped onto the fallback interval in `App::start_sync`:
  `auto` → 5 min, `off` → none, else the given duration.)

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

- Failed uploads go to a queue in `data.sqlite`, retried with backoff; survive
  restart.
- A `401` mid-sync should flip the app into the logged-out state and fire the
  session-expired notification — don't silently spin. Today `SyncStream` surfaces
  it as `SyncEvent::Error` and `drain_sync` only logs it; wiring the reaction is
  a TODO.
- `pause_on_metered` (Windows) and a manual global pause both stop all network
  work but keep watching the filesystem, so the queue is ready when sync resumes.

## Versioning

Not built (Config > Updates and the diagnostics copy show raw
`CARGO_PKG_VERSION`). One version string, resolved at build time, to be used
everywhere — Config > Updates, the diagnostics copy, Sentry `release`, the API
`User-Agent`.

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
  it can still check and _show_ the latest release, but auto-install stays off.
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

- **Logging** — not built. `tracing` + `tracing-subscriber`, rolling file in the
  data dir (`tracing-appender`), level from `[advanced].log_level`, plus stderr in
  debug builds. Config > Advanced's "open logs folder" / "copy diagnostics" read
  from here. (Right now everything just `eprintln!`s.)
- **Crash / error reporting** — not built. `sentry` at a private DSN, `release` =
  the version string above, `environment` = channel. Put the `tracing` layer in
  first so switching Sentry on later is just client init; don't block on it.
- **The opt-in gate IS built.** `[advanced].crash_reports` is tri-state:
  **absent** until answered. On the first `Ready`, if it's absent, `App` shows a
  one-time `egui::Modal` ("upload anonymous error reports?") and writes the
  answer; backdrop / `Esc` counts as no. Not re-asked on later sign-ins — a fresh
  `config.toml` (or a future "Reset config") brings it back. Also a plain
  checkbox in Config > Advanced. Reports would ship stack traces and local paths,
  hence opt-in.

## Window behavior

**BUILT** (`app/mod.rs`).

- egui's own popups (combo boxes, the first-run modal, in-window confirms) keep
  focus inside the window, so auto-hide was never a real threat to the settings
  UI. Only a native dialog (`rfd` file picker) would hand focus away; the spots
  that add one must suppress auto-hide for its duration, same as `Connecting` /
  `Validating` already do.
- The shared title bar (`App::header`, a `Panel::top` above every screen) carries
  the screen name and the **—** minimise button. The button always works.
- **Esc** also minimises — consumed at end-of-frame via `input_mut().consume_key`
  so an open combo / the first-run modal claims it first, and skipped during
  `Connecting` / `Validating` (Esc there reads as "cancel the dialog").
- `[window].hide_on_focus_loss` (default `true`) gates only the _auto_-hide. When
  off, the window leaves only via **—**, `Esc`, `Quit`, or the tray toggle.
- Reopening the tray always lands on a clean screen: `App::sync_shown_screen`
  calls `ConfigApp::reset()` / `HomeApp::reset()` on every visible-state change.

## Theme

**BUILT** (`src/theme.rs`). The palette is **not** hard-coded — it comes from
`GET /api/branding`, fetched once in `main` (right after device config) with a
copy baked into the binary (`assets/branding.json`) as the offline fallback.
`App` holds the `Branding` and, every frame like the zoom-factor sync, re-runs
`theme::resolve` → `theme::apply` if the outcome changed.

`resolve(pref, account_theme, system, branding)` picks light vs dark:

- `Theme::Light` / `Theme::Dark` — explicit, wins.
- `Theme::Auto` — `ctx.system_theme()`, else `branding.colors.default_scheme`.
- `Theme::Account` — the `/api/me` `theme` bool (**`true` = light, `false` =
  dark** per 2032's `infra.md`; `null` = follow system). Carried on
  `SessionStatus::Valid(Me)`. A fresh device login bounces back through
  `Validating` once so this is populated without an extra request; until then it
  falls back to follow-system.

`apply` maps the chosen `Brand*` palette (`bg`, `bg_elevated`, `text`,
`text_muted`, `border`, `accent`, `accent_hover`, `danger`, `on_accent`,
`focus_ring` — all `#rrggbb`, parsed with `Color32::from_hex`, each with a sane
fallback) onto `egui::Visuals` (panel/window fill, text + weak-text override,
hyperlink = accent, error = danger, the five `widgets` states, selection) and
calls `ctx.set_theme` + `ctx.set_visuals_of`.

Identity fields are wired into the UI too: the sign-in screen shows `APP_NAME` +
`tagline` and an `attribution_text` link; Config › Account links
`homepage_url/settings` and `docs_url` and shows the attribution; Home shows the
attribution line.

Typography is intentionally **not** applied: `font_source_url` is `null` (nothing
to download) and egui can't resolve a CSS family by name — the bundled font
stands in. `assets` is `[]` and unused. Wire either up here if the service starts
serving them.

Example `/api/branding` payload (pretty-printed, matches `assets/branding.json`):

```json
{
  "schema_version": 1,
  "updated_at": "2026-08-28",
  "identity": {
    "name": "2032", "short_name": "2032", "tagline": "Retro game save sync",
    "homepage_url": "https://2032.cloud", "docs_url": "https://cr.2032.cloud/docs",
    "attribution_text": "Saves synced with 2032"
  },
  "colors": {
    "default_scheme": "dark",
    "light": { "bg": "#f0e0d6", "bg_elevated": "#f8efe8", "text": "#800000", "text_muted": "#9c5c50", "border": "#d8c3b2", "accent": "#1c71d8", "accent_hover": "#155cb3", "danger": "#b32828", "on_accent": "#ffffff", "focus_ring": "#1c71d8" },
    "dark":  { "bg": "#282a2e", "bg_elevated": "#31343a", "text": "#c5c8c6", "text_muted": "#8b8f93", "border": "#3c4046", "accent": "#1c71d8", "accent_hover": "#3d8ae5", "danger": "#e06c75", "on_accent": "#ffffff", "focus_ring": "#1c71d8" }
  },
  "typography": { "font_family": "Inter, system-ui, Avenir, Helvetica, Arial, sans-serif", "font_source_url": null, "weights": [400, 500, 700] },
  "assets": [],
  "usage": { "guidelines_url": null, "notes": "Use the 2032 name and logo to link back to 2032. Don't recolor the logo or imply endorsement." }
}
```

## Open questions

None blocking. In rough build order:

1. **Sync engine** — the consumer of `SyncEvent`: diff `latest.content_hash`
   against disk, download/write, drive the offline upload queue, conflict
   resolution, cursor persistence. The `data.sqlite` store it writes through is
   **BUILT** (`src/store.rs`); this is the remaining half.
2. **Home window** — instance list, add-game flow, save history, conflict picker.
3. `tracing` logging (then Sentry DSN later).
4. `build.rs` versioning (Updates panel shows raw `CARGO_PKG_VERSION` today).
5. Tray context menu.
6. OS autostart registration; `[sync].upload_trigger` / `pause_on_metered`
   behaviour. (Theme wiring is done — see Theme.)
7. Launcher / process-watch model for "pull right before you play".
8. Auto-updater + signing key — last; key generated just before first release.
