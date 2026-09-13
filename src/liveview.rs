//! In-place human hand-off. The headless browser keeps running untouched; the human gets a live
//! view of the current tab in a local viewer window (their own Chrome, as an app window), which
//! streams CDP screencast frames and sends mouse, keyboard and paste back as trusted input. Page
//! state is never reloaded or copied, because it never leaves the browser it lives in.
//!
//! The viewer is served on 127.0.0.1 behind an unguessable token and exists only while the
//! hand-off runs.

use crate::browser::{Browser, PageRef};
use crate::keys;
use anyhow::{anyhow, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

const VIEWER: &str = include_str!("liveview.html");

pub struct Frame {
    meta: String,
    jpeg: Vec<u8>,
}

/// Where the CDP event handler drops screencast frames of the tab being viewed.
pub struct LiveSink {
    pub session: String,
    frames: watch::Sender<Option<Arc<Frame>>>,
}

impl LiveSink {
    pub fn publish(&self, params: &Value) {
        let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str().unwrap_or_default()) else { return };
        let mut meta = params["metadata"].clone();
        meta["t"] = json!("frame");
        // A watch channel keeps only the newest frame, so a slow viewer skips frames instead of lagging.
        self.frames.send_replace(Some(Arc::new(Frame { meta: meta.to_string(), jpeg })));
    }
}

struct Channels {
    token: String,
    message: String,
    frames: watch::Receiver<Option<Arc<Frame>>>,
    page: watch::Receiver<Value>,
    dialog: watch::Receiver<Value>,
    finished: watch::Receiver<bool>,
    control: mpsc::UnboundedSender<Value>,
    connections: Arc<AtomicUsize>,
}

enum Outcome {
    Done,
    Closed,
    Timeout,
    BrowserGone,
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens the viewer as a new tab in the user's default browser, next to everything they already
/// have open, rather than as a separate window.
fn open_viewer(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn mouse_params(m: &Value) -> Value {
    let mut p = json!({ "type": m["type"], "x": m["x"], "y": m["y"], "modifiers": m["modifiers"] });
    for k in ["button", "buttons", "clickCount", "deltaX", "deltaY"] {
        if !m[k].is_null() {
            p[k] = m[k].clone();
        }
    }
    p
}

fn key_params(m: &Value) -> Value {
    let key = m["key"].as_str().unwrap_or_default();
    let mask = m["modifiers"].as_i64().unwrap_or(0);
    let mut p = json!({ "type": m["type"], "key": key, "modifiers": mask });
    // CDP rejects the whole event when an optional field is null, so copy only the ones present.
    for (from, to) in [("code", "code"), ("keyCode", "windowsVirtualKeyCode"), ("location", "location"), ("autoRepeat", "autoRepeat")] {
        if !m[from].is_null() {
            p[to] = m[from].clone();
        }
    }
    if let Some(t) = m["text"].as_str() {
        p["text"] = json!(t);
        p["unmodifiedText"] = json!(t);
    }
    if m["type"] != "keyUp" {
        let commands = keys::mac_commands(mask, key);
        if !commands.is_empty() {
            p["commands"] = json!(commands);
        }
    }
    p
}

impl Browser {
    async fn start_screencast(&self, page: &PageRef) -> Result<()> {
        page.cdp
            // Every frame, at up to 2560px: sharp text; on localhost the larger JPEGs cost no noticeable latency.
            .send_session(&page.session, "Page.startScreencast", json!({ "format": "jpeg", "quality": 85, "maxWidth": 2560, "maxHeight": 1600, "everyNthFrame": 1 }))
            .await
            .map_err(|e| anyhow!(e))?;
        // A static page may not repaint for a while; send one frame right away.
        let metrics = page.cdp.send_session(&page.session, "Page.getLayoutMetrics", json!({})).await.map_err(|e| anyhow!(e))?;
        if let Ok(shot) = page.cdp.send_session(&page.session, "Page.captureScreenshot", json!({ "format": "jpeg", "quality": 85, "optimizeForSpeed": true })).await {
            let vv = &metrics["cssVisualViewport"];
            let params = json!({ "data": shot["data"], "metadata": { "deviceWidth": vv["clientWidth"], "deviceHeight": vv["clientHeight"], "offsetTop": 0, "pageScaleFactor": 1 } });
            if let Some(live) = self.state.lock().unwrap().live.as_ref().filter(|l| l.session == page.session) {
                live.publish(&params);
            }
        }
        Ok(())
    }

    /// Hands the running browser to a human through the live viewer and waits until they click
    /// Done, close the viewer, or the timeout passes. Nothing is relaunched or reloaded.
    pub async fn hand_off(&self, message: &str, timeout: Duration) -> Result<String> {
        let mut page = self.page().await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let token = random_token();
        let url = format!("http://127.0.0.1:{}/handoff/{token}", listener.local_addr()?.port());

        let (frames_tx, frames_rx) = watch::channel(None);
        let (page_tx, page_rx) = watch::channel(Value::Null);
        let (dialog_tx, dialog_rx) = watch::channel(Value::Null);
        let (finished_tx, finished_rx) = watch::channel(false);
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Value>();
        let connections = Arc::new(AtomicUsize::new(0));

        self.state.lock().unwrap().live = Some(LiveSink { session: page.session.clone(), frames: frames_tx });
        self.start_screencast(&page).await?;
        let server = tokio::spawn(serve(
            listener,
            Channels {
                token,
                message: message.to_string(),
                frames: frames_rx,
                page: page_rx,
                dialog: dialog_rx,
                finished: finished_rx,
                control: control_tx,
                connections: connections.clone(),
            },
        ));
        eprintln!("browser-rs: hand-off viewer at {url}");
        if std::env::var_os("BROWSER_RS_NO_VIEWER").is_none() {
            open_viewer(&url);
        }

        // Input is replayed in order by one task, so a slow event never stalls the viewer loop.
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<(String, &'static str, Value)>();
        let input_cdp = page.cdp.clone();
        let input_task = tokio::spawn(async move {
            while let Some((session, method, params)) = input_rx.recv().await {
                let _ = tokio::time::timeout(Duration::from_secs(2), input_cdp.send_session(&session, method, params)).await;
            }
        });

        let started = Instant::now();
        let mut tabs_seen = self.state.lock().unwrap().tabs.len();
        let mut closed_at: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(150));
        let outcome = loop {
            tokio::select! {
                msg = control_rx.recv() => {
                    let Some(msg) = msg else { break Outcome::Closed };
                    let s = page.session.clone();
                    match msg["t"].as_str().unwrap_or_default() {
                        "mouse" => { let _ = input_tx.send((s, "Input.dispatchMouseEvent", mouse_params(&msg))); }
                        "key" => { let _ = input_tx.send((s, "Input.dispatchKeyEvent", key_params(&msg))); }
                        "text" => { let _ = input_tx.send((s, "Input.insertText", json!({ "text": msg["text"] }))); }
                        "reload" => { let _ = input_tx.send((s, "Page.reload", json!({}))); }
                        "back" => {
                            let p = page.clone();
                            if let Ok(h) = p.cdp.send_session(&p.session, "Page.getNavigationHistory", json!({})).await {
                                let i = h["currentIndex"].as_i64().unwrap_or(0);
                                if i > 0 {
                                    let _ = input_tx.send((s, "Page.navigateToHistoryEntry", json!({ "entryId": h["entries"][(i - 1) as usize]["id"] })));
                                }
                            }
                        }
                        "tab" => { if let Some(i) = msg["index"].as_u64() { let _ = self.tab_select(i as usize).await; } }
                        "dialog" => {
                            let mut p = json!({ "accept": msg["accept"].as_bool().unwrap_or(false) });
                            if let Some(text) = msg["text"].as_str() { p["promptText"] = json!(text); }
                            let _ = page.cdp.send_session(&page.session, "Page.handleJavaScriptDialog", p).await;
                            let _ = self.tab_write(&page, |t| t.dialog = None);
                        }
                        "done" => break Outcome::Done,
                        "closed" => closed_at = Some(Instant::now()),
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    if page.cdp.is_closed() { break Outcome::BrowserGone; }
                    if started.elapsed() > timeout { break Outcome::Timeout; }
                    if let Some(t) = closed_at {
                        // "closed" also fires when the viewer merely reloads; only a viewer that stays gone counts.
                        if connections.load(Ordering::SeqCst) > 0 { closed_at = None; }
                        else if t.elapsed() > Duration::from_secs(2) { break Outcome::Closed; }
                    }
                    let (info, dialog, current) = {
                        let mut st = self.state.lock().unwrap();
                        // Follow tabs the page opens (OAuth pop-ups and the like), as a browser would.
                        if st.tabs.len() > tabs_seen { st.current = st.tabs.len() - 1; }
                        tabs_seen = st.tabs.len();
                        let cur = st.tabs.get(st.current);
                        let info = json!({
                            "t": "page", "url": cur.map(|t| t.url.clone()).unwrap_or_default(),
                            "tabs": st.tabs.iter().map(|t| t.title.clone()).collect::<Vec<_>>(), "current": st.current,
                        });
                        let dialog = cur.and_then(|t| t.dialog.as_ref()).map(|d| json!({ "t": "dialog", "kind": d.kind, "message": d.message, "default": d.default_prompt })).unwrap_or(Value::Null);
                        (info, dialog, cur.map(|t| t.session_id.clone()))
                    };
                    page_tx.send_if_modified(|v| if *v != info { *v = info; true } else { false });
                    dialog_tx.send_if_modified(|v| if *v != dialog { *v = dialog; true } else { false });
                    if let Some(cur) = current.filter(|c| *c != page.session) {
                        let _ = page.cdp.send_session(&page.session, "Page.stopScreencast", json!({})).await;
                        if let Ok(next) = self.page().await {
                            page = next;
                            if let Some(live) = self.state.lock().unwrap().live.as_mut() { live.session = cur; }
                            let _ = self.start_screencast(&page).await;
                        }
                    }
                }
            }
        };

        finished_tx.send_replace(true);
        let _ = page.cdp.send_session(&page.session, "Page.stopScreencast", json!({})).await;
        self.state.lock().unwrap().live = None;
        drop(input_tx);
        let _ = input_task.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.abort();

        Ok(match outcome {
            Outcome::Done => "The user finished and handed control back. The page kept its exact state.".into(),
            Outcome::Closed => "The user closed the viewer, which counts as done. The page kept its exact state.".into(),
            Outcome::Timeout => format!("The user did not finish within {}s; continuing with the page as it is now.", timeout.as_secs()),
            Outcome::BrowserGone => "The browser closed during the hand-off; it restarts on the next call.".into(),
        })
    }
}

async fn serve(listener: TcpListener, ch: Channels) {
    let ch = Arc::new(ch);
    while let Ok((stream, _)) = listener.accept().await {
        let ch = ch.clone();
        tokio::spawn(async move {
            let _ = connection(stream, &ch).await;
        });
    }
}

async fn connection(mut stream: TcpStream, ch: &Channels) -> Result<()> {
    // Small input messages must go out immediately, not wait to be coalesced by Nagle's algorithm.
    let _ = stream.set_nodelay(true);
    // Peek at the request line to route it; the WebSocket handshake then reads the request itself.
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let n = stream.peek(&mut buf).await?;
            if n == 0 || buf[..n].windows(2).any(|w| w == b"\r\n") || n == buf.len() {
                return Ok::<usize, std::io::Error>(n);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();

    if path == format!("/handoff/{}", ch.token) || path != format!("/ws/{}", ch.token) {
        // Read the request before replying: closing a socket with unread request bytes resets the
        // connection and truncates the response on the client.
        let mut request = Vec::new();
        let mut chunk = [0u8; 2048];
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 65536 {
                match tokio::io::AsyncReadExt::read(&mut stream, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await;
    }
    if path == format!("/handoff/{}", ch.token) {
        let body = VIEWER.as_bytes();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await?;
        stream.write_all(body).await?;
        return Ok(());
    }
    if path != format!("/ws/{}", ch.token) {
        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }

    let ws = tokio_tungstenite::accept_async(stream).await?;
    ch.connections.fetch_add(1, Ordering::SeqCst);
    let result = viewer_session(ws, ch).await;
    ch.connections.fetch_sub(1, Ordering::SeqCst);
    result
}

async fn viewer_session(ws: tokio_tungstenite::WebSocketStream<TcpStream>, ch: &Channels) -> Result<()> {
    let (mut tx, mut rx) = ws.split();
    let (mut frames, mut page, mut dialog, mut finished) = (ch.frames.clone(), ch.page.clone(), ch.dialog.clone(), ch.finished.clone());
    tx.send(Message::text(json!({ "t": "info", "message": ch.message }).to_string())).await?;
    let current_page = page.borrow_and_update().clone();
    if !current_page.is_null() {
        tx.send(Message::text(current_page.to_string())).await?;
    }
    let first = frames.borrow_and_update().clone();
    if let Some(f) = first {
        tx.send(Message::text(f.meta.clone())).await?;
        tx.send(Message::binary(f.jpeg.clone())).await?;
    }
    loop {
        tokio::select! {
            r = frames.changed() => {
                if r.is_err() { break; }
                let frame = frames.borrow_and_update().clone();
                if let Some(f) = frame {
                    tx.send(Message::text(f.meta.clone())).await?;
                    tx.send(Message::binary(f.jpeg.clone())).await?;
                }
            }
            r = page.changed() => {
                if r.is_err() { break; }
                let v = page.borrow_and_update().clone();
                tx.send(Message::text(v.to_string())).await?;
            }
            r = dialog.changed() => {
                if r.is_err() { break; }
                let v = dialog.borrow_and_update().clone();
                if !v.is_null() { tx.send(Message::text(v.to_string())).await?; }
            }
            _ = finished.changed() => {
                let _ = tx.send(Message::text(json!({ "t": "finished" }).to_string())).await;
                break;
            }
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                        let _ = ch.control.send(v);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    Ok(())
}
