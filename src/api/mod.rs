//! The full `cr.2032.cloud` device API, plus the stateful pieces built on top of
//! it (the realtime event stream and the sync orchestrator).
//!
//! This module deliberately depends on nothing from the rest of the crate, no
//! `config`, no `constants`, no `egui`, so it can be lifted into its own crate
//! later. Everything it needs (the API base URL, a session id, a client name, a
//! repaint/wake callback) is passed in.

mod client;
mod device;
mod error;
mod events;
mod models;
mod sync;

pub use client::{Client, UploadOutcome, fetch_branding, fetch_device_config, set_user_agent};
pub use device::{DeviceEvent, DeviceFlow, SessionCheck, SessionStatus, open_in_browser, revoke_in_background};
pub use error::{Error, Result};
pub use events::{EventStream, StreamEvent};
pub use models::{
    BrandAsset, BrandColors, BrandIdentity, BrandPalette, BrandTypography, BrandUsage, Branding, Console, DeviceConfig, DiagFixture, Game, GameInstance, Highlight, InstanceArt, Me, NewGameInstance,
    Role, SaveMeta, Timestamp,
};
pub use sync::{SyncEvent, SyncStream};

use serde::de::DeserializeOwned;

/// Turn a finished response into either the decoded body or a typed [`Error`].
/// `401`/`403` always collapse to [`Error::Unauthorized`] so callers can treat
/// "session died" uniformly.
pub(crate) fn read_json<T: DeserializeOwned>(resp: reqwest::blocking::Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json()?);
    }
    match status.as_u16() {
        401 | 403 => Err(Error::Unauthorized),
        code => Err(Error::Status { status: code, body: resp.text().unwrap_or_default() }),
    }
}

/// Like [`read_json`] but for endpoints that return no body (`204`).
pub(crate) fn expect_no_content(resp: reqwest::blocking::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    match status.as_u16() {
        401 | 403 => Err(Error::Unauthorized),
        code => Err(Error::Status { status: code, body: resp.text().unwrap_or_default() }),
    }
}
