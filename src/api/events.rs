//! Realtime notifications from `GET /api/events` (a WebSocket).
//!
//! The server sends `{"type":"save","instanceId":"…"}` whenever another device
//! uploads a genuinely new save. It's a pure relay with no backfill, a missed
//! message is caught by the next `?since=` poll, so this type only surfaces
//! "instance X changed", never the save itself. Reconnects on its own with
//! backoff.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::Duration;

use serde::Deserialize;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// The socket is up (fired on every (re)connect).
    Connected,
    /// A new save landed for this instance elsewhere. Fetch details with a poll.
    SaveChanged { instance_id: String },
    /// The socket dropped; a reconnect attempt follows.
    Disconnected { reason: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Wire {
    Save {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    #[serde(other)]
    Unknown,
}

pub struct EventStream {
    rx: Receiver<StreamEvent>,
    stop: Arc<AtomicBool>,
}

impl EventStream {
    pub fn start(base: &str, session: &str, wake: impl Fn() + Send + 'static) -> Self {
        let url = ws_url(base);
        let session = session.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);

        std::thread::Builder::new().name("api-events".into()).spawn(move || run(&url, &session, &tx, &stop_worker, &wake)).expect("spawn api-events thread");

        Self { rx, stop }
    }

    pub fn events(&self) -> TryIter<'_, StreamEvent> {
        self.rx.try_iter()
    }

    /// Block up to `dur` for the next event. `None` on timeout or once the
    /// worker has exited.
    pub fn recv_timeout(&self, dur: Duration) -> Option<StreamEvent> {
        self.rx.recv_timeout(dur).ok()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let scheme_swapped =
        base.strip_prefix("https://").map(|rest| format!("wss://{rest}")).or_else(|| base.strip_prefix("http://").map(|rest| format!("ws://{rest}"))).unwrap_or_else(|| base.to_owned());
    format!("{scheme_swapped}/api/events")
}

fn run(url: &str, session: &str, tx: &Sender<StreamEvent>, stop: &AtomicBool, wake: &impl Fn()) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        match connect_once(url, session, tx, stop, wake) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(reason) => {
                let _ = tx.send(StreamEvent::Disconnected { reason });
                wake();
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        sleep_with_stop(backoff, stop);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One connection's lifetime. `Ok` = a clean close or a stop request; `Err` =
/// something to reconnect after.
fn connect_once(url: &str, session: &str, tx: &Sender<StreamEvent>, stop: &AtomicBool, wake: &impl Fn()) -> Result<(), String> {
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    let bearer = format!("Bearer {session}").parse().map_err(|_| "invalid session for auth header".to_string())?;
    request.headers_mut().insert("Authorization", bearer);

    let (mut socket, _response) = tungstenite::connect(request).map_err(|e| e.to_string())?;
    set_read_timeout(&mut socket, Duration::from_millis(500));

    let _ = tx.send(StreamEvent::Connected);
    wake();

    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }

        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(Wire::Save { instance_id }) = serde_json::from_str(text.as_str()) {
                    let _ = tx.send(StreamEvent::SaveChanged { instance_id });
                    wake();
                }
            }
            Ok(Message::Close(_)) => return Err("server closed the connection".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                // Idle tick: nothing to read, so push any queued pong out.
                let _ = socket.flush();
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Err("connection closed".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn set_read_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, dur: Duration) {
    match socket.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(dur));
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(dur));
        }
        _ => {}
    }
}

fn sleep_with_stop(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut slept = Duration::ZERO;
    while slept < total && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        slept += step;
    }
}
