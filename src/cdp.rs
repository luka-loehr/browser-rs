//! A minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//!
//! Chrome reads NUL-terminated JSON messages from fd 3 and writes them to fd 4. There is no
//! websocket, no HTTP discovery endpoint and no port: two pipes and two threads. Replies are
//! matched to requests by id; events go to one synchronous handler (which updates browser
//! state) and to one-shot waiters registered by callers that need to block on an event.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Reply = Result<Value, String>;
type Handler = Box<dyn Fn(&Event) + Send + Sync>;

struct Waiter {
    pred: Box<dyn Fn(&Event) -> bool + Send>,
    tx: oneshot::Sender<Event>,
}

struct Shared {
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    waiters: Mutex<Vec<Waiter>>,
    closed: AtomicBool,
}

pub struct Cdp {
    writer: Mutex<File>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Cdp {
    /// `to_chrome` is our end of Chrome's fd 3, `from_chrome` our end of its fd 4.
    /// `handler` sees every event, on the reader thread, before any waiter does.
    pub fn new(to_chrome: File, from_chrome: File, handler: Handler) -> Arc<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            waiters: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });
        let reader_shared = shared.clone();
        std::thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || read_loop(from_chrome, reader_shared, handler))
            .expect("spawn cdp reader");
        Arc::new(Self { writer: Mutex::new(to_chrome), shared, next_id: AtomicU64::new(1) })
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    pub async fn send(&self, method: &str, params: Value) -> Reply {
        self.send_full(None, method, params, DEFAULT_TIMEOUT).await
    }

    pub async fn send_session(&self, session: &str, method: &str, params: Value) -> Reply {
        self.send_full(Some(session), method, params, DEFAULT_TIMEOUT).await
    }

    pub async fn send_full(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Reply {
        if self.is_closed() {
            return Err("browser is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);
        let mut bytes = serde_json::to_vec(&msg).unwrap();
        bytes.push(0);
        if let Err(e) = self.writer.lock().unwrap().write_all(&bytes) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(format!("write to browser failed: {e}"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err("browser is closed".into()),
            Err(_) => {
                self.shared.pending.lock().unwrap().remove(&id);
                Err(format!("{method} timed out after {}ms", timeout.as_millis()))
            }
        }
    }

    /// Register interest in an event *before* triggering it, then await the receiver.
    pub fn wait_for(&self, pred: impl Fn(&Event) -> bool + Send + 'static) -> oneshot::Receiver<Event> {
        let (tx, rx) = oneshot::channel();
        self.shared.waiters.lock().unwrap().push(Waiter { pred: Box::new(pred), tx });
        rx
    }
}

fn read_loop(from_chrome: File, shared: Arc<Shared>, handler: Handler) {
    let mut reader = BufReader::with_capacity(1 << 16, from_chrome);
    let mut buf = Vec::with_capacity(1 << 16);
    loop {
        buf.clear();
        match reader.read_until(0, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        let Ok(mut msg) = serde_json::from_slice::<Value>(&buf) else { continue };
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let reply = match msg.get_mut("error") {
                Some(err) => Err(err.get("message").and_then(Value::as_str).unwrap_or("protocol error").to_string()),
                None => Ok(msg.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            if let Some(tx) = shared.pending.lock().unwrap().remove(&id) {
                let _ = tx.send(reply);
            }
            continue;
        }
        let event = Event {
            method: msg.get("method").and_then(Value::as_str).unwrap_or_default().to_string(),
            params: msg.get_mut("params").map(Value::take).unwrap_or(Value::Null),
            session_id: msg.get("sessionId").and_then(Value::as_str).map(str::to_string),
        };
        handler(&event);
        let mut waiters = shared.waiters.lock().unwrap();
        let mut i = 0;
        while i < waiters.len() {
            if waiters[i].tx.is_closed() {
                waiters.swap_remove(i);
            } else if (waiters[i].pred)(&event) {
                let w = waiters.swap_remove(i);
                let _ = w.tx.send(event.clone());
            } else {
                i += 1;
            }
        }
    }
    shared.closed.store(true, Ordering::SeqCst);
    for (_, tx) in shared.pending.lock().unwrap().drain() {
        let _ = tx.send(Err("browser is closed".into()));
    }
    shared.waiters.lock().unwrap().clear();
    handler(&Event { method: "__closed".into(), params: Value::Null, session_id: None });
}
