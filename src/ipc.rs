//! Single-instance enforcement over a local socket.

use crate::constants::*;
use std::io::{BufReader, Read as _, Write};
use std::sync::mpsc::Receiver;

use interprocess::local_socket::{ConnectOptions, GenericFilePath, GenericNamespaced, ListenerOptions, NameType, ToFsName, ToNsName, traits::ListenerExt};

pub enum Instance {
    Primary(Receiver<()>),
    Secondary,
}

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
