use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::constants::*;

static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub enum LoginState {
    #[default]
    None,

    LoggedIn {
        session_id: Arc<str>,
    },
}

impl Serialize for LoginState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::None => serializer.serialize_none(),
            Self::LoggedIn { session_id } => serializer.serialize_str(session_id),
        }
    }
}

impl<'de> Deserialize<'de> for LoginState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let token = Option::<String>::deserialize(deserializer)?;

        Ok(match token {
            Some(token) => Self::LoggedIn { session_id: Arc::from(token) },
            None => Self::None,
        })
    }
}

/// A polling cadence: automatic, disabled, or a fixed interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollInterval {
    /// Let the daemon decide (stream-driven, with a slow safety-net poll).
    Auto,
    /// Never poll on a timer.
    Off,
    /// Poll on this fixed interval.
    Every(Duration),
}

impl Serialize for PollInterval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            PollInterval::Auto => s.serialize_str("auto"),
            PollInterval::Off => s.serialize_str("off"),
            PollInterval::Every(d) => s.serialize_str(&format_duration(d.as_secs())),
        }
    }
}

impl<'de> Deserialize<'de> for PollInterval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(PollInterval::Auto),
            "off" | "never" | "none" | "0" => Ok(PollInterval::Off),
            other => parse_duration(other)
                .map(PollInterval::Every)
                .ok_or_else(|| serde::de::Error::custom(format!(r#"invalid poll interval {other:?}: use "auto", "off", or a value like "30s", "5m", "1h""#))),
        }
    }
}

/// A required, explicit interval (no auto/off) e.g. the update-check cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval(pub Duration);

impl Serialize for Interval {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(self.0.as_secs()))
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        parse_duration(raw.trim()).filter(|d| !d.is_zero()).map(Interval).ok_or_else(|| serde::de::Error::custom(format!(r#"invalid interval {raw:?}: use a value like "6h" or "30m""#)))
    }
}

/// Parse `"<n>"` (seconds), or `"<n>"` suffixed with `s`, `m`, `h`, or `d`.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let last = s.chars().next_back()?;
    let (digits, mult): (&str, u64) = match last.to_ascii_lowercase() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3_600),
        'd' => (&s[..s.len() - 1], 86_400),
        '0'..='9' => (s, 1),
        _ => return None,
    };
    let n: u64 = digits.trim().parse().ok()?;
    n.checked_mul(mult).map(Duration::from_secs)
}

/// Render seconds back to the largest whole unit (`"2h"`, `"90s"`, …).
fn format_duration(secs: u64) -> String {
    for (unit, sym) in [(86_400, 'd'), (3_600, 'h'), (60, 'm')] {
        if secs != 0 && secs.is_multiple_of(unit) {
            return format!("{}{sym}", secs / unit);
        }
    }
    format!("{secs}s")
}

macro_rules! kebab_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name { $($variant),+ }
    };
}

kebab_enum!(
    /// When a watched save file changing on disk turns into an upload.
    UploadTrigger { OnChange, OnEmulatorExit, Manual }
);
kebab_enum!(
    /// What to do when a save diverged both locally and on the server.
    ConflictPolicy { Ask, PreferLocal, PreferRemote, PreferNewest }
);
kebab_enum!(
    /// `Account` follows the server setting from `GET /api/me`.
    Theme { Account, Auto, Light, Dark }
);
kebab_enum!(UpdateChannel { Stable, Prerelease });
kebab_enum!(
    /// How far the updater goes on its own once a release is available.
    UpdateAction { Notify, Download, Install }
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Startup {
    /// Register with the OS to launch at login.
    pub launch_on_login: bool,
    /// Start with the windows hidden to the tray.
    pub start_hidden: bool,
    /// The user answered "Not now" to the first-run install prompt; don't offer
    /// it again (they can still install from Config › Startup).
    pub skip_install_prompt: bool,
    /// The first-run flow (including the "CoinCell lives in the tray" explainer)
    /// has been completed. Until then the window starts visible, ignores
    /// `start_hidden`, and never auto-hides.
    pub onboarded: bool,
}

impl Default for Startup {
    fn default() -> Self {
        Self { launch_on_login: false, start_hidden: true, skip_install_prompt: false, onboarded: false }
    }
}

/// The `[sync]` section. Named `SyncSettings` to avoid shadowing `std::marker::Sync`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    /// Master switch. `false` pauses all syncing.
    pub enabled: bool,
    pub poll: PollInterval,
    pub upload_trigger: UploadTrigger,
    pub conflict: ConflictPolicy,
    /// Pause network activity on a metered connection (Windows).
    pub pause_on_metered: bool,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self { enabled: true, poll: PollInterval::Auto, upload_trigger: UploadTrigger::OnChange, conflict: ConflictPolicy::Ask, pause_on_metered: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    pub on_pull: bool,
    pub on_conflict: bool,
    pub on_error: bool,
    pub on_session_expired: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true, on_pull: true, on_conflict: true, on_error: true, on_session_expired: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme: Theme::Account }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowRules {
    /// Auto-minimise a window when it loses focus. Off by default: it's fiddly
    /// around transient focus changes, and the `—` button, `Esc`, and the tray
    /// click are all reliable manual ways to hide now. Those work regardless of
    /// this.
    pub hide_on_focus_loss: bool,
    /// egui zoom factor. `1.0` is native; clamped to `UI_SCALE_RANGE` on use.
    pub ui_scale: f32,
}

impl Default for WindowRules {
    fn default() -> Self {
        Self { hide_on_focus_loss: false, ui_scale: 1.0 }
    }
}

/// Accepted range for [`WindowRules::ui_scale`].
pub const UI_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=3.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Updates {
    pub channel: UpdateChannel,
    pub auto_check: bool,
    pub check_interval: Interval,
    pub on_update: UpdateAction,
}

impl Default for Updates {
    fn default() -> Self {
        Self { channel: UpdateChannel::Stable, auto_check: true, check_interval: Interval(Duration::from_secs(24 * 3_600)), on_update: UpdateAction::Notify }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// Base URL for the device API. Override for local dev / self-host.
    pub api_base: Arc<str>,
    pub log_level: LogLevel,
    /// Opt-in anonymous crash reporting. `None` = not yet asked; the first-run
    /// prompt writes `Some(_)` once the user answers, and the key stays absent
    /// from `config.toml` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_reports: Option<bool>,
}

impl Default for Advanced {
    fn default() -> Self {
        Self { api_base: Arc::from(API_BASE_ROUTE), log_level: LogLevel::Info, crash_reports: None }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "session_id")]
    login_state: LoginState,
    // Flattened catch-all: keep it directly after the only other scalar field so
    // any unknown scalar it carries still serialises before the [section] tables
    // below (TOML rejects a bare key after a table). Also means removed keys
    // aren't destroyed on the next save.
    #[serde(flatten)]
    extra: toml::Table,

    pub startup: Startup,
    pub sync: SyncSettings,
    pub notifications: Notifications,
    pub appearance: Appearance,
    pub window: WindowRules,
    pub updates: Updates,
    pub advanced: Advanced,
}

impl Config {
    pub fn init() {
        let _ = CONFIG.set(Mutex::new(Self::load()));
    }

    fn slot() -> &'static Mutex<Config> {
        CONFIG.get().expect("Config::init() must run before Config::get / Config::update")
    }

    pub fn get<T>(f: impl FnOnce(&Config) -> T) -> T {
        let guard = Self::slot().lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    pub fn update(f: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        let mut guard = Self::slot().lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard);
        guard.save()
    }

    pub fn session_id(&self) -> Option<Arc<str>> {
        match &self.login_state {
            LoginState::LoggedIn { session_id } => Some(session_id.clone()),
            LoginState::None => None,
        }
    }

    pub fn set_session(&mut self, session_id: Arc<str>) {
        self.login_state = LoginState::LoggedIn { session_id };
    }

    /// Drop the stored session. Used on logout, including when the server-side
    /// revoke call couldn't be reached.
    pub fn clear_session(&mut self) {
        self.login_state = LoginState::None;
    }

    /// `true` once the user has answered the crash-reporting prompt either way.
    pub fn crash_reports_answered(&self) -> bool {
        self.advanced.crash_reports.is_some()
    }

    pub fn set_crash_reports(&mut self, enabled: bool) {
        self.advanced.crash_reports = Some(enabled);
    }

    /// The stored UI scale, clamped to the supported range.
    pub fn ui_scale(&self) -> f32 {
        self.window.ui_scale.clamp(*UI_SCALE_RANGE.start(), *UI_SCALE_RANGE.end())
    }

    fn path() -> anyhow::Result<PathBuf> {
        fs::create_dir_all(*CONFIG_DIR)?;
        Ok(CONFIG_DIR.join("config.toml"))
    }

    fn load() -> Config {
        // Bootstrap: this runs before `logging::init`, so it prints directly.
        let config = match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("config: load failed, using defaults ({e:#})");
                Config::default()
            }
        };
        let _ = config.save();
        config
    }

    fn try_load() -> anyhow::Result<Config> {
        let path = Self::path()?;

        let Some(contents) = read_if_exists(&path)? else {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        };

        match toml::from_str(&contents) {
            Ok(cfg) => Ok(cfg),
            Err(e) => {
                let backup = back_up(&path)?;
                eprintln!("config: invalid ({e}), backed up to {}", backup.display());
                let cfg = Config::default();
                cfg.save()?;
                Ok(cfg)
            }
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        let rendered = toml::to_string_pretty(self)?;
        // write_atomic(&Self::path()?, &comment_unknown_keys(&rendered, &self.extra))
        write_atomic(&Self::path()?, &rendered)
    }
}

// fn comment_unknown_keys(rendered: &str, extra: &toml::Table) -> String {
//     if extra.is_empty() {
//         return rendered.to_owned();
//     }

//     const NOTE: &str = "# LOST AND FOUND (idk what these keys are for but i'm keeping them cus im nice)";
//     let mut seen = std::collections::HashSet::new();
//     let mut out = String::with_capacity(rendered.len() + extra.len() * (NOTE.len() + 1));
//     let mut note_inserted = false;

//     for line in rendered.lines() {
//         if let Some(key) = top_level_key(line)
//             && extra.contains_key(key)
//             && seen.insert(key.to_owned())
//             && !note_inserted
//         {
//             note_inserted = true;
//             out.push_str(NOTE);
//             out.push('\n');
//         }
//         out.push_str(line);
//         out.push('\n');
//     }
//     out
// }

// fn top_level_key(line: &str) -> Option<&str> {
//     if line.trim().is_empty() || line.starts_with([' ', '\t', '#']) {
//         return None;
//     }
//     let body = line.trim_start_matches('[');
//     let end = body.find(['=', '.', ']', ' ']).unwrap_or(body.len());
//     let key = body[..end].trim().trim_matches('"');
//     (!key.is_empty()).then_some(key)
// }

fn read_if_exists(path: &Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config.toml");
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));

    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn back_up(path: &Path) -> anyhow::Result<PathBuf> {
    let target = (0u32..)
        .map(|n| {
            let name = if n == 0 { "config.bak.toml".to_owned() } else { format!("config.bak.{n}.toml") };
            path.with_file_name(name)
        })
        .find(|candidate| !candidate.exists())
        .expect("an infinite range always yields a free filename");

    fs::rename(path, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let src = "\
future_flag = true
nickname = \"cell\"

[future_section]
retries = 3
";
        let cfg: Config = toml::from_str(src).expect("parse");
        // let rewritten = comment_unknown_keys(&toml::to_string_pretty(&cfg).expect("serialize"), &cfg.extra);
        let rewritten = toml::to_string_pretty(&cfg).expect("serialize");
        let reparsed: Config = toml::from_str(&rewritten).expect("re-parse");

        assert_eq!(reparsed.extra, cfg.extra);
        assert!(cfg.extra.contains_key("future_flag"));
        assert!(cfg.extra.contains_key("future_section"));
    }

    // #[test]
    // fn unknown_keys_get_a_comment() {
    //     let cfg: Config = toml::from_str("future_flag = true\n\n[future_section]\nretries = 3\n").expect("parse");
    //     let out = comment_unknown_keys(&toml::to_string_pretty(&cfg).expect("serialize"), &cfg.extra);

    //     assert_eq!(out.matches("LOST AND FOUND").count(), 1, "exactly one lost-and-found note:\n{out}");
    //     // A config with no unknown keys must not be tagged.
    //     let recognised: Config = toml::from_str("").expect("parse");
    //     assert!(!comment_unknown_keys(&toml::to_string_pretty(&recognised).expect("serialize"), &recognised.extra).contains("LOST AND FOUND"));
    // }

    #[test]
    fn sections_round_trip_alongside_unknown_keys() {
        let src = "\
session_id = \"abc\"
mystery = 7

[sync]
enabled = false
poll = \"5m\"
conflict = \"prefer-newest\"

[updates]
channel = \"prerelease\"
";
        let cfg: Config = toml::from_str(src).expect("parse");
        assert_eq!(cfg.session_id().as_deref(), Some("abc"));
        assert!(!cfg.sync.enabled);
        assert_eq!(cfg.sync.poll, PollInterval::Every(Duration::from_secs(300)));
        assert_eq!(cfg.sync.conflict, ConflictPolicy::PreferNewest);
        assert_eq!(cfg.updates.channel, UpdateChannel::Prerelease);
        // sections and keys left unset fall back to their defaults
        assert_eq!(cfg.sync.upload_trigger, UploadTrigger::OnChange);
        assert!(cfg.startup.start_hidden);

        // let rewritten = comment_unknown_keys(&toml::to_string_pretty(&cfg).expect("serialize"), &cfg.extra);
        let rewritten = toml::to_string_pretty(&cfg).expect("serialize");
        let reparsed: Config = toml::from_str(&rewritten).expect("re-parse");
        assert_eq!(reparsed.extra, cfg.extra);
        assert!(reparsed.extra.contains_key("mystery"));
        assert_eq!(reparsed.sync.poll, PollInterval::Every(Duration::from_secs(300)));
        assert_eq!(reparsed.updates.channel, UpdateChannel::Prerelease);
    }

    #[test]
    fn crash_reports_is_absent_until_answered() {
        let mut cfg = Config::default();
        assert!(!cfg.crash_reports_answered());
        assert!(!toml::to_string_pretty(&cfg).unwrap().contains("crash_reports"));

        cfg.set_crash_reports(false);
        assert!(cfg.crash_reports_answered());
        assert!(toml::to_string_pretty(&cfg).unwrap().contains("crash_reports = false"));
    }

    #[test]
    fn durations_parse_and_render_canonically() {
        #[derive(Serialize, Deserialize)]
        struct W {
            poll: PollInterval,
            every: Interval,
        }

        let w: W = toml::from_str("poll = \"90s\"\nevery = \"120m\"\n").expect("parse");
        assert_eq!(w.poll, PollInterval::Every(Duration::from_secs(90)));
        assert_eq!(w.every, Interval(Duration::from_secs(7_200)));

        let rendered = toml::to_string(&w).expect("serialize");
        assert!(rendered.contains("poll = \"90s\""), "{rendered}");
        assert!(rendered.contains("every = \"2h\""), "{rendered}");

        assert!(toml::from_str::<W>("poll = \"auto\"\nevery = \"0s\"\n").is_err(), "Interval rejects zero");
    }
}
