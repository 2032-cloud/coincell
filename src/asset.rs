//! Persistent on-disk cache for remote art: console and game box art / icons,
//! and IGDB covers.
//!
//! The games catalog can list ~1500 entries. The card grid only asks `egui` for
//! the art of cards that are on or just off screen (it virtualizes rows), and
//! this layer makes each of those a single fetch ever: bytes are written under
//! the OS cache dir and served straight from disk on later runs, so a relaunch
//! costs no network at all.
//!
//! It's an `egui` [`BytesLoader`] for `http` / `https` URIs, registered after
//! `egui_extras::install_image_loaders` so it's tried first (egui walks bytes
//! loaders newest to oldest) and the stock network loader never sees these URIs.
//! On a miss it fetches once, on its own thread, same as the stock loader, then
//! writes the result to disk.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::task::Poll;

use eframe::egui;
use eframe::egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use eframe::egui::mutex::Mutex;

use crate::constants::{CLIENT_NAME, PROJECT_DIRS};
use crate::sync::{sha256_hex, write_atomic};

const PROTOCOLS: [&str; 2] = ["http://", "https://"];
const ID: &str = "coincell::asset::DiskCache";

/// Register the cache on `ctx`. Call once, right after
/// `egui_extras::install_image_loaders`.
pub fn install(ctx: &egui::Context) {
    ctx.add_bytes_loader(Arc::new(DiskCache::new()));
}

type Entry = Poll<Result<Art, String>>;

#[derive(Clone)]
struct Art {
    bytes: Arc<[u8]>,
    mime: Option<String>,
}

struct DiskCache {
    /// `None` if the cache dir couldn't be created; then it's a plain fetcher.
    dir: Option<PathBuf>,
    mem: Arc<Mutex<HashMap<String, Entry>>>,
}

impl DiskCache {
    fn new() -> Self {
        let dir = PROJECT_DIRS.cache_dir().join("art");
        let dir = match fs::create_dir_all(&dir) {
            Ok(()) => Some(dir),
            Err(e) => {
                tracing::warn!("art disk cache off, can't create {}: {e}", dir.display());
                None
            }
        };
        Self { dir, mem: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// `<cache>/art/<sha256(uri)>.<ext>`, ext copied from the URL so `image` and
    /// egui have the format hint they like (falls back to `img`).
    fn disk_path(&self, uri: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let ext = uri
            .rsplit(['/', '?', '#'])
            .next()
            .and_then(|seg| seg.rsplit_once('.'))
            .map(|(_, e)| e)
            .filter(|e| (2..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("img");
        Some(dir.join(format!("{}.{ext}", sha256_hex(uri.as_bytes()))))
    }
}

impl BytesLoader for DiskCache {
    fn id(&self) -> &str {
        ID
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        if !PROTOCOLS.iter().any(|p| uri.starts_with(p)) {
            return Err(LoadError::NotSupported);
        }

        let mut mem = self.mem.lock();
        if let Some(entry) = mem.get(uri).cloned() {
            return match entry {
                Poll::Ready(Ok(art)) => Ok(BytesPoll::Ready { size: None, bytes: Bytes::Shared(art.bytes), mime: art.mime }),
                Poll::Ready(Err(e)) => Err(LoadError::Loading(e)),
                Poll::Pending => Ok(BytesPoll::Pending { size: None }),
            };
        }
        mem.insert(uri.to_owned(), Poll::Pending);
        drop(mem);

        let uri = uri.to_owned();
        let path = self.disk_path(&uri);
        let mem = Arc::clone(&self.mem);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("art-cache".into())
            .spawn(move || {
                let result = resolve(&uri, path.as_deref());
                if let Err(e) = &result {
                    tracing::debug!("art {uri}: {e}");
                }
                mem.lock().insert(uri, Poll::Ready(result));
                ctx.request_repaint();
            })
            .expect("spawn art-cache thread");

        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.mem.lock().remove(uri);
    }

    fn forget_all(&self) {
        self.mem.lock().clear();
    }

    fn byte_size(&self) -> usize {
        self.mem.lock().values().map(|e| if let Poll::Ready(Ok(a)) = e { a.bytes.len() } else { 0 }).sum()
    }

    fn has_pending(&self) -> bool {
        self.mem.lock().values().any(|e| matches!(e, Poll::Pending))
    }
}

/// Disk first, then network once, persisting what it fetches.
fn resolve(uri: &str, disk: Option<&Path>) -> Result<Art, String> {
    if let Some(path) = disk
        && let Ok(bytes) = fs::read(path)
        && !bytes.is_empty()
    {
        return Ok(Art { bytes: bytes.into(), mime: None });
    }

    let resp = http().get(uri).send().map_err(|e| format!("fetch: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let mime = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|s| s.split(';').next().unwrap_or(s).trim().to_owned());
    let bytes = resp.bytes().map_err(|e| format!("read body: {e}"))?.to_vec();

    if let Some(path) = disk
        && let Err(e) = write_atomic(path, &bytes)
    {
        tracing::debug!("art cache write {}: {e}", path.display());
    }
    Ok(Art { bytes: bytes.into(), mime })
}

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::blocking::Client::builder().user_agent(CLIENT_NAME.as_str()).build().expect("build art http client"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> DiskCache {
        DiskCache { dir: Some(std::env::temp_dir().join("coincell-art-test")), mem: Arc::new(Mutex::new(HashMap::new())) }
    }

    #[test]
    fn disk_path_is_stable_and_keeps_a_sane_extension() {
        let c = cache();
        let a = c.disk_path("https://x/api/consoles/gba/games/foo/box_art.png").unwrap();
        let b = c.disk_path("https://x/api/consoles/gba/games/foo/box_art.png").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.extension().unwrap(), "png");

        let igdb = c.disk_path("https://images.igdb.com/igdb/image/upload/t_cover_big/co1abc.jpg").unwrap();
        assert_eq!(igdb.extension().unwrap(), "jpg");
        assert_ne!(igdb, a);
    }

    #[test]
    fn disk_path_defaults_the_extension_when_the_url_has_none() {
        let c = cache();
        let p = c.disk_path("https://example.com/art/12345").unwrap();
        assert_eq!(p.extension().unwrap(), "img");
    }

    #[test]
    fn non_http_uris_are_not_ours() {
        let c = cache();
        let ctx = egui::Context::default();
        assert!(matches!(c.load(&ctx, "file:///tmp/x.png"), Err(LoadError::NotSupported)));
        assert!(matches!(c.load(&ctx, "bytes://x"), Err(LoadError::NotSupported)));
    }
}
