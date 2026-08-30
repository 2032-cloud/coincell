//! The single build version, resolved from git by `build.rs` and used
//! everywhere a version is shown or sent: Config › Updates, copy-diagnostics,
//! the Sentry `release` / `environment`, and the device-API `User-Agent`.

/// `X.Y.Z` on a release build, else `{CARGO_PKG_VERSION}+dev.<hash>[-dirty]`.
pub const VERSION: &str = env!("COINCELL_VERSION");

/// `"stable"` | `"prerelease"` | `"development"`. Only the first two self-update;
/// a `development` build can still check and show the latest release.
pub const CHANNEL: &str = env!("COINCELL_CHANNEL");

/// Short commit hash, or `"unknown"` when built without git.
pub const COMMIT: &str = env!("COINCELL_COMMIT");

/// The exact target triple this binary was built for, e.g.
/// `x86_64-pc-windows-msvc`. Used by the updater to pick the matching release
/// asset (`coincell-vX.Y.Z-<triple>.{zip,tar.gz}`).
pub const TARGET: &str = env!("COINCELL_TARGET");

/// `owner/repo` whose GitHub Releases the updater checks.
pub const REPO: &str = "2032-cloud/coincell";

/// Whether this build is eligible to self-update (i.e. not a `development` one).
pub fn is_release() -> bool {
    CHANNEL != "development"
}
