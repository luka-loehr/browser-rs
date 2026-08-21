//! A browser-automation MCP server built on the OS's native webview (WKWebView on macOS,
//! via `wry`) instead of a bundled Chromium binary. `tao`'s event loop owns the main thread
//! and the actual WebView; the MCP stdio server runs on a background thread and sends it
//! commands over a channel, since a webview must live on the platform's UI thread.
//!
//! **Headless by default.** The window is created hidden (`with_visible(false)`) — WKWebView
//! renders and can be navigated/evaluated/screenshotted whether or not its window is on screen.
//! Call `open_window()`/`close_window()` to show or hide it; neither affects the server process.
//! The process's lifecycle is tied only to its MCP stdio connection (same model as
//! `@playwright/mcp`): closing the window — by the user clicking its close button, or via
//! `close_window()` — just hides it, it does not exit the process. The server only exits when
//! its client disconnects (stdin EOF).
//!
//! macOS only for now — Windows (WebView2) / Linux (WebKitGTK) would need their own code paths
//! behind wry's cross-platform API, not implemented here yet.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use tokio::sync::oneshot;
use wry::WebViewBuilder;

const DEFAULT_WIDTH: f64 = 1280.0;
const DEFAULT_HEIGHT: f64 = 800.0;
const MAX_CONSOLE_ENTRIES: usize = 500;

type JsResult = Result<String, String>;
type ConsoleLog = Arc<Mutex<Vec<String>>>;

enum UserEvent {
    Navigate(String, oneshot::Sender<JsResult>),
    Eval(String, oneshot::Sender<JsResult>),
    Screenshot(oneshot::Sender<Result<Vec<u8>, String>>),
    Resize(f64, f64, oneshot::Sender<JsResult>),
    ResetSize(oneshot::Sender<JsResult>),
    Zoom(f64, oneshot::Sender<JsResult>),
    OpenWindow(oneshot::Sender<JsResult>),
    CloseWindow(oneshot::Sender<JsResult>),
    WindowStatus(oneshot::Sender<JsResult>),
}

#[derive(Clone)]
struct BrowserHandle {
    proxy: EventLoopProxy<UserEvent>,
    console: ConsoleLog,
}

impl BrowserHandle {
    async fn navigate(&self, url: String) -> JsResult {
        self.roundtrip(|tx| UserEvent::Navigate(url, tx)).await
    }

    async fn eval(&self, js: String) -> JsResult {
        self.roundtrip(|tx| UserEvent::Eval(js, tx)).await
    }

    async fn resize(&self, w: f64, h: f64) -> JsResult {
        self.roundtrip(|tx| UserEvent::Resize(w, h, tx)).await
    }

    async fn reset_size(&self) -> JsResult {
        self.roundtrip(UserEvent::ResetSize).await
    }

    async fn zoom(&self, scale: f64) -> JsResult {
        self.roundtrip(|tx| UserEvent::Zoom(scale, tx)).await
    }

    async fn open_window(&self) -> JsResult {
        self.roundtrip(UserEvent::OpenWindow).await
    }

    async fn close_window(&self) -> JsResult {
        self.roundtrip(UserEvent::CloseWindow).await
    }

    async fn window_status(&self) -> JsResult {
        self.roundtrip(UserEvent::WindowStatus).await
    }

    async fn screenshot(&self) -> Result<Vec<u8>, String> {
        let (tx, rx) = oneshot::channel();
        if self.proxy.send_event(UserEvent::Screenshot(tx)).is_err() {
            return Err("browser window has closed".into());
        }
        rx.await.unwrap_or_else(|_| Err("browser window has closed".into()))
    }

    async fn roundtrip(&self, build: impl FnOnce(oneshot::Sender<JsResult>) -> UserEvent) -> JsResult {
        let (tx, rx) = oneshot::channel();
        if self.proxy.send_event(build(tx)).is_err() {
            return Err("browser window has closed".into());
        }
        rx.await.unwrap_or_else(|_| Err("browser window has closed".into()))
    }
}

fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SelectorParams {
    /// CSS selector of the target element
    selector: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ClickAtParams {
    /// X coordinate in CSS pixels, relative to the viewport
    x: f64,
    /// Y coordinate in CSS pixels, relative to the viewport
    y: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TypeParams {
    selector: String,
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EvalParams {
    /// JavaScript expression to evaluate in the page; its value is returned
    code: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ScrollParams {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ScrollByParams {
    dx: f64,
    dy: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResizeParams {
    /// New window width in logical (CSS) pixels
    width: f64,
    /// New window height in logical (CSS) pixels
    height: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ZoomParams {
    /// 1.0 = 100%, 1.5 = 150%, 0.5 = 50%, etc.
    scale: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConsoleLogsParams {
    /// Only return the last N entries (default: all captured, up to 500)
    limit: Option<usize>,
}

#[derive(Clone)]
struct BrowserServer {
    browser: BrowserHandle,
}

fn to_mcp_err(e: String) -> McpError {
    McpError::internal_error(e, None)
}

#[tool_router]
impl BrowserServer {
    #[tool(description = "Navigate the browser window to a URL")]
    async fn navigate(&self, Parameters(p): Parameters<NavigateParams>) -> Result<String, McpError> {
        self.browser.navigate(p.url).await.map_err(to_mcp_err)
    }

    #[tool(description = "Click the first element matching a CSS selector")]
    async fn click(&self, Parameters(p): Parameters<SelectorParams>) -> Result<String, McpError> {
        let js = format!(
            "(function(){{ const el = document.querySelector({sel}); if (!el) return JSON.stringify({{ok:false,error:'not found'}}); el.scrollIntoView({{block:'center',inline:'center'}}); el.click(); return JSON.stringify({{ok:true}}); }})()",
            sel = js_string_literal(&p.selector)
        );
        self.browser.eval(js).await.map_err(to_mcp_err)
    }

    #[tool(description = "Click at exact viewport coordinates (CSS pixels), regardless of what's under them — use when you need to click a precise point rather than a selector")]
    async fn click_at(&self, Parameters(p): Parameters<ClickAtParams>) -> Result<String, McpError> {
        let js = format!(
            "(function(){{ const el = document.elementFromPoint({x},{y}); if (!el) return JSON.stringify({{ok:false,error:'nothing at that point'}}); el.click(); return JSON.stringify({{ok:true, tag: el.tagName}}); }})()",
            x = p.x, y = p.y
        );
        self.browser.eval(js).await.map_err(to_mcp_err)
    }

    #[tool(description = "Type text into the first element matching a CSS selector (sets .value and fires input/change events, so React/Vue-style forms pick it up)")]
    async fn r#type(&self, Parameters(p): Parameters<TypeParams>) -> Result<String, McpError> {
        let js = format!(
            "(function(){{ const el = document.querySelector({sel}); if (!el) return JSON.stringify({{ok:false,error:'not found'}}); el.focus(); el.value = {text}; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return JSON.stringify({{ok:true}}); }})()",
            sel = js_string_literal(&p.selector),
            text = js_string_literal(&p.text)
        );
        self.browser.eval(js).await.map_err(to_mcp_err)
    }

    #[tool(description = "Scroll to an absolute position (window.scrollTo)")]
    async fn scroll(&self, Parameters(p): Parameters<ScrollParams>) -> Result<String, McpError> {
        let js = format!("window.scrollTo({}, {}); JSON.stringify({{ok:true}})", p.x, p.y);
        self.browser.eval(js).await.map_err(to_mcp_err)
    }

    #[tool(description = "Scroll relative to the current position (window.scrollBy)")]
    async fn scroll_by(&self, Parameters(p): Parameters<ScrollByParams>) -> Result<String, McpError> {
        let js = format!("window.scrollBy({}, {}); JSON.stringify({{ok:true}})", p.dx, p.dy);
        self.browser.eval(js).await.map_err(to_mcp_err)
    }

    #[tool(description = "Get the visible text content of the page (document.body.innerText)")]
    async fn get_text(&self) -> Result<String, McpError> {
        self.browser.eval("document.body.innerText".to_string()).await.map_err(to_mcp_err)
    }

    #[tool(description = "Get the full HTML source of the current page (document.documentElement.outerHTML)")]
    async fn get_html(&self) -> Result<String, McpError> {
        self.browser
            .eval("document.documentElement.outerHTML".to_string())
            .await
            .map_err(to_mcp_err)
    }

    #[tool(description = "Evaluate arbitrary JavaScript in the page and return its result")]
    async fn eval_js(&self, Parameters(p): Parameters<EvalParams>) -> Result<String, McpError> {
        self.browser.eval(p.code).await.map_err(to_mcp_err)
    }

    #[tool(description = "Resize the browser window (logical/CSS pixels)")]
    async fn resize(&self, Parameters(p): Parameters<ResizeParams>) -> Result<String, McpError> {
        self.browser.resize(p.width, p.height).await.map_err(to_mcp_err)
    }

    #[tool(description = "Reset the browser window back to its default size (1280x800)")]
    async fn reset_size(&self) -> Result<String, McpError> {
        self.browser.reset_size().await.map_err(to_mcp_err)
    }

    #[tool(description = "Set the page zoom level natively (1.0 = 100%). Affects rendering, not just a CSS transform")]
    async fn zoom(&self, Parameters(p): Parameters<ZoomParams>) -> Result<String, McpError> {
        self.browser.zoom(p.scale).await.map_err(to_mcp_err)
    }

    #[tool(
        description = "Show the browser window on screen, for when a human should watch the session. \
Purely visual — the browser runs headless by default and every other tool works identically whether the \
window is open or closed. Does not affect the server process."
    )]
    async fn open_window(&self) -> Result<String, McpError> {
        self.browser.open_window().await.map_err(to_mcp_err)
    }

    #[tool(
        description = "Hide the browser window (back to headless). The server and the page state keep \
running exactly as before — this only affects whether a human can see it, same as open_window()."
    )]
    async fn close_window(&self) -> Result<String, McpError> {
        self.browser.close_window().await.map_err(to_mcp_err)
    }

    #[tool(description = "Check whether the browser window is currently shown on screen or hidden (headless)")]
    async fn window_status(&self) -> Result<String, McpError> {
        self.browser.window_status().await.map_err(to_mcp_err)
    }

    #[tool(description = "Take a PNG screenshot of the current browser view (the visible viewport, native WKWebView snapshot) and return it as an image")]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let png = self.browser.screenshot().await.map_err(to_mcp_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::image(STANDARD.encode(png), "image/png")]))
    }

    #[tool(description = "Get JavaScript console messages (log/warn/error/info) captured from the page since the last clear_console_logs call or server start")]
    fn get_console_logs(&self, Parameters(p): Parameters<ConsoleLogsParams>) -> String {
        let logs = self.browser.console.lock().unwrap();
        let slice: Vec<&String> = match p.limit {
            Some(n) => logs.iter().rev().take(n).rev().collect(),
            None => logs.iter().collect(),
        };
        serde_json::to_string(&slice).unwrap()
    }

    #[tool(description = "Clear the captured JavaScript console log buffer")]
    fn clear_console_logs(&self) -> String {
        self.browser.console.lock().unwrap().clear();
        "cleared".to_string()
    }
}

#[tool_handler]
impl ServerHandler for BrowserServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("browser-mcp-rs", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Browser automation backed by the OS-native webview (WKWebView on macOS) instead of a \
                 bundled Chromium binary. Runs headless by default — navigate, click, type, scroll, zoom, \
                 read text/HTML, run arbitrary JS, capture console logs, and take real PNG screenshots all \
                 work with no window ever shown. Call open_window() only if a human should watch; \
                 close_window() hides it again. Neither affects this server process, which stays alive \
                 until its MCP connection closes — same lifecycle model as @playwright/mcp. macOS only \
                 for now.",
            )
    }
}

/// Overrides console.log/warn/error/info to also forward to Rust via `window.ipc.postMessage`,
/// while still calling the original method (so devtools still shows everything too). Injected
/// on every navigation via `with_initialization_script`, so it survives `navigate()` calls.
const CONSOLE_HOOK_JS: &str = r#"
(function() {
  const levels = ['log', 'warn', 'error', 'info', 'debug'];
  for (const level of levels) {
    const original = console[level];
    console[level] = function(...args) {
      try {
        const message = args.map(a => {
          try { return typeof a === 'string' ? a : JSON.stringify(a); }
          catch (e) { return String(a); }
        }).join(' ');
        window.ipc.postMessage(JSON.stringify({ level, message }));
      } catch (e) {}
      return original.apply(console, args);
    };
  }
})();
"#;

#[cfg(target_os = "macos")]
mod macos_screenshot {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{MainThreadMarker, NSDictionary};
    use objc2_web_kit::WKWebView;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::oneshot;

    pub fn take_snapshot(webview: &WKWebView, tx: oneshot::Sender<Result<Vec<u8>, String>>) {
        let slot: StdMutex<Option<oneshot::Sender<Result<Vec<u8>, String>>>> = StdMutex::new(Some(tx));
        let _mtm = MainThreadMarker::new().expect("must run on the main thread");

        let block = RcBlock::new(move |img: *mut NSImage, _err: *mut objc2_foundation::NSError| {
            let Some(tx) = slot.lock().unwrap().take() else { return };
            let result = (|| -> Result<Vec<u8>, String> {
                let img: Retained<NSImage> =
                    unsafe { Retained::retain(img) }.ok_or("takeSnapshot returned no image")?;
                let tiff = img.TIFFRepresentation().ok_or("could not get TIFF representation")?;
                let mtm = MainThreadMarker::new().ok_or("not on main thread")?;
                let bitmap: Retained<NSBitmapImageRep> =
                    NSBitmapImageRep::initWithData(mtm.alloc(), &tiff)
                        .ok_or("could not build NSBitmapImageRep")?;
                let props = NSDictionary::dictionary();
                let png = unsafe {
                    bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)
                }
                .ok_or("could not encode PNG")?;
                Ok(png.to_vec())
            })();
            let _ = tx.send(result);
        });

        unsafe {
            webview.takeSnapshotWithConfiguration_completionHandler(None, &block);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let console: ConsoleLog = Arc::new(Mutex::new(Vec::new()));

    // Hidden by default: WKWebView renders and can be navigated/evaluated/screenshotted whether
    // or not its host window is on screen. open_window()/close_window() just toggle visibility.
    let window = WindowBuilder::new()
        .with_title("browser-mcp-rs")
        .with_inner_size(tao::dpi::LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT))
        .with_visible(false)
        .build(&event_loop)?;

    let ipc_console = console.clone();
    let webview = WebViewBuilder::new()
        .with_url("about:blank")
        .with_initialization_script(CONSOLE_HOOK_JS)
        .with_ipc_handler(move |req| {
            let body = req.body();
            if let Ok(mut logs) = ipc_console.lock() {
                logs.push(body.clone());
                if logs.len() > MAX_CONSOLE_ENTRIES {
                    let excess = logs.len() - MAX_CONSOLE_ENTRIES;
                    logs.drain(0..excess);
                }
            }
        })
        .build(&window)?;

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(async move {
            let server = BrowserServer { browser: BrowserHandle { proxy, console } };
            match server.serve(stdio()).await {
                Ok(service) => {
                    let _ = service.waiting().await;
                }
                Err(e) => eprintln!("MCP server error: {e}"),
            }
            std::process::exit(0);
        });
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // The user clicking the window's close button hides it, same as close_window() —
                // it must NOT end the process. The server only exits on MCP stdin EOF (see the
                // background thread below), matching @playwright/mcp's lifecycle model.
                window.set_visible(false);
            }
            Event::UserEvent(UserEvent::Navigate(url, tx)) => {
                let result = webview.load_url(&url).map(|_| "ok".to_string()).map_err(|e| e.to_string());
                let _ = tx.send(result);
            }
            Event::UserEvent(UserEvent::Eval(js, tx)) => {
                // Each call gets its own slot for its own callback closure — evaluate_script_with_callback
                // requires `Fn`, and oneshot::Sender::send consumes self, so the sender is taken out through
                // a Mutex<Option<_>> scoped to just this one call (not shared across concurrent evals).
                let slot: Mutex<Option<oneshot::Sender<JsResult>>> = Mutex::new(Some(tx));
                let eval_result = webview.evaluate_script_with_callback(&js, move |raw| {
                    if let Some(tx) = slot.lock().unwrap().take() {
                        let parsed: JsResult = match serde_json::from_str::<serde_json::Value>(&raw) {
                            Ok(serde_json::Value::String(s)) => Ok(s),
                            Ok(other) => Ok(other.to_string()),
                            Err(_) => Ok(raw),
                        };
                        let _ = tx.send(parsed);
                    }
                });
                if let Err(e) = eval_result {
                    eprintln!("evaluate_script failed: {e}");
                }
            }
            Event::UserEvent(UserEvent::Resize(w, h, tx)) => {
                window.set_inner_size(tao::dpi::LogicalSize::new(w, h));
                let _ = tx.send(Ok(format!("resized to {w}x{h}")));
            }
            Event::UserEvent(UserEvent::ResetSize(tx)) => {
                window.set_inner_size(tao::dpi::LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT));
                let _ = tx.send(Ok(format!("reset to {DEFAULT_WIDTH}x{DEFAULT_HEIGHT}")));
            }
            Event::UserEvent(UserEvent::Zoom(scale, tx)) => {
                let result = webview.zoom(scale).map(|_| format!("zoom set to {scale}")).map_err(|e| e.to_string());
                let _ = tx.send(result);
            }
            Event::UserEvent(UserEvent::OpenWindow(tx)) => {
                window.set_visible(true);
                let _ = tx.send(Ok("window shown".to_string()));
            }
            Event::UserEvent(UserEvent::CloseWindow(tx)) => {
                window.set_visible(false);
                let _ = tx.send(Ok("window hidden".to_string()));
            }
            Event::UserEvent(UserEvent::WindowStatus(tx)) => {
                let visible = window.is_visible();
                let _ = tx.send(Ok(format!("{{\"visible\":{visible}}}")));
            }
            #[cfg(target_os = "macos")]
            Event::UserEvent(UserEvent::Screenshot(tx)) => {
                use wry::WebViewExtMacOS;
                macos_screenshot::take_snapshot(&webview.webview(), tx);
            }
            #[cfg(not(target_os = "macos"))]
            Event::UserEvent(UserEvent::Screenshot(tx)) => {
                let _ = tx.send(Err("screenshot is only implemented on macOS".to_string()));
            }
            _ => {}
        }
    })
}
