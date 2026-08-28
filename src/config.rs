use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::constants::*;

static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
static OAUTH_SPEC: OnceLock<OauthSpec> = OnceLock::new();

#[derive(Debug, Clone, Deserialize)]
pub struct OauthSpec {
    auth0_domain: Arc<str>,
    client_id: Arc<str>,
    audience: Arc<str>,
    scope: Arc<str>,
    api_base: Arc<str>,
}

impl OauthSpec {
    pub fn init() {
        let _ = OAUTH_SPEC.set(Self::load().expect("Api Spec request FAILED!"));
    }

    fn load() -> anyhow::Result<Self> {
        let client = API_CLIENT.clone();

        Ok(client.get(format!("{API_BASE_ROUTE}/auth/device/config")).send()?.json()?)
    }

    fn slot() -> &'static OauthSpec {
        OAUTH_SPEC.get().expect("OauthSpec::init() must run before OauthSpec::get")
    }

    pub fn auth0_domain() -> &'static str {
        &Self::slot().auth0_domain
    }
    pub fn client_id() -> &'static str {
        &Self::slot().client_id
    }
    pub fn audience() -> &'static str {
        &Self::slot().audience
    }
    pub fn scope() -> &'static str {
        &Self::slot().scope
    }
    pub fn api_base() -> &'static str {
        &Self::slot().api_base
    }
}

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "session_id")]
    login_state: LoginState,
    // doesnt corrupt excess data incase anything gets removed or whatever
    #[serde(flatten)]
    extra: toml::Table,
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

    fn path() -> anyhow::Result<PathBuf> {
        fs::create_dir_all(*CONFIG_DIR)?;
        Ok(CONFIG_DIR.join("config.toml"))
    }

    fn load() -> Config {
        let config = match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("config: falling back to defaults: {e:#}");
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
                eprintln!("config: {} was invalid ({e}); backed up to {}", path.display(), backup.display());
                let cfg = Config::default();
                cfg.save()?;
                Ok(cfg)
            }
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        let rendered = toml::to_string_pretty(self)?;
        write_atomic(&Self::path()?, &comment_unknown_keys(&rendered, &self.extra))
    }
}

fn comment_unknown_keys(rendered: &str, extra: &toml::Table) -> String {
    if extra.is_empty() {
        return rendered.to_owned();
    }

    const NOTE: &str = "# LOST AND FOUND (idk what these keys are for but i'm keeping them cus im nice)";
    let mut seen = std::collections::HashSet::new();
    let mut out = String::with_capacity(rendered.len() + extra.len() * (NOTE.len() + 1));
    let mut note_inserted = false;

    for line in rendered.lines() {
        if let Some(key) = top_level_key(line)
            && extra.contains_key(key)
            && seen.insert(key.to_owned())
            && !note_inserted
        {
            note_inserted = true;
            out.push_str(NOTE);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn top_level_key(line: &str) -> Option<&str> {
    if line.trim().is_empty() || line.starts_with([' ', '\t', '#']) {
        return None;
    }
    let body = line.trim_start_matches('[');
    let end = body.find(['=', '.', ']', ' ']).unwrap_or(body.len());
    let key = body[..end].trim().trim_matches('"');
    (!key.is_empty()).then_some(key)
}

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
        let rewritten = comment_unknown_keys(&toml::to_string_pretty(&cfg).expect("serialize"), &cfg.extra);
        let reparsed: Config = toml::from_str(&rewritten).expect("re-parse");

        assert_eq!(reparsed.extra, cfg.extra);
        assert!(cfg.extra.contains_key("future_flag"));
        assert!(cfg.extra.contains_key("future_section"));
    }

    #[test]
    fn unknown_keys_get_a_comment() {
        let cfg: Config = toml::from_str("future_flag = true\n\n[future_section]\nretries = 3\n").expect("parse");
        let out = comment_unknown_keys(&toml::to_string_pretty(&cfg).expect("serialize"), &cfg.extra);

        assert_eq!(out.matches("# not recognised").count(), 2, "one note per top-level extra key:\n{out}");
        // A recognised field must not be tagged.
        let recognised: Config = toml::from_str("").expect("parse");
        assert!(!comment_unknown_keys(&toml::to_string_pretty(&recognised).expect("serialize"), &recognised.extra).contains("# not recognised"));
    }
}
