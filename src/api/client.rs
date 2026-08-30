//! Blocking REST client for the device API. One `Client` = one base URL + one
//! session id. Cheap to `clone` (shared `reqwest` pool inside).

use std::sync::{Arc, OnceLock};

use reqwest::blocking::RequestBuilder;

use crate::api::models::{Consoles, GameInstances, Games, IdOnly, Saves, Upload};
use crate::api::{Branding, Console, DeviceConfig, Error, Game, GameInstance, Me, NewGameInstance, Result, SaveMeta, expect_no_content, read_json};

static USER_AGENT: OnceLock<String> = OnceLock::new();

/// Set the `User-Agent` sent on every request from this module. Call once at
/// startup, before the first request; later calls are ignored. Without it,
/// requests carry `reqwest`'s default agent.
pub fn set_user_agent(ua: impl Into<String>) {
    let _ = USER_AGENT.set(ua.into());
}

/// A blocking client carrying the configured `User-Agent` (if [`set_user_agent`]
/// ran). Shared by [`Client`] and the session-less free functions.
pub(super) fn http_client() -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(ua) = USER_AGENT.get() {
        builder = builder.user_agent(ua);
    }
    builder.build().unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Result of `POST /api/game-instances/:id/saves`.
#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub id: String,
    /// `true` if the bytes matched an existing save (server just bumped its
    /// `uploaded_at`); `false` if a new save row was created.
    pub duplicate: bool,
}

#[derive(Clone)]
pub struct Client {
    base: Arc<str>,
    session: Arc<str>,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(base: impl Into<Arc<str>>, session: impl Into<Arc<str>>) -> Self {
        Self { base: normalise_base(base.into()), session: session.into(), http: http_client() }
    }

    /// A copy pointed at a different session (e.g. right after a fresh login).
    pub fn with_session(&self, session: impl Into<Arc<str>>) -> Self {
        Self { base: self.base.clone(), session: session.into(), http: self.http.clone() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    // ---- account -----------------------------------------------------------

    /// `GET /api/me`. Doubles as a cheap "is my session still good?" probe.
    pub fn me(&self) -> Result<Me> {
        read_json(self.get("/api/me").send()?)
    }

    /// `POST /auth/device/logout`: revokes exactly this session.
    pub fn logout(&self) -> Result<()> {
        expect_no_content(self.post("/auth/device/logout").send()?)
    }

    // ---- catalog (public, but the bearer is harmless) --------------------

    pub fn consoles(&self) -> Result<Vec<Console>> {
        Ok(read_json::<Consoles>(self.get("/api/consoles").send()?)?.consoles)
    }

    pub fn games(&self, console_slug: &str) -> Result<Vec<Game>> {
        Ok(read_json::<Games>(self.get(&format!("/api/consoles/{console_slug}/games")).send()?)?.games)
    }

    // ---- game instances --------------------------------------------------

    /// `GET /api/game-instances`. With `since` (a prior `last_saved_at` value),
    /// only instances with a newer save come back.
    pub fn game_instances(&self, since: Option<&str>) -> Result<Vec<GameInstance>> {
        // The only non-URL-safe character in a `YYYY-MM-DD HH:MM:SS` cursor is
        // the space; the server rejects anything that isn't that exact shape.
        let path = match since {
            Some(since) => format!("/api/game-instances?since={}", since.replace(' ', "%20")),
            None => "/api/game-instances".to_owned(),
        };
        Ok(read_json::<GameInstances>(self.get(&path).send()?)?.game_instances)
    }

    /// `POST /api/game-instances`: returns the new instance id.
    pub fn create_game_instance(&self, new: &NewGameInstance) -> Result<String> {
        Ok(read_json::<IdOnly>(self.post("/api/game-instances").json(new).send()?)?.id)
    }

    // ---- saves ---------------------------------------------------------------

    /// `GET /api/game-instances/:id/saves`, newest first.
    pub fn saves(&self, instance_id: &str) -> Result<Vec<SaveMeta>> {
        Ok(read_json::<Saves>(self.get(&format!("/api/game-instances/{instance_id}/saves")).send()?)?.saves)
    }

    /// `POST /api/game-instances/:id/saves` with raw bytes.
    pub fn upload_save(&self, instance_id: &str, bytes: Vec<u8>) -> Result<UploadOutcome> {
        let resp = self.post(&format!("/api/game-instances/{instance_id}/saves")).header(reqwest::header::CONTENT_TYPE, "application/octet-stream").body(bytes).send()?;
        let up: Upload = read_json(resp)?;
        Ok(UploadOutcome { id: up.id, duplicate: up.duplicate })
    }

    /// `GET /api/game-instances/:id/saves/:saveId`: the raw save bytes.
    pub fn download_save(&self, instance_id: &str, save_id: &str) -> Result<Vec<u8>> {
        let resp = self.get(&format!("/api/game-instances/{instance_id}/saves/{save_id}")).send()?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.bytes()?.to_vec());
        }
        match status.as_u16() {
            401 | 403 => Err(Error::Unauthorized),
            code => Err(Error::Status { status: code, body: resp.text().unwrap_or_default() }),
        }
    }

    // ---- internals -------------------------------------------------------

    fn get(&self, path: &str) -> RequestBuilder {
        self.http.get(self.url(path)).bearer_auth(&self.session)
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.http.post(self.url(path)).bearer_auth(&self.session)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

/// `GET /auth/device/config`: no session required, so it's a free function, not
/// a [`Client`] method (you need its result before you can build a `Client`).
pub fn fetch_device_config(api_base: &str) -> Result<DeviceConfig> {
    let http = http_client();
    let url = format!("{}/auth/device/config", api_base.trim_end_matches('/'));
    read_json(http.get(url).send()?)
}

/// `GET /api/branding`: public, no session. Also a free function: the app wants
/// it before (or without) a `Client`, e.g. to theme the sign-in screen. Callers
/// fall back to the baked copy on any error.
pub fn fetch_branding(api_base: &str) -> Result<Branding> {
    let http = http_client();
    let url = format!("{}/api/branding", api_base.trim_end_matches('/'));
    read_json(http.get(url).send()?)
}

fn normalise_base(base: Arc<str>) -> Arc<str> {
    let trimmed = base.trim_end_matches('/');
    if trimmed.len() == base.len() { base } else { Arc::from(trimmed) }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the session id.
        f.debug_struct("Client").field("base", &self.base).finish_non_exhaustive()
    }
}
