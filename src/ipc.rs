//! Single-instance enforcement over a local socket.

use crate::constants::*;
use std::io::{BufReader, Read as _, Write};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use interprocess::local_socket::{ConnectOptions, GenericFilePath, GenericNamespaced, Listener, ListenerOptions, NameType, ToFsName, ToNsName, traits::ListenerExt};

pub enum Instance {
    Primary(Receiver<()>),
    Secondary,
}

fn socket_name() -> anyhow::Result<interprocess::local_socket::Name<'static>> {
    Ok(if GenericNamespaced::is_supported() { IPC_NAME.to_ns_name::<GenericNamespaced>()? } else { format!("/tmp/{IPC_NAME}.sock").to_fs_name::<GenericFilePath>()? })
}

/// Own the socket, or tell the running instance to show itself and bow out.
pub fn acquire() -> anyhow::Result<Instance> {
    let name = socket_name()?;

    let listener = match ListenerOptions::new().name(name.borrow()).create_sync() {
        Ok(listener) => listener,
        Err(_) => {
            let mut stream = ConnectOptions::new().name(name).connect_sync()?;
            stream.write_all(WAKE_WORD)?;
            return Ok(Instance::Secondary);
        }
    };
    Ok(serve(listener))
}

/// Like [`acquire`], but if the socket is currently held, keep retrying the
/// primary bind for up to `timeout` before falling back to [`acquire`]'s
/// behaviour. Used right after a self-update relaunch, to wait out the previous
/// process releasing its lock instead of instantly bailing as `Secondary`.
pub fn acquire_wait(timeout: Duration) -> anyhow::Result<Instance> {
    let deadline = Instant::now() + timeout;
    loop {
        let name = socket_name()?;
        match ListenerOptions::new().name(name.borrow()).create_sync() {
            Ok(listener) => return Ok(serve(listener)),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(120)),
            Err(_) => return acquire(),
        }
    }
}

/// Spawn the accept loop that turns a wake-word ping from a later instance into
/// a `()` on the returned channel.
fn serve(listener: Listener) -> Instance {
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut buffer = Vec::<u8>::new();
            let mut reader = BufReader::new(&mut stream);
            if reader.read_to_end(&mut buffer).is_ok() && WAKE_WORD == buffer {
                let _ = wake_tx.send(());
            }
        }
    });
    Instance::Primary(wake_rx)
}
