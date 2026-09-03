mod config;
mod fonts;
mod home;
mod icons;
mod mapping;

use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::api::{self, Branding, Client, DeviceConfig, DeviceEvent, DeviceFlow, DiagFixture, GameInstance, Role, SessionCheck, SessionStatus};
use crate::app::config::{ConfigApp, ConfigOutcome};
use crate::app::home::{HomeApp, HomeOutcome, HomeView};
use crate::app::icons::MINIMISE;
use crate::config::{Config, UpdateAction, UpdateChannel};
use crate::constants::CLIENT_NAME;
use crate::notice::{self, Notice};
use crate::store::Store;
use crate::sync::{EngineEvent, RestoreSource, Status, StuckReason, SyncEngine};
use crate::theme::{self, Scheme};
use crate::update::{self, Available, StagedUpdate};

/// Self-update UI state, driven from Config › Updates.
pub(crate) enum Updater {
    Idle,
    Checking(Receiver<Result<Option<Available>, String>>),
    /// Check finished: `Some` = a newer release, `None` = up to date.
    Checked(Option<Available>),
    /// Downloading + verifying the archive in the background (`on_update =
    /// download`, or a manual pre-download). Nothing swapped yet.
    Staging(Receiver<Result<StagedUpdate, String>>),
    /// A verified binary is on disk next to the installed exe; `commit` is
    /// instant. Survives across sessions - re-adopted on the next launch.
    Staged(StagedUpdate),
    Installing(Receiver<Result<(), String>>),
    /// Swap done; the window is closing and the new process is relaunching.
    Restarting,
    Error(String),
}

impl Updater {
    /// One-line status for the Config panel.
    pub(crate) fn status(&self) -> String {
        match self {
            Updater::Idle => format!("Version {}, not checked this session.", crate::version::VERSION),
            Updater::Checking(_) => "Checking for updates…".into(),
            Updater::Checked(av) => update::describe(av),
            Updater::Staging(_) => "Downloading update…".into(),
            Updater::Staged(s) => format!("Update {} downloaded, ready to install.", s.version),
            Updater::Installing(_) => "Downloading and installing…".into(),
            Updater::Restarting => "Restarting…".into(),
            Updater::Error(e) => format!("Update check failed: {e}"),
        }
    }
    /// The release to offer an install for, if any.
    pub(crate) fn offer(&self) -> Option<&Available> {
        match self {
            Updater::Checked(Some(av)) => Some(av),
            _ => None,
        }
    }
    /// A verified, downloaded update waiting for an instant `commit`.
    pub(crate) fn staged(&self) -> Option<&StagedUpdate> {
        match self {
            Updater::Staged(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn busy(&self) -> bool {
        matches!(self, Updater::Checking(_) | Updater::Staging(_) | Updater::Installing(_) | Updater::Restarting)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Hidden,
    ShowConfig,
    ShowHome,
}

impl WindowState {
    fn hidden(&self) -> bool {
        matches!(self, WindowState::Hidden)
    }
    fn is_home(&self) -> bool {
        matches!(self, WindowState::ShowHome)
    }
    fn is_config(&self) -> bool {
        matches!(self, WindowState::ShowConfig)
    }
}

enum AuthView {
    LoggedOut { error: Option<Arc<str>> },

    Validating { check: SessionCheck },

    Connecting { flow: DeviceFlow, approval: Option<Approval> },

    Ready,
}

struct Approval {
    user_code: Arc<str>,
    verification_uri: Arc<str>,
    verification_uri_complete: Arc<str>,
}

enum LoginIntent {
    None,
    StartFlow,
    ReopenBrowser,
}

/// A native file picker (`rfd`) running on its own thread. While one is live the
/// window won't auto-hide; the chosen path is handed back when it lands.
struct PendingPick {
    instance_id: String,
    kind: PickKind,
    rx: Receiver<Option<std::path::PathBuf>>,
}

enum PickKind {
    /// Pick the local save file to bind - routed to `HomeApp::deliver_save_pick`.
    Save,
    /// Pick the ROM / content file for the launcher. `launch_after` continues
    /// straight into `begin_play` once it's set.
    Content { launch_after: bool },
}

/// Client state for the diagnostics fixture store. Inert unless the account is
/// [`Role::privileged`]: at that point CoinCell pre-fills launcher content paths
/// from the store, and offers to publish a manually-picked file back to it.
#[derive(Default)]
struct Diag {
    /// The store index, fetched once per session; kept current after an upload.
    index: Vec<DiagFixture>,
    /// `true` once the index has been fetched at least once.
    have_index: bool,
    /// A running provision pass (fetch index + download + set content paths).
    pass: Option<Receiver<DiagResult>>,
    /// A running upload of a manually-picked file.
    upload: Option<Receiver<Result<DiagFixture, String>>>,
    /// A pending "publish this file?" modal for an admin who just picked one.
    ask_publish: Option<PublishFixture>,
}

struct DiagResult {
    index: Vec<DiagFixture>,
    /// How many instances got a content path filled in.
    provisioned: usize,
}

struct PublishFixture {
    game_name: String,
    console_slug: String,
    game_slug: String,
    path: std::path::PathBuf,
}

/// A game the user launched through CoinCell (one at a time).
struct Launch {
    instance_id: String,
    phase: LaunchPhase,
}

enum LaunchPhase {
    /// Pre-launch sync in flight; spawn the emulator on `EngineEvent::Rechecked`
    /// for this instance, or when `deadline` passes (offline).
    Checking { deadline: Instant },
    /// Emulator running. `child` is polled with `try_wait` each tick and killed
    /// on a confirmed Stop.
    Running { child: Child },
}

pub struct App {
    state: WindowState,
    prev_state: WindowState,
    focus_latch: bool,
    quitting: bool,
    /// Set once when a session first goes Ready and the crash-report preference
    /// has never been answered; drives the one-time first-run prompt.
    ask_crash_reports: bool,
    /// Set on first Ready when running a loose release build that isn't
    /// installed; drives the one-time "install CoinCell?" prompt.
    ask_install: bool,
    /// Set on first Ready until `[startup].onboarded`; drives the one-time
    /// "CoinCell lives in the tray" explainer (shown after usage data).
    ask_tray_intro: bool,
    wake_rx: Receiver<()>,
    auth: AuthView,
    device_config: DeviceConfig,
    /// Service branding (palette, name, links). Refreshed from `GET /api/branding`
    /// at launch, baked-in copy as the fallback.
    branding: Branding,
    /// The account's theme setting from `GET /api/me` (`Some(true)` = light,
    /// `Some(false)` = dark, `None` = follow system). Outer `None` = not fetched
    /// yet. Drives `Theme::Account`.
    account_theme: Option<Option<bool>>,
    /// Last scheme handed to [`theme::apply`], so we only re-apply on a change.
    applied_scheme: Option<Scheme>,
    /// Live while a session is Ready. Dropping it stops the stream + poller and
    /// the engine worker.
    sync: Option<SyncEngine>,
    /// Every game instance on the account, from the engine's hydrate, what Home
    /// renders. `false` until the first `Hydrated` lands.
    catalog: Vec<GameInstance>,
    catalog_ready: bool,
    /// A save-file picker Home asked for, running off the UI thread. While set,
    /// the window won't auto-hide.
    pending_pick: Option<PendingPick>,
    /// Last `Visible(_)` we told the OS window, so we only re-send on a change.
    visible_cmd: Option<bool>,
    /// Frames left where `reconcile_visibility` re-asserts unconditionally, to
    /// out-last eframe's first-paint `set_visible(true)`.
    visible_settling: u8,
    config_app: ConfigApp,
    home_app: HomeApp,
    updater: Updater,
    /// When the next automatic update check is due (`[updates].auto_check`).
    /// `None` while auto-check is off or a check is in flight.
    next_update_check: Option<Instant>,
    /// The in-flight check was started by the timer, so `drain_updater` should
    /// apply `[updates].on_update` when it resolves.
    auto_check_pending: bool,
    /// The in-flight `Staging` run came from an auto `on_update = download`, so
    /// post `Notice::UpdateReady` once the binary is on disk.
    notify_when_staged: bool,
    /// The sync engine's last reported stream connectivity, for the Config ›
    /// Sync status line. `None` while no engine is running.
    stream_online: Option<Status>,
    /// A game launched through CoinCell's per-console emulator profiles. One at
    /// a time; `None` when nothing is running or being checked.
    launch: Option<Launch>,
    /// Account tier from `GET /api/me`; gates the diagnostics fixture store.
    role: Role,
    diag: Diag,
}

impl App {
    pub fn new(ctx: egui::Context, wake_rx: Receiver<()>, device_config: DeviceConfig, branding: Branding, start_hidden: bool) -> Self {
        fonts::install(&ctx);
        egui_extras::install_image_loaders(&ctx);
        // Persistent disk cache for remote art; must come after the line above so
        // it's tried ahead of egui_extras' network loader.
        crate::asset::install(&ctx);

        // Theme the first frame (the sign-in screen) before anything draws.
        let scheme = theme::resolve(Config::get(|c| c.appearance.theme), None, ctx.system_theme(), &branding);
        theme::apply(&ctx, &branding, scheme);

        let auth = match api_client() {
            Some(client) => AuthView::Validating { check: SessionCheck::start(client, waker(&ctx)) },
            None => AuthView::LoggedOut { error: None },
        };
        // `[startup].start_hidden`: begin in the tray, no window. The tray toggle
        // sends `Visible(true)` from here, so the two stay in step.
        let state = if start_hidden { WindowState::Hidden } else { WindowState::ShowConfig };
        Self {
            state,
            prev_state: state,
            focus_latch: true,
            quitting: false,
            ask_crash_reports: false,
            ask_install: false,
            ask_tray_intro: false,
            wake_rx,
            auth,
            device_config,
            branding,
            account_theme: None,
            applied_scheme: Some(scheme),
            sync: None,
            catalog: Vec::new(),
            catalog_ready: false,
            pending_pick: None,
            visible_cmd: None,
            visible_settling: 0,
            config_app: ConfigApp::new(),
            home_app: HomeApp::new(),
            updater: Updater::Idle,
            next_update_check: None,
            auto_check_pending: false,
            notify_when_staged: false,
            stream_online: None,
            launch: None,
            role: Role::User,
            diag: Diag::default(),
        }
    }

    /// Re-resolve the theme from the current preference / account / OS and apply
    /// it if it changed. Called every frame, like the zoom-factor sync.
    fn sync_theme(&mut self, ctx: &egui::Context) {
        let want = theme::resolve(Config::get(|c| c.appearance.theme), self.account_theme, ctx.system_theme(), &self.branding);
        if self.applied_scheme != Some(want) {
            theme::apply(ctx, &self.branding, want);
            self.applied_scheme = Some(want);
        }
    }

    /// Start the sync engine (which owns the realtime + polling stream), unless
    /// syncing is paused or one is already running.
    fn start_sync(&mut self, ctx: &egui::Context) {
        if self.sync.is_some() || !Config::get(|c| c.sync.enabled) {
            return;
        }
        let Some(client) = api_client() else { return };
        self.sync = Some(SyncEngine::start(client, waker(ctx)));
    }

    /// Drain what the engine has surfaced. Download/write already happened on the
    /// worker; this keeps the catalog current and reacts to session expiry.
    fn drain_sync(&mut self, ctx: &egui::Context) {
        let Some(engine) = &self.sync else {
            self.stream_online = None;
            return;
        };
        let mut session_expired = false;
        let mut launch_ready: Option<String> = None;
        let mut launch_cancel = false;
        let mut provision_after = false;
        let checking = match &self.launch {
            Some(Launch { instance_id, phase: LaunchPhase::Checking { .. } }) => Some(instance_id.clone()),
            _ => None,
        };
        for event in engine.events() {
            match event {
                EngineEvent::Hydrated { instances } => {
                    self.catalog = instances;
                    self.catalog_ready = true;
                    provision_after = true;
                }
                EngineEvent::SaveAdvanced { instance_id, latest } => {
                    if let Some(row) = self.catalog.iter_mut().find(|g| g.id == instance_id) {
                        row.last_saved_at = Some(latest.uploaded_at.clone());
                        row.latest_save = Some(latest);
                    }
                }
                EngineEvent::Status(status) => {
                    tracing::debug!("stream {status:?}");
                    self.stream_online = Some(status);
                }
                EngineEvent::Pulled { instance_id } => {
                    tracing::info!("pulled newer save for {instance_id}");
                    notice::post(Notice::Pulled { game: self.game_label(&instance_id) });
                }
                EngineEvent::Pushed { instance_id } => tracing::info!("pushed save for {instance_id}"),
                EngineEvent::Restored { instance_id } => {
                    tracing::info!("restored a save for {instance_id}");
                    self.home_app.note_restored(&instance_id);
                }
                EngineEvent::PushPending { instance_id } => tracing::debug!("{instance_id}: local change waiting (manual upload)"),
                EngineEvent::Conflict { instance_id } => {
                    tracing::warn!("{instance_id}: conflict, resolve in Home");
                    if checking.as_deref() == Some(instance_id.as_str()) {
                        launch_cancel = true; // don't play into an unresolved conflict
                    }
                    notice::post(Notice::Conflict { game: self.game_label(&instance_id) });
                }
                EngineEvent::Rechecked { instance_id } => {
                    if checking.as_deref() == Some(instance_id.as_str()) {
                        launch_ready = Some(instance_id);
                    }
                }
                // Transient/internal errors stay in the log; only a wedged
                // instance (`Stuck`) is worth interrupting the user for.
                EngineEvent::Error(message) => tracing::warn!("sync: {message}"),
                EngineEvent::Stuck { instance_id, reason } => {
                    let game = self.game_label(&instance_id);
                    let detail = match reason {
                        StuckReason::BackupFailed => {
                            format!("{game}: an update from another device is on hold. CoinCell couldn't back up your local save first - check the file isn't open in an emulator.")
                        }
                        StuckReason::UploadRetrying => format!("{game}: the latest save isn't uploading yet. CoinCell will keep retrying."),
                    };
                    tracing::warn!("sync stuck [{instance_id}]: {detail}");
                    notice::post(Notice::Error { detail });
                }
                EngineEvent::SessionExpired => session_expired = true,
            }
        }
        if session_expired {
            self.handle_session_expired(ctx);
        }
        if launch_cancel {
            self.launch = None;
        } else if let Some(id) = launch_ready {
            self.spawn_emulator(&id);
        }
        if provision_after {
            self.provision_diag(ctx);
        }
    }

    /// A human name for an instance id, for notice text; the id itself if the
    /// catalog doesn't have it (e.g. it was deleted server side).
    fn game_label(&self, instance_id: &str) -> String {
        self.catalog.iter().find(|g| g.id == instance_id).map(|g| g.name.clone()).unwrap_or_else(|| instance_id.to_owned())
    }

    /// Spawn a native picker on its own thread (RFD inits COM per call, so any
    /// thread is fine). The window stops auto-hiding until it resolves.
    fn open_pick(&mut self, ctx: &egui::Context, instance_id: String, kind: PickKind, title: String) {
        if self.pending_pick.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                let _ = tx.send(mapping::pick_save_file(&title));
                ctx.request_repaint();
            })
            .expect("spawn file-dialog thread");
        self.pending_pick = Some(PendingPick { instance_id, kind, rx });
    }

    /// Hand a finished pick to its destination (`None` = cancelled). The focus
    /// latch is re-armed: the native dialog held focus, and the OS can be a
    /// frame or two returning it, which would otherwise read as a focus-loss
    /// auto-hide.
    fn drain_pending_pick(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_pick else { return };
        let Ok(result) = pending.rx.try_recv() else { return };
        let PendingPick { instance_id, kind, .. } = self.pending_pick.take().expect("checked just above");
        self.focus_latch = true;
        match kind {
            PickKind::Save => self.home_app.deliver_save_pick(&instance_id, result),
            PickKind::Content { launch_after } => {
                let Some(path) = result else { return };
                if let Err(e) = Store::write(|s| s.set_content_path(&instance_id, Some(&path))) {
                    self.home_app.note_error(format!("Couldn't save the content path: {e:#}"));
                    return;
                }
                self.home_app.refresh_instances();
                self.maybe_offer_diag_publish(&instance_id, &path);
                if launch_after {
                    self.begin_play(ctx, instance_id);
                }
            }
        }
    }

    /// The user pressed Play. Route to the content picker if no ROM is set yet,
    /// else start the pre-launch sync check.
    fn begin_play(&mut self, ctx: &egui::Context, instance_id: String) {
        if self.launch.is_some() {
            return; // one at a time; the button should already be disabled
        }
        let book = match Store::get(|s| s.instance(&instance_id)) {
            Ok(Some(b)) => b,
            _ => return self.home_app.note_error("That game isn't mapped to a save file.".into()),
        };
        let slug = self.catalog.iter().find(|g| g.id == instance_id).map(|g| g.console_slug.clone()).or(book.console_slug.clone());
        let has_profile = slug.as_deref().is_some_and(|s| Config::get(|c| c.launchers.get(s).is_some_and(|l| l.is_set())));
        if !has_profile {
            return self.home_app.note_error("Set up an emulator for this console in Settings \u{203a} Emulators first.".into());
        }
        if book.content_path.is_none() {
            let name = self.game_label(&instance_id);
            self.open_pick(ctx, instance_id, PickKind::Content { launch_after: true }, format!("ROM / content file for {name}"));
            return;
        }
        if let Some(engine) = &self.sync {
            engine.recheck(&instance_id);
        }
        self.launch = Some(Launch { instance_id, phase: LaunchPhase::Checking { deadline: Instant::now() + Duration::from_secs(3) } });
    }

    /// Build the argv from the console's `[launchers]` profile and spawn the
    /// emulator. Called once the pre-launch check settles (or times out).
    fn spawn_emulator(&mut self, instance_id: &str) {
        let book = match Store::get(|s| s.instance(instance_id)) {
            Ok(Some(b)) => b,
            _ => {
                self.launch = None;
                return;
            }
        };
        let Some(content) = book.content_path.as_ref() else {
            self.launch = None;
            return;
        };
        let slug = self.catalog.iter().find(|g| g.id == instance_id).map(|g| g.console_slug.clone()).or(book.console_slug.clone());
        let Some(profile) = slug.as_deref().and_then(|s| Config::get(|c| c.launchers.get(s).cloned())).filter(|p| p.is_set()) else {
            self.launch = None;
            return self.home_app.note_error("No emulator is configured for this console any more.".into());
        };
        let argv = profile.argv(&content.to_string_lossy());
        tracing::info!("launching {instance_id}: {argv:?}");
        match std::process::Command::new(&argv[0]).args(&argv[1..]).spawn() {
            Ok(child) => self.launch = Some(Launch { instance_id: instance_id.to_owned(), phase: LaunchPhase::Running { child } }),
            Err(e) => {
                self.launch = None;
                self.home_app.note_error(format!("Couldn't start {}: {e}", argv[0]));
            }
        }
    }

    /// Advance the launcher: fire the pre-launch spawn on timeout, and clear +
    /// resync when a launched emulator exits.
    fn tick_launch(&mut self, ctx: &egui::Context) {
        let Some(launch) = &mut self.launch else { return };
        match &mut launch.phase {
            LaunchPhase::Checking { deadline } => {
                if Instant::now() >= *deadline {
                    let id = launch.instance_id.clone();
                    self.spawn_emulator(&id);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
            }
            LaunchPhase::Running { child } => match child.try_wait() {
                Ok(None) => ctx.request_repaint_after(Duration::from_secs(1)),
                _ => {
                    let id = launch.instance_id.clone();
                    tracing::info!("emulator for {id} exited; syncing");
                    self.launch = None;
                    if let Some(engine) = &self.sync {
                        engine.recheck(&id);
                    }
                }
            },
        }
    }

    /// Confirmed Stop from the detail page: hard-kill the emulator, then let the
    /// normal exit path (`tick_launch`) reap it and push the save.
    fn stop_emulator(&mut self) {
        if let Some(Launch { phase: LaunchPhase::Running { child }, .. }) = &mut self.launch {
            let _ = child.kill();
        }
    }

    // ---- diagnostics fixture store ------------------------------------------

    /// After a privileged user picks a content file by hand, and the store has
    /// no fixture for that game yet, queue the "publish it?" modal.
    fn maybe_offer_diag_publish(&mut self, instance_id: &str, path: &std::path::Path) {
        // Wait until we've seen the index, so we can actually tell whether the
        // store already has one.
        if !self.role.privileged() || !self.diag.have_index || self.diag.ask_publish.is_some() {
            return;
        }
        let Some(inst) = self.catalog.iter().find(|g| g.id == instance_id) else { return };
        let Some(game_slug) = inst.game_slug.clone() else { return };
        let have = self.diag.index.iter().any(|f| f.console_slug == inst.console_slug && f.game_slug == game_slug);
        if have {
            return;
        }
        self.diag.ask_publish = Some(PublishFixture { game_name: inst.name.clone(), console_slug: inst.console_slug.clone(), game_slug, path: path.to_path_buf() });
    }

    /// If the account is privileged, run a background pass: fetch the fixture
    /// index and, for every mapped instance with a known game and no content
    /// path, download its fixture (cached, hash-checked) and set it as the
    /// launcher content path. No-op otherwise, or while a pass is already going.
    fn provision_diag(&mut self, ctx: &egui::Context) {
        if !self.role.privileged() || self.diag.pass.is_some() {
            return;
        }
        let Some(client) = api_client() else { return };

        // (instance_id, console_slug, game_slug) for mapped instances missing a
        // content path. Custom instances have no game slug, so no fixture.
        let need: std::collections::HashMap<String, bool> = Store::get(|s| s.instances()).map(|v| v.into_iter().map(|r| (r.game_instance_id, r.content_path.is_some())).collect()).unwrap_or_default();
        let work: Vec<(String, String, String)> =
            self.catalog.iter().filter_map(|g| Some((g.id.clone(), g.console_slug.clone(), g.game_slug.clone()?))).filter(|(id, _, _)| matches!(need.get(id), Some(false))).collect();
        if work.is_empty() && self.diag.have_index {
            return; // nothing to fill and the index for the publish check is fresh enough
        }

        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("diag-provision".into())
            .spawn(move || {
                let _ = tx.send(run_diag_provision(&client, work));
                ctx.request_repaint();
            })
            .expect("spawn diag-provision thread");
        self.diag.pass = Some(rx);
    }

    /// Publish a manually-picked file as the fixture for its game.
    fn start_diag_upload(&mut self, ctx: &egui::Context, pf: PublishFixture) {
        let Some(client) = api_client() else { return };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("diag-upload".into())
            .spawn(move || {
                let out = (|| {
                    let bytes = std::fs::read(&pf.path).map_err(|e| format!("read {}: {e}", pf.path.display()))?;
                    let name = pf.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "fixture.bin".into());
                    client.diag_upload(&pf.console_slug, &pf.game_slug, &name, bytes).map_err(|e| format!("{e:#}"))
                })();
                let _ = tx.send(out);
                ctx.request_repaint();
            })
            .expect("spawn diag-upload thread");
        self.diag.upload = Some(rx);
    }

    /// Drain the diagnostics worker channels each frame.
    fn drain_diag(&mut self) {
        if let Some(rx) = &self.diag.pass
            && let Ok(res) = rx.try_recv()
        {
            self.diag.pass = None;
            self.diag.index = res.index;
            self.diag.have_index = true;
            if res.provisioned > 0 {
                tracing::info!("diag: filled {} launcher content path(s) from the fixture store", res.provisioned);
                self.home_app.refresh_instances();
            }
        }
        if let Some(rx) = &self.diag.upload
            && let Ok(res) = rx.try_recv()
        {
            self.diag.upload = None;
            match res {
                Ok(fx) => {
                    tracing::info!("diag: published a fixture for {}/{}", fx.console_slug, fx.game_slug);
                    self.diag.index.retain(|f| !(f.console_slug == fx.console_slug && f.game_slug == fx.game_slug));
                    self.diag.index.push(fx);
                }
                Err(e) => self.home_app.note_error(format!("Couldn't publish the diagnostic fixture: {e}")),
            }
        }
    }

    /// One-time modal for a privileged user who just picked a file by hand and
    /// the store doesn't have one for that game yet.
    fn diag_publish_prompt(&mut self, ctx: &egui::Context) {
        let Some(pf) = &self.diag.ask_publish else { return };
        let game = pf.game_name.clone();
        let resp = egui::Modal::new(egui::Id::new("diag_publish")).show(ctx, |ui| {
            ui.set_max_width(300.0);
            ui.heading("Add to the diagnostics store?");
            ui.add_space(6.0);
            ui.label(format!(
                "Publish this file as the diagnostic fixture for {game}? Other privileged accounts testing {game} will then get it automatically, so nobody has to track the file down twice."
            ));
            ui.add_space(12.0);
            let mut pick = None;
            ui.horizontal(|ui| {
                if ui.button("Not now").clicked() {
                    pick = Some(false);
                }
                if ui.button("Publish").clicked() {
                    pick = Some(true);
                }
            });
            pick
        });
        if let Some(publish) = resp.inner.or_else(|| resp.should_close().then_some(false)) {
            let pf = self.diag.ask_publish.take().expect("checked above");
            if publish {
                self.start_diag_upload(ctx, pf);
            }
        }
    }

    /// Pick up an update a previous session downloaded but never committed:
    /// surface it in Config › Updates, and if `on_update = install`, apply it
    /// now. Called once the account is `Ready`. `update::staged()` self-cleans a
    /// stale marker, so this also sweeps a leftover from an older version.
    fn adopt_staged_update(&mut self, ctx: &egui::Context) {
        if self.updater.busy() {
            return;
        }
        let Some(staged) = update::staged() else { return };
        tracing::info!("resuming staged update {} ({})", staged.version, staged.tag);
        self.updater = Updater::Staged(staged);
        if crate::version::is_release() && Config::get(|c| c.updates.on_update) == UpdateAction::Install {
            self.start_update_install(ctx);
        }
    }

    /// Kick off a GitHub Releases check on a worker thread. `auto` marks it as
    /// timer-driven so `drain_updater` applies `[updates].on_update` on finish.
    fn start_update_check(&mut self, ctx: &egui::Context, auto: bool) {
        if self.updater.busy() {
            return;
        }
        self.auto_check_pending = auto;
        let allow_prerelease = Config::get(|c| c.updates.channel) == UpdateChannel::Prerelease;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let _ = tx.send(update::check(allow_prerelease).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            })
            .expect("spawn update-check thread");
        self.updater = Updater::Checking(rx);
    }

    /// Work out when the next automatic check is due from the last persisted
    /// check and `[updates].check_interval`; an overdue / never-run check is
    /// scheduled a short way out so launch isn't hammered.
    fn schedule_update_check(&mut self) {
        const SOON: Duration = Duration::from_secs(45);
        let interval = Config::get(|c| c.updates.check_interval.0);
        let elapsed = Store::get(|s| s.last_update_check()).ok().flatten().and_then(|ts| crate::sync::parse_utc(&ts)).map(|then| Duration::from_secs((crate::sync::now_epoch() - then).max(0) as u64));
        let wait = match elapsed {
            Some(e) if e < interval => (interval - e).max(SOON),
            _ => SOON,
        };
        self.next_update_check = Some(Instant::now() + wait);
    }

    /// Fire an automatic check when one is due. Called every `logic()` tick.
    fn tick_update_check(&mut self, ctx: &egui::Context) {
        if !crate::version::is_release() {
            return;
        }
        if !Config::get(|c| c.updates.auto_check) {
            self.next_update_check = None;
            return;
        }
        // Don't stomp an in-flight check, a surfaced offer, or a staged update.
        if self.updater.busy() || self.updater.offer().is_some() || self.updater.staged().is_some() {
            return;
        }
        if self.next_update_check.is_none() {
            self.schedule_update_check();
        }
        if self.next_update_check.is_some_and(|due| Instant::now() >= due) {
            self.start_update_check(ctx, true);
        }
    }

    /// Download + verify the pending update on a worker thread *without* swapping
    /// it in - it lands as `Updater::Staged`. Backs `on_update = download`.
    fn start_update_stage(&mut self, ctx: &egui::Context) {
        let Updater::Checked(Some(av)) = std::mem::replace(&mut self.updater, Updater::Idle) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("update-stage".into())
            .spawn(move || {
                let _ = tx.send(update::stage(&av).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            })
            .expect("spawn update-stage thread");
        self.updater = Updater::Staging(rx);
    }

    /// Put the pending update in place on a worker thread: an instant `commit`
    /// when it's already `Staged`, otherwise a full download-verify-swap.
    fn start_update_install(&mut self, ctx: &egui::Context) {
        let job: Box<dyn FnOnce() -> Result<(), String> + Send> = match std::mem::replace(&mut self.updater, Updater::Idle) {
            Updater::Staged(staged) => Box::new(move || update::commit(&staged).map_err(|e| format!("{e:#}"))),
            Updater::Checked(Some(av)) => Box::new(move || update::apply(&av).map_err(|e| format!("{e:#}"))),
            other => {
                self.updater = other;
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("update-apply".into())
            .spawn(move || {
                let _ = tx.send(job());
                ctx.request_repaint();
            })
            .expect("spawn update-apply thread");
        self.updater = Updater::Installing(rx);
    }

    /// Advance the updater state machine. On a finished auto-check, record the
    /// time and apply `[updates].on_update`; on a finished install, shut down so
    /// the relaunched process can take over.
    fn drain_updater(&mut self, ctx: &egui::Context) {
        let next = match &self.updater {
            Updater::Checking(rx) => match rx.try_recv() {
                Ok(Ok(av)) => Some(Updater::Checked(av)),
                Ok(Err(e)) => Some(Updater::Error(e)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Updater::Error("the check thread stopped unexpectedly".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            Updater::Staging(rx) => match rx.try_recv() {
                Ok(Ok(staged)) => Some(Updater::Staged(staged)),
                Ok(Err(e)) => Some(Updater::Error(e)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Updater::Error("the download thread stopped unexpectedly".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            Updater::Installing(rx) => match rx.try_recv() {
                Ok(Ok(())) => Some(Updater::Restarting),
                Ok(Err(e)) => Some(Updater::Error(e)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Updater::Error("the update thread stopped unexpectedly".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            _ => None,
        };
        let Some(state) = next else { return };

        let check_finished = matches!(self.updater, Updater::Checking(_));
        let stage_finished = matches!(self.updater, Updater::Staging(_));
        let was_auto = std::mem::take(&mut self.auto_check_pending);
        self.updater = state;

        if check_finished {
            let _ = Store::write(|s| s.set_last_update_check(&crate::sync::now_utc_string()));
            self.schedule_update_check();
            if was_auto && let Updater::Checked(Some(av)) = &self.updater {
                let version = av.version.to_string();
                tracing::info!("auto update check: {version} available");
                match Config::get(|c| c.updates.on_update) {
                    UpdateAction::Notify => notice::post(Notice::UpdateReady { version }),
                    // Pre-fetch now; the notice waits until the bytes are on disk.
                    UpdateAction::Download => {
                        self.notify_when_staged = true;
                        self.start_update_stage(ctx);
                    }
                    UpdateAction::Install => self.start_update_install(ctx),
                }
            }
        }

        if stage_finished {
            match &self.updater {
                Updater::Staged(s) => {
                    let version = s.version.to_string();
                    tracing::info!("update {version} staged, ready to install");
                    if std::mem::take(&mut self.notify_when_staged) {
                        notice::post(Notice::UpdateReady { version });
                    }
                }
                Updater::Error(e) => {
                    tracing::warn!("staging update failed: {e}");
                    self.notify_when_staged = false;
                }
                _ => {}
            }
        }

        if matches!(self.updater, Updater::Restarting) {
            tracing::info!("update applied; closing so the new build can relaunch");
            self.sync = None;
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Reset the sub-screen to its default whenever the visible window changes,
    /// so reopening the tray always lands on a clean state.
    fn sync_shown_screen(&mut self) {
        if self.state == self.prev_state {
            return;
        }
        match self.state {
            WindowState::ShowConfig => self.config_app.reset(),
            WindowState::ShowHome => self.home_app.on_reopen(),
            WindowState::Hidden => {}
        }
        self.prev_state = self.state;
    }

    /// Tear down every trace of the current session and return to the sign-in
    /// screen, showing `error` there if one is given.
    fn clear_session_state(&mut self, error: Option<Arc<str>>) {
        let _ = Config::update(|c| c.clear_session());
        self.sync = None;
        self.catalog = Vec::new();
        self.catalog_ready = false;
        self.pending_pick = None;
        self.auth = AuthView::LoggedOut { error };
        self.focus_latch = true;
        self.ask_crash_reports = false;
        self.ask_install = false;
        self.ask_tray_intro = false;
        self.next_update_check = None;
        self.auto_check_pending = false;
        self.notify_when_staged = false;
        self.updater = Updater::Idle;
        // Stop tracking any launched emulator; leave it running.
        self.launch = None;
        self.role = Role::User;
        self.diag = Diag::default();
        self.account_theme = None;
        self.config_app.reset();
        self.home_app.reset();
    }

    /// User-initiated logout: also tell the server to revoke this session.
    fn begin_logout(&mut self, ctx: &egui::Context) {
        if let Some(client) = api_client() {
            api::revoke_in_background(client);
        }
        self.clear_session_state(None);
        ctx.request_repaint();
    }

    /// The engine hit a `401`/`403` mid-sync. The session is already dead, so no
    /// revoke call, just drop back to sign-in with a note.
    fn handle_session_expired(&mut self, ctx: &egui::Context) {
        if matches!(self.auth, AuthView::LoggedOut { .. }) {
            return; // already handled this round
        }
        tracing::warn!("session expired mid-sync, signing out");
        self.clear_session_state(Some(Arc::from("Your session expired. Please sign in again.")));
        notice::post(Notice::SessionExpired);
        ctx.request_repaint();
    }

    /// Pick the next pending first-run modal, in order: install → usage data →
    /// tray explainer. Called on first `Ready` and after each one resolves; when
    /// nothing is pending all three flags stay `false`.
    fn advance_first_run_prompts(&mut self) {
        self.ask_install = false;
        self.ask_crash_reports = false;
        self.ask_tray_intro = false;
        if crate::install::needs_first_run_prompt() {
            self.ask_install = true;
        } else if !Config::get(|c| c.crash_reports_answered()) {
            self.ask_crash_reports = true;
        } else if !Config::get(|c| c.startup.onboarded) {
            self.ask_tray_intro = true;
        }
    }

    /// One-time modal shown right after the first sign-in when the crash-report
    /// preference has never been set. Either choice is recorded; a backdrop /
    /// Esc dismiss counts as "no".
    fn crash_reports_prompt(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("first_run_crash_reports")).show(ctx, |ui| {
            ui.set_max_width(260.0);
            ui.heading("Crash reports");
            ui.add_space(6.0);
            ui.label("Send anonymous crash and error reports to help fix bugs? Save contents and personal data are never included. You can change this any time under Advanced.");
            ui.add_space(12.0);
            let mut pick = None;
            ui.horizontal(|ui| {
                if ui.button("No thanks").clicked() {
                    pick = Some(false);
                }
                if ui.button("Send reports").clicked() {
                    pick = Some(true);
                }
            });
            pick
        });

        if let Some(enabled) = resp.inner.or_else(|| resp.should_close().then_some(false)) {
            let _ = Config::update(|c| c.set_crash_reports(enabled));
            self.advance_first_run_prompts();
        }
    }

    /// One-time modal after the usage-data prompt: explain that CoinCell lives
    /// in the tray so a fresh install that starts hidden isn't a mystery. "Got
    /// it" sets `[startup].onboarded`, which also re-enables `start_hidden` and
    /// focus-loss auto-hide from here on.
    fn tray_intro_prompt(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("first_run_tray_intro")).show(ctx, |ui| {
            ui.set_max_width(280.0);
            ui.heading("CoinCell runs in the tray");
            ui.add_space(6.0);
            ui.label("Closing this window doesn't quit CoinCell, it keeps syncing from the system tray.");
            ui.add_space(6.0);
            ui.label("\u{2022}  Left-click the tray icon for your games");
            ui.label("\u{2022}  Right-click it for settings");
            ui.label("\u{2022}  The close button (top right) or Esc sends this window back to the tray");
            ui.add_space(12.0);
            ui.vertical_centered(|ui| ui.button("Got it").clicked()).inner
        });

        if resp.inner || resp.should_close() {
            let _ = Config::update(|c| c.startup.onboarded = true);
            self.advance_first_run_prompts();
        }
    }

    /// One-time modal on the first sign-in from a loose release build: offer to
    /// install into a stable location (which also enables self-update) and hand
    /// off to the installed copy. "Not now" is remembered.
    fn install_prompt(&mut self, ctx: &egui::Context) {
        let dst = crate::install::canonical_exe().ok();
        let resp = egui::Modal::new(egui::Id::new("first_run_install")).show(ctx, |ui| {
            ui.set_max_width(280.0);
            ui.heading("Install CoinCell?");
            ui.add_space(6.0);
            ui.label("You're running CoinCell from wherever you unzipped it. Installing copies it to a stable location, adds a Start Menu / launcher entry, and lets it keep itself up to date.");
            if let Some(dst) = &dst {
                ui.add_space(4.0);
                ui.small(dst.display().to_string());
            }
            ui.add_space(12.0);
            let mut pick = None;
            ui.horizontal(|ui| {
                if ui.button("Not now").clicked() {
                    pick = Some(false);
                }
                if ui.button("Install").clicked() {
                    pick = Some(true);
                }
            });
            pick
        });

        let Some(install) = resp.inner.or_else(|| resp.should_close().then_some(false)) else {
            return;
        };
        if install {
            match crate::install::install() {
                Ok(path) => return self.relaunch_from(ctx, path),
                Err(e) => tracing::error!("first-run install failed: {e:#}"),
            }
        }
        // Declined, dismissed, or it failed: don't nag again (the Config button
        // stays), and move on to the next first-run prompt.
        let _ = Config::update(|c| c.startup.skip_install_prompt = true);
        self.advance_first_run_prompts();
    }

    /// A self-install / update-in-place wrote the binary at `path`; spawn it
    /// (with the post-update flag so it waits out our single-instance lock) and
    /// close this process.
    fn relaunch_from(&mut self, ctx: &egui::Context, path: std::path::PathBuf) {
        tracing::info!("handing off to {}", path.display());
        self.sync = None;
        self.quitting = true;
        if let Err(e) = std::process::Command::new(&path).arg("--relaunched-after-update").spawn() {
            tracing::error!("couldn't spawn {}: {e}", path.display());
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Set desired window state. The OS window is brought in line by
    /// [`Self::reconcile_visibility`] on the next `logic()` tick, not here.
    fn hide(&mut self) {
        self.state = WindowState::Hidden;
        self.focus_latch = false;
    }

    /// Command the OS window to match `state` (visible unless `Hidden`). eframe
    /// force-shows the window once after the first paint (its anti-flash hack)
    /// and `ViewportBuilder::with_visible` is ignored, so this is the only thing
    /// that reliably keeps a `start_hidden` launch hidden. For the first few
    /// frames it re-asserts unconditionally (and repaints) to out-last that
    /// one-shot show; after that it only sends on a change.
    fn reconcile_visibility(&mut self, ctx: &egui::Context) {
        let want_visible = !self.state.hidden();
        let settling = self.visible_settling < 3;
        if settling {
            self.visible_settling += 1;
            ctx.request_repaint();
        }
        if settling || self.visible_cmd != Some(want_visible) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want_visible));
            self.visible_cmd = Some(want_visible);
        }
    }

    /// The shared title bar for every screen (Config, Home, sign-in): screen
    /// name on the left, a minimise-to-tray button on the right. Hidden while a
    /// native dialog would be up is unnecessary, it's just chrome.
    fn header(&mut self, ui: &mut egui::Ui) {
        let title = if !matches!(self.auth, AuthView::Ready) {
            "CoinCell"
        } else if self.state.is_home() {
            "Home"
        } else {
            "Config"
        };

        let mut minimise = false;
        egui::Panel::top("app_header").exact_size(26.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    minimise = ui.button(egui::RichText::new(MINIMISE).size(14.0)).on_hover_text("Minimise to tray (Esc)").clicked();
                });
            });
        });
        if minimise {
            self.hide();
        }
    }

    fn show_home(&mut self) {
        self.state = WindowState::ShowHome;
        self.focus_latch = true;
    }

    fn show_config(&mut self) {
        self.state = WindowState::ShowConfig;
        self.focus_latch = true;
    }

    fn pump_auth(&mut self, ctx: &egui::Context) {
        enum Next {
            Stay,
            Ready,
            /// Session just saved after a device login, bounce through
            /// validation once to confirm it and pick up the account theme.
            Revalidate,
            LoggedOut(Option<Arc<str>>),
        }

        let next = match &mut self.auth {
            AuthView::Connecting { flow, approval } => {
                let mut done: Option<Arc<str>> = None;
                let mut failed: Option<Arc<str>> = None;
                for event in flow.events() {
                    match event {
                        DeviceEvent::AwaitingApproval { user_code, verification_uri, verification_uri_complete } => {
                            *approval = Some(Approval { user_code, verification_uri, verification_uri_complete });
                        }
                        DeviceEvent::Completed { session_id } => done = Some(session_id),
                        DeviceEvent::Failed { message } => failed = Some(message),
                    }
                }

                if let Some(session_id) = done {
                    match Config::update(|c| c.set_session(session_id)) {
                        Ok(()) => Next::Revalidate,
                        Err(e) => Next::LoggedOut(Some(Arc::from(format!("couldn't save login: {e:#}")))),
                    }
                } else if let Some(message) = failed {
                    Next::LoggedOut(Some(message))
                } else {
                    Next::Stay
                }
            }
            AuthView::Validating { check } => match check.poll() {
                Some(SessionStatus::Valid(me)) => {
                    self.account_theme = Some(me.theme);
                    self.role = me.role;
                    Next::Ready
                }
                Some(SessionStatus::Invalid) => Next::LoggedOut(Some(Arc::from("Your session has expired. Please sign in again."))),
                Some(SessionStatus::Unknown(msg)) => {
                    tracing::info!("couldn't verify stored session, keeping it: {msg}");
                    Next::Ready
                }
                None => Next::Stay,
            },
            _ => Next::Stay,
        };

        match next {
            Next::Stay => {}
            Next::Ready => {
                self.auth = AuthView::Ready;
                self.focus_latch = true;
                self.advance_first_run_prompts();
                self.adopt_staged_update(ctx);
                self.schedule_update_check();
                self.start_sync(ctx);
            }
            Next::Revalidate => match api_client() {
                Some(client) => self.auth = AuthView::Validating { check: SessionCheck::start(client, waker(ctx)) },
                None => self.auth = AuthView::Ready,
            },
            Next::LoggedOut(error) => {
                self.auth = AuthView::LoggedOut { error };
                self.focus_latch = true;
                self.sync = None;
            }
        }
    }

    fn login_screen(&self, ui: &mut egui::Ui) -> LoginIntent {
        let mut intent = LoginIntent::None;
        let identity = &self.branding.identity;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.22).max(0.0));
                ui.heading(crate::constants::APP_NAME);
                if !identity.tagline.is_empty() {
                    ui.add_space(2.0);
                    ui.weak(&identity.tagline);
                }
                ui.add_space(18.0);
                match &self.auth {
                    AuthView::Validating { .. } => {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Checking your session…");
                    }
                    AuthView::LoggedOut { error } => {
                        if ui.button("Retrieve login code").clicked() {
                            intent = LoginIntent::StartFlow;
                        }
                        if let Some(error) = error {
                            ui.add_space(12.0);
                            ui.colored_label(egui::Color32::LIGHT_RED, error.as_ref());
                        }
                    }
                    AuthView::Connecting { approval: None, .. } => {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Requesting a login code…");
                    }
                    AuthView::Connecting { approval: Some(approval), .. } => {
                        ui.label("Enter this code when your browser asks for it:");
                        ui.add_space(4.0);
                        if ui.button(approval.user_code.as_ref()).clicked() {
                            ui.copy_text((*approval.user_code).to_string());
                        };
                        ui.add_space(8.0);
                        ui.hyperlink(approval.verification_uri.as_ref());
                        ui.add_space(4.0);
                        if ui.button("Open browser again").clicked() {
                            intent = LoginIntent::ReopenBrowser;
                        }

                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(16.0);

                        ui.label("You can also navigate to settings -> Link a Device and input the code manually");

                        ui.add_space(16.0);

                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label("Waiting for approval…");
                    }
                    AuthView::Ready => {}
                }
            });

            if !identity.attribution_text.is_empty() {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    ui.hyperlink_to(egui::RichText::new(&identity.attribution_text).weak().small(), &identity.homepage_url);
                });
            }
        });
        intent
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        let scale = Config::get(|c| c.ui_scale());
        if (ctx.zoom_factor() - scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(scale);
        }

        self.sync_theme(ctx);

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Down, .. } => {
                    if self.state.hidden() {
                        self.show_home();
                    } else {
                        if self.state.is_config() {
                            self.state = WindowState::ShowHome;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                TrayIconEvent::Click { button: MouseButton::Right, button_state: MouseButtonState::Down, .. } => {
                    if self.state.hidden() {
                        self.show_config();
                    } else {
                        if self.state.is_home() {
                            self.state = WindowState::ShowConfig;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                _ => {}
            }
        }
        while self.wake_rx.try_recv().is_ok() {
            if self.state.hidden() {
                self.show_home();
            } else {
                if self.state.is_home() {
                    self.state = WindowState::ShowConfig;
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        self.pump_auth(ctx);
        self.drain_sync(ctx);
        notice::pump();
        self.drain_pending_pick(ctx);
        self.drain_diag();
        self.tick_launch(ctx);
        self.tick_update_check(ctx);
        self.drain_updater(ctx);
        self.sync_shown_screen();
        self.reconcile_visibility(ctx);

        if self.state.is_config() {
            self.config_app.logic(ctx, frame);
        } else if self.state.is_home() {
            self.home_app.logic(ctx, frame);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.state.hidden() {
            // Paint a valid (blank, themed) frame so a window that's momentarily
            // shown, e.g. during the first-frame settle, is never just black.
            egui::CentralPanel::default().show(ui, |_| {});
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            return;
        }

        let busy_auth = matches!(self.auth, AuthView::Connecting { .. } | AuthView::Validating { .. });
        // Anything that hands focus to a native window we don't own, plus the
        // first-run modals: losing focus to one of those must not minimise us.
        let modal_active = busy_auth || self.pending_pick.is_some() || self.ask_install || self.ask_crash_reports || self.ask_tray_intro || self.diag.ask_publish.is_some();

        let lost_focus = ui.input(|i| {
            if i.focused {
                self.focus_latch = false;
                false
            } else {
                !self.focus_latch
            }
        });
        // Never auto-hide before onboarding (even if the user enabled it), nor
        // while a modal is up.
        if lost_focus && !self.quitting && !modal_active && Config::get(|c| c.startup.onboarded && c.window.hide_on_focus_loss) {
            ui.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide();
        }

        // Shared title bar (carries the minimise button). Esc is handled at the
        // end of the frame so an open popup or the modal claims it first.
        self.header(ui);

        if matches!(self.auth, AuthView::Ready) {
            if self.ask_install {
                self.install_prompt(ui.ctx());
            } else if self.ask_crash_reports {
                self.crash_reports_prompt(ui.ctx());
            } else if self.ask_tray_intro {
                self.tray_intro_prompt(ui.ctx());
            } else if self.diag.ask_publish.is_some() {
                self.diag_publish_prompt(ui.ctx());
            }

            if self.state.is_config() {
                match self.config_app.ui(ui, frame, &self.branding, &self.catalog, &self.updater, self.stream_online) {
                    ConfigOutcome::Stay => {}
                    ConfigOutcome::SyncNow => {
                        if let Some(engine) = &self.sync {
                            engine.sync_now();
                        }
                    }
                    ConfigOutcome::RestoreBackup { instance_id, content_hash } => {
                        if let Some(engine) = &self.sync {
                            engine.restore(&instance_id, RestoreSource::Backup { content_hash });
                        }
                    }
                    ConfigOutcome::CheckForUpdate => self.start_update_check(ui.ctx(), false),
                    ConfigOutcome::InstallUpdate => self.start_update_install(ui.ctx()),
                    ConfigOutcome::RelaunchFrom(path) => self.relaunch_from(ui.ctx(), path),
                    ConfigOutcome::LogOut => self.begin_logout(ui.ctx()),
                    ConfigOutcome::Quit => {
                        self.quitting = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            } else if self.state.is_home()
                && let Some(client) = api_client()
            {
                let launch = match &self.launch {
                    None => home::LaunchStatus::Idle,
                    Some(Launch { instance_id, phase: LaunchPhase::Checking { .. } }) => home::LaunchStatus::Checking(instance_id),
                    Some(Launch { instance_id, phase: LaunchPhase::Running { .. } }) => home::LaunchStatus::Running(instance_id),
                };
                let home = HomeView { catalog: &self.catalog, ready: self.catalog_ready, api_base: &self.device_config.api_base, branding: &self.branding, client: &client, launch };
                match self.home_app.ui(ui, frame, home) {
                    HomeOutcome::Stay => {}
                    HomeOutcome::Refresh => {
                        if let Some(engine) = &self.sync {
                            engine.rehydrate();
                        }
                    }
                    HomeOutcome::MappedInstance { instance_id } => {
                        if let Some(engine) = &self.sync {
                            engine.recheck(&instance_id);
                            engine.rehydrate();
                        }
                    }
                    HomeOutcome::RecheckInstance { instance_id } => {
                        if let Some(engine) = &self.sync {
                            engine.recheck(&instance_id);
                        }
                    }
                    HomeOutcome::ResolveConflict { instance_id, keep_local } => {
                        if let Some(engine) = &self.sync {
                            engine.resolve_conflict(&instance_id, keep_local);
                        }
                    }
                    HomeOutcome::OpenSaveDialog { instance_id, title } => {
                        self.open_pick(ui.ctx(), instance_id, PickKind::Save, title);
                    }
                    HomeOutcome::Restore { instance_id, source } => {
                        if let Some(engine) = &self.sync {
                            engine.restore(&instance_id, source);
                        }
                    }
                    HomeOutcome::Play { instance_id } => self.begin_play(ui.ctx(), instance_id),
                    HomeOutcome::PickContent { instance_id } => {
                        let name = self.game_label(&instance_id);
                        self.open_pick(ui.ctx(), instance_id, PickKind::Content { launch_after: false }, format!("ROM / content file for {name}"));
                    }
                    HomeOutcome::StopEmulator => self.stop_emulator(),
                }
            }
        } else {
            match self.login_screen(ui) {
                LoginIntent::StartFlow => {
                    self.auth = AuthView::Connecting { flow: DeviceFlow::start(self.device_config.clone(), CLIENT_NAME.as_str(), waker(ui.ctx())), approval: None };
                }
                LoginIntent::ReopenBrowser => {
                    if let AuthView::Connecting { approval: Some(approval), .. } = &self.auth {
                        api::open_in_browser(&approval.verification_uri_complete);
                    }
                }
                LoginIntent::None => {}
            }
        }

        // Esc minimises, but only if nothing this frame (an open combo, the
        // first-run modal, a text edit) already claimed the key.
        if !modal_active && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.hide();
        }

        // A `hide()` above takes effect on the next `logic()` tick; nudge one.
        if self.state.hidden() {
            ui.ctx().request_repaint();
        }
        ui.request_repaint_after(Duration::from_millis(100));
    }
}

/// Open a file or folder in the OS file manager.
fn open_path(path: &std::path::Path) {
    let cmd = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

/// A `Client` for the current session, or `None` when logged out.
fn api_client() -> Option<Client> {
    let (base, session) = Config::get(|c| (c.advanced.api_base.clone(), c.session_id()));
    session.map(|session| Client::new(base, session))
}

/// A repaint callback for the API's background workers, so their events surface
/// promptly instead of waiting for the next timed repaint.
fn waker(ctx: &egui::Context) -> impl Fn() + Send + Clone + 'static {
    let ctx = ctx.clone();
    move || ctx.request_repaint()
}

/// Worker body for [`App::provision_diag`]: fetch the fixture index, then for
/// each `(instance, console, game)` that has a matching fixture, make sure the
/// bytes are cached and point the instance's content path at them.
fn run_diag_provision(client: &Client, work: Vec<(String, String, String)>) -> DiagResult {
    let index = client.diag_index().unwrap_or_else(|e| {
        tracing::debug!("diag index: {e:#}");
        Vec::new()
    });
    let mut provisioned = 0;
    for (id, console, game) in work {
        let Some(fx) = index.iter().find(|f| f.console_slug == console && f.game_slug == game) else { continue };
        match ensure_fixture_cached(client, fx) {
            Ok(path) => {
                if Store::write(|s| s.set_content_path(&id, Some(&path))).is_ok() {
                    provisioned += 1;
                }
            }
            Err(e) => tracing::warn!("diag fixture for {id}: {e:#}"),
        }
    }
    DiagResult { index, provisioned }
}

/// Ensure a fixture's bytes sit in the cache and return the file path. Layout:
/// `<cache>/diag/<hash-prefix>/<original filename>`. A present file is trusted
/// only if its hash still matches; anything else is re-fetched.
fn ensure_fixture_cached(client: &Client, fx: &DiagFixture) -> anyhow::Result<std::path::PathBuf> {
    let dir = crate::constants::PROJECT_DIRS.cache_dir().join("diag").join(&fx.content_hash[..fx.content_hash.len().min(12)]);
    let name = std::path::Path::new(&fx.filename).file_name().unwrap_or(std::ffi::OsStr::new("fixture.bin"));
    let path = dir.join(name);

    if let Ok(bytes) = std::fs::read(&path)
        && crate::sync::sha256_hex(&bytes) == fx.content_hash
    {
        return Ok(path);
    }

    let bytes = client.diag_fetch(&fx.console_slug, &fx.game_slug).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    if crate::sync::sha256_hex(&bytes) != fx.content_hash {
        anyhow::bail!("downloaded fixture hash didn't match the index");
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, &bytes)?;
    tracing::info!("diag: cached fixture {} ({} bytes)", fx.filename, bytes.len());
    Ok(path)
}
