# coincell - design outline

Working notes for the sync daemon. Companion to 2032's `infra.md` / `design.md`
(the backend this talks to). Sections tagged **BUILT** are implemented and the
prose describes what the code does; everything else is design intent. `<!-- -->`
marks rejected ideas and undecided forks.

Built so far: the `config.toml` layer + config window, the shared window chrome
(header / minimise / Esc / UI scale), session identity, the whole `src/api/`
module (REST client, device auth, WebSocket event stream, sync orchestrator),
the `data.sqlite` store (`src/store.rs`), branding-driven theming
(`src/theme.rs` off `GET /api/branding`), logging + crash reporting
(`src/logging.rs`, `tracing` + `sentry`), the sync **engine** (`src/sync/` -
both directions, the filesystem watcher, the offline upload queue, conflict
resolution), and the **Home window** (`src/app/home.rs` - the game list, local
search, the add-game flow, and the instance detail page). Left on Home: save
history + restore. Not started: the tray menu, versioning, the updater.

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

- `GET /auth/device/config` - public bootstrap (Auth0 domain/client_id/audience/scope + `api_base`)
- `POST /auth/device/token` - mint session (one-time, from the Auth0 JWT)
- `POST /auth/device/logout` - revoke _this_ session, no `:id`
- `GET /api/me` - `sub` + `theme` only (no name/email/picture on `cr.`)
- list / create game instances - list takes `?since=<ts>` (instances whose latest
  save is newer than `ts`) and embeds a `latestSave` object per row:
  `{ id, content_hash, size_bytes, uploaded_at, starred, note }`
- `GET /api/game-instances/:id/saves` - list; `POST` same path - push raw
  `application/octet-stream`, returns `{ id, duplicate }`; `GET …/:saveId` —
  download raw bytes
- `GET /api/events` - WebSocket upgrade (sessionBearer); the realtime
  save-updated stream, one per-user Durable Object behind it (see Download
  direction)
- `GET /api/consoles`, `GET /api/consoles/:slug/games` - public catalog + art.
  Consoles carry `validSaveSizes: number[]`, the only save-size check we need. A
  game's `name` is already resolved for the account's `preferred_region`
  server-side; the response's `titles` / `native_region` are ignored by the
  client.
- `GET /api/branding` - public, snake_case. The service's presentation layer
  (name, tagline, homepage / docs URLs, attribution line, a light + dark colour
  palette, `default_scheme`, typography hints). Client theme comes from here (see
  Theme). Payload example under Theme.
- Nothing that edits existing data (instance rename, star, note, retention limit,
  account/theme) is reachable here - that's browser-only on `2032.cloud`.
- Retention: backend keeps `save_retention_limit` (default 5) non-starred saves
  per instance, cycles oldest out. Starred saves never cycle.
  `(game_instance_id, content_hash)` is unique - re-uploading identical bytes
  just bumps `uploaded_at`, so redundant uploads are cheap and safe. `POST …/saves`
  returns `duplicate: true` in that case.

<!-- casing: the backend is being standardised to all-snake_case responses.
     Until that job lands, `api::models` names every field snake_case and adds a
     `#[serde(alias = "camelCaseName")]` per field so it decodes both; `starred`
     reads via a bool-or-int helper. Strip the aliases once the backend is done. -->

## API module (`src/api/`)

**BUILT.** The whole device API as a self-contained module that depends on
nothing else in the crate (no `config`, no `constants`, no `egui`) - everything
it needs is passed in, so it can be lifted into its own crate later. The old
`src/auth.rs` is gone, folded in here. New deps: `thiserror`, `tungstenite`
(rustls, webpki roots), `serde_json`.

- `error` - `Error` (`thiserror`) with `is_unauthorized()`; `401`/`403` from
  anywhere collapse to `Error::Unauthorized`, the one variant the UI reacts to.
- `models` - every wire type, casing-tolerant (see above). `Timestamp` is a
  string newtype whose byte order is chronological, so it doubles as the cursor.
  `Branding` + `Brand*` sub-structs decode `/api/branding` (snake_case, no
  aliases needed).
- `client` - blocking `Client { base, session }`, `Clone`, `with_session()`, one
  method per REST endpoint (`me`, `consoles`, `games`, `game_instances(since)`,
  `create_game_instance`, `saves`, `upload_save`, `download_save`, `logout`).
  Plus free `fetch_device_config(api_base)` and `fetch_branding(api_base)` (both
  used before a `Client` exists).
- `device` - the Auth0 device flow, now egui-free: `DeviceFlow` (channel of
  `DeviceEvent`), `SessionCheck` (`Valid(Me)` / `Invalid` / `Unknown`) - the
  `Me` body rides along so the app gets `theme` without a second request —
  `revoke_in_background`, `open_in_browser`. Each spawner takes a
  `wake: impl Fn()` - the app passes `ctx.request_repaint`, a library `|| {}`.
- `events` - `EventStream`: a `tungstenite` (blocking, rustls) WebSocket to
  `/api/events`. Parses `{"type":"save","instanceId":…}` into
  `StreamEvent::{Connected, SaveChanged, Disconnected}`, self-reconnects with
  backoff, uses a 500 ms socket read-timeout to stay responsive to `stop()` and
  to flush keepalive pongs.
- `sync` - `SyncStream`: the stateful orchestration. Opens the `EventStream`
  **before** the first poll (no gap), does a full hydrate (emits `Synced`), then
  on each WS ping / `poll_now()` / fallback tick re-polls `?since=cursor` and
  emits `Changed { instance_id, latest }`, advancing the cursor by max
  `last_saved_at`. Same-second boundary is covered by a targeted `saves()` fetch
  when the stream names an instance the poll didn't return. On reconnect it
  re-polls from the cursor. Also emits `Connected` / `Disconnected` / `Error` /
  `Unauthorized` (a `401`/`403` gets its own variant, not a generic error
  string). It does **not** touch the filesystem or a store - that's the engine.

App wiring: `App` holds the `DeviceConfig` and the `Branding` (both fetched once
in `main` from `advanced.api_base`), the account theme from
`SessionStatus::Valid(Me)`, an `Option<SyncEngine>` (started on `Ready`, dropped
on logout / when `[sync].enabled` is false), and the `catalog:
Vec<GameInstance>` the engine feeds it. `drain_sync()` keeps the catalog current,
logs the informational events, and reacts to `SessionExpired`.

## Config split: preferences vs state

Two files, different lifecycles:

- **`config.toml`** (exists) - user-set preferences + identity. Hand-editable.
  Already preserves unknown keys. Keep it small.
- **`data.sqlite`** (**BUILT** - `src/store.rs`; goes in `DATA_DIR` —
  `ProjectDirs::data_dir()`, deliberately separate from the config dir) —
  operational state the daemon owns, not meant to be hand-edited. Holds: path ↔
  `game_instance_id` mappings and per-instance pause flags; per-instance sync
  bookkeeping (last-synced hash / save id, last uploaded hash); the offline
  upload queue; the last-stream-position timestamp (`SyncStream`'s cursor);
  conflict markers; a launch-time cache of each console's `validSaveSizes`; and
  the `save_backups` index (see Overwrite guard). Rebuildable from the backend
  without losing user intent.

<!-- putting watch bookkeeping in config.toml would mean the user's editor fights
     the daemon's writes, and a corrupt state file would nuke their preferences -->

**DECIDED: SQLite, not a JSON blob.** Adds a dep, but the offline queue + save
history + per-instance reconciliation are relational and will only grow; JSON
would be rewritten whole on every change and get awkward fast.

**BUILT** (`src/store.rs`), matching every point below:

- `rusqlite` with the `bundled` feature - compiles SQLite in, no system libsqlite
  dependency, which matters for the standalone Release binaries.
- WAL mode (`synchronous = NORMAL`, `foreign_keys = ON`, 5 s `busy_timeout`).
  Single writer behind a `Mutex`, mirroring the existing `Config` pattern
  (`OnceLock<Mutex<_>>`, closures over a guard) so call sites never see SQL or
  connection handling - `Store::get(|s| …)` for reads, `Store::write(|s| …)`
  running the whole closure as one transaction (with a `ROLLBACK` guard so a
  panicked writer can't wedge the next one). Typed methods on `Store` per
  operation; the SQL lives only there.
- Schema versioned from day one - `PRAGMA user_version` + an append-only
  `MIGRATIONS` list, each entry applied (schema change + version bump) inside a
  transaction.
- Same robustness manners as `Config`: on open/migrate failure the file (and its
  `-wal` / `-shm` siblings) is moved to `data.bak[.N].sqlite` and rebuilt; if
  even that fails the daemon runs against an in-memory database rather than
  crashing.
- Tables: `instances` (path ↔ id, pause, sync bookkeeping + conflict columns),
  `upload_queue` (bytes snapshotted at enqueue, `UNIQUE(instance, hash)`,
  `ON DELETE CASCADE` from `instances`), `console_save_sizes` (JSON `validSaveSizes`
  cache), `meta` (k/v - holds the `sync_cursor`).

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
     size ranges later, treat the cached shape as "list or range", not "list". -->

<!-- crash_reports is tri-state: absent = ask, present = answered. Absence, not a
     `false` default, is what the first-run prompt keys off, see Logging below. -->

Per-game watch config (path ↔ `game_instance_id`, per-instance pause) is
**Home-menu territory**, not this file - games are added/removed there. `[sync]`
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
screen - Config, Home, sign-in - with the screen name on the left and the
minimise-to-tray **—** button on the right. Not per-screen; `App` owns it.

Rail sections:

1. **Account** - "Signed in on this device", short session id, an
   "Manage account & sessions ↗" link to `2032.cloud/settings` for what `cr.`
   can't do (rename/revoke other sessions, theme, retention, deletion). Sign-out
   is the rail's footer button, not here. _(No `sub` shown yet - would need a
   `GET /api/me` fetch the app doesn't do outside validation.)_
2. **Sync** - `enabled`, poll interval, upload trigger, conflict policy,
   pause-on-metered, and a **Sync now** button (`ConfigOutcome::SyncNow` →
   `SyncEngine::sync_now`), disabled when `enabled` is off. _(No live status line
   yet - the engine emits `EngineEvent::Status` but Home isn't built to show it.)_
3. **Startup** - launch on login, start hidden. `start_hidden` **is** honoured:
   `App` starts in `WindowState::Hidden` and `App::reconcile_visibility` (see
   Window behavior) sends `ViewportCommand::Visible(false)` on the first frames —
   eframe ignores `ViewportBuilder::with_visible` and force-shows once after the
   first paint, so re-asserting is what actually keeps it hidden.
   _(Launch-on-login still records the pref only; OS autostart not wired.)_
4. **Notifications** - master toggle + the four per-event toggles (disabled while
   the master is off). These gate `notice::post` (see Notifications); delivery
   itself is still log-only.
5. **Appearance** - theme (follow account / follow system / light / dark;
   applied live via `src/theme.rs`, see Theme) and **UI scale**
   (`window.ui_scale`, a preset % combo). Scale is applied live too: `App::logic`
   pushes it to `ctx.set_zoom_factor` whenever it drifts from the stored value.
6. **Updates** - current version (`CARGO_PKG_VERSION` for now), channel,
   check-automatically, on-update action. _(No "Check now" - no updater yet.)_
7. **Advanced** - log level (both it and the crash-reports toggle note "applies
   on restart"), crash-reports checkbox (shows "not answered yet" when the pref
   is absent), API base URL (commits on focus-loss, blank reverts),
   open-config-folder / open-data-folder / open-logs-folder / copy-diagnostics
   buttons. Diagnostics now also reports the logs path and resolved log level.

**Rail footer** (below a `bottom_up` gap): **Log out** and **Quit CoinCell**.
Each switches the working area to a confirmation "section" (`ConfirmLogout` /
`ConfirmQuit`) with the warning text + Cancel / affirm buttons. **Cancel returns
to Account**; the affirm bubbles a `ConfigOutcome` up to `App`:

- `LogOut` → `api::revoke_in_background` (fire-and-forget
  `POST /auth/device/logout`), `Config::clear_session()` regardless of that call,
  drop the `SyncStream`, back to the sign-in screen.
- `Quit` → `App` sets `quitting = true` (suppresses the focus-loss auto-hide for
  that frame) and sends `ViewportCommand::Close`, ending `run_native` cleanly.

Not built: **Restart**, **Reset config**.

## Home window

**STARTED** (`src/app/home.rs`). **DECIDED: coincell owns the full instance
lifecycle natively - no bouncing to the website for a first bind.**

### Built

**`HomeApp` is a small mode machine** (`Mode::{List, PickConsole, PickGame,
Compose, MapPrompt}`); `ui()` takes the mode out with `mem::replace`, each
`*_view` returns the next mode. `reset()` (on every show, and after add-game)
drops back to `List` and clears scratch state.

**`Mode::List` - the per-game list.** Top to bottom:

- **`[+ Add game]`** + a refresh button (`icons::SYNC` → `HomeOutcome::Refresh`
  → `SyncEngine::rehydrate()`) + a search box. Fuzzy search is local —
  `fuzzy-matcher` (`SkimMatcherV2`, case-insensitive), scoring each instance as
  the **max** match across `name` / `default_name` / `custom_name`, so a session
  renamed "100% Completion" over _Mother 3_ matches `100`, `mother`, `comp`,
  `3`, … .
- **Vertical scroll area** holding a **two-wide grid of cards**. `card()` (a
  painter + child `Ui`, an immediate-mode "component", not a `Widget` impl) takes
  a `CardData { title, subtitle, art_url, key, dot, hover }` so consoles / games
  reuse it. Box art on top, a strip below with a mapped/not-mapped dot (list
  cards only) + title + muted subtitle. Instance art via `box_art_url` mirroring
  2032's `boxArtUrl` fallback chain (`{api_base}/api/consoles/…` curated, IGDB
  `t_cover_big` when there's a resolved cover but no curated file); console /
  game art comes pre-resolved as `box_art_url` on the API row. Loaded by
  `egui_extras` image loaders (`install_image_loaders` in `App::new`; `http` +
  `image` features), then `asset::install` layers a disk cache over the network
  loader (see Art cache), over a hue-from-key + initials placeholder.
- **Art requests are bounded to what's near the viewport.** `card()` allocates
  its rect, then bails (no paint, no `egui::Image`, so no fetch) unless the rect,
  grown by `CARD_PREFETCH` vertically, intersects the clip rect. `render_grid`
  lays out every row (fine for account-sized lists); `render_grid_virtual`
  (used by `PickGame`, the catalog is ~1500 games) wraps a `ScrollArea::show_rows`
  so only near-visible rows are built at all. Shared bits: `card_width`,
  `card_row`, `card_art_h`, `CARD_GAP`, `CARD_STRIP_H`.
- **Sections**: **Mapped** first (what you look at day to day), then **Not
  mapped** (usually everything else on the account). Headings only show when both
  are non-empty; steady state (all mapped) is just the grid. Sort within a
  section: newest-changed first (`last_saved_at` desc, no-save last), or by score
  when searching. A card click opens `Mode::Detail`.

**Region-aware game names**: resolved **server-side**. `Game.name` already
reflects the account's `preferred_region` (a per-account setting on the website,
like theme), so the client just renders `name` everywhere (cards, `Compose`,
search haystack) and carries no region/title code paths. The response still
includes `titles` + `native_region`; the client ignores them.

**Fonts** (`src/app/fonts.rs`, `fonts::install`): starts from
`egui::FontDefinitions::default()` (Ubuntu covers Latin/Greek/Cyrillic) and
`add_fallback`s two families, consulted only for glyphs the primary lacks:
`assets/Phosphor.ttf` for the icon codepoints in `app::icons`, and
`assets/NotoSansCJKjp-Regular.otf` (~16 MB, SIL OFL 1.1) for JP/KR/ZH game
titles. `ab_glyph` 0.2.32 / ttf-parser 0.25 handle CFF, so the `.otf` loads
(verified: `set_fonts` parses it with no panic). Only the `jp` regional build is
bundled: `kr` / `sc` / `tc` / `hk` carry the same glyph coverage and differ only
in default Han shapes, and egui's fallback stops at the first font with the
glyph, so chaining them does nothing.

`GET /api/me` carries more than this (Auth0 profile, session id) and its shape
differs by domain, so every field on `Me` is `#[serde(default)]`: it doubles as
the "session valid" probe and a missing key must not fail the parse.

<!-- Both game and instance names are resolved server-side now (game name follows
     the account's preferred region, instance name adds the `custom_name`
     override on top). The client renders whatever `name` it's given. -->


**Add-game flow** - `+` → `PickConsole` → `PickGame` → `Compose` → `MapPrompt`:

- `PickConsole` / `PickGame` reuse the search + card grid over background-fetched
  `client.consoles()` / `client.games(slug)` (a `Task<T>` - spawn thread, poll a
  channel each frame, `ctx.request_repaint()` on done). `PickGame` also has a
  "Not listed - enter it manually" path for an unlinked instance. The consoles
  job also `Store::cache_console_sizes`es every console's `validSaveSizes` for
  the mapping check / a future detail page.
- `Compose` - the chosen console + game (or a required "Game name" field for
  custom), an optional "Session name" (`custom_name`), a `Create` button →
  `client.create_game_instance(NewGameInstance{..})` (background) → the new id.
- `MapPrompt` - "map a save now, or later?". **Later** → `HomeOutcome::Refresh`.
  **Now** → the shared `file_pick()` widget (below), same as the detail page.
- `src/app/mapping.rs` holds `pick_save_file` (`rfd`, returns a `PathBuf`),
  `PickedSave::inspect` (read + hash + size-check, `None` if the file can't be
  read), `human_size` / `describe_sizes`.

**`file_pick()`** (free fn, shared by `MapPrompt` + `Detail`, state in `FilePick`):

We only ever map a file that **already exists**. The user is told to save once in
their emulator first, which guarantees the name, location and format are right,
so there's no folder-plus-name guessing.

- **Choose save file…** → `HomeOutcome::OpenSaveDialog`; `App` runs
  `mapping::pick_save_file` on its own thread (see Window behavior — no freeze, no
  auto-hide), `HomeApp::deliver_save_pick` builds a `PickedSave` via `inspect`
  (or sets an error if the file can't be read, e.g. the emulator has it locked).
- A pick whose size isn't in `validSaveSizes` (`describe_sizes` for the phrasing)
  is a **soft block** with **Use it anyway**.
- **Local vs server**: `file_pick` takes the instance's newest server save
  (`remote: Option<&SaveMeta>`, `None` from `MapPrompt` since a just created
  instance has none). When the picked file's `content_hash` differs from the
  server's, the confirm step becomes two buttons: **Upload my local file** (this
  file wins, the server copy stays in history) or **Use the server's save**
  (replaces the local file). No divergence → a plain **Confirm, map this file**.
- Commit → `Store::bind_instance(id, path, console_slug)`, then seed the sync
  baseline with `record_synced` so the engine's next `reconcile` does the right
  move on its own: "upload mine" seeds `last_synced_hash = server hash` →
  `Push`; "use server's" (and the already-in-sync case) seeds
  `last_synced_hash = local hash` → `Pull` / `Idle`; no server save → leave it
  unset → `Push`. Then `HomeOutcome::MappedInstance` → `App` calls
  `SyncEngine::recheck(id)` + `rehydrate()`.

<!-- TODO (mapping): once the backend serves expected save-file extensions per
     console / game, pass them to `rfd` as a filter in `pick_save_file`
     (`add_filter("Save files", &["srm","sav",…])`). Nothing consumes an
     extension list yet; the picker is unfiltered for now. -->

**Data flow**: the catalog is `Vec<GameInstance>` held by `App`, fed by
`EngineEvent::Hydrated { instances }` (every hydrate) and patched by
`SaveAdvanced { instance_id, latest }`. `HomeApp` caches the set of mapped
`game_instance_id`s from the store, refreshed on `reset()` and after a map.

**New deps**: `egui_extras` (image/http), `fuzzy-matcher`, `rfd`, and `rustls`
pinned to `ring` with `CryptoProvider::install_default()` first thing in `main`
(reqwest / tungstenite / ureq otherwise leave rustls unable to pick a provider →
panic).

### Instance detail (`Mode::Detail`) **[BUILT]**

Any List card click → `Mode::Detail(DetailState)`. Shows art + name + console,
"last save on server", and:

- **Not mapped** → the shared `file_pick()` widget (see the add-game section),
  here passed `inst.latest_save` so the local-vs-server choice can appear;
  `console_sizes` for the check is `Store::console_sizes(slug)`, `[]` (no check)
  when that console was never opened in add-game here.
- **Mapped** → the watched path + **Open folder**, a one-line `Status` from the
  same `reconcile` the engine uses (in sync / newer on server / local changes
  not uploaded / conflict), a **Pause syncing** checkbox (`Store::set_paused` →
  `RecheckInstance`), **Sync now** (`RecheckInstance` → `SyncEngine::recheck`),
  and **Unmap** (confirm → `Store::unbind_instance` → `RecheckInstance`; keeps
  the instance + the file, just stops syncing here).
- **Conflict banner** (when `InstanceRecord.conflict` is set): **Keep my local
  copy** / **Keep the server copy** → `HomeOutcome::ResolveConflict { keep_local
  }` → `SyncEngine::resolve_conflict` (`push(force)` or `pull`, either way
  `record_synced` clears the marker).
- A link out to the website for rename / star / notes (`cr.` can't do those).

`open_path` moved to `app/mod.rs` (shared by Config + Detail). `HomeApp::mapped`
(the cached set) is refreshed on any bind/unbind here.

### Not built yet

- **Save history + restore** - on demand, `client.saves(id)`, sorted
  client-side; restore an older / starred **server** save to disk. Would go on
  the detail page. Needs a `Control::Restore { instance_id, save_id,
  content_hash }` on the engine (download-on-worker, through the same overwrite
  guard). Separately, a UI onto the `save_backups` the guard already collects
  (list / restore a pre-overwrite local snapshot) once its shape is decided.

<!-- TODO (backend): `/api/events` only pushes `{type:"save",instanceId}`. Add a
     `{type:"instance",…}` (created / renamed / deleted) push so Home's catalog
     stays live without the manual refresh button. Client side:
     `EventStream::StreamEvent::InstanceChanged` → `SyncStream` re-hydrates or
     emits a targeted update → `App` patches the catalog. Pairs with the
     theme-push TODO under Theme - same per-account Durable Object. -->

## Notifications

**STARTED** (`src/notice.rs`). A process-wide queue any thread posts to;
`App::logic` calls `notice::pump()` once a frame to drain it to a `Sink`.

- `Notice::{Pulled, Conflict, Error, SessionExpired}`, one per `[notifications]`
  toggle. `post()` reads `[notifications]` and drops a notice whose master or
  per-kind flag is off, then dedupes by `dedup_key` (e.g. `conflict:<game>`):
  the same notice inside a 10s window is dropped, so a burst of pulls or a
  flapping conflict is one line, not ten. `Queue::admit` is the pure
  (config + `now` in, bool out) core, unit-tested.
- **The delivery backend is deliberately unbuilt.** `Sink` has one impl,
  `LogSink` (a `tracing` line), as the default. A real OS-toast sink
  (`notify-rust`, or a hand-rolled per-platform one, undecided, see the tray /
  notification tradeoff notes) drops in via `notice::set_sink` in `main` with no
  change to any call site. Until then notices land only in the log.
- Wired posts: `EngineEvent::Pulled` and `Conflict` (from `App::drain_sync`,
  resolved to a game name via the catalog by `App::game_label`) and
  `SessionExpired` (from `handle_session_expired`). `EngineEvent::Error` is left
  unwired pending a call on which sync errors deserve a toast (`on_error` is
  already in config + the settings UI).

## Art cache

**BUILT** (`src/asset.rs`). An `egui` `BytesLoader` for `http`/`https`,
registered by `asset::install(ctx)` right after
`egui_extras::install_image_loaders` so it's tried first (egui walks bytes
loaders newest to oldest); the stock network loader then never sees an art URL.

- Two layers: an in-memory `HashMap<uri, Poll<Result<bytes, err>>>` (same shape
  as the stock loader) over files under `PROJECT_DIRS.cache_dir()/art/`, named
  `<sha256(uri)>.<ext>` (ext copied from the URL, `img` if none). On a memory
  miss it spawns one thread (like the stock loader): disk hit returns immediately
  with no network; disk miss fetches once via a shared blocking `reqwest::Client`
  and writes the bytes back to disk. So repeat views and, more importantly,
  relaunches cost no network.
- The catalog is ~1500 games; this plus the near-viewport gating in `card()` /
  `render_grid_virtual` (see Home window) is what keeps us from hammering our
  own endpoint and IGDB. If the cache dir can't be created it degrades to a
  plain fetcher (no persistence).
- Not done: eviction / max size (the dir grows unbounded, though art is small and
  rarely changes) and any freshness check (a changed asset needs a manual cache
  clear until then).

## Session identity

**BUILT** (`constants.rs`). The `client_name` sent at bootstrap is:

```
{APP_NAME} - {username}@{device_name} - {platform}
  e.g.  CoinCell - ethan@tower - Windows
```

- `whoami` (2.x - the whole API is fallible now, no `fallible` submodule):
  `username()`, `devicename()`, `platform()`. `user@host` is what actually
  distinguishes two machines where the login name and location match.
- Degrades a piece at a time: no device name → `CoinCell - ethan - Windows`; no
  username either → `CoinCell - Windows`. Never the crate's `"Unknown"` filler.
- `constants` exposes the parts too - `USERNAME`, `DEVICE_NAME`, `PLATFORM`
  (empty string = unavailable) - and the assembled `CLIENT_NAME`.
- The backend appends its own `request.cf` location. The name is editable on the
  website (infra.md), so this is only the default.

<!-- the OS username leaves the machine and shows in the user's own Sessions list.
     own account, own data, acceptable, noting it. -->

## Tray interaction

**DECIDED: click routing, not a popup menu.** Left click opens/focuses **Home**,
right click opens/focuses **Config** (`Menu::new()` stays empty). Both buttons
are load-bearing and in use, so a `tray-icon` popup menu is not planned. Sync
now / Pause sync / Quit all live in the Config UI, which right click reaches in
one action.

## Sync engine

**STARTED** (`src/sync/`). The consumer of `api::SyncStream` (BUILT): it turns
`SyncEvent`s into disk writes, uploads, and conflict markers, reading and writing
bookkeeping through the `data.sqlite` store (BUILT, `src/store.rs`).

### Shape

`SyncEngine` wraps a `SyncStream` **and** a `notify-debouncer-full` watcher. One
worker thread owns both, drains the `SyncEvent`s and the debounced filesystem
events, and does all the I/O. It talks to `App` over two channels: an
`EngineEvent` mpsc out (`Hydrated { instances }` / `SaveAdvanced` for Home's
catalog, plus `Status` / `Pulled` / `Pushed` / `PushPending` / `Conflict` /
`Error` / `SessionExpired`), a `Control` mpsc in (`SyncNow`, which polls **and**
force-pushes + drains the queue; `Rehydrate` for Home's refresh button; `Recheck
{ instance_id }` after Home binds / pauses / unmaps; `ResolveConflict {
instance_id, keep_local }` from the detail page). `App` holds `Option<SyncEngine>` in
place of the old `Option<SyncStream>` - started when auth goes `Ready`, dropped
on logout / `[sync].enabled = false`. **All I/O (network + disk) is on the worker
thread, never the UI thread.** Store access is the global `Store::get` /
`Store::write`, `Mutex`-guarded for exactly this cross-thread use. The worker
also keeps a `remote: HashMap<id, SaveMeta>` (from hydrate / `Changed`) so a
filesystem-triggered reconcile has server state to compare against. The loop
drains stream + control + fs events each tick, sleeps 150 ms when idle, and runs
a 60 s belt-and-braces rescan + queue drain.

Modules under `src/sync/`:

- `hash` **[BUILT]** - `sha256_hex(bytes)`: lowercase-hex SHA-256, byte-for-byte
  the backend's `content_hash` (2032 `src/worker/crypto.ts` - `crypto.subtle`
  SHA-256, hex, confirmed).
- `disk` **[BUILT]** - `LocalFile::read(path)` → `Missing` | `Present { hash,
len, modified }`; `write_atomic(path, &bytes)` (temp file in the same dir →
  `sync_all` → rename, same manners as `Config::save`).
- `reconcile` **[BUILT]** - the pure decision function (below). No I/O; unit-tested.
- `time` **[BUILT]** - `parse_utc` / `format_utc` / `now_utc_string` (Hinnant
  civil-time, no calendar crate) for the server's `"YYYY-MM-DD HH:MM:SS"` format.
- `engine` (`mod.rs`) **[BUILT]** - the worker: hydrate completeness pass,
  per-event handling, the filesystem watcher, both sync directions, the offline
  upload queue, and conflict resolution (`resolve_conflict`, driven by the detail
  page). Left: save-history restore.

### Reconcile decision

`reconcile(local, remote: Option<&SaveMeta>, book, policy) -> Action`, from the
on-disk `LocalFile`, the server's `latest_save` (`None` if the instance has no
save yet), the store's `InstanceRecord` (carrying `last_synced_hash`), and
`[sync].conflict`. It compares three hashes - `local`, `remote`
(`remote.content_hash`), `synced` (`book.last_synced_hash`):

| `local`     | vs `remote` / `synced`                | `Action`     |
| ----------- | ------------------------------------- | ------------ |
| any         | `remote` is `None`, `local` missing   | `Idle`       |
| present     | `remote` is `None`                    | `Push`       |
| missing     | `synced == remote`                    | `Idle`       |
| missing     | else                                  | `Pull`       |
| == `remote` | `synced == remote`                    | `Idle`       |
| == `remote` | `synced != remote`                    | `MarkSynced` |
| != `remote` | `synced == local` (local untouched)   | `Pull`       |
| != `remote` | `synced == remote` (remote untouched) | `Push`       |
| != `remote` | else (both moved, or no baseline)     | `Conflict`   |

The `Conflict` row is then filtered by `[sync].conflict`: `ask` keeps it,
`prefer-local` → `Push`, `prefer-remote` → `Pull`, `prefer-newest` compares the
local file's mtime to `remote.uploaded_at` (parsed with a no-dep civil-time →
epoch helper) and picks the side; if the file has no mtime it falls back to
`Conflict` rather than guess.

Acting on each:

- `Pull` - `client.download_save(id, remote.id)` → **overwrite guard** (below) →
  `disk::write_atomic` → `Store::record_synced(id, remote_hash, remote.id)` →
  `EngineEvent::Pulled`.
- `MarkSynced` - bytes already match the server; `record_synced` only, no write,
  no event.
- `Push` - `push()` (Upload direction, below): upload the bytes → `Pushed`, or
  under `upload_trigger = manual` leave the file alone and emit `PushPending`.
- `Conflict` - `Store::set_conflict(id, local, remote)` + `EngineEvent::Conflict`;
  resolution UI lives in Home (see Conflict policy). A `conflict = "prefer-*"`
  policy collapses this to `Pull` / `Push` / `Idle` before it ever surfaces.
- `Idle` - nothing.

Instances that are **paused** (`InstanceRecord::paused`) or **not bound** in the
store are skipped before `reconcile` is even called. `missing + synced == remote`
is `Idle` (the user deleted a synced save; don't fight them - a manual
"restore" in Home re-pulls).

### Overwrite guard **[BUILT]**

`pull` never writes the server's bytes over local bytes the user hasn't uploaded
without keeping a copy first. `Worker::guard_overwrite(book, incoming_hash,
server_save_id, reason)` reads the current file and, if
`needs_backup(local_hash, incoming_hash, last_uploaded_hash)` (local differs from
what we're about to write **and** isn't the last thing we pushed), writes the
bytes to `BACKUP_DIR/<local_hash>` (content-addressed, `DATA_DIR/save-backups/`,
next to `data.sqlite`) and indexes them in `save_backups`
(`game_instance_id`, `original_path`, `content_hash`, `size`, `replaced_with`,
`server_save_id`, `reason` = `"pull"` / `"conflict"`, `overwritten_at`). It
returns `Deferred` if the file is locked (retry next round) and `Aborted` (with
an `Error`) if the snapshot can't be written - the pull is skipped rather than
lose bytes. `last_synced_hash` is **not** consulted: the map-time "use the
server's copy" path seeds it to the local hash for bytes that were never sent.
`save_backups` has **no FK / cascade** to `instances` - a backup outlives an
unmap. Not built yet: any pruning (the dir grows unbounded; saves are tiny) and
any UI or restore path.

### Event handling

- `Synced { instances }` - the completeness pass: for every backend instance that
  is also bound locally, `reconcile` against its `latest_save` and act. Then
  `Store::set_sync_cursor(max last_saved_at)`.
- `Changed { instance_id, latest }` - `reconcile` that one instance, act, advance
  the cursor.
- `Connected` / `Disconnected { reason }` - `EngineEvent::Status` for a Home
  status line.
- `Error { message }` - `EngineEvent::Error`, logged; non-fatal, the stream keeps
  running.
- `Unauthorized` - `EngineEvent::SessionExpired`. `App` clears the session, drops
  the engine, returns to sign-in, and fires the session-expired notification.
  (This is a new `SyncEvent` variant - `SyncStream` previously folded `401` into
  a generic `Error` string, which `drain_sync` only logged.)

- `Connected` also triggers a `drain_queue()`.

"Sync now" (Config, and the future tray menu) sends `Control::SyncNow`: the
worker does `SyncStream::poll_now()` **and** a forced `rescan(true)` (pushes even
under `manual`) **and** `drain_queue()`. `[sync].poll` maps onto the stream's
fallback interval in `App::start_sync` (`auto` → 5 min, `off` → none, else the
given duration).

### Upload direction (local → backend) **[BUILT]**

- **`notify-debouncer-full`** (wraps `ReadDirectoryChangesW` / inotify /
  FSEvents), 3 s debounce. The worker watches the **parent directory** of every
  mapped save path non-recursively (watching the file directly breaks on
  write-temp-then-rename); `resync_watches()` re-diffs that set on hydrate and on
  `Recheck`. On any debounced batch the worker just does a full `rescan()`
  (re-reconcile every mapped instance) rather than matching event paths, which is
  robust and cheap for a handful of small files. A 60 s idle tick does the same
  plus a queue drain, catching anything the watcher missed.
- `rescan()` per instance: `reconcile(local, cached remote, book, policy)` using
  the worker's `remote: HashMap<id, SaveMeta>` (populated from hydrate /
  `Changed`, since a filesystem event carries no server data). Any verdict is
  acted on, so this also picks up `Pull` / `Conflict` cases the download side
  would have handled.
- `Action::Push` → `push()`: read the bytes (retry ~5×/250 ms if
  `disk::is_locked` — Windows sharing violation / POSIX advisory lock), hash,
  `client.upload_save` → `record_uploaded` + `record_synced(hash, new save id)`
  (disk and server agree now; also clears any conflict) + a **synthetic
  `SaveMeta`** in the cache with a "now" `uploaded_at` until the stream's
  catch-up delivers the real row. Skips if `last_uploaded_hash` already matches
  or the item is already queued. `[sync].upload_trigger = manual` → no upload,
  emit `PushPending` (a forced "Sync now" overrides). `on-emulator-exit` behaves
  as `on-change` for now (process-watch is the launcher's job).
- **Offline queue**: on `upload_save` failure → `clear_stale_uploads(id, hash)`
  then `enqueue_upload(id, hash, bytes)`. `drain_queue()` runs on `Connected`,
  on "Sync now", and on the 60 s tick: per-item exponential backoff (`5s, 10s,
  20s, … cap 5 min`, off `attempts` + `last_attempt_at`), `dequeue_upload` +
  `record_uploaded` on success, `record_upload_failure` (bumps `attempts`) on
  failure. Survives restart. `EngineEvent::Pushed` on either path.

<!-- REJECTED: blocking/intercepting read() as the change signal. Needs a
     filesystem filter driver (Windows) or FUSE (Linux), elevated install,
     per-OS. notify covers the write side; the read side is the launcher's job. -->

**Launcher model (later, the clean endgame for "right before you play").** Let
the user register their emulator command with coincell. Launching through
coincell does: pull latest for that instance → exec emulator → on exit, push. No
races. Falls back to process-watch (poll the process list for a configured
emulator exe) for users who launch the emulator directly.

### Conflict policy

Conflict = local file hash ≠ last-synced hash **and** remote latest hash ≠
last-synced hash **and** the two differ (the `else` row of the table above).

- Default (`conflict = "ask"`): `reconcile` returns `Conflict`, the engine writes
  a marker (`Store::set_conflict`) and emits `EngineEvent::Conflict`, and the
  **instance detail page** shows a banner with **Keep my local copy** / **Keep
  the server copy**. The choice goes back as `HomeOutcome::ResolveConflict` →
  `SyncEngine::resolve_conflict(id, keep_local)`: `push(force)` uploads the local
  file, or `pull` writes the server's; either way `record_synced` clears the
  marker. Neither side's bytes are destroyed - the loser stays on the server as
  history / on disk until overwritten.
- `prefer-newest` uses `uploaded_at` vs local mtime; `prefer-local` /
  `prefer-remote` are explicit. Applied in `reconcile` (it takes the policy), so
  a conflict never reaches the store / detail page under a `prefer-*` setting.
- Never delete a local save file. Never discard bytes that weren't uploaded.
- **TODO**: after a keep-one resolution, also star the kept save so retention
  can't cycle it out (needs a `star` call `cr.` doesn't expose - browser-only).

### Offline / resilience

- Failed uploads go to the `upload_queue` table, retried with per-item
  exponential backoff; survive restart. **[BUILT]**
- `Unauthorized` mid-sync flips the app to logged-out and fires the
  session-expired notification - see Event handling. **[BUILT]**
- `[sync].enabled = false` drops the whole engine (`App` sets `self.sync = None`).
  A finer `pause_on_metered` (Windows) / per-instance pause that keeps the
  watcher running but suspends network work is **[TODO]** (`InstanceRecord.paused`
  is already honoured by `rescan` / `hydrate`, just has no UI).

## Versioning

Not built (Config > Updates and the diagnostics copy show raw
`CARGO_PKG_VERSION`). One version string, resolved at build time, to be used
everywhere - Config > Updates, the diagnostics copy, Sentry `release`, the API
`User-Agent`.

- **Release build** - HEAD sits exactly on a version tag and the tree is clean.
  Version is that tag's semver (`0.2.0`); channel `stable`, or `prerelease` if
  the tag says so. This is the only kind of build ever distributed.
- **Development build** - anything else. Version is
  `{CARGO_PKG_VERSION}+dev.{short_hash}` (`-dirty` suffix if the tree isn't
  clean), channel **always `development`** regardless of `[updates].channel`.
- A `build.rs` (or `vergen` / `shadow-rs`) resolves this from
  `git describe --tags --dirty` / `git rev-parse`, emitted as a `rustc-env` var;
  `CARGO_PKG_VERSION` is the fallback when git isn't available (source tarball).
- The updater treats a `development` build as "never has an update to offer" —
  it can still check and _show_ the latest release, but auto-install stays off.
  Only `stable` / `prerelease` builds self-update.

## Updater

Built **last** - after everything else in this doc is realised. Nothing is
distributed to anyone until then, so there's no installed base to migrate and the
signing key can be generated right before the first real release.

- Source: GitHub Releases for this repo (binaries attached per release). No mirror
  through the Worker.
- Check: `GET /repos/<owner>/<repo>/releases/latest` (or `/releases` filtered for
  the prerelease channel), semver-compare `tag_name` to the running version.
  Unauthenticated GitHub API is 60 req/hr/IP - a 24 h check is nowhere near that;
  back off on `403`.
- Apply: download the asset for this OS/arch, **verify a detached signature
  (minisign or cosign) against a public key baked into the binary** - planned
  from the first distributed build, not optional - then self-replace. Windows
  can't overwrite a running exe: rename self → drop new exe → spawn new → exit.
  `self_update` / `self-replace` crates do most of this against GH Releases.
- Coordinate with the single-instance socket lock: the new process must wait for
  the old one to drop it. Sequence the handoff in `ipc`.
- UI: `[updates].on_update` decides notify / auto-download / auto-install. The
  Config button walks **Check → Downloading → Install `vX.Y.Z` (restart)**.

## Logging & observability

- **Logging** - **BUILT** (`src/logging.rs`). `tracing` through a
  `tracing-subscriber` registry: a daily-rotated file
  (`DATA_DIR/logs/coincell.YYYY-MM-DD.log`, keeps 7, non-blocking, no ANSI) plus
  a stderr layer in debug builds. A `Targets` filter puts the crate at
  `[advanced].log_level` and caps dependencies at WARN. `logging::init()` runs in
  `main` right after `Config::init` and returns `Guards` that `main` holds (flush
  on drop). A panic hook logs `panic: {info}` before the prior hook. Nothing
  prints directly any more, except two `Config::load` bootstrap lines that run
  before the subscriber exists. Config > Advanced's open-logs-folder /
  copy-diagnostics point here. Level changes need a restart.
- **Crash / error reporting** - **BUILT, gated off by default**. `sentry` +
  `sentry-tracing` (rustls transport). `SENTRY_DSN` is a `const` in
  `logging.rs`; the client only initialises when it's non-empty **and**
  `[advanced].crash_reports == Some(true)`, so the opt-in is respected. The
  `sentry_tracing` layer is always in the registry (a no-op with no client):
  ERROR becomes a Sentry event, WARN/INFO become breadcrumbs. `release` is
  `CARGO_PKG_VERSION` for now (TODO: real build version + `environment` = channel
  once `build.rs` versioning lands). Toggling crash reports needs a restart.
- **The opt-in gate IS built.** `[advanced].crash_reports` is tri-state:
  **absent** until answered. On the first `Ready`, if it's absent, `App` shows a
  one-time `egui::Modal` ("upload anonymous error reports?") and writes the
  answer; backdrop / `Esc` counts as no. Not re-asked on later sign-ins - a fresh
  `config.toml` (or a future "Reset config") brings it back. Also a plain
  checkbox in Config > Advanced. Reports would ship stack traces and local paths,
  hence opt-in.

## Window behavior

**BUILT** (`app/mod.rs`).

- egui's own popups (combo boxes, the first-run modal, in-window confirms) keep
  focus inside the window, so auto-hide was never a real threat to the settings
  UI. A native window we don't own **would** hand focus away, so auto-hide and
  the Esc-minimise are gated by `modal_active` = `busy_auth` (the device flow /
  session check) **or** `pending_pick.is_some()` (the save-file picker, below).
- The **save-file picker runs off the UI thread**: Home returns
  `HomeOutcome::OpenSaveDialog`, `App` spawns a thread that calls
  `mapping::pick_save_file` (RFD inits COM per call, so any thread is fine),
  parks the `Receiver` in `App.pending_pick`, and hands the `PathBuf` back via
  `HomeApp::deliver_save_pick` once it lands. So the window
  neither freezes nor minimises while the dialog is up. On resolve `App` also
  **re-arms `focus_latch`** — the dialog held focus and the OS can take a frame
  or two returning it, which would otherwise read as a focus-loss auto-hide.
- **Visibility is state-driven.** `hide()` / `show_home()` / `show_config()` only
  set `WindowState`; `App::reconcile_visibility` (end of every `logic()`) sends
  `ViewportCommand::Visible(!hidden)` when it drifts from the last one sent. For
  the first 3 frames it re-asserts unconditionally (and forces repaints) because
  eframe force-shows the window once after the first paint (anti-flash hack) and
  ignores `ViewportBuilder::with_visible` — re-asserting is the only reliable way
  to honour `start_hidden`. The hidden branch of `ui()` still paints an empty
  `CentralPanel` so a briefly-shown window is themed-blank, never black.
- `App` starts in `WindowState::Hidden` when `[startup].start_hidden`, else
  `ShowConfig`.
- The shared title bar (`App::header`, a `Panel::top` above every screen) carries
  the screen name and the **—** minimise button. The button always works.
- **Esc** also minimises - consumed at end-of-frame via `input_mut().consume_key`
  so an open combo / the first-run modal claims it first, and skipped while
  `modal_active`.
- `[window].hide_on_focus_loss` (default `true`) gates only the _auto_-hide. When
  off, the window leaves only via **—**, `Esc`, `Quit`, or the tray toggle.
- Reopening the tray lands on a clean screen: `App::sync_shown_screen` calls
  `ConfigApp::reset()` / `HomeApp::on_reopen()` on every visible-state change.
  The one exception: `HomeApp::on_reopen()` **keeps** an unfinished `MapPrompt`
  (an instance was created server-side and still needs a mapping decision), so
  minimising / switching away mid add-game — including with the off-thread file
  picker still open — doesn't strand it. A native dialog can't be dismissed
  programmatically; the flow just resumes on reopen, and "I'll map it later" is
  always the way out.

## Theme

**BUILT** (`src/theme.rs`). The palette is **not** hard-coded - it comes from
`GET /api/branding`, fetched once in `main` (right after device config) with a
copy baked into the binary (`assets/branding.json`) as the offline fallback.
`App` holds the `Branding` and, every frame like the zoom-factor sync, re-runs
`theme::resolve` → `theme::apply` if the outcome changed.

`resolve(pref, account_theme, system, branding)` picks light vs dark:

- `Theme::Light` / `Theme::Dark` - explicit, wins.
- `Theme::Auto` - `ctx.system_theme()`, else `branding.colors.default_scheme`.
- `Theme::Account` - the `/api/me` `theme` bool (**`true` = light, `false` =
  dark** per 2032's `infra.md`; `null` = follow system). Carried on
  `SessionStatus::Valid(Me)`. A fresh device login bounces back through
  `Validating` once so this is populated without an extra request; until then it
  falls back to follow-system.

<!-- TODO (backend + client): the per-user Durable Object behind `/api/events` is
     per account, so it can push theme changes down the same socket. Plan: extend
     the WS message set with `{type:"theme",value:<bool|null>}`; `EventStream`
     parses it into a new `StreamEvent::Theme`, `SyncStream` forwards a
     `SyncEvent::AccountTheme(Option<bool>)`, and `App` updates `account_theme`
     live - replacing today's fetch-once-per-launch behaviour and the "reconcile
     on mismatch" note. Until then a website theme change only lands on the next
     coincell launch / revalidation. -->

`apply` maps the chosen `Brand*` palette (`bg`, `bg_elevated`, `text`,
`text_muted`, `border`, `accent`, `accent_hover`, `danger`, `on_accent`,
`focus_ring` - all `#rrggbb`, parsed with `Color32::from_hex`, each with a sane
fallback) onto `egui::Visuals` (panel/window fill, text + weak-text override,
hyperlink = accent, error = danger, the five `widgets` states, selection) and
calls `ctx.set_theme` + `ctx.set_visuals_of`.

Identity fields are wired into the UI too: the sign-in screen shows `APP_NAME` +
`tagline` and an `attribution_text` link; Config › Account links
`homepage_url/settings` and `docs_url` and shows the attribution; Home shows the
attribution line.

Typography is intentionally **not** applied: `font_source_url` is `null` (nothing
to download) and egui can't resolve a CSS family by name - the bundled font
stands in. `assets` is `[]` and unused. Wire either up here if the service starts
serving them.

Example `/api/branding` payload (pretty-printed, matches `assets/branding.json`):

```json
{
  "schema_version": 1,
  "updated_at": "2026-08-28",
  "identity": {
    "name": "2032",
    "short_name": "2032",
    "tagline": "Retro game save sync",
    "homepage_url": "https://2032.cloud",
    "docs_url": "https://cr.2032.cloud/docs",
    "attribution_text": "Saves synced with 2032"
  },
  "colors": {
    "default_scheme": "dark",
    "light": {
      "bg": "#f0e0d6",
      "bg_elevated": "#f8efe8",
      "text": "#800000",
      "text_muted": "#9c5c50",
      "border": "#d8c3b2",
      "accent": "#1c71d8",
      "accent_hover": "#155cb3",
      "danger": "#b32828",
      "on_accent": "#ffffff",
      "focus_ring": "#1c71d8"
    },
    "dark": {
      "bg": "#282a2e",
      "bg_elevated": "#31343a",
      "text": "#c5c8c6",
      "text_muted": "#8b8f93",
      "border": "#3c4046",
      "accent": "#1c71d8",
      "accent_hover": "#3d8ae5",
      "danger": "#e06c75",
      "on_accent": "#ffffff",
      "focus_ring": "#1c71d8"
    }
  },
  "typography": {
    "font_family": "Inter, system-ui, Avenir, Helvetica, Arial, sans-serif",
    "font_source_url": null,
    "weights": [400, 500, 700]
  },
  "assets": [],
  "usage": {
    "guidelines_url": null,
    "notes": "Use the 2032 name and logo to link back to 2032. Don't recolor the logo or imply endorsement."
  }
}
```

## Open questions

None blocking. In rough build order:

1. **Sync engine** (`src/sync/`) - BUILT: both directions, the debounced
   filesystem watcher, the offline upload queue, conflict resolution,
   `Unauthorized` handling. Left: save-history restore, and a finer pause than
   "drop the whole engine" (`pause_on_metered` / metered-connection awareness).
2. **Home window** (`src/app/home.rs`) - BUILT: the game list + local fuzzy
   search + mapped/unmapped split + box art, the add-game flow, and the instance
   detail page (map an existing save, pause, unmap, sync-now, conflict picker).
   Left: save history + restore on the detail page.
3. `build.rs` versioning (Updates panel shows raw `CARGO_PKG_VERSION` today) -
   also feeds Sentry `release` / `environment` and the API `User-Agent`.
4. Multilingual game titles: names come pre-resolved from the server (per the
   account's region); the client just bundles `NotoSansCJKjp-Regular.otf` so
   non-Latin ones render. Done bar the license file.
5. OS autostart registration; `[sync].upload_trigger` / `pause_on_metered`
   behaviour. (Theme wiring is done - see Theme. Tray interaction is done, click
   routing, see that section.)
6. Launcher / process-watch model for "pull right before you play".
7. Auto-updater + signing key - last; key generated just before first release.
