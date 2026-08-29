//! The Home window: the per-game list, plus the add-game flow.
//!
//! `Mode::List` is the default, a search box and a two-wide card grid split into
//! **Not mapped** / **Mapped** sections. The `+` button walks
//! `PickConsole → PickGame → Compose → MapPrompt`, each screen reusing the same
//! searchable card grid. Catalog fetches (`consoles`, `games`, `create`) run on
//! background threads via [`Task`].
//!
//! The instance catalog comes from the sync engine's hydrate via `App`; which
//! instances are mapped comes from the `data.sqlite` store, cached here and
//! refreshed on [`HomeApp::reset`].

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;

use eframe::egui;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::api::{Branding, Client, Console, Game, GameInstance, NewGameInstance, SaveMeta};
use crate::app::mapping::{self, PickedSave};
use crate::app::{icons, open_path};
use crate::config::ConflictPolicy;
use crate::store::{InstanceRecord, Store};
use crate::sync::{Action, LocalFile, reconcile};
use crate::theme::homepage_path;

/// Borrowed context `App` hands to Home each frame.
pub struct HomeView<'a> {
    pub catalog: &'a [GameInstance],
    /// `false` until the first hydrate lands, show a spinner until then.
    pub ready: bool,
    /// The server's own origin, for building box-art URLs.
    pub api_base: &'a str,
    pub branding: &'a Branding,
    pub client: &'a Client,
}

pub enum HomeOutcome {
    Stay,
    /// Re-fetch the instance list from the server (`SyncEngine::rehydrate`).
    Refresh,
    /// A local save was just bound to `instance_id`, recheck it now, then
    /// refresh the catalog.
    MappedInstance {
        instance_id: String,
    },
    /// Re-check one instance now (pause toggled, unmapped, "sync now").
    RecheckInstance {
        instance_id: String,
    },
    /// Keep one side of a recorded conflict.
    ResolveConflict {
        instance_id: String,
        keep_local: bool,
    },
    /// Open a native file picker off the UI thread; the chosen path comes back
    /// via [`HomeApp::deliver_save_pick`].
    OpenSaveDialog {
        instance_id: String,
        title: String,
    },
}

enum Mode {
    List,
    PickConsole,
    PickGame { console: Console },
    Compose { console: Console, game: Option<Game>, game_name: String, session_name: String },
    MapPrompt(MapPrompt),
    Detail(DetailState),
}

/// The "choose an existing save file, size-check, decide local vs server, bind"
/// flow, embedded in both `MapPrompt` and `Detail`.
#[derive(Default)]
struct FilePick {
    picked: Option<PickedSave>,
    /// The user chose to keep a file whose size didn't match.
    override_size: bool,
    /// A picker is open on its own thread; controls are disabled until it
    /// resolves via `deliver_save_pick`.
    dialog_pending: bool,
}

struct MapPrompt {
    instance_id: String,
    console: Console,
    /// Display name of the thing we just created.
    label: String,
    pick: FilePick,
}

struct DetailState {
    instance_id: String,
    /// The console's accepted save sizes, cached on entry for the size check.
    console_sizes: Vec<u64>,
    pick: FilePick,
    confirm_unmap: bool,
}

pub struct HomeApp {
    mode: Mode,
    search: String,
    /// `game_instance_id`s with a local save path bound. Refreshed on `reset()`
    /// and after a successful map here.
    mapped: HashSet<String>,
    consoles: Option<Task<Vec<Console>>>,
    games: Option<Task<Vec<Game>>>,
    create: Option<Task<String>>,
    error: Option<String>,
}

impl HomeApp {
    pub fn new() -> Self {
        let mut app = Self { mode: Mode::List, search: String::new(), mapped: HashSet::new(), consoles: None, games: None, create: None, error: None };
        app.reset();
        app
    }

    /// Full reset: back to the list, everything cleared. Used on construction
    /// and on logout.
    pub fn reset(&mut self) {
        self.mode = Mode::List;
        self.search.clear();
        self.consoles = None;
        self.games = None;
        self.create = None;
        self.error = None;
        self.reload_mapped();
    }

    /// Called when Home becomes the visible screen. Like [`Self::reset`], but
    /// keeps a `MapPrompt` (an instance was created and still needs a mapping
    /// decision) or a `Detail` with a file picker still out, so minimising
    /// mid-flow doesn't strand it.
    pub fn on_reopen(&mut self) {
        let keep = match &self.mode {
            Mode::MapPrompt(_) => true,
            Mode::Detail(d) => d.pick.dialog_pending,
            _ => false,
        };
        if keep {
            self.reload_mapped();
        } else {
            self.reset();
        }
    }

    fn reload_mapped(&mut self) {
        self.mapped = match Store::get(|s| s.instances()) {
            Ok(rows) => rows.into_iter().map(|r| r.game_instance_id).collect(),
            Err(e) => {
                tracing::warn!("home: reading bound instances: {e:#}");
                HashSet::new()
            }
        };
    }

    pub fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {}

    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame, view: HomeView<'_>) -> HomeOutcome {
        let mut outcome = HomeOutcome::Stay;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(2.0);

            // Take the mode out so the per-mode views can hold `&mut self` freely;
            // each returns the next mode.
            let mode = std::mem::replace(&mut self.mode, Mode::List);
            self.mode = match mode {
                Mode::List => self.list_view(ui, &view, &mut outcome),
                Mode::PickConsole => self.pick_console_view(ui, &view),
                Mode::PickGame { console } => self.pick_game_view(ui, &view, console),
                Mode::Compose { console, game, game_name, session_name } => self.compose_view(ui, &view, console, game, game_name, session_name),
                Mode::MapPrompt(prompt) => self.map_prompt_view(ui, &view, &mut outcome, prompt),
                Mode::Detail(state) => self.detail_view(ui, &view, &mut outcome, state),
            };
        });

        outcome
    }

    // ---- list -----------------------------------------------------------

    fn list_view(&mut self, ui: &mut egui::Ui, view: &HomeView<'_>, outcome: &mut HomeOutcome) -> Mode {
        let mut next = Mode::List;

        ui.horizontal(|ui| {
            if ui.button("+ Add game").clicked() {
                self.search.clear();
                self.consoles = Some(Task::spawn(ui.ctx().clone(), {
                    let client = view.client.clone();
                    move || client.consoles().map_err(|e| e.to_string())
                }));
                next = Mode::PickConsole;
            }
            if ui.button(icons::SYNC).on_hover_text("Refresh from the server").clicked() {
                *outcome = HomeOutcome::Refresh;
            }
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search games").desired_width(ui.available_width()));
        });
        ui.add_space(6.0);

        if !view.ready {
            centered_spinner(ui, "Loading your games…");
            return next;
        }

        let query = self.search.trim().to_owned();
        let (mapped, unmapped) = self.partition(view, &query);
        let mut opened: Option<String> = None;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if view.catalog.is_empty() {
                empty_state(ui, view.branding, "No games on this account yet.", Some("Add one"));
            } else if mapped.is_empty() && unmapped.is_empty() {
                empty_state(ui, view.branding, &format!("No games match “{query}”."), None);
            } else {
                // Mapped first, they're what you look at day to day; unmapped is
                // usually everything else on the account and matters less.
                if !mapped.is_empty() {
                    if !unmapped.is_empty() {
                        heading(ui, "Mapped", mapped.len());
                    }
                    let cards: Vec<_> = mapped.iter().map(|i| instance_card(i, view.api_base, true)).collect();
                    if let Some(i) = render_grid(ui, &cards) {
                        opened = Some(mapped[i].id.clone());
                    }
                    ui.add_space(14.0);
                }
                if !unmapped.is_empty() {
                    if !mapped.is_empty() {
                        heading(ui, "Not mapped", unmapped.len());
                    }
                    let cards: Vec<_> = unmapped.iter().map(|i| instance_card(i, view.api_base, false)).collect();
                    if let Some(i) = render_grid(ui, &cards) {
                        opened = Some(unmapped[i].id.clone());
                    }
                }
                ui.add_space(8.0);
            }
        });

        if let Some(id) = opened {
            let console_sizes = view.catalog.iter().find(|g| g.id == id).and_then(|g| Store::get(|s| s.console_sizes(&g.console_slug)).ok().flatten()).unwrap_or_default();
            return Mode::Detail(DetailState { instance_id: id, console_sizes, pick: FilePick::default(), confirm_unmap: false });
        }
        next
    }

    fn partition<'c>(&self, view: &HomeView<'c>, query: &str) -> (Vec<&'c GameInstance>, Vec<&'c GameInstance>) {
        let matcher = SkimMatcherV2::default().ignore_case();
        let mut scored: Vec<(&GameInstance, i64)> = Vec::with_capacity(view.catalog.len());
        for inst in view.catalog {
            if query.is_empty() {
                scored.push((inst, 0));
            } else if let Some(score) = instance_score(&matcher, inst, query) {
                scored.push((inst, score));
            }
        }
        if query.is_empty() {
            scored.sort_by(|a, b| recency_key(b.0).cmp(&recency_key(a.0)));
        } else {
            scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| recency_key(b.0).cmp(&recency_key(a.0))));
        }

        let mut mapped = Vec::new();
        let mut unmapped = Vec::new();
        for (inst, _) in scored {
            if self.mapped.contains(&inst.id) { mapped.push(inst) } else { unmapped.push(inst) }
        }
        (mapped, unmapped)
    }

    // ---- pick a console ------------------------------------------------

    fn pick_console_view(&mut self, ui: &mut egui::Ui, view: &HomeView<'_>) -> Mode {
        if picker_header(ui, &mut self.search, "Add a game", "Choose a console") {
            return Mode::List; // Back
        }

        let query = self.search.trim().to_owned();
        let mut chosen: Option<Console> = None;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.consoles.as_mut().map(Task::state) {
            None | Some(TaskState::Running) => centered_spinner(ui, "Loading consoles…"),
            Some(TaskState::Failed(e)) => retry_state(ui, e),
            Some(TaskState::Done(list)) => {
                let shown = filter_sorted(list, &query, |c| c.name.as_str(), |c| c.description.as_str());
                if shown.is_empty() {
                    empty_state(ui, view.branding, &format!("No consoles match “{query}”."), None);
                } else {
                    let cards: Vec<_> = shown.iter().map(|c| CardData::plain(&c.slug, &c.name, &c.description, c.box_art_url.clone())).collect();
                    if let Some(i) = render_grid(ui, &cards) {
                        chosen = Some(shown[i].clone());
                    }
                }
            }
        });

        if self.consoles.as_mut().is_some_and(|t| matches!(t.state(), TaskState::Failed(_))) && ui.button("Try again").clicked() {
            self.consoles = Some(Task::spawn(ui.ctx().clone(), {
                let client = view.client.clone();
                move || client.consoles().map_err(|e| e.to_string())
            }));
        }

        if let Some(console) = chosen {
            self.search.clear();
            self.games = Some(Task::spawn(ui.ctx().clone(), {
                let (client, slug) = (view.client.clone(), console.slug.clone());
                move || client.games(&slug).map_err(|e| e.to_string())
            }));
            return Mode::PickGame { console };
        }
        Mode::PickConsole
    }

    // ---- pick a game ------------------------------------------------

    fn pick_game_view(&mut self, ui: &mut egui::Ui, view: &HomeView<'_>, console: Console) -> Mode {
        if picker_header(ui, &mut self.search, &console.name, "Choose a game") {
            self.search.clear();
            return Mode::PickConsole; // Back
        }

        let mut next: Option<Mode> = None;
        if ui.button("Not listed, enter it manually").clicked() {
            next = Some(Mode::Compose { console: console.clone(), game: None, game_name: String::new(), session_name: String::new() });
        }
        ui.add_space(4.0);

        let query = self.search.trim().to_owned();
        let mut chosen: Option<Game> = None;

        // `render_grid_virtual` brings its own `ScrollArea` (the catalog can be
        // ~1500 games, so only near-visible rows are built and only their art is
        // fetched); the other states are small and need none.
        match self.games.as_mut().map(Task::state) {
            None | Some(TaskState::Running) => centered_spinner(ui, "Loading games…"),
            Some(TaskState::Failed(e)) => retry_state(ui, e),
            Some(TaskState::Done(list)) => {
                let shown = filter_sorted(list, &query, |g| g.name.as_str(), |g| g.description.as_str());
                if shown.is_empty() {
                    empty_state(ui, view.branding, &format!("No games match “{query}”."), None);
                } else {
                    let cards: Vec<_> = shown.iter().map(|g| CardData::plain(&g.slug, &g.name, &g.description, g.box_art_url.clone())).collect();
                    if let Some(i) = render_grid_virtual(ui, &cards) {
                        chosen = Some(shown[i].clone());
                    }
                }
            }
        }

        if let Some(mode) = next {
            self.search.clear();
            return mode;
        }
        if let Some(game) = chosen {
            self.search.clear();
            return Mode::Compose { console, game: Some(game), game_name: String::new(), session_name: String::new() };
        }
        Mode::PickGame { console }
    }

    // ---- name & create ---------------------------------------------

    fn compose_view(&mut self, ui: &mut egui::Ui, view: &HomeView<'_>, console: Console, game: Option<Game>, mut game_name: String, mut session_name: String) -> Mode {
        let mut back = false;
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                back = true;
            }
            ui.heading("Name it");
        });
        if back {
            return Mode::PickGame { console };
        }
        ui.separator();
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let art = game.as_ref().map(|g| g.box_art_url.clone()).unwrap_or_else(|| console.box_art_url.clone());
            ui.add(egui::Image::new(art).fit_to_exact_size(egui::vec2(48.0, 64.0)).corner_radius(4.0).show_loading_spinner(false));
            ui.vertical(|ui| {
                match &game {
                    Some(g) => ui.label(egui::RichText::new(&g.name).strong()),
                    None => ui.label(egui::RichText::new("Custom game").strong()),
                };
                ui.weak(&console.name);
            });
        });
        ui.add_space(10.0);

        egui::Grid::new("compose_grid").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
            if game.is_none() {
                ui.label("Game name");
                ui.add(egui::TextEdit::singleline(&mut game_name).hint_text("e.g. Mother 3").desired_width(ui.available_width()));
                ui.end_row();
            }
            ui.label("Session name");
            ui.add(egui::TextEdit::singleline(&mut session_name).hint_text("optional, e.g. 100% run").desired_width(ui.available_width()));
            ui.end_row();
        });
        ui.small("The session name is your label for this playthrough. Leave it blank to just use the game name.");
        ui.add_space(10.0);

        if let Some(err) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, err);
            ui.add_space(4.0);
        }

        let custom_missing_name = game.is_none() && game_name.trim().is_empty();
        let polled = poll_owned(&mut self.create);

        if matches!(polled, Polled::Running) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Creating…");
            });
        } else if ui.add_enabled(!custom_missing_name, egui::Button::new("Create")).clicked() {
            self.error = None;
            let body = NewGameInstance {
                console_slug: console.slug.clone(),
                game_slug: game.as_ref().map(|g| g.slug.clone()),
                game_name: game.is_none().then(|| game_name.trim().to_owned()),
                custom_name: some_trimmed(&session_name),
            };
            self.create = Some(Task::spawn(ui.ctx().clone(), {
                let client = view.client.clone();
                move || client.create_game_instance(&body).map_err(|e| e.to_string())
            }));
        }

        match polled {
            Polled::Failed(e) => {
                self.error = Some(format!("Couldn't create: {e}"));
                self.create = None;
            }
            Polled::Done(id) => {
                let label = if !session_name.trim().is_empty() {
                    session_name.trim().to_owned()
                } else if let Some(g) = &game {
                    g.name.clone()
                } else {
                    game_name.trim().to_owned()
                };
                self.create = None;
                self.reload_mapped();
                return Mode::MapPrompt(MapPrompt { instance_id: id, console, label, pick: FilePick::default() });
            }
            Polled::Running | Polled::None => {}
        }

        Mode::Compose { console, game, game_name, session_name }
    }

    // ---- map now / later --------------------------------------------

    fn map_prompt_view(&mut self, ui: &mut egui::Ui, _view: &HomeView<'_>, outcome: &mut HomeOutcome, mut prompt: MapPrompt) -> Mode {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(icons::ROCKET).size(16.0));
            ui.heading(format!("Added “{}”", prompt.label));
        });
        ui.separator();
        ui.add_space(8.0);

        if let Some(err) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, err);
            ui.add_space(6.0);
        }

        let show_later = prompt.pick.picked.is_none() && !prompt.pick.dialog_pending;
        if show_later {
            ui.label("Map a local save file to it now, or later from its page?");
            ui.add_space(10.0);
        }

        // A just created instance has nothing on the server yet, so there's no
        // local vs server question here.
        let done =
            file_pick(ui, &mut prompt.pick, &prompt.instance_id, &prompt.label, &prompt.console.name, &prompt.console.valid_save_sizes, None, Some(&prompt.console.slug), &mut self.error, outcome);

        if show_later && ui.button("I'll map it later").clicked() {
            *outcome = HomeOutcome::Refresh;
            self.error = None;
            self.finish_add();
            return Mode::List;
        }
        if done {
            self.finish_add();
            return Mode::List;
        }
        Mode::MapPrompt(prompt)
    }

    // ---- instance detail ------------------------------------------

    fn detail_view(&mut self, ui: &mut egui::Ui, view: &HomeView<'_>, outcome: &mut HomeOutcome, mut state: DetailState) -> Mode {
        let mut back = false;
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                back = true;
            }
        });
        if back {
            return Mode::List;
        }

        let Some(inst) = view.catalog.iter().find(|g| g.id == state.instance_id) else {
            return Mode::List; // instance vanished from the catalog
        };

        ui.horizontal(|ui| {
            ui.add(egui::Image::new(box_art_url(view.api_base, inst)).fit_to_exact_size(egui::vec2(48.0, 64.0)).corner_radius(4.0).show_loading_spinner(false));
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(display_title(inst)).strong().size(15.0));
                ui.weak(subtitle(inst));
            });
        });
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        if let Some(err) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, err);
            ui.add_space(6.0);
        }

        ui.label(format!("Last save on server: {}", inst.last_saved_at.as_ref().map_or("none yet", |t| t.as_str())));

        let book = Store::get(|s| s.instance(&state.instance_id)).ok().flatten();

        if let Some(conflict) = book.as_ref().and_then(|b| b.conflict.as_ref()) {
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.colored_label(ui.visuals().warn_fg_color, egui::RichText::new("Conflict").strong());
                ui.label("This save changed here and on another device since the last sync.");
                ui.small(format!("detected {}", conflict.detected_at));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Keep my local copy").clicked() {
                        *outcome = HomeOutcome::ResolveConflict { instance_id: state.instance_id.clone(), keep_local: true };
                    }
                    if ui.button("Keep the server copy").clicked() {
                        *outcome = HomeOutcome::ResolveConflict { instance_id: state.instance_id.clone(), keep_local: false };
                    }
                });
            });
        }
        ui.add_space(10.0);

        match &book {
            None => {
                ui.label("Not mapped to a local save file.");
                ui.add_space(8.0);
                if file_pick(
                    ui,
                    &mut state.pick,
                    &state.instance_id,
                    display_title(inst),
                    &inst.console_name,
                    &state.console_sizes,
                    inst.latest_save.as_ref(),
                    Some(&inst.console_slug),
                    &mut self.error,
                    outcome,
                ) {
                    self.reload_mapped();
                    state.pick = FilePick::default();
                }
            }
            Some(b) => {
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Local file:");
                    ui.monospace(b.save_path.display().to_string());
                });
                if let Some(dir) = b.save_path.parent()
                    && ui.button("Open folder").clicked()
                {
                    open_path(dir);
                }
                ui.add_space(4.0);
                ui.label(format!("Status: {}", instance_status(b, inst)));
                ui.add_space(8.0);

                let mut paused = b.paused;
                if ui.checkbox(&mut paused, "Pause syncing this game").changed() {
                    match Store::write(|s| s.set_paused(&state.instance_id, paused)) {
                        Ok(()) => *outcome = HomeOutcome::RecheckInstance { instance_id: state.instance_id.clone() },
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui.button("Sync now").clicked() {
                        *outcome = HomeOutcome::RecheckInstance { instance_id: state.instance_id.clone() };
                    }
                    if state.confirm_unmap {
                        if ui.button("Cancel").clicked() {
                            state.confirm_unmap = false;
                        }
                        if ui.button(egui::RichText::new("Really unmap").color(ui.visuals().error_fg_color)).clicked() {
                            match Store::write(|s| s.unbind_instance(&state.instance_id)) {
                                Ok(()) => {
                                    self.reload_mapped();
                                    state.confirm_unmap = false;
                                    *outcome = HomeOutcome::RecheckInstance { instance_id: state.instance_id.clone() };
                                }
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }
                    } else if ui.button("Unmap").clicked() {
                        state.confirm_unmap = true;
                    }
                });
                ui.small("Unmapping keeps the game on your account and the file on disk; it just stops syncing here.");
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.hyperlink_to(format!("Rename, star or add notes on {} ↗", view.branding.identity.name), homepage_path(view.branding, "games"));

        Mode::Detail(state)
    }

    /// Drop add-game scratch state and re-read which instances are mapped.
    fn finish_add(&mut self) {
        self.search.clear();
        self.consoles = None;
        self.games = None;
        self.create = None;
        self.reload_mapped();
    }

    /// Result of an off-thread picker `App` ran for us. `None` = cancelled.
    /// Ignored unless the matching flow is still on screen.
    pub fn deliver_save_pick(&mut self, instance_id: &str, path: Option<PathBuf>) {
        let sizes: Vec<u64> = match &self.mode {
            Mode::MapPrompt(p) if p.instance_id == instance_id => p.console.valid_save_sizes.clone(),
            Mode::Detail(d) if d.instance_id == instance_id => d.console_sizes.clone(),
            _ => return,
        };
        let picked = match path {
            Some(file) => match PickedSave::inspect(file, &sizes) {
                some @ Some(_) => some,
                None => {
                    self.error = Some("Couldn't read that file. Is the emulator still writing to it?".to_owned());
                    None
                }
            },
            None => None, // cancelled
        };
        let pick = match &mut self.mode {
            Mode::MapPrompt(p) if p.instance_id == instance_id => &mut p.pick,
            Mode::Detail(d) if d.instance_id == instance_id => &mut d.pick,
            _ => return,
        };
        pick.dialog_pending = false;
        if picked.is_some() {
            pick.picked = picked;
            pick.override_size = false;
        }
    }
}

/// The shared "pick an existing save file, size-check, decide local vs server,
/// bind" flow. Sets `outcome` to `OpenSaveDialog` when a picker is requested;
/// returns `true` once the mapping is written (the caller emits
/// `MappedInstance`).
///
/// `remote` is the instance's newest save on the server, if any. When it differs
/// from the picked file we make the user choose which one wins, and seed the
/// sync baseline so the engine's next reconcile does the matching push or pull.
#[allow(clippy::too_many_arguments)]
fn file_pick(
    ui: &mut egui::Ui,
    pick: &mut FilePick,
    instance_id: &str,
    label: &str,
    console_name: &str,
    sizes: &[u64],
    remote: Option<&SaveMeta>,
    console_slug: Option<&str>,
    error: &mut Option<String>,
    outcome: &mut HomeOutcome,
) -> bool {
    if pick.dialog_pending {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Waiting for the picker…");
        });
        return false;
    }

    let mut open_file = false;

    let Some(picked) = &pick.picked else {
        ui.horizontal(|ui| {
            open_file = ui.button("Choose save file…").clicked();
        });
        ui.small("Save once in your emulator so the file exists, then pick it here.");
        if open_file {
            pick.dialog_pending = true;
            pick.override_size = false;
            *outcome = HomeOutcome::OpenSaveDialog { instance_id: instance_id.to_owned(), title: format!("Save file for {label}") };
        }
        return false;
    };

    ui.horizontal_wrapped(|ui| {
        ui.strong("File:");
        ui.monospace(picked.path.display().to_string());
    });
    ui.label(format!("Size: {}", mapping::human_size(picked.size)));
    ui.add_space(8.0);

    let blocked = !picked.size_ok && !pick.override_size;
    if blocked {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("That's {}, but {console_name} saves are usually {}. This might be the wrong file.", mapping::human_size(picked.size), mapping::describe_sizes(sizes)),
        );
        ui.add_space(6.0);
    }

    // `Some` only when the server holds a save that differs from this file.
    let diverging = remote.filter(|r| r.content_hash != picked.hash);

    // `Some(seed)` once the user has committed; `seed` is the baseline hash +
    // save id to record after binding (`None` = no server save, leave it unset
    // so the engine uploads this file).
    let mut commit: Option<Option<(String, String)>> = None;

    if ui.button("Choose a different file").clicked() {
        open_file = true;
    }
    if blocked {
        if ui.button("Use it anyway").clicked() {
            pick.override_size = true;
        }
    } else if let Some(r) = diverging {
        ui.colored_label(ui.visuals().warn_fg_color, "Server file mismatch.");
        ui.horizontal(|ui| {
            if ui.button("Use local").on_hover_text("This file becomes the newest save; the server copy stays in history.").clicked() {
                commit = Some(Some((r.content_hash.clone(), r.id.clone())));
            }
            if ui.button(egui::RichText::new("Overwrite with remote").color(ui.visuals().warn_fg_color)).on_hover_text("Replaces this file with the server copy.").clicked() {
                commit = Some(Some((picked.hash.clone(), r.id.clone())));
            }
        });
    } else if ui.button("Confirm, map this file").clicked() {
        commit = Some(remote.map(|r| (picked.hash.clone(), r.id.clone())));
    }

    if !blocked && let Some(r) = diverging {
        ui.add_space(4.0);
        ui.small(format!("Server copy: {}, uploaded {}.", mapping::human_size(r.size_bytes), r.uploaded_at));
    }

    if open_file {
        pick.dialog_pending = true;
        pick.override_size = false;
        *outcome = HomeOutcome::OpenSaveDialog { instance_id: instance_id.to_owned(), title: format!("Save file for {label}") };
        return false;
    }

    if let Some(seed) = commit {
        let path = picked.path.clone();
        let write = Store::write(|s| {
            s.bind_instance(instance_id, &path, console_slug)?;
            match &seed {
                Some((hash, save_id)) => s.record_synced(instance_id, hash, save_id),
                None => Ok(()),
            }
        });
        match write {
            Ok(()) => {
                *error = None;
                *outcome = HomeOutcome::MappedInstance { instance_id: instance_id.to_owned() };
                return true;
            }
            Err(e) => *error = Some(format!("Couldn't save the mapping: {e:#}")),
        }
    }
    false
}

/// One-line sync status for the detail page, from the same [`reconcile`] the
/// engine uses (raw verdict, `ask` policy).
fn instance_status(book: &InstanceRecord, inst: &GameInstance) -> String {
    if book.conflict.is_some() {
        return "conflict, resolve above".to_owned();
    }
    let local = match LocalFile::read(&book.save_path) {
        Ok(local) => local,
        Err(_) => return "can't read the local file".to_owned(),
    };
    match reconcile(&local, inst.latest_save.as_ref(), book, ConflictPolicy::Ask) {
        Action::Idle | Action::MarkSynced => "in sync".to_owned(),
        Action::Pull => "a newer save is on the server".to_owned(),
        Action::Push => "local changes not yet uploaded".to_owned(),
        Action::Conflict => "conflict".to_owned(),
    }
}

enum Polled<T> {
    None,
    Running,
    Done(T),
    Failed(String),
}

/// Poll a task without holding a borrow of it, so the caller can clear it in the
/// same breath. Only worth it for one-shot tasks whose result is consumed
/// immediately (here: `create`).
fn poll_owned<T: Clone + Send + 'static>(task: &mut Option<Task<T>>) -> Polled<T> {
    match task.as_mut().map(Task::state) {
        None => Polled::None,
        Some(TaskState::Running) => Polled::Running,
        Some(TaskState::Done(v)) => Polled::Done(v.clone()),
        Some(TaskState::Failed(e)) => Polled::Failed(e.to_owned()),
    }
}

fn some_trimmed(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

// ---- background task ----------------------------------------------------

struct Task<T> {
    rx: mpsc::Receiver<Result<T, String>>,
    cached: Option<Result<T, String>>,
}

enum TaskState<'a, T> {
    Running,
    Done(&'a T),
    Failed(&'a str),
}

impl<T: Send + 'static> Task<T> {
    fn spawn(ctx: egui::Context, job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("home-fetch".into())
            .spawn(move || {
                let _ = tx.send(job());
                ctx.request_repaint();
            })
            .expect("spawn home-fetch thread");
        Self { rx, cached: None }
    }

    fn state(&mut self) -> TaskState<'_, T> {
        if self.cached.is_none() {
            match self.rx.try_recv() {
                Ok(result) => self.cached = Some(result),
                Err(mpsc::TryRecvError::Empty) => return TaskState::Running,
                Err(mpsc::TryRecvError::Disconnected) => self.cached = Some(Err("the request was dropped".to_owned())),
            }
        }
        match self.cached.as_ref().unwrap() {
            Ok(value) => TaskState::Done(value),
            Err(msg) => TaskState::Failed(msg),
        }
    }
}

// ---- card + grid ------------------------------------------------------

struct CardData {
    title: String,
    subtitle: String,
    art_url: String,
    key: String,
    dot: Option<egui::Color32>,
    hover: String,
}

impl CardData {
    fn plain(key: &str, title: &str, subtitle: &str, art_url: String) -> Self {
        Self { title: title.to_owned(), subtitle: subtitle.to_owned(), art_url, key: key.to_owned(), dot: None, hover: title.to_owned() }
    }
}

fn instance_card(inst: &GameInstance, api_base: &str, mapped: bool) -> CardData {
    let dot = Some(if mapped { egui::Color32::from_rgb(0x3f, 0xb9, 0x50) } else { egui::Color32::from_rgb(0xd0, 0x9a, 0x2a) });
    let hover = if mapped { display_title(inst).to_owned() } else { format!("{}, not mapped to a local save yet", display_title(inst)) };
    CardData { title: display_title(inst).to_owned(), subtitle: subtitle(inst), art_url: box_art_url(api_base, inst), key: inst.id.clone(), dot, hover }
}

const CARD_GAP: f32 = 8.0;

/// Column width for a two-wide grid in `ui`. One middle gap; the -1 keeps
/// rounding from ever overflowing the row and eating the right margin.
fn card_width(ui: &egui::Ui) -> f32 {
    ((ui.available_width() - CARD_GAP - 1.0) / 2.0).floor().max(90.0)
}

/// One grid row (up to two cards); sets `clicked` to the flat index if hit.
fn card_row(ui: &mut egui::Ui, row: usize, chunk: &[CardData], card_w: f32, clicked: &mut Option<usize>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (col, data) in chunk.iter().enumerate() {
            if col > 0 {
                ui.add_space(CARD_GAP);
            }
            if card(ui, data, card_w).clicked() {
                *clicked = Some(row * 2 + col);
            }
        }
    });
    ui.add_space(CARD_GAP);
}

/// Two-wide card grid, every row laid out. Fine for account-sized lists.
fn render_grid(ui: &mut egui::Ui, cards: &[CardData]) -> Option<usize> {
    let card_w = card_width(ui);
    let mut clicked = None;
    for (row, chunk) in cards.chunks(2).enumerate() {
        card_row(ui, row, chunk, card_w, &mut clicked);
    }
    clicked
}

/// Two-wide card grid, virtualized: builds its own vertical `ScrollArea` and
/// only lays out rows near the viewport, so a 1500-entry catalog only touches
/// (and only requests art for) what's on or just off screen.
fn render_grid_virtual(ui: &mut egui::Ui, cards: &[CardData]) -> Option<usize> {
    // `row_h` picks the visible range; it only has to be about right (card art
    // is re-measured per row inside). `card()`'s own visibility check is the
    // real gate on art requests.
    let row_h = card_art_h(card_width(ui)) + CARD_STRIP_H + CARD_GAP;
    let rows = cards.len().div_ceil(2);
    let mut clicked = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, rows, |ui, range| {
        let card_w = card_width(ui);
        for row in range {
            let chunk = &cards[row * 2..((row + 1) * 2).min(cards.len())];
            card_row(ui, row, chunk, card_w, &mut clicked);
        }
    });
    clicked
}

/// Card = box art (3:4) on top, a fixed info strip below.
const CARD_STRIP_H: f32 = 40.0;
/// Load art for cards within this many px of the viewport, so scrolling doesn't
/// chase the images.
const CARD_PREFETCH: f32 = 240.0;

fn card_art_h(width: f32) -> f32 {
    (width * 4.0 / 3.0).round()
}

fn card(ui: &mut egui::Ui, data: &CardData, width: f32) -> egui::Response {
    const RADIUS: f32 = 8.0;
    let art_h = card_art_h(width);

    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, art_h + CARD_STRIP_H), egui::Sense::click());
    // Skip the paint (and, crucially, the art request in `egui::Image`) for cards
    // well outside the viewport. `expand2` widens the test vertically so cards
    // just below the fold still preload.
    if !ui.is_rect_visible(rect.expand2(egui::vec2(0.0, CARD_PREFETCH))) {
        return resp;
    }

    let painter = ui.painter().clone();
    painter.rect_filled(rect, RADIUS, ui.visuals().faint_bg_color);

    let art_rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, art_h));
    let (bg, initials) = placeholder(&data.key, &data.title);
    painter.rect_filled(art_rect, RADIUS, bg);
    painter.text(art_rect.center(), egui::Align2::CENTER_CENTER, initials, egui::FontId::proportional(20.0), egui::Color32::from_white_alpha(210));
    egui::Image::new(data.art_url.clone()).show_loading_spinner(false).corner_radius(RADIUS).paint_at(ui, art_rect);

    let strip = egui::Rect::from_min_max(egui::pos2(rect.min.x, art_rect.max.y), rect.max).shrink2(egui::vec2(8.0, 5.0));
    let mut strip_ui = ui.new_child(egui::UiBuilder::new().max_rect(strip).layout(egui::Layout::top_down(egui::Align::Min)));
    strip_ui.spacing_mut().item_spacing.y = 1.0;
    strip_ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        if let Some(dot) = data.dot {
            let (r, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot);
        }
        ui.add(egui::Label::new(egui::RichText::new(&data.title).size(12.5)).truncate());
    });
    if !data.subtitle.is_empty() {
        strip_ui.add(egui::Label::new(egui::RichText::new(&data.subtitle).size(10.0).weak()).truncate());
    }

    if resp.hovered() {
        painter.rect_stroke(rect, RADIUS, egui::Stroke::new(1.0, ui.visuals().widgets.hovered.bg_stroke.color), egui::StrokeKind::Inside);
    }
    resp.on_hover_text(&data.hover)
}

// ---- shared UI bits ------------------------------------------------

/// A picker header: `[Back]  <title> · <subtitle>` then a full-width search box.
/// Returns `true` if Back was pressed.
fn picker_header(ui: &mut egui::Ui, search: &mut String, title: &str, subtitle: &str) -> bool {
    let mut back = false;
    ui.horizontal(|ui| {
        if ui.button("Back").clicked() {
            back = true;
        }
        ui.label(egui::RichText::new(title).strong());
        ui.weak(format!("· {subtitle}"));
    });
    ui.add_space(4.0);
    ui.add(egui::TextEdit::singleline(search).hint_text("Search").desired_width(ui.available_width()));
    ui.add_space(6.0);
    back
}

fn centered_spinner(ui: &mut egui::Ui, label: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        ui.spinner();
        ui.add_space(6.0);
        ui.weak(label);
    });
}

fn retry_state(ui: &mut egui::Ui, err: &str) {
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.colored_label(ui.visuals().error_fg_color, "Couldn't load that from the server.");
        ui.add_space(4.0);
        ui.small(err);
    });
}

fn heading(ui: &mut egui::Ui, label: &str, count: usize) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(format!("{label}  ({count})")).strong().size(12.0));
    ui.add_space(4.0);
}

fn empty_state(ui: &mut egui::Ui, branding: &Branding, message: &str, link: Option<&str>) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.weak(message);
        if let Some(text) = link {
            ui.add_space(6.0);
            ui.hyperlink_to(format!("{text} on {} ↗", branding.identity.name), homepage_path(branding, "games"));
        }
    });
}

// ---- pure helpers ---------------------------------------------------

fn display_title(inst: &GameInstance) -> &str {
    inst.custom_name.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&inst.name)
}

fn base_name(inst: &GameInstance) -> &str {
    inst.default_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&inst.name)
}

fn subtitle(inst: &GameInstance) -> String {
    let base = base_name(inst);
    if display_title(inst) != base { format!("{} · {}", inst.console_name, base) } else { inst.console_name.clone() }
}

/// Mirrors 2032's `boxArtUrl`: curated art for unlinked instances, IGDB when we
/// have a resolved cover but no curated file, our endpoint otherwise.
fn box_art_url(api_base: &str, inst: &GameInstance) -> String {
    let base = format!("{}/api", api_base.trim_end_matches('/'));
    match &inst.game_slug {
        None => format!("{base}/consoles/{}/box_art.png", inst.console_slug),
        Some(game_slug) => {
            if !inst.art.has_box_art
                && let Some(id) = &inst.art.igdb_image_id
            {
                format!("https://images.igdb.com/igdb/image/upload/t_cover_big/{id}.jpg")
            } else {
                format!("{base}/consoles/{}/games/{game_slug}/box_art.png", inst.console_slug)
            }
        }
    }
}

/// A stable colour + one/two initials for the art placeholder behind the image.
fn placeholder(key: &str, title: &str) -> (egui::Color32, String) {
    let hash = key.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let hue = (hash % 360) as f32 / 360.0;
    let color = egui::Color32::from(egui::ecolor::Hsva::new(hue, 0.38, 0.42, 1.0));
    let initials: String = title.split_whitespace().filter_map(|w| w.chars().next()).take(2).flat_map(|c| c.to_uppercase()).collect();
    (color, if initials.is_empty() { "?".to_owned() } else { initials })
}

fn instance_score(matcher: &SkimMatcherV2, inst: &GameInstance, query: &str) -> Option<i64> {
    [Some(inst.name.as_str()), inst.default_name.as_deref(), inst.custom_name.as_deref()].into_iter().flatten().filter_map(|hay| matcher.fuzzy_match(hay, query)).max()
}

fn recency_key(inst: &GameInstance) -> Option<&str> {
    inst.last_saved_at.as_ref().map(|t| t.as_str())
}

/// Score a catalog row (console / game): a name hit outranks any description
/// hit; a description-only hit still counts.
fn cat_score(matcher: &SkimMatcherV2, name: &str, desc: &str, query: &str) -> Option<i64> {
    match (matcher.fuzzy_match(name, query), matcher.fuzzy_match(desc, query)) {
        (Some(n), _) => Some(n + 500),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }
}

fn filter_sorted<'a, T>(items: &'a [T], query: &str, name: impl Fn(&T) -> &str, desc: impl Fn(&T) -> &str) -> Vec<&'a T> {
    if query.is_empty() {
        return items.iter().collect();
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut scored: Vec<(&T, i64)> = items.iter().filter_map(|it| cat_score(&matcher, name(it), desc(it), query).map(|s| (it, s))).collect();
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(it, _)| it).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::InstanceArt;

    fn gi(id: &str, name: &str, default_name: Option<&str>, custom_name: Option<&str>, game_slug: Option<&str>) -> GameInstance {
        GameInstance {
            id: id.into(),
            console_slug: "gba".into(),
            console_name: "Game Boy Advance".into(),
            game_slug: game_slug.map(str::to_owned),
            name: name.into(),
            default_name: default_name.map(str::to_owned),
            custom_name: custom_name.map(str::to_owned),
            last_saved_at: None,
            starred_count: 0,
            unstarred_count: 0,
            highlights: vec![],
            latest_save: None,
            art: InstanceArt { has_icon: false, has_box_art: false, igdb_image_id: None },
        }
    }

    #[test]
    fn title_prefers_nonblank_custom_name() {
        assert_eq!(display_title(&gi("1", "Mother 3", None, Some("100% Completion"), Some("mother-3"))), "100% Completion");
        assert_eq!(display_title(&gi("1", "Mother 3", None, Some("   "), Some("mother-3"))), "Mother 3");
        assert_eq!(display_title(&gi("1", "Mother 3", None, None, Some("mother-3"))), "Mother 3");
    }

    #[test]
    fn subtitle_adds_real_name_only_when_renamed() {
        assert_eq!(subtitle(&gi("1", "Mother 3", Some("Mother 3"), None, Some("mother-3"))), "Game Boy Advance");
        assert_eq!(subtitle(&gi("1", "Mother 3", Some("Mother 3"), Some("100% run"), Some("mother-3"))), "Game Boy Advance · Mother 3");
    }

    #[test]
    fn fuzzy_matches_either_name_across_fragments() {
        let m = SkimMatcherV2::default().ignore_case();
        let inst = gi("1", "100% Completion", Some("Mother 3"), Some("100% Completion"), Some("mother-3"));
        for q in ["100", "mother", "comp", "3", "cmpltn"] {
            assert!(instance_score(&m, &inst, q).is_some(), "expected {q:?} to match");
        }
        assert!(instance_score(&m, &inst, "zelda").is_none());
    }

    #[test]
    fn cat_score_prefers_name_hits() {
        let m = SkimMatcherV2::default().ignore_case();
        let name_hit = cat_score(&m, "Mother 3", "a quirky rpg", "mother").unwrap();
        let desc_hit = cat_score(&m, "Chrono Trigger", "a time-travel rpg", "rpg").unwrap();
        assert!(name_hit > desc_hit);
        assert!(cat_score(&m, "Mother 3", "rpg", "zelda").is_none());
    }

    #[test]
    fn filter_sorted_passes_everything_through_unranked_when_empty() {
        let items = ["b", "a", "c"];
        let out = filter_sorted(&items, "", |s| *s, |_| "");
        assert_eq!(out, [&"b", &"a", &"c"]);
    }

    #[test]
    fn box_art_url_follows_the_2032_fallback_chain() {
        let custom = gi("1", "My Hack", None, None, None);
        assert_eq!(box_art_url("https://cr.2032.cloud/", &custom), "https://cr.2032.cloud/api/consoles/gba/box_art.png");

        let mut igdb = gi("1", "Mother 3", Some("Mother 3"), None, Some("mother-3"));
        igdb.art.igdb_image_id = Some("co1abc".into());
        assert_eq!(box_art_url("https://cr.2032.cloud", &igdb), "https://images.igdb.com/igdb/image/upload/t_cover_big/co1abc.jpg");

        let mut curated = gi("1", "Mother 3", Some("Mother 3"), None, Some("mother-3"));
        curated.art.has_box_art = true;
        curated.art.igdb_image_id = Some("co1abc".into());
        assert_eq!(box_art_url("https://cr.2032.cloud", &curated), "https://cr.2032.cloud/api/consoles/gba/games/mother-3/box_art.png");
    }

    #[test]
    fn recency_orders_newest_first_then_no_save_last() {
        let mut a = gi("a", "A", None, None, None);
        let mut b = gi("b", "B", None, None, None);
        let c = gi("c", "C", None, None, None);
        a.last_saved_at = Some(crate::api::Timestamp("2026-08-01 00:00:00".into()));
        b.last_saved_at = Some(crate::api::Timestamp("2026-08-28 00:00:00".into()));
        let mut v = [&a, &b, &c];
        v.sort_by(|x, y| recency_key(y).cmp(&recency_key(x)));
        assert_eq!(v.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["b", "a", "c"]);
    }
}
