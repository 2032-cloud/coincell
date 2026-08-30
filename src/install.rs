//! Self-install / -uninstall: put the binary in a stable per-user location and
//! register it with the OS. No admin rights, nothing outside the user's profile.
//!
//! The stable location is also what makes self-update work: `self-replace` swaps
//! the running executable in place, so it has to sit somewhere the user can
//! write and that won't move under it.
//!
//! - Windows: `%LOCALAPPDATA%\Programs\CoinCell\coincell.exe`, an
//!   `HKCU\...\Run` autostart value, and an Add/Remove Programs entry.
//! - Linux: `~/.local/bin/coincell`, a menu `.desktop`, an icon under
//!   `hicolor`, and (when enabled) an `~/.config/autostart` `.desktop`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::constants::{APP_NAME, PROJECT_DIRS};

const BIN_STEM: &str = "coincell";

/// Absolute path the installed binary lives at.
pub fn canonical_exe() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("no home directory")?;
    #[cfg(windows)]
    let path = dirs.data_local_dir().join("Programs").join(APP_NAME).join(format!("{BIN_STEM}.exe"));
    #[cfg(not(windows))]
    let path = dirs.executable_dir().context("no XDG executable directory")?.join(BIN_STEM);
    Ok(path)
}

/// `true` when this process is running *from* the installed location.
pub fn running_installed() -> bool {
    match (std::env::current_exe(), canonical_exe()) {
        (Ok(here), Ok(installed)) => same_path(&here, &installed),
        _ => false,
    }
}

/// `true` when an installed copy exists on disk (this process or not).
pub fn is_installed() -> bool {
    canonical_exe().map(|p| p.is_file()).unwrap_or(false)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Copy this executable to [`canonical_exe`], write the OS registrations, and
/// return the installed path. Idempotent: re-running over an existing install
/// just refreshes both.
pub fn install() -> Result<PathBuf> {
    let src = std::env::current_exe().context("current_exe")?;
    let dst = canonical_exe()?;

    if same_path(&src, &dst) {
        register().context("refreshing OS registration")?;
        return Ok(dst);
    }

    let dir = dst.parent().context("install path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    place_binary(&src, &dst).with_context(|| format!("copy binary to {}", dst.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
    }

    register().context("OS registration")?;
    tracing::info!("installed to {}", dst.display());
    Ok(dst)
}

/// Copy `src` over `dst`, renaming a busy `dst` aside first (Windows can't
/// overwrite a running exe, but it *can* be renamed).
fn place_binary(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::copy(src, dst) {
        Ok(_) => Ok(()),
        Err(_) if dst.exists() => {
            let aside = dst.with_extension("old");
            let _ = std::fs::remove_file(&aside);
            std::fs::rename(dst, &aside)?;
            std::fs::copy(src, dst)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Undo everything [`install`] did. `purge` also deletes the config, database,
/// logs and art cache. The binary itself goes last (scheduled for deletion on
/// Windows if it's the running process).
pub fn uninstall(purge: bool) -> Result<()> {
    unregister();

    if purge {
        for dir in [PROJECT_DIRS.config_dir(), PROJECT_DIRS.data_dir(), PROJECT_DIRS.cache_dir()] {
            if let Err(e) = std::fs::remove_dir_all(dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("purge {}: {e}", dir.display());
            }
        }
    }

    let dst = canonical_exe()?;
    #[cfg(windows)]
    {
        if running_installed() {
            self_replace::self_delete().context("schedule self-deletion")?;
        } else {
            let _ = std::fs::remove_file(&dst);
        }
        if let Some(dir) = dst.parent() {
            let _ = std::fs::remove_dir(dir); // only if now empty
        }
    }
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&dst);
    }

    tracing::info!("uninstalled{}", if purge { " and purged user data" } else { "" });
    Ok(())
}

/// Toggle the OS "run at login" registration for the installed binary. Called
/// by the Config › Startup checkbox and by [`install`] / [`uninstall`].
pub fn set_autostart(enabled: bool) -> Result<()> {
    imp::set_autostart(enabled)
}

fn register() -> Result<()> {
    imp::register()?;
    let want = crate::config::Config::get(|c| c.startup.launch_on_login);
    imp::set_autostart(want)
}

fn unregister() {
    imp::unregister();
    let _ = imp::set_autostart(false);
}

// ---- Windows ---------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    fn arp_key() -> String {
        format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{APP_NAME}")
    }

    pub(super) fn register() -> Result<()> {
        let exe = canonical_exe()?;
        let exe_s = exe.to_string_lossy().into_owned();
        let dir_s = exe.parent().unwrap_or(Path::new("")).to_string_lossy().into_owned();

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (arp, _) = hkcu.create_subkey(arp_key())?;
        arp.set_value("DisplayName", &APP_NAME)?;
        arp.set_value("DisplayVersion", &crate::version::VERSION)?;
        arp.set_value("Publisher", &crate::constants::COMPANY_NAME)?;
        arp.set_value("DisplayIcon", &exe_s)?;
        arp.set_value("InstallLocation", &dir_s)?;
        arp.set_value("UninstallString", &format!("\"{exe_s}\" --uninstall"))?;
        arp.set_value("QuietUninstallString", &format!("\"{exe_s}\" --uninstall --purge --quiet"))?;
        arp.set_value("NoModify", &1u32)?;
        arp.set_value("NoRepair", &1u32)?;

        // TODO(notifications): also drop a Start Menu shortcut carrying an
        // AppUserModelID so Windows toasts render as "CoinCell" and can relaunch
        // the app from an actioned toast. Needs COM (IShellLink + IPropertyStore)
        // or a registered HKCU\Software\Classes\AppUserModelId entry.
        Ok(())
    }

    pub(super) fn unregister() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all(arp_key());
    }

    pub(super) fn set_autostart(enabled: bool) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu.create_subkey(RUN_KEY)?;
        if enabled {
            let exe = canonical_exe()?;
            run.set_value(APP_NAME, &format!("\"{}\"", exe.to_string_lossy()))?;
        } else {
            let _ = run.delete_value(APP_NAME);
        }
        Ok(())
    }
}

// ---- Linux (and other unix) ---------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::*;
    use crate::constants::ICON_BYTES;

    fn base() -> Result<directories::BaseDirs> {
        directories::BaseDirs::new().context("no home directory")
    }

    fn menu_desktop() -> Result<PathBuf> {
        Ok(base()?.data_dir().join("applications").join(format!("{BIN_STEM}.desktop")))
    }
    fn autostart_desktop() -> Result<PathBuf> {
        Ok(base()?.config_dir().join("autostart").join(format!("{BIN_STEM}.desktop")))
    }
    fn icon_path() -> Result<PathBuf> {
        Ok(base()?.data_dir().join("icons/hicolor/128x128/apps").join(format!("{BIN_STEM}.png")))
    }

    fn desktop_entry(autostart: bool) -> Result<String> {
        let exec = canonical_exe()?;
        let mut s = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={APP_NAME}\n\
             Comment=Sync retro game save files\n\
             Exec=\"{exec}\"\n\
             Icon={BIN_STEM}\n\
             Terminal=false\n\
             Categories=Utility;Game;\n\
             StartupNotify=false\n",
            exec = exec.display(),
        );
        if autostart {
            s.push_str("X-GNOME-Autostart-enabled=true\n");
        }
        Ok(s)
    }

    pub(super) fn register() -> Result<()> {
        let icon = icon_path()?;
        if let Some(dir) = icon.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&icon, ICON_BYTES).with_context(|| format!("write {}", icon.display()))?;

        let menu = menu_desktop()?;
        if let Some(dir) = menu.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&menu, desktop_entry(false)?).with_context(|| format!("write {}", menu.display()))?;
        Ok(())
    }

    pub(super) fn unregister() {
        for p in [menu_desktop(), autostart_desktop(), icon_path()] {
            if let Ok(p) = p {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    pub(super) fn set_autostart(enabled: bool) -> Result<()> {
        let path = autostart_desktop()?;
        if enabled {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, desktop_entry(true)?)?;
        } else {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }
}
