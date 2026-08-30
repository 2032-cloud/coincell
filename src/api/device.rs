//! Auth0 device-authorization flow and session lifecycle.
//!
//! [`DeviceFlow::start`] spawns a worker that requests a device code, polls
//! Auth0 for an access token, then trades it for a device session id via
//! `POST {api_base}/auth/device/token`. Progress is reported over an
//! [`std::sync::mpsc`] channel; after each message the `wake` callback fires so
//! a UI can request a repaint (pass `|| {}` if you don't need it).

use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::api::models::Session;
use crate::api::{Client, DeviceConfig, Error, Me, Result, read_json};

// ---- device login flow -----------------------------------------------------

pub enum DeviceEvent {
    AwaitingApproval { user_code: Arc<str>, verification_uri: Arc<str>, verification_uri_complete: Arc<str> },
    Completed { session_id: Arc<str> },
    Failed { message: Arc<str> },
}

pub struct DeviceFlow {
    rx: Receiver<DeviceEvent>,
}

impl DeviceFlow {
    pub fn start(cfg: DeviceConfig, client_name: impl Into<String>, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let client_name = client_name.into();
        std::thread::Builder::new()
            .name("api-device-flow".into())
            .spawn(move || {
                if let Err(err) = run_flow(&cfg, &client_name, &tx, &wake) {
                    let _ = tx.send(DeviceEvent::Failed { message: Arc::from(err.to_string()) });
                }
                wake();
            })
            .expect("spawn api-device-flow thread");
        Self { rx }
    }

    pub fn events(&self) -> TryIter<'_, DeviceEvent> {
        self.rx.try_iter()
    }
}

fn run_flow(cfg: &DeviceConfig, client_name: &str, tx: &Sender<DeviceEvent>, wake: &impl Fn()) -> Result<()> {
    let http = super::client::http_client();

    let device = request_device_code(&http, cfg)?;
    let _ = tx.send(DeviceEvent::AwaitingApproval {
        user_code: Arc::from(device.user_code.as_str()),
        verification_uri: Arc::from(device.verification_uri.as_str()),
        verification_uri_complete: Arc::from(device.verification_uri_complete.as_str()),
    });
    wake();
    open_in_browser(&device.verification_uri_complete);

    let access_token = poll_for_token(&http, cfg, &device)?;
    let session_id = bootstrap_session(&http, cfg, client_name, &access_token)?;

    let _ = tx.send(DeviceEvent::Completed { session_id: Arc::from(session_id.as_str()) });
    Ok(())
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    #[serde(default)]
    interval: Option<u64>,
    expires_in: u64,
}

fn request_device_code(http: &reqwest::blocking::Client, cfg: &DeviceConfig) -> Result<DeviceCode> {
    let form = [("client_id", cfg.client_id.as_str()), ("scope", cfg.scope.as_str()), ("audience", cfg.audience.as_str())];
    let res = http.post(format!("https://{}/oauth/device/code", cfg.auth0_domain)).form(&form).send()?;
    if !res.status().is_success() {
        return Err(Error::LoginFailed(Arc::from(format!("device code request failed: {} {}", res.status(), res.text().unwrap_or_default()))));
    }
    Ok(res.json()?)
}

#[derive(Deserialize)]
struct AccessToken {
    access_token: String,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

fn poll_for_token(http: &reqwest::blocking::Client, cfg: &DeviceConfig, device: &DeviceCode) -> Result<String> {
    let mut wait = Duration::from_secs(device.interval.unwrap_or(5));
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        std::thread::sleep(wait);
        if Instant::now() >= deadline {
            return Err(Error::LoginExpired);
        }

        let form = [("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", device.device_code.as_str()), ("client_id", cfg.client_id.as_str())];
        let res = http.post(format!("https://{}/oauth/token", cfg.auth0_domain)).form(&form).send()?;
        if res.status().is_success() {
            return Ok(res.json::<AccessToken>()?.access_token);
        }

        let err: TokenError = res.json()?;
        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => wait += Duration::from_secs(5),
            other => {
                let detail = err.error_description.map(|d| format!(" ({d})")).unwrap_or_default();
                return Err(Error::LoginFailed(Arc::from(format!("{other}{detail}"))));
            }
        }
    }
}

#[derive(Serialize)]
struct BootstrapRequest<'a> {
    client_name: &'a str,
}

fn bootstrap_session(http: &reqwest::blocking::Client, cfg: &DeviceConfig, client_name: &str, access_token: &str) -> Result<String> {
    let name = client_name.trim();
    let name = if name.is_empty() { "API client" } else { name };
    let res = http.post(format!("{}/auth/device/token", cfg.api_base.trim_end_matches('/'))).bearer_auth(access_token).json(&BootstrapRequest { client_name: name }).send()?;
    Ok(read_json::<Session>(res)?.session_id)
}

// ---- session validation --------------------------------------------------

pub enum SessionStatus {
    /// The session is good; carries the `GET /api/me` body so the caller can
    /// pick up `sub` / `theme` without a second request.
    Valid(Me),
    Invalid,
    /// Couldn't tell (network error). Caller should keep the session and retry.
    Unknown(Arc<str>),
}

pub struct SessionCheck {
    rx: Receiver<SessionStatus>,
}

impl SessionCheck {
    pub fn start(client: Client, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("api-session-check".into())
            .spawn(move || {
                let status = match client.me() {
                    Ok(me) => SessionStatus::Valid(me),
                    Err(Error::Unauthorized) => SessionStatus::Invalid,
                    Err(err) => SessionStatus::Unknown(Arc::from(err.to_string())),
                };
                let _ = tx.send(status);
                wake();
            })
            .expect("spawn api-session-check thread");
        Self { rx }
    }

    pub fn poll(&self) -> Option<SessionStatus> {
        self.rx.try_recv().ok()
    }
}

/// Fire-and-forget `POST /auth/device/logout`. The caller clears its local
/// session regardless of the outcome.
pub fn revoke_in_background(client: Client) {
    std::thread::Builder::new()
        .name("api-logout".into())
        .spawn(move || {
            if let Err(err) = client.logout() {
                tracing::warn!("logout request failed: {err}");
            }
        })
        .ok();
}

pub fn open_in_browser(url: &str) {
    let spawned = if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    let _ = spawned;
}
