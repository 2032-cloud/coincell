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
resolution, save-history restore), and the **Home window** (`src/app/home.rs` -
the game list, local search, the add-game flow, the instance detail page, and the
per-game save-history screen), **build-time versioning** (`build.rs` +
`src/version.rs`), and **CI + release workflows** (`.github/workflows/`, signed
GitHub Releases on `v*` tags), **self-install** (`src/install.rs`), the
**self-updater** (`src/update.rs`, manual + automatic checks), the **emulator
watch** (`src/emulator_watch.rs`), and the **OS-toast notification sink**
(`src/toast.rs`, `notify-rust`). Not started: the tray popup menu (decided
against - click routing instead).

## What coincell is

- Standalone Rust daemon + tray app. The first real client of 2032's
  `cr.2032.cloud` device-auth API.
- Watches user-configured local save files (per game), pushes changes up, pulls
  newer versions down.
- Lives in the tray. Two frameless windows, bottom-right: **Home** (left-click,
  the per-game list) and **Config** (right-click). Hide via the header's **—**
  button, `Esc`, or the tray click; opt-in auto-hide on focus loss
  (`[window].hide_on_focus_loss`, default off).
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
  direction). Message: `{"type":"save","instance_id":…}`, ideally with an inlined
  `"save":{id,content_hash,size_bytes,uploaded_at,starred,note}` so the client
  acts without a follow-up poll. The DO relays to every socket including the
  uploader's (harmless echo → `MarkSynced`), so a client can also use it to
  confirm its own upload made it through.
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
  `/api/events`. Parses `{"type":"save","instance_id":…,"save":{…}?}` into
  `StreamEvent::{Connected, SaveChanged { instance_id, save: Option<SaveMeta> },
  Disconnected}` (`instance_id` also accepts the legacy `instanceId` alias),
  self-reconnects with backoff, 500 ms socket read-timeout to stay responsive to
  `stop()` and flush keepalive pongs.
- `sync` - `SyncStream`: the stateful orchestration. Opens the `EventStream`
  **before** the first poll (no gap), does a full hydrate (emits `Synced`), then:
  a `SaveChanged` **with** an inlined `save` object → emit `Changed { instance_id,
  latest }` directly and bump the cursor by `save.uploaded_at`, **no HTTP**; a
  `SaveChanged` **without** it (older server), a `poll_now()`, a fallback tick, or
  a reconnect → re-poll `?since=cursor` and emit a `Changed` per instance,
  advancing the cursor by max `last_saved_at`, with a targeted `saves()` fetch
  covering the same-second boundary. Also emits `Connected` / `Disconnected` / `Error` /
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
skip_install_prompt = false   # set by "Not now" on the first-run install prompt
onboarded = false             # first-run flow done (incl. the tray explainer);
                              # until true the window starts visible + never auto-hides

[sync]
enabled = true                # global pause switch
poll = "auto"                 # "auto" | "30s" | "5m" | "off"
upload_trigger = "on-change"  # "on-change" (debounced) | "on-emulator-exit" | "manual"
conflict = "ask"              # "ask" | "prefer-local" | "prefer-remote" | "prefer-newest"
pause_on_metered = true       # Windows metered-connection awareness
watch_emulators = true        # nudge sync when a watched emulator starts / exits
emulators = ["retroarch", …] # executable basenames (no ext); a broad default set

[notifications]
enabled = true
on_pull = true
on_conflict = true
on_error = true
on_session_expired = true

[appearance]
theme = "account"             # "account" (follow /api/me) | "auto" | "light" | "dark"

[window]
hide_on_focus_loss = false    # opt-in;, button + Esc + tray click always work
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
area, not a text sidebar. Eight rail sections. There is always exactly one
section selected; `reset()`
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
3. **Startup** - an **install / uninstall** block (see Install), launch on
   login, start hidden, and (a stray `[window]` toggle that fits here better
   than its own section) **hide the window when it loses focus** =
   `window.hide_on_focus_loss`. `start_hidden` **is** honoured: `App` starts in
   `WindowState::Hidden` and `App::reconcile_visibility` (see Window behavior)
   sends `ViewportCommand::Visible(false)` on the first frames, eframe ignores
   `ViewportBuilder::with_visible` and force-shows once after the first paint, so
   re-asserting is what actually keeps it hidden. The launch-on-login checkbox
   now calls `install::set_autostart` (HKCU Run value / `~/.config/autostart`
   `.desktop`); it only bites once CoinCell is installed.
4. **Notifications** - master toggle + the four per-event toggles (disabled while
   the master is off). These gate `notice::post` (see Notifications); delivery
   itself is still log-only.
5. **Appearance** - theme (follow account / follow system / light / dark;
   applied live via `src/theme.rs`, see Theme) and **UI scale**
   (`window.ui_scale`, a preset % combo). Scale is applied live too: `App::logic`
   pushes it to `ctx.set_zoom_factor` whenever it drifts from the stored value.
6. **Updates** - current version (`version::VERSION`, resolved from git at build
   time - see Versioning; a "development build" note shows when
   `!version::is_release()`), channel, check-automatically, on-update action, a
   **Check for updates** button + status line, a "last checked N ago" line, and a
   **Download & install** / **Install now** button once a newer release is found
   or pre-staged. Drives `App::updater` (see Updater). Automatic background
   checks on `[updates].check_interval` run too; `[updates].on_update` = `notify`
   posts a `Notice`, `download` pre-fetches + verifies the binary then posts the
   `Notice`, `install` auto-applies.
7. **Save backups** - every pre-overwrite local snapshot the engine has kept
   (`Store::backups()`), newest first, labelled by game (name resolved from the
   catalog, else the raw instance id + original path for a deleted game). This is
   the catch-all and the only place backups for an unmapped / deleted game are
   reachable; per-game history with the server saves alongside lives on the
   game's page in Home. Each row: size, relative age, `reason` gloss, **Open
   folder**, **Delete** (inline confirm), and **Restore** - enabled only when the
   instance is currently mapped (`ConfigOutcome::RestoreBackup` →
   `SyncEngine::restore`), disabled with a hint otherwise. Delete drops the index
   row and, if nothing else references the content-addressed blob
   (`Store::delete_backup` returns the now-orphaned hash), its file. Rail icon is
   `icons::RESTORE` (Phosphor clock-counter-clockwise).
8. **Advanced** - log level (both it and the crash-reports toggle note "applies
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
  `mapping::pick_save_file` on its own thread (see Window behavior, no freeze, no
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

### Save history (`Mode::History`) **[BUILT]**

Reached by a **Save history…** button on a *mapped* instance's detail page.
`HistoryState` holds the `instance_id`, a `Task<Vec<SaveMeta>>` for
`client.saves(id)` (newest first, no client sort - the server orders it), the
instance's `Store::backups_for(id)` rows, and an inline `Confirm`. One vertical
`ScrollArea`, two headed sections:

- **On the server** - one `history_row` per `SaveMeta`: a leading star indicator
  (`★` gold / `☆` muted - plain chars via font fallback, not a Phosphor glyph;
  `cr.` has no star endpoint so it's display-only), `size · humanized age`
  (`sync::humanize_since`, raw timestamp on hover), the `note` if any, and a
  trailing **Restore** (inline "Confirm restore" / "Cancel"). The row whose hash
  matches the catalog's `latest_save` shows "current", no button.
- **Local backups** - one `history_row` per `SaveBackup`: the star slot instead
  holds **Delete** (inline "Delete" / "Keep" confirm; `Store::delete_backup`
  drops the row and, when it returns the now-orphaned content hash, the blob
  file), size + age, a `reason` gloss, trailing **Restore**.

Restore of either kind bubbles `HomeOutcome::Restore { instance_id, source }`
(`sync::RestoreSource::{Server{save_id}, Backup{content_hash}}`) →
`SyncEngine::restore`. When the engine emits `EngineEvent::Restored`, `App` calls
`HomeApp::note_restored`, which acknowledges it in-screen and nulls the `saves`
task so the list re-fetches. Row actions are collected out of the two layout
closures through a `Cell<Option<HistAction>>` and applied after the scroll area.

### Not built yet

- Nothing here. (Backend-side: `/api/events` instance push, and a `star` call so
  a kept-after-conflict save can be pinned - see the TODOs below.)

<!-- TODO (backend): `/api/events` only pushes `{type:"save",instanceId}`. Add a
     `{type:"instance",…}` (created / renamed / deleted) push so Home's catalog
     stays live without the manual refresh button. Client side:
     `EventStream::StreamEvent::InstanceChanged` → `SyncStream` re-hydrates or
     emits a targeted update → `App` patches the catalog. Pairs with the
     theme-push TODO under Theme - same per-account Durable Object. -->

## Notifications

**BUILT** (`src/notice.rs` + `src/toast.rs`). A process-wide queue any thread
posts to; `App::logic` calls `notice::pump()` once a frame to drain it to a
`Sink`.

- `Notice::{Pulled, Conflict, Error, SessionExpired, UpdateReady, Test}`, one per
  `[notifications]` toggle (`UpdateReady` / `Test` ride the master switch alone).
  `post()` reads `[notifications]` and drops a notice whose master or per-kind
  flag is off, then dedupes by `dedup_key` (e.g. `conflict:<game>`): the same
  notice inside a 10s window is dropped, so a burst of pulls or a flapping
  conflict is one line, not ten. `Queue::admit` is the pure (config + `now` in,
  bool out) core, unit-tested.
- **Delivery: `notify-rust`.** `toast::install()` (from `main`, once) swaps the
  default `LogSink` for `ToastSink`. It builds one `notify_rust::Notification`
  per notice on a dedicated `toast` worker thread - `pump` runs on the UI thread
  and the `Sink` contract forbids blocking it; both notify-rust backends
  (zbus/D-Bus on Linux, WinRT on Windows) can stall briefly. One crate covers
  both targets; the Linux path is pure-Rust zbus (no `libdbus`).
  - **Windows branding**: toasts carry `.app_id(constants::APP_USER_MODEL_ID)`
    (`com.p51.CoinCell`). `install::ensure_app_id()` writes the matching
    `HKCU\Software\Classes\AppUserModelId\com.p51.CoinCell` class key
    (`DisplayName`, `IconUri` → `icon.png` beside the exe) - the unpackaged-app
    route, no shortcut `IPropertyStore` surgery. Called from `install::register()`
    on a real install *and* from `main` at startup so loose runs are branded too;
    `unregister()` drops the key + icon.
  - **Linux**: `.icon("coincell")`, the themed name `install` drops under
    `hicolor`.
- Wired posts: `EngineEvent::Pulled` and `Conflict` (from `App::drain_sync`,
  resolved to a game name via the catalog by `App::game_label`), `SessionExpired`
  (from `handle_session_expired`), `UpdateReady` (from `drain_updater`). A
  **Send a test notification** button in Config › Notifications calls
  `notice::send_test()`, which hands `Notice::Test` straight to the sink past the
  gate + dedup. `EngineEvent::Error` is still unwired pending a call on which
  sync errors deserve a toast (`on_error` is already in config + the UI).

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

`src/tray/` splits by platform: `ui_thread.rs` (Windows/macOS, icon built on the
event-loop thread) and `gtk.rs` (Linux/BSD, a dedicated thread running a GTK
main loop - needs the `gtk` crate, a Linux-target dependency).

## Sync engine

**STARTED** (`src/sync/`). The consumer of `api::SyncStream` (BUILT): it turns
`SyncEvent`s into disk writes, uploads, and conflict markers, reading and writing
bookkeeping through the `data.sqlite` store (BUILT, `src/store.rs`).

### Shape

`SyncEngine` wraps a `SyncStream` **and** a `notify-debouncer-full` watcher. One
worker thread owns both, drains the `SyncEvent`s and the debounced filesystem
events, and does all the I/O. It talks to `App` over two channels: an
`EngineEvent` mpsc out (`Hydrated { instances }` / `SaveAdvanced` for Home's
catalog, plus `Status` / `Pulled` / `Pushed` / `Restored` / `PushPending` /
`Conflict` / `Error` / `SessionExpired`), a `Control` mpsc in (`SyncNow`, which
polls **and** force-pushes + drains the queue; `Rehydrate` for Home's refresh
button; `Recheck { instance_id }` after Home binds / pauses / unmaps;
`ResolveConflict { instance_id, keep_local }` from the detail page;
`Restore { instance_id, source }` from the history screen / Config › Save
backups). `App` holds `Option<SyncEngine>` in
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
  civil-time, no calendar crate) for the server's `"YYYY-MM-DD HH:MM:SS"` format,
  plus `humanize_since` ("5 minutes ago", months = 30 d, years = 365 d) for the
  history UI.
- `engine` (`mod.rs`) **[BUILT]** - the worker: hydrate completeness pass,
  per-event handling, the filesystem watcher, both sync directions, the offline
  upload queue, conflict resolution (`resolve_conflict`), and restore
  (`restore`, `Control::Restore { instance_id, source: RestoreSource }`, both
  driven by Home). `restore`: fetch the bytes (a server save via
  `download_save`, or a local backup blob via `read_backup_blob`, hash-checked
  either way) → `guard_overwrite(reason="restore")` snapshots the current file →
  `write_atomic` → **re-upload** as the newest save (its own upload path, not
  `push`, so `manual` trigger and the dedupe short-circuits don't apply; on
  failure the disk write stands and the upload is queued). Re-uploading is what
  makes a restore stick - otherwise the next `reconcile` sees `local != remote`,
  `synced == local` and pulls the newer save straight back over it.
  `EngineEvent::Restored { instance_id }` on success (logged; `HomeApp` also
  refreshes its history screen).

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
`server_save_id` (now `Option<&str>` - `None` for a backup-blob restore, which
has no server save id), `reason` = `"pull"` / `"conflict"` / `"restore"`,
`overwritten_at`). It returns `Deferred` if the file is locked (retry next round)
and `Aborted` (with an `Error`) if the snapshot can't be written - the pull is
skipped rather than lose bytes. `last_synced_hash` is **not** consulted: the
map-time "use the server's copy" path seeds it to the local hash for bytes that
were never sent. `save_backups` has **no FK / cascade** to `instances` - a backup
outlives an unmap. Restore of a snapshot is via the history UI / Config › Save
backups (see those); `Store::delete_backup` there prunes a row and its blob when
nothing else references it. Still not built: automatic pruning (the dir grows
unbounded without a manual delete; saves are tiny).

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
  `disk::is_locked`, Windows sharing violation / POSIX advisory lock), hash,
  `client.upload_save` → `record_uploaded` + `record_synced(hash, new save id)`
  (disk and server agree now; also clears any conflict) + a **synthetic
  `SaveMeta`** in the cache with a "now" `uploaded_at` until the stream's
  catch-up delivers the real row. Skips if `last_uploaded_hash` already matches
  or the item is already queued. `[sync].upload_trigger = manual` → no upload,
  emit `PushPending` (a forced "Sync now" overrides). `on-emulator-exit` behaves
  as `on-change` plus the emulator watch below (which force-pushes on exit).

### Emulator watch **[BUILT]**

`src/emulator_watch.rs` - a thread `SyncEngine::start` spawns alongside the
worker (shares its `stop` `Arc`). Every 4 s it `sysinfo`-scans the process list
for `[sync].emulators` basenames (name + exe file-stem, lowercased, extension
stripped) and, when that running set **changes**, sends `Control::SyncNow` -
which force-pulls (catch another device's save before the emulator loads a stale
one) and force-pushes (so `on-emulator-exit` is real). The set is seeded from
what's already running at startup, so an emulator open when CoinCell launches
doesn't fire a spurious "started" sync but its exit still pushes. No ROM / launch
command handling - which save belongs to which game already comes from the
path↔instance mapping. Gated by `[sync].enabled && [sync].watch_emulators`;
editable list in Config › Sync. New dep: `sysinfo` (`system` feature only).
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

**BUILT** (`build.rs` + `src/version.rs`). One version string, resolved from git
at build time, used everywhere: Config › Updates, copy-diagnostics, the Sentry
`release` + `environment`, and the device-API `User-Agent` (`constants::USER_AGENT`
→ `api::set_user_agent` in `main`, applied by `api::client::http_client()` on
every request the module makes).

`build.rs` shells out to `git` and emits three `cargo:rustc-env` vars that
`version.rs` re-exports as `pub const`s (`VERSION`, `CHANNEL`, `COMMIT`), plus
`version::is_release()`:

- **Release build** - HEAD sits exactly on a clean `vX.Y.Z` tag
  (`git describe --tags --exact-match --match 'v[0-9]*'`, and
  `git status --porcelain` empty). `VERSION` = the tag minus its `v`; `CHANNEL` =
  `prerelease` if the semver has a pre-release segment (`v0.2.0-rc.1`), else
  `stable`. The only kind of build ever distributed.
- **Development build** - anything else. `VERSION` =
  `{CARGO_PKG_VERSION}+dev.{short_hash}` (`-dirty` suffix when the tree isn't
  clean), `CHANNEL` = `development`. No git at all (a source tarball) →
  `VERSION` = bare `CARGO_PKG_VERSION`, `COMMIT` = `unknown`, still
  `development`.
- The updater treats a `development` build as "never has an update to offer" -
  it can still check and _show_ the latest release, but auto-install stays off
  (`version::is_release()` gates it). Config › Updates says as much under the
  version line. Only `stable` / `prerelease` builds self-update.
- `rerun-if-changed` on `build.rs`, `.git/HEAD`, `.git/index` - unstaged edits
  won't refresh the `-dirty` marker, which is fine (dev builds aren't shipped).

## Build & CI

**BUILT** - `.github/workflows/ci.yml` + `release.yml`.

Targets (two, x86-64 only for now):

| Triple | Runner | Notes |
| --- | --- | --- |
| `x86_64-pc-windows-msvc` | `windows-latest` | The default Windows triple; best egui / tray-icon / rfd support. `windows_subsystem = "windows"` (already set for non-debug) hides the console. |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | glibc 2.35 floor - lower than `ubuntu-latest`. Dynamically linked (a static-musl GUI build is fragile: winit still `dlopen`s X11/Wayland/GL). |

Linux needs `libgtk-3-dev libxdo-dev libayatana-appindicator3-dev
libxkbcommon-dev libwayland-dev` at build time (rfd + tray-icon + winit).
Bundled `rusqlite` compiles SQLite from source - needs a C compiler, no system
lib.

- **`ci.yml`** - `push` to `main` + all PRs. Matrix `{ubuntu-latest,
  windows-latest}`: `cargo fmt --check` (Linux only), `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo test`. These compile as `development`
  builds; nothing is published.
- **`release.yml`** - on a `v*` tag. A `test` job (which first asserts
  `Cargo.toml`'s version equals the tag, minus any pre-release suffix - the
  binary itself is stamped from the tag by `build.rs`, but the two are kept in
  lockstep for tarball builds / tooling / sanity) gates a `build` matrix over
  the two triples: `cargo build --release`, package (Windows `.zip`, Linux
  `.tar.gz`), **minisign-sign** each archive (see Updater), emit a `.sha256`,
  and attach archive + `.minisig` + `.sha256` to the GitHub Release via
  `softprops/action-gh-release`. `prerelease: true` when the tag name contains a
  `-` (so `v0.2.0-rc.1` → GitHub pre-release, and `build.rs` → `prerelease`
  channel).
- **Nightlies** - not a third channel, not per-push. If ever wanted, a scheduled
  job cuts a `vX.Y.Z-nightly.<date>` pre-release when `main` moved; it rides the
  existing `prerelease` channel and stays monotonic.
- **Cutting a release** - `cargo release <patch|minor|major|rc> --execute`
  (config in `release.toml`): bumps `Cargo.toml` + `Cargo.lock`, commits, tags
  `vX.Y.Z`, pushes. The tag triggers `release.yml`. Never touch the GitHub
  Releases UI or hand-edit the version. `release.toml` sets `publish = false` -
  this is not a crates.io crate.

**Future targets** (add as needs arise, roughly this order):

- `cargo-zigbuild` to pin an older glibc (`x86_64-unknown-linux-gnu.2.31`)
  without an old-distro container, and to cross-compile the rest from one Linux
  runner.
- macOS (`aarch64-apple-darwin` + `x86_64-apple-darwin`, or a universal binary) -
  well supported by the stack, but needs codesigning + notarization to avoid
  Gatekeeper friction, so it pairs with the signed-updater work.
- `aarch64-unknown-linux-gnu` (ARM SBCs / servers), then
  `aarch64-pc-windows-msvc` (Windows on ARM).

## Install

**BUILT** (`src/install.rs`). Self-install as a *mode of the one binary*, not a
separate installer exe. Puts the binary somewhere stable (so `self-replace` can
swap it) and registers it per-user, no admin.

- `canonical_exe()` - Windows `%LOCALAPPDATA%\Programs\CoinCell\coincell.exe`,
  Linux `~/.local/bin/coincell`. `running_installed()` / `is_installed()` gate
  the UI.
- `install()` - copy self there (renaming a busy target to `<exe>.old` first -
  the same trick the updater uses; `cleanup_stale()` in `main` sweeps that
  leftover next launch), then `register()`: Windows writes an Add/Remove
  Programs key (`HKCU\...\Uninstall\CoinCell`, `UninstallString` = `"<exe>"
  --uninstall`), a **Start Menu `.lnk`** (`%APPDATA%\...\Start Menu\Programs\
  CoinCell.lnk`, via `mslnk` - so Start-menu search finds it), and per
  `[startup].launch_on_login` the `HKCU\...\Run` value; Linux writes a menu
  `.desktop` under `~/.local/share/applications`, the icon under
  `hicolor/128x128`, and (when enabled) an `~/.config/autostart` entry.
- **Install acts as an update**: on success (first-run prompt *or* the Config
  buttons) `App` gets `ConfigOutcome::RelaunchFrom(path)` / the modal calls
  `relaunch_from` - spawn the installed exe with `--relaunched-after-update`,
  drop the engine, close. Config › Startup also offers "Update the installed
  copy from this build" when you're running a loose copy alongside an install.
- **First-run prompt** - on the first `Ready` from a loose *release* build with
  nothing installed and `!skip_install_prompt`, a one-time `egui::Modal`
  ("Install CoinCell?", **Install** / **Not now**). "Not now" (or dismiss) sets
  `[startup].skip_install_prompt`; the Config button stays. Shown before the
  crash-reports prompt; declining chains to it.
- `uninstall(purge)` - undo all of it (ARP key, `.lnk`, Run value, Linux
  `.desktop`s + icon); `purge` also deletes config / db / logs / cache. The
  binary goes last (`self_replace::self_delete()` schedules it on Windows;
  `remove_file` on Unix). Reachable from the ARP entry (`coincell --uninstall
  [--purge]`, dispatched in `main` before the single-instance check) and a
  confirm button in Config › Startup.
- `set_autostart(bool)` - the standalone toggle the launch-on-login checkbox
  calls.
- **Windows `.exe` icon**: `build.rs` embeds `assets/coincell.ico` (generated
  from `icon_128_128.png`) via `winresource` when `CARGO_CFG_TARGET_OS ==
  windows`, so Explorer / taskbar / Alt-Tab / the `.lnk` show the real icon.
  The running window's icon is set separately via `ViewportBuilder::with_icon`.
- **AppUserModelID**: `register()` writes an `HKCU\...\AppUserModelId\`
  class key (`com.p51.CoinCell`) so Windows toasts render as "CoinCell" with our
  icon - see Notifications. No shortcut `IPropertyStore` surgery.

## Updater

**BUILT** (`src/update.rs` + `src/app` wiring). Manual **Check for updates** in
Config › Updates *and* automatic background checks:

- `App::tick_update_check` (every `logic()` tick, release builds only) fires an
  auto-check when `[updates].auto_check` and `Instant::now()` is past
  `App::next_update_check`. That instant is set by `schedule_update_check` from
  the persisted `store` `meta` key `last_update_check` + `[updates].check_interval`
  (a never-run / overdue check is scheduled ~45 s out, not instantly). Skipped
  while `updater.busy()`, an offer is surfaced, or an update is `Staged`.
- `drain_updater`, when a check finishes: persists `now` to `last_update_check`,
  re-arms the timer, and if it was an auto-check that found something, applies
  `[updates].on_update` -
  - `notify` → `notice::post(Notice::UpdateReady { version })`, leave it in
    `Updater::Checked(Some)`;
  - `download` → `start_update_stage` (background `update::stage`: download +
    verify + write the binary beside the installed exe, no swap) → `Updater::
    Staged`; the `Notice::UpdateReady` is held until the bytes are actually on
    disk (`App::notify_when_staged`);
  - `install` → `start_update_install` straight away.
- **Staged updates survive a restart.** `update::staged()` reads the
  `.coincell.staged.meta` marker beside the exe and returns a `StagedUpdate`
  when the binary is present and newer than the running build (self-cleaning
  otherwise). `App::adopt_staged_update` (on `Next::Ready`) picks it back up into
  `Updater::Staged` - or commits it immediately when `on_update = install`.
  `install::uninstall` calls `update::discard_staged`.
- `start_update_install` is instant when the update is already `Staged`
  (`update::commit` = `self_replace` + relaunch, no re-download); otherwise it's
  a full `update::apply` (= `stage` then `commit`). Config › Updates shows
  **Install now** vs **Download & install** accordingly.
- `start_update_check(auto)` carries the auto/manual distinction via
  `App::auto_check_pending`. `Notice::UpdateReady` rides the master
  `[notifications].enabled` only (no per-kind flag - `on_update = notify` is the
  opt-in).

**Signing (minisign).** Detached `.minisig` per release archive, verified in-app
with the `minisign-verify` crate (pure Rust, no libsodium). Chosen over cosign:
tiny, offline, a short public key that bakes in as a `const`.

- **Key**: an Ed25519 minisign keypair, generated once with `rsign2`
  (`cargo install rsign2`; `rsign generate -W` for an unencrypted secret key -
  fine because the secret only ever lives in a GitHub Actions secret and offline,
  never on a build runner's disk beyond the signing step).
- The **public** key is committed at `assets/minisign.pub` and baked into the
  binary (`include_str!`, like `branding.json`). The **secret** key is the repo
  secret `MINISIGN_SECRET_KEY` (whole file contents). `.gitignore` blocks
  `minisign.key` / `*.sec`. (If the key is password-protected instead, add
  `MINISIGN_PASSWORD` and uncomment the pipe in `release.yml`.)
- `release.yml` signs each archive after building and uploads the `.minisig`
  alongside it; it also self-verifies against `assets/minisign.pub` when that
  file is present.

**Update flow** (`src/update.rs`):

- Source: GitHub Releases for `version::REPO` (public, unauthenticated).
- `check(allow_prerelease)` - `GET /repos/<owner>/<repo>/releases?per_page=30`,
  drop drafts (and pre-releases unless `[updates].channel = prerelease`),
  semver-compare each `tag_name` (minus `v`) to `version::VERSION`, keep the
  highest that's strictly newer **and** ships an asset matching
  `version::TARGET` (the exact triple, emitted by `build.rs`) plus its
  `.minisig`. Returns `Option<Available>`. A `development` build can still call
  this to *see* the latest; `apply` refuses.
- `stage(&Available)` - refuse unless `version::is_release()` **and**
  `install::running_installed()`. Download archive + `.minisig`, verify the
  signature against the baked-in `assets/minisign.pub` (`minisign-verify`,
  `allow_legacy = true` so either prehashed or legacy rsign2 sigs pass), check
  the `.sha256` if present, unpack the binary (`zip` on Windows, `tar`+`flate2`
  on Unix), write it to `.coincell.staged[.exe]` next to the installed exe (same
  volume; temp file + rename, so a killed download can't leave a half-written
  binary) with a `.coincell.staged.meta` marker (`{tag, version}`). Returns
  `StagedUpdate`.
- `commit(&StagedUpdate)` - same guards, then `self_replace::self_replace`
  (rename running exe aside, move staged into place), `discard_staged`, spawn
  `<installed exe> --relaunched-after-update`, return `Ok`.
- `apply(&Available)` = `stage` then `commit` - the do-it-all-now path
  (`on_update = install`, or the Config button with nothing pre-staged).
- `staged() -> Option<StagedUpdate>` - reads the marker; drops a marker with no
  binary / an unparseable one / one not newer than the running build.
  `discard_staged()` removes both files.
- Handoff: `App` on a `commit` `Ok` drops the sync engine, sets `quitting`, and
  closes the window; the spawned process runs `ipc::acquire_wait(8s)`, retrying
  the single-instance bind until the old process has released it.
- UI: `App::updater` (`Idle` / `Checking` / `Checked(Option<Available>)` /
  `Staging` / `Staged(StagedUpdate)` / `Installing` / `Restarting` / `Error`),
  advanced by `App::drain_updater` each frame off worker-thread channels,
  rendered by Config › Updates.

**New deps**: `self-replace`, `minisign-verify`, `semver`;
`zip` (`cfg(windows)`), `tar` + `flate2` (`cfg(unix)`); `winreg` (`cfg(windows)`,
for `install.rs`).

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
  ERROR becomes a Sentry event, WARN/INFO become breadcrumbs. `release` =
  `version::VERSION`, `environment` = `version::CHANNEL` (see Versioning).
  Toggling crash reports needs a restart.
- **The opt-in gate IS built.** `[advanced].crash_reports` is tri-state:
  **absent** until answered. It's the middle step of the first-run modal chain
  (install → this → tray explainer; see Window behavior): a one-time
  `egui::Modal` ("upload anonymous error reports?") whose answer is written;
  backdrop / `Esc` counts as no. Not re-asked on later sign-ins - a fresh
  `config.toml` (or a future "Reset config") brings it back. Also a plain
  checkbox in Config > Advanced. Reports would ship stack traces and local paths,
  hence opt-in.

## Window behavior

**BUILT** (`app/mod.rs`).

- egui's own popups (combo boxes, the first-run modals, in-window confirms) keep
  focus inside the window, so auto-hide was never a real threat to the settings
  UI. A native window we don't own **would** hand focus away, so auto-hide and
  the Esc-minimise are gated by `modal_active` = `busy_auth` (the device flow /
  session check), `pending_pick.is_some()` (the save-file picker, below), or any
  of the three first-run modals (`ask_install` / `ask_crash_reports` /
  `ask_tray_intro`).
- **First-run flow** (all one-time, after the first `Ready`, in order): the
  install prompt (see Install), the usage-data prompt (see Logging), then a
  **"CoinCell runs in the tray"** explainer. `App::advance_first_run_prompts`
  sets the next pending flag; each modal's resolve calls it again. Until
  `[startup].onboarded` (set by the explainer's "Got it"), the window **starts
  visible regardless of `start_hidden`** (`main` ANDs the two) and **never
  auto-hides** (the focus-loss check also requires `onboarded`). Existing
  installs hit the explainer once on the first launch after upgrading.
- The **save-file picker runs off the UI thread**: Home returns
  `HomeOutcome::OpenSaveDialog`, `App` spawns a thread that calls
  `mapping::pick_save_file` (RFD inits COM per call, so any thread is fine),
  parks the `Receiver` in `App.pending_pick`, and hands the `PathBuf` back via
  `HomeApp::deliver_save_pick` once it lands. So the window
  neither freezes nor minimises while the dialog is up. On resolve `App` also
  **re-arms `focus_latch`**, the dialog held focus and the OS can take a frame
  or two returning it, which would otherwise read as a focus-loss auto-hide.
- **Visibility is state-driven.** `hide()` / `show_home()` / `show_config()` only
  set `WindowState`; `App::reconcile_visibility` (end of every `logic()`) sends
  `ViewportCommand::Visible(!hidden)` when it drifts from the last one sent. For
  the first 3 frames it re-asserts unconditionally (and forces repaints) because
  eframe force-shows the window once after the first paint (anti-flash hack) and
  ignores `ViewportBuilder::with_visible`, re-asserting is the only reliable way
  to honour `start_hidden`. The hidden branch of `ui()` still paints an empty
  `CentralPanel` so a briefly-shown window is themed-blank, never black.
- `App` starts in `WindowState::Hidden` when `[startup].start_hidden`
  **and** `[startup].onboarded`, else `ShowConfig`.
- The shared title bar (`App::header`, a `Panel::top` above every screen) carries
  the screen name and the **—** minimise button. The button always works.
- **Esc** also minimises - consumed at end-of-frame via `input_mut().consume_key`
  so an open combo / the first-run modal claims it first, and skipped while
  `modal_active`.
- `[window].hide_on_focus_loss` (default `false`) is opt-in and gates only the
  _auto_-hide; it's fiddly around transient focus changes so it's off by default
  now that **—**, `Esc`, and the tray click are all solid manual hides. When off
  (the default), the window leaves only via those, `Quit`, or the tray toggle.
- Reopening the tray lands on a clean screen: `App::sync_shown_screen` calls
  `ConfigApp::reset()` / `HomeApp::on_reopen()` on every visible-state change.
  The one exception: `HomeApp::on_reopen()` **keeps** an unfinished `MapPrompt`
  (an instance was created server-side and still needs a mapping decision), so
  minimising / switching away mid add-game, including with the off-thread file
  picker still open, doesn't strand it. A native dialog can't be dismissed
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
   `Unauthorized` handling, save-history restore. Left: a finer pause than
   "drop the whole engine" (`pause_on_metered` / metered-connection awareness).
2. **Home window** (`src/app/home.rs`) - BUILT: the game list + local fuzzy
   search + mapped/unmapped split + box art, the add-game flow, the instance
   detail page (map an existing save, pause, unmap, sync-now, conflict picker),
   and the per-game save-history screen (server saves + local backups, restore /
   delete). Nothing left.
3. `build.rs` versioning - **DONE**. Wired into Config › Updates,
   copy-diagnostics, Sentry `release` / `environment`, and the device-API
   `User-Agent`.
4. **CI + release workflows** - **DONE** (`.github/workflows/ci.yml` +
   `release.yml`). Lint/test on push+PR; `v*` tags build the two triples, sign
   with minisign, and publish a GitHub Release. See Build & CI.
5. Multilingual game titles - **DONE**. Server-resolved names, `jp` CJK font
   bundled, license in place, unused CJK otfs removed.
6. **Self-install + onboarding** - **DONE** (`src/install.rs` + first-run
   modals). Install / uninstall from Config › Startup and `coincell --uninstall`;
   HKCU Run / `~/.config/autostart`; Windows Start Menu `.lnk`; the HKCU
   AppUserModelID class key for branded toasts; the install → usage-data →
   tray-explainer first-run chain; stay-visible-until-`onboarded`. Nothing left.
7. **Self-updater** - **BUILT** (`src/update.rs` + `src/app`). Manual + automatic
   checks (`check_interval`, persisted `last_update_check`); all three `on_update`
   actions (notify / download-pre-stage / install); `stage` (verify minisign +
   sha256, write beside the exe + marker) → `commit` (`self-replace` → relaunch
   via the `ipc::acquire_wait` handoff); a `Staged` update is re-adopted on the
   next launch. Nothing left.
8. **Emulator watch** - **BUILT** (`src/emulator_watch.rs`). `sysinfo` process
   poll; start/exit of a `[sync].emulators` basename → `Control::SyncNow`. Covers
   `on-emulator-exit` and the "pull right before you play" case without a real
   launcher.
9. **`pause_on_metered`** - Windows metered-connection awareness; still just a
   config flag.
10. **Notification delivery backend** - **BUILT** (`src/toast.rs`). `notify-rust`
    on a worker thread; Windows toasts branded via the HKCU AppUserModelID class
    key; Config › Notifications "Send a test notification" button. Left:
    `EngineEvent::Error` → `Notice::Error` (which errors deserve a toast).
