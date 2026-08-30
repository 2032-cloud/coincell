//! Resolves the one build-time version string from git, emitted as `rustc-env`
//! vars that `src/version.rs` reads:
//!
//! - `COINCELL_VERSION` - `X.Y.Z` on a release build (HEAD is exactly on a
//!   clean `vX.Y.Z` tag), else `{CARGO_PKG_VERSION}+dev.{short_hash}` with a
//!   trailing `-dirty` when the tree isn't clean.
//! - `COINCELL_CHANNEL` - `stable` (plain semver tag), `prerelease` (tag with a
//!   pre-release segment), or `development` (everything else). The updater only
//!   self-updates the first two.
//! - `COINCELL_COMMIT` - short commit hash, or `unknown` when built without git.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Best-effort: re-run when HEAD moves or something is staged. Working-tree
    // edits that aren't staged won't re-trigger, but dev builds aren't
    // distributed so a slightly stale `-dirty` marker is harmless.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let (version, channel, commit) = resolve(&pkg);

    println!("cargo:rustc-env=COINCELL_VERSION={version}");
    println!("cargo:rustc-env=COINCELL_CHANNEL={channel}");
    println!("cargo:rustc-env=COINCELL_COMMIT={commit}");
}

/// Run `git` with `args`; `None` if git is missing, not a repo, or the command
/// fails or prints nothing.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn resolve(pkg: &str) -> (String, String, String) {
    let Some(commit) = git(&["rev-parse", "--short=9", "HEAD"]) else {
        // No git (a source tarball): bare package version, never self-updates.
        return (pkg.to_owned(), "development".to_owned(), "unknown".to_owned());
    };

    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());

    if !dirty && let Some(tag) = git(&["describe", "--tags", "--exact-match", "--match", "v[0-9]*"]) {
        let ver = tag.strip_prefix('v').unwrap_or(&tag).to_owned();
        // A pre-release segment ("-rc.1", "-beta") ⇒ prerelease channel.
        let channel = if ver.contains('-') { "prerelease" } else { "stable" };
        return (ver, channel.to_owned(), commit);
    }

    let suffix = if dirty { "-dirty" } else { "" };
    (format!("{pkg}+dev.{commit}{suffix}"), "development".to_owned(), commit)
}
