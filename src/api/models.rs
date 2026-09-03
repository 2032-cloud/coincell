//! Wire types for the device API.
//!
//! The server is mid-migration to all-`snake_case` responses. Field names below
//! are the `snake_case` target; each field that used to be `camelCase` carries a
//! `#[serde(alias = "…")]` so this keeps decoding both spellings until the
//! backend job lands, after which the aliases can be dropped.

use serde::{Deserialize, Serialize};

/// `GET /auth/device/config`: the Auth0 parameters a client needs, plus the
/// server's own origin. Fetchable with no credentials.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub auth0_domain: String,
    pub client_id: String,
    pub audience: String,
    pub scope: String,
    /// The server's own origin, e.g. `https://cr.2032.cloud`.
    pub api_base: String,
}

/// `GET /api/me`: only the two fields a device session actually uses. The
/// unified payload also carries a nested `user` block (Auth0 `sub` / `email`
/// / …), `preferred_region` (the server already applies it to game names), and
/// `account_status` / deletion timestamps (always `"active"` / `null` on the
/// device API). Everything here stays lenient - a missing field must never fail
/// the parse, since this doubles as the "session is valid" probe - so both the
/// old flat shape and the unified one decode.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    /// `None` = follow system; `Some(true/false)` = light/dark.
    #[serde(default)]
    pub theme: Option<bool>,
    /// Account tier, straight off `users.role`. Only `Admin` / `SuperAdmin` may
    /// read or write the diagnostics fixture store (see [`DiagFixture`]).
    #[serde(default)]
    pub role: Role,
}

/// The `role` field on `GET /api/me`. Unknown values decode as [`Role::Other`]
/// (unprivileged) so a new tier never breaks the session probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    User,
    Admin,
    SuperAdmin,
    #[serde(other)]
    Other,
}

impl Role {
    /// `true` for `Admin` and above - unlocks the diagnostics fixture store.
    pub fn privileged(self) -> bool {
        matches!(self, Role::Admin | Role::SuperAdmin)
    }
}

/// One entry in the diagnostics fixture store (`/api/diag/*`): a reference
/// binary a privileged user published for a game, keyed by console + game slug,
/// so others testing the same game don't each have to locate the file. Used
/// only to pre-fill a launcher content path; nothing else consumes it.
#[derive(Debug, Clone, Deserialize)]
pub struct DiagFixture {
    #[serde(alias = "consoleSlug")]
    pub console_slug: String,
    #[serde(alias = "gameSlug")]
    pub game_slug: String,
    pub filename: String,
    #[serde(default, alias = "sizeBytes")]
    pub size_bytes: u64,
    #[serde(alias = "contentHash")]
    pub content_hash: String,
}

/// `GET /api/branding`: the service's presentation layer (name, palette,
/// typography) so a client's chrome can follow the backend instead of
/// hard-coding it. Public, no session required. Born snake_case, so unlike the
/// rest of `models` it needs no `#[serde(alias)]`. A copy is baked into the
/// binary (`assets/branding.json`) and this refreshes it at launch.
#[derive(Debug, Clone, Deserialize)]
pub struct Branding {
    pub schema_version: u32,
    /// Date the payload was last changed, `YYYY-MM-DD`.
    pub updated_at: String,
    pub identity: BrandIdentity,
    pub colors: BrandColors,
    pub typography: BrandTypography,
    /// Logo / wordmark files. Shape isn't pinned down yet (the live payload sends
    /// `[]`); every field is optional so an unknown object still decodes.
    #[serde(default)]
    pub assets: Vec<BrandAsset>,
    #[serde(default)]
    pub usage: BrandUsage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrandIdentity {
    pub name: String,
    pub short_name: String,
    pub tagline: String,
    pub homepage_url: String,
    pub docs_url: String,
    /// Short "synced with X" credit line for unobtrusive placement.
    pub attribution_text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrandColors {
    /// `"dark"` or `"light"`: the scheme to fall back to when nothing else (an
    /// explicit preference, the OS, the account) has decided.
    pub default_scheme: String,
    pub light: BrandPalette,
    pub dark: BrandPalette,
}

/// One scheme's worth of colours. Every value is a CSS hex string (`#rrggbb`).
#[derive(Debug, Clone, Deserialize)]
pub struct BrandPalette {
    pub bg: String,
    pub bg_elevated: String,
    pub text: String,
    pub text_muted: String,
    pub border: String,
    pub accent: String,
    pub accent_hover: String,
    pub danger: String,
    pub on_accent: String,
    pub focus_ring: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BrandTypography {
    /// A CSS font stack. Advisory only: the app ships its own bundled font and
    /// can't resolve system families by name.
    pub font_family: String,
    /// A downloadable web font, if the service wants clients to match exactly.
    /// `None` today, so nothing is fetched.
    pub font_source_url: Option<String>,
    #[serde(default)]
    pub weights: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrandAsset {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BrandUsage {
    pub guidelines_url: Option<String>,
    #[serde(default)]
    pub notes: String,
}

/// A UTC timestamp in the server's `YYYY-MM-DD HH:MM:SS` format. Fixed-width and
/// zero-padded, so byte order is chronological order: which is exactly what the
/// `?since=` cursor needs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub String);

impl Timestamp {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `GET /api/consoles`.
#[derive(Debug, Clone, Deserialize)]
pub struct Console {
    pub slug: String,
    pub name: String,
    pub description: String,
    /// Raw byte counts an upload for this console is allowed to be. The only
    /// save-size check a client needs.
    #[serde(alias = "validSaveSizes")]
    pub valid_save_sizes: Vec<u64>,
    #[serde(alias = "iconUrl")]
    pub icon_url: String,
    #[serde(alias = "boxArtUrl")]
    pub box_art_url: String,
}

/// `GET /api/consoles/:slug/games`.
#[derive(Debug, Clone, Deserialize)]
pub struct Game {
    pub slug: String,
    /// The display name, already resolved server-side for the account's
    /// `preferred_region`. The response also carries per-region `titles` and a
    /// `native_region`, but the client doesn't need them: `name` is authoritative.
    pub name: String,
    pub description: String,
    #[serde(alias = "iconUrl")]
    pub icon_url: String,
    #[serde(alias = "boxArtUrl")]
    pub box_art_url: String,
}

/// One row of `GET /api/game-instances`.
#[derive(Debug, Clone, Deserialize)]
pub struct GameInstance {
    pub id: String,
    #[serde(alias = "consoleSlug")]
    pub console_slug: String,
    #[serde(alias = "consoleName")]
    pub console_name: String,
    /// `None` for a custom / unlinked instance (romhacks etc.).
    #[serde(alias = "gameSlug")]
    pub game_slug: Option<String>,
    /// Display name: `custom_name`, else the canonical/`game_name`.
    pub name: String,
    /// The bound game's own name, ignoring any `custom_name` override.
    #[serde(alias = "defaultName")]
    pub default_name: Option<String>,
    #[serde(alias = "customName")]
    pub custom_name: Option<String>,
    /// `uploaded_at` of the newest save, or `None` if the instance has none.
    #[serde(alias = "lastSavedAt")]
    pub last_saved_at: Option<Timestamp>,
    #[serde(alias = "starredCount")]
    pub starred_count: u32,
    #[serde(alias = "unstarredCount")]
    pub unstarred_count: u32,
    pub highlights: Vec<Highlight>,
    /// The newest save's metadata: same shape as a `saves` row: so reacting to
    /// a change here needs no follow-up request.
    #[serde(alias = "latestSave")]
    pub latest_save: Option<SaveMeta>,
    pub art: InstanceArt,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Highlight {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceArt {
    #[serde(alias = "hasIcon")]
    pub has_icon: bool,
    #[serde(alias = "hasBoxArt")]
    pub has_box_art: bool,
    #[serde(alias = "igdbImageId")]
    pub igdb_image_id: Option<String>,
}

/// A `saves` row, and the `latestSave` object inside a game instance. Note the
/// `snake_case` field names and the integer `starred`.
#[derive(Debug, Clone, Deserialize)]
pub struct SaveMeta {
    pub id: String,
    pub size_bytes: u64,
    pub uploaded_at: Timestamp,
    #[serde(deserialize_with = "flex_bool")]
    pub starred: bool,
    pub note: Option<String>,
    pub content_hash: String,
}

/// Body for `POST /api/game-instances`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NewGameInstance {
    pub console_slug: String,
    /// Omit for a custom / unlinked instance; then `game_name` is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
}

/// Accept `starred` whether the server sends `0`/`1` or a real boolean.
fn flex_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrInt {
        Bool(bool),
        Int(i64),
    }
    Ok(match BoolOrInt::deserialize(d)? {
        BoolOrInt::Bool(b) => b,
        BoolOrInt::Int(n) => n != 0,
    })
}

// ---- response envelopes (internal) --------------------------------------------

#[derive(Deserialize)]
pub(crate) struct Consoles {
    pub consoles: Vec<Console>,
}

#[derive(Deserialize)]
pub(crate) struct Games {
    pub games: Vec<Game>,
}

#[derive(Deserialize)]
pub(crate) struct GameInstances {
    #[serde(alias = "gameInstances")]
    pub game_instances: Vec<GameInstance>,
}

#[derive(Deserialize)]
pub(crate) struct Saves {
    pub saves: Vec<SaveMeta>,
}

#[derive(Deserialize)]
pub(crate) struct DiagRoms {
    pub roms: Vec<DiagFixture>,
}

#[derive(Deserialize)]
pub(crate) struct IdOnly {
    pub id: String,
}

#[derive(Deserialize)]
pub(crate) struct Upload {
    pub id: String,
    pub duplicate: bool,
}

#[derive(Deserialize)]
pub(crate) struct Session {
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn me_is_lenient() {
        // the old flat cr. shape (no `role`, `sub` at top level)
        let me: Me = serde_json::from_str(r#"{"sub":"auth0|x","theme":null,"preferred_region":"USA"}"#).unwrap();
        assert!(me.theme.is_none());
        assert_eq!(me.role, Role::User); // absent -> default

        // missing everything still parses (it's also the session probe)
        assert!(serde_json::from_str::<Me>("{}").is_ok());

        // the unified shape: nested `user`, top-level `role`, deletion fields
        let me: Me = serde_json::from_str(
            r#"{"user":{"sub":"x","name":null,"email":"a@b.c","picture":null},"theme":true,
                "preferred_region":"USA","role":"admin","account_status":"active",
                "deletion_requested_at":null,"purge_at":null}"#,
        )
        .unwrap();
        assert_eq!(me.theme, Some(true));
        assert!(me.role.privileged());
    }

    #[test]
    fn role_defaults_and_tolerates_the_unknown() {
        assert_eq!(serde_json::from_str::<Me>("{}").unwrap().role, Role::User);
        assert_eq!(serde_json::from_str::<Me>(r#"{"role":"super_admin"}"#).unwrap().role, Role::SuperAdmin);
        assert!(serde_json::from_str::<Me>(r#"{"role":"admin"}"#).unwrap().role.privileged());
        // a tier the client doesn't know decodes as unprivileged, never an error
        let me: Me = serde_json::from_str(r#"{"role":"moderator"}"#).unwrap();
        assert_eq!(me.role, Role::Other);
        assert!(!me.role.privileged());
    }

    #[test]
    fn game_decodes_ignoring_the_server_side_name_resolution_fields() {
        // the response still carries `titles` / `native_region`; the client drops them
        let g: Game = serde_json::from_str(
            r#"{"slug":"mother-3","name":"Mother 3","description":"","titles":[{"name":"MOTHER3","region":"JPN","language":"ja"}],"native_region":"JPN","icon_url":"i","box_art_url":"b"}"#,
        )
        .unwrap();
        assert_eq!(g.name, "Mother 3");
        assert_eq!(g.box_art_url, "b");
    }
}
