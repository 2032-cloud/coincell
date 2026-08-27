//! Single-instance enforcement over a local socket.
//!
//! The first process to start claims a named socket and listens on it. Any later
//! process fails to claim the socket, connects to the existing one, sends the
//! wake word, and exits — which the primary instance turns into a "bring the
//! window back" request.

use crate::constants::*;
use std::io::{BufReader, Read as _, Write};
use std::sync::mpsc::Receiver;

use interprocess::local_socket::{ConnectOptions, GenericFilePath, GenericNamespaced, ListenerOptions, NameType, ToFsName, ToNsName, traits::ListenerExt};

/// Outcome of trying to claim the single-instance lock.
pub enum Instance {
    /// This process is the primary instance. The channel yields a message every
    /// time another launch attempt asks us to wake up.
    Primary(Receiver<()>),
    /// Another instance was already running and has been signalled; this process
    /// should exit immediately.
    Secondary,
}

/// Attempt to become the single running instance of the app.
pub fn acquire() -> anyhow::Result<Instance> {
    let name = if GenericNamespaced::is_supported() {
        IPC_NAME.to_ns_name::<GenericNamespaced>()?
    } else {
        format!("/tmp/{IPC_NAME}.sock").to_fs_name::<GenericFilePath>()?
    };

    let listener = match ListenerOptions::new().name(name.borrow()).create_sync() {
        Ok(listener) => listener,
        Err(_) => {
            let mut stream = ConnectOptions::new().name(name).connect_sync()?;
            stream.write_all(WAKE_WORD)?;
            return Ok(Instance::Secondary);
        }
    };

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

    Ok(Instance::Primary(wake_rx))
}
