//! Self-update against this repo's GitHub Releases.
//!
//! `check` queries the Releases API and semver-compares. `apply` downloads the
//! archive for this exact target triple plus its detached `.minisig`, verifies
//! the signature against the public key baked in at build time and the
//! `.sha256`, unpacks the new binary, swaps it in with `self-replace`, and
//! relaunches with `--relaunched-after-update` so the new process waits out the
//! old one's single-instance lock (see `ipc::acquire_wait`).
//!
//! All of this runs on a worker thread in `App`, never the UI thread. On a
//! successful `apply`, `App` drops the sync engine and closes the window; the
//! spawned process takes over.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::constants::USER_AGENT;
use crate::sync::sha256_hex;
use crate::version;

/// The public key that release archives must verify against. Committed at
/// `assets/minisign.pub`; the matching secret key signs in CI (`release.yml`).
const PUBKEY: &str = include_str!("../assets/minisign.pub");

const BIN_STEM: &str = "coincell";
const ARCHIVE_EXT: &str = if cfg!(windows) { ".zip" } else { ".tar.gz" };

/// A release strictly newer than the running build.
pub struct Available {
    pub version: semver::Version,
    pub tag: String,
    pub notes: String,
    archive_url: String,
    sig_url: String,
    sha_url: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().user_agent(USER_AGENT.as_str()).timeout(Duration::from_secs(60)).build().unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Look for a newer release. `allow_prerelease` follows `[updates].channel`.
/// Returns `Ok(None)` when up to date. A `development` build can still call this
/// to *see* the latest, but [`apply`] will refuse.
pub fn check(allow_prerelease: bool) -> Result<Option<Available>> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=30", version::REPO);
    let releases: Vec<GhRelease> = http()
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .context("reach the GitHub API")?
        .error_for_status()
        .context("GitHub API status")?
        .json()
        .context("decode the releases list")?;

    let running = semver::Version::parse(version::VERSION).with_context(|| format!("parse running version {}", version::VERSION))?;

    let mut best: Option<Available> = None;
    for r in releases {
        if r.draft || (r.prerelease && !allow_prerelease) {
            continue;
        }
        let Ok(ver) = semver::Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name)) else {
            continue;
        };
        if ver <= running {
            continue;
        }
        if best.as_ref().is_some_and(|b| b.version >= ver) {
            continue;
        }

        let want_archive = |n: &str| n.contains(version::TARGET) && n.ends_with(ARCHIVE_EXT);
        let Some(archive) = r.assets.iter().find(|a| want_archive(&a.name)) else {
            continue; // this release has no build for our platform
        };
        let sig = r.assets.iter().find(|a| a.name == format!("{}.minisig", archive.name));
        let Some(sig) = sig else { continue };
        let sha = r.assets.iter().find(|a| a.name == format!("{}.sha256", archive.name));

        best = Some(Available {
            version: ver,
            tag: r.tag_name.clone(),
            notes: r.body.clone(),
            archive_url: archive.browser_download_url.clone(),
            sig_url: sig.browser_download_url.clone(),
            sha_url: sha.map(|a| a.browser_download_url.clone()),
        });
    }
    Ok(best)
}

/// Download, verify, and swap in `av`, then spawn the new binary. On `Ok`, the
/// caller must shut this process down; the spawned one is taking over.
pub fn apply(av: &Available) -> Result<()> {
    if !version::is_release() {
        bail!("this is a development build and doesn't self-update");
    }
    if !crate::install::running_installed() {
        bail!("updates apply to the installed copy, install CoinCell first");
    }

    let client = http();
    let archive = client.get(&av.archive_url).send()?.error_for_status()?.bytes()?.to_vec();
    let sig_text = client.get(&av.sig_url).send()?.error_for_status()?.text()?;

    // 1. Signature (the release archive was signed in CI with the matching key).
    let pk = minisign_verify::PublicKey::decode(PUBKEY).map_err(|e| anyhow!("bad baked-in public key: {e:?}"))?;
    let sig = minisign_verify::Signature::decode(&sig_text).map_err(|e| anyhow!("bad signature file: {e:?}"))?;
    pk.verify(&archive, &sig, true).map_err(|e| anyhow!("signature verification failed: {e:?}"))?;

    // 2. Checksum, if the release published one.
    if let Some(sha_url) = &av.sha_url {
        let sha_text = client.get(sha_url).send()?.error_for_status()?.text()?;
        let want = sha_text.split_whitespace().next().unwrap_or_default().to_lowercase();
        let got = sha256_hex(&archive);
        if !want.is_empty() && want != got {
            bail!("checksum mismatch: expected {want}, got {got}");
        }
    }

    // 3. Pull the binary out of the archive.
    let new_bytes = extract_binary(&archive).context("extract the new binary from the archive")?;
    if new_bytes.len() < 1024 {
        bail!("extracted binary is implausibly small ({} bytes)", new_bytes.len());
    }

    // 4. Stage it next to the installed exe (same volume → atomic swap) and
    //    replace the running executable.
    let installed = crate::install::canonical_exe()?;
    let dir = installed.parent().context("install dir has no parent")?;
    let staged = dir.join(format!(".{BIN_STEM}.update-{}.tmp", std::process::id()));
    std::fs::write(&staged, &new_bytes).with_context(|| format!("stage {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }

    let swap = self_replace::self_replace(&staged).context("swap in the new binary");
    let _ = std::fs::remove_file(&staged);
    swap?;

    tracing::info!("updated to {} ({}), relaunching", av.version, av.tag);
    std::process::Command::new(&installed).arg("--relaunched-after-update").spawn().context("relaunch the updated binary")?;
    Ok(())
}

#[cfg(windows)]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_file() && entry.name().rsplit(['/', '\\']).next() == Some("coincell.exe") {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    bail!("no coincell.exe inside the archive")
}

#[cfg(unix)]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_bin = entry.path()?.file_name() == Some(std::ffi::OsStr::new(BIN_STEM));
        if is_bin {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    bail!("no {BIN_STEM} inside the archive")
}

/// Small helper for the Config UI: a one-line "up to date" / "vX.Y.Z available".
pub fn describe(available: &Option<Available>) -> String {
    match available {
        Some(a) => format!("Update available: {} (you have {})", a.version, version::VERSION),
        None => format!("Up to date ({})", version::VERSION),
    }
}

impl Available {
    pub fn short_notes(&self, max: usize) -> String {
        let notes = self.notes.trim();
        if notes.chars().count() <= max {
            notes.to_owned()
        } else {
            let mut s: String = notes.chars().take(max).collect();
            s.push('…');
            s
        }
    }
}
