//! Auth0 device-authorization flow.
//!
//! [`AuthFlow::start`] spawns a worker thread that walks the same chain as
//! `test-device-flow.mjs`: request a device code, poll Auth0 for an access
//! token, then trade that token for a 2032 session id via
//! `POST {api_base}/auth/device/token`. Progress and the final session id are
//! reported back to the UI over an [`std::sync::mpsc`] channel; the UI persists
//! the session id into [`crate::config::Config`].

use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::config::OauthSpec;
use crate::constants::{API_CLIENT, APP_NAME};

pub enum AuthEvent {
    AwaitingApproval {
        user_code: Arc<str>,
        verification_uri: Arc<str>,
        verification_uri_complete: Arc<str>,
    },

    Completed {
        session_id: Arc<str>,
    },

    Failed {
        message: Arc<str>,
    },
}

pub struct AuthFlow {
    rx: Receiver<AuthEvent>,
}

impl AuthFlow {
    pub fn start(ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("auth-device-flow".into())
            .spawn(move || {
                if let Err(err) = run(&tx, &ctx) {
                    let _ = tx.send(AuthEvent::Failed {
                        message: Arc::from(format!("{err:#}")),
                    });
                }
                ctx.request_repaint();
            })
            .expect("spawn auth-device-flow thread");
        Self { rx }
    }

    pub fn events(&self) -> TryIter<'_, AuthEvent> {
        self.rx.try_iter()
    }
}

pub enum SessionStatus {
    Valid,

    Invalid,

    Unknown(Arc<str>),
}

pub struct SessionCheck {
    rx: Receiver<SessionStatus>,
}

impl SessionCheck {
    pub fn start(ctx: egui::Context, session_id: Arc<str>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("auth-session-check".into())
            .spawn(move || {
                let status = match validate_session(&session_id) {
                    Ok(true) => SessionStatus::Valid,
                    Ok(false) => SessionStatus::Invalid,
                    Err(err) => SessionStatus::Unknown(Arc::from(format!("{err:#}"))),
                };
                let _ = tx.send(status);
                ctx.request_repaint();
            })
            .expect("spawn auth-session-check thread");
        Self { rx }
    }

    pub fn poll(&self) -> Option<SessionStatus> {
        self.rx.try_recv().ok()
    }
}

fn validate_session(session_id: &str) -> anyhow::Result<bool> {
    let api_base = OauthSpec::api_base();
    let res = API_CLIENT.get(format!("{api_base}/api/me")).bearer_auth(session_id).send()?;
    match res.status().as_u16() {
        200..=299 => Ok(true),
        401 | 403 => Ok(false),
        other => anyhow::bail!("unexpected status from /api/me: {other} {}", res.text().unwrap_or_default()),
    }
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

fn run(tx: &Sender<AuthEvent>, ctx: &egui::Context) -> anyhow::Result<()> {
    let device = request_device_code()?;

    let _ = tx.send(AuthEvent::AwaitingApproval {
        user_code: Arc::from(device.user_code.as_str()),
        verification_uri: Arc::from(device.verification_uri.as_str()),
        verification_uri_complete: Arc::from(device.verification_uri_complete.as_str()),
    });
    ctx.request_repaint();
    open_in_browser(&device.verification_uri_complete);

    let access_token = poll_for_token(&device)?;
    let session_id = bootstrap_session(&access_token)?;

    let _ = tx.send(AuthEvent::Completed {
        session_id: Arc::from(session_id.as_str()),
    });
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

fn request_device_code() -> anyhow::Result<DeviceCode> {
    let domain = OauthSpec::auth0_domain();
    let form = [("client_id", OauthSpec::client_id()), ("scope", OauthSpec::scope()), ("audience", OauthSpec::audience())];
    let res = API_CLIENT.post(format!("https://{domain}/oauth/device/code")).form(&form).send()?;
    if !res.status().is_success() {
        anyhow::bail!("device code request failed: {} {}", res.status(), res.text().unwrap_or_default());
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

fn poll_for_token(device: &DeviceCode) -> anyhow::Result<String> {
    let domain = OauthSpec::auth0_domain();
    let client_id = OauthSpec::client_id();
    let mut wait = Duration::from_secs(device.interval.unwrap_or(5));
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        std::thread::sleep(wait);
        if Instant::now() >= deadline {
            anyhow::bail!("login expired before it was approved");
        }

        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device.device_code.as_str()),
            ("client_id", client_id),
        ];
        let res = API_CLIENT.post(format!("https://{domain}/oauth/token")).form(&form).send()?;

        if res.status().is_success() {
            return Ok(res.json::<AccessToken>()?.access_token);
        }

        let err: TokenError = res.json()?;
        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => wait += Duration::from_secs(5),
            other => anyhow::bail!("login failed: {other}{}", err.error_description.map(|d| format!(" ({d})")).unwrap_or_default()),
        }
    }
}

#[derive(Serialize)]
struct BootstrapRequest<'a> {
    client_name: &'a str,
}

#[derive(Deserialize)]
struct BootstrapResponse {
    session_id: String,
}

fn bootstrap_session(access_token: &str) -> anyhow::Result<String> {
    let api_base = OauthSpec::api_base();
    let res = API_CLIENT
        .post(format!("{api_base}/auth/device/token"))
        .bearer_auth(access_token)
        .json(&BootstrapRequest { client_name: APP_NAME })
        .send()?;
    if !res.status().is_success() {
        anyhow::bail!("session bootstrap failed: {} {}", res.status(), res.text().unwrap_or_default());
    }
    Ok(res.json::<BootstrapResponse>()?.session_id)
}
