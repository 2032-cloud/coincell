//! Self-update against this repo's GitHub Releases.
//!
//! `check` queries the Releases API and semver-compares. The download splits in
//! two so `[updates].on_update = download` can pre-fetch:
//!
//! - [`stage`] downloads the archive for this exact target triple plus its
//!   detached `.minisig`, verifies the signature (against the key baked in at
//!   build time) and the `.sha256`, unpacks the new binary, and writes it next
//!   to the installed exe as `.coincell.staged` with a `.coincell.staged.meta`
//!   marker. Nothing is swapped.
//! - [`commit`] swaps a staged binary in with `self-replace` and relaunches with
//!   `--relaunched-after-update` so the new process waits out the old one's
//!   single-instance lock (see `ipc::acquire_wait`).
//! - [`apply`] = `stage` then `commit`, the do-it-all-now path.
//! - [`staged`] reports a leftover staged update from a previous session (and
//!   self-cleans a stale or half-written one).
//!
//! All of this runs on a worker thread in `App`, never the UI thread. On a
//! successful `commit`, `App` drops the sync engine and closes the window; the
//! spawned process takes over.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::constants::USER_AGENT;
use crate::sync::sha256_hex;
use crate::version;

/// The public key that release archives must verify against. Committed at
/// `assets/minisign.pub`; the matching secret key signs in CI (`release.yml`).
const PUBKEY: &str = include_str!("../assets/minisign.pub");

const BIN_STEM: &str = "coincell";
const ARCHIVE_EXT: &str = if cfg!(windows) { ".zip" } else { ".tar.gz" };
const EXE_SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };

/// A release strictly newer than the running build.
pub struct Available {
    pub version: semver::Version,
    pub tag: String,
    pub notes: String,
    archive_url: String,
    sig_url: String,
    sha_url: Option<String>,
}

/// A verified update already downloaded next to the installed exe, ready for an
/// instant [`commit`] (this session or a later one).
#[derive(Clone)]
pub struct StagedUpdate {
    pub version: semver::Version,
    pub tag: String,
    path: PathBuf,
}

/// `.coincell.staged.meta` next to the staged binary: what [`staged`] reads back.
#[derive(Serialize, Deserialize)]
struct Marker {
    tag: String,
    version: String,
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

/// Download + verify `av` and write its binary next to the installed exe as a
/// staged update. No swap, no relaunch - see [`commit`].
pub fn stage(av: &Available) -> Result<StagedUpdate> {
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

    // 4. Write it beside the installed exe (same volume -> commit is an atomic
    //    swap later). Temp file + rename so a killed download never leaves a
    //    half-written binary that `staged()` would trust.
    let (bin, meta) = staged_paths()?;
    let tmp = bin.with_file_name(format!(".{BIN_STEM}.staged.download-{}", std::process::id()));
    std::fs::write(&tmp, &new_bytes).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &bin).with_context(|| format!("stage {}", bin.display()))?;

    let marker = serde_json::to_vec(&Marker { tag: av.tag.clone(), version: av.version.to_string() }).context("encode the staged-update marker")?;
    std::fs::write(&meta, marker).with_context(|| format!("write {}", meta.display()))?;

    tracing::info!("staged update {} ({}) at {}", av.version, av.tag, bin.display());
    Ok(StagedUpdate { version: av.version.clone(), tag: av.tag.clone(), path: bin })
}

/// Swap a staged binary in for the running one and spawn it. On `Ok`, the caller
/// must shut this process down; the spawned one is taking over.
pub fn commit(staged: &StagedUpdate) -> Result<()> {
    if !version::is_release() {
        bail!("this is a development build and doesn't self-update");
    }
    if !crate::install::running_installed() {
        bail!("updates apply to the installed copy, install CoinCell first");
    }
    let installed = crate::install::canonical_exe()?;

    let swap = self_replace::self_replace(&staged.path).context("swap in the staged binary");
    discard_staged(); // marker + the (already copied) staged file, even if the swap failed
    swap?;

    tracing::info!("updated to {} ({}), relaunching", staged.version, staged.tag);
    std::process::Command::new(&installed).arg("--relaunched-after-update").spawn().context("relaunch the updated binary")?;
    Ok(())
}

/// Download, verify, and swap in `av` in one go, then spawn the new binary.
pub fn apply(av: &Available) -> Result<()> {
    let staged = stage(av)?;
    commit(&staged)
}

/// `(staged binary, marker)` paths, both next to the installed exe.
fn staged_paths() -> Result<(PathBuf, PathBuf)> {
    let installed = crate::install::canonical_exe()?;
    let dir = installed.parent().context("install dir has no parent")?;
    Ok((dir.join(format!(".{BIN_STEM}.staged{EXE_SUFFIX}")), dir.join(format!(".{BIN_STEM}.staged.meta"))))
}

/// A verified update from an earlier [`stage`] that hasn't been committed yet.
/// Self-cleaning: a marker with no binary, an unreadable marker, or one that
/// isn't newer than the running build is deleted and `None` returned.
pub fn staged() -> Option<StagedUpdate> {
    let (bin, meta) = staged_paths().ok()?;
    if !bin.is_file() {
        let _ = std::fs::remove_file(&meta);
        return None;
    }
    let parsed = std::fs::read_to_string(&meta).ok().and_then(|t| serde_json::from_str::<Marker>(&t).ok());
    let Some(marker) = parsed.filter(|m| marker_supersedes(&m.version, version::VERSION)) else {
        discard_staged();
        return None;
    };
    let version = semver::Version::parse(&marker.version).ok()?;
    Some(StagedUpdate { version, tag: marker.tag, path: bin })
}

/// Remove any staged binary + marker. Best-effort.
pub fn discard_staged() {
    if let Ok((bin, meta)) = staged_paths() {
        let _ = std::fs::remove_file(bin);
        let _ = std::fs::remove_file(meta);
    }
}

/// `true` when a staged marker's version parses and is strictly newer than
/// `running`.
fn marker_supersedes(marker_version: &str, running: &str) -> bool {
    matches!((semver::Version::parse(marker_version), semver::Version::parse(running)), (Ok(m), Ok(r)) if m > r)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_supersedes_compares_semver() {
        assert!(marker_supersedes("0.2.0", "0.1.9"));
        assert!(!marker_supersedes("0.1.0", "0.1.0"));
        assert!(!marker_supersedes("0.1.0", "0.2.0"));
        assert!(!marker_supersedes("garbage", "0.1.0"));
    }

    #[test]
    fn marker_round_trips_through_json() {
        let json = serde_json::to_string(&Marker { tag: "v0.2.0".into(), version: "0.2.0".into() }).unwrap();
        let back: Marker = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag, "v0.2.0");
        assert_eq!(back.version, "0.2.0");
    }
}
