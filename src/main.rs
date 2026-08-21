//! A lightweight browser-automation MCP server built on the OS's native webview
//! (WKWebView on macOS, via `wry`) instead of a bundled Chromium binary. `tao`'s event loop
//! owns the main thread and the actual WebView; the MCP stdio server runs on a background
//! thread and sends it commands over a channel, since a webview must live on the platform's
//! UI thread.
//!
//! Scope note: this is a first pass covering navigate/eval/click/type/read-DOM. It does not
//! yet implement `screenshot` — WKWebView's `takeSnapshot:completionHandler:` requires direct
//! objc2/WebKit FFI bridging that needs a real display to verify, so rather than ship an
//! unverified stub pretending to work, the tool returns a clear "not implemented" error.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use std::sync::Mutex;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use tokio::sync::oneshot;
use wry::WebViewBuilder;

type JsResult = Result<String, String>;

enum UserEvent {
    Navigate(String, oneshot::Sender<JsResult>),
    Eval(String, oneshot::Sender<JsResult>),
}

#[derive(Clone)]
struct BrowserHandle {
    proxy: EventLoopProxy<UserEvent>,
}

impl BrowserHandle {
    async fn navigate(&self, url: String) -> JsResult {
        let (tx, rx) = oneshot::channel();
        if self.proxy.send_event(UserEvent::Navigate(url, tx)).is_err() {
            return Err("browser window has closed".into());
        }
        rx.await.unwrap_or_else(|_| Err("browser window has closed".into()))
    }

    async fn eval(&self, js: String) -> JsResult {
        let (tx, rx) = oneshot::channel();
        if self.proxy.send_event(UserEvent::Eval(js, tx)).is_err() {
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
struct TypeParams {
    selector: String,
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EvalParams {
    /// JavaScript expression to evaluate in the page; its value is returned
    code: String,
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
            "(function(){{ const el = document.querySelector({sel}); if (!el) return JSON.stringify({{ok:false,error:'not found'}}); el.click(); return JSON.stringify({{ok:true}}); }})()",
            sel = js_string_literal(&p.selector)
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

    #[tool(description = "Get the visible text content of the page (document.body.innerText)")]
    async fn get_text(&self) -> Result<String, McpError> {
        self.browser
            .eval("document.body.innerText".to_string())
            .await
            .map_err(to_mcp_err)
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

    #[tool(
        description = "Not yet implemented. WKWebView screenshots require direct objc2/WebKit FFI \
(takeSnapshot:completionHandler:) that this first version doesn't ship — tracked as a known gap, see the README."
    )]
    fn screenshot(&self) -> Result<String, McpError> {
        Err(McpError::internal_error(
            "screenshot is not implemented in this version — see browser-mcp-rs/README.md",
            None,
        ))
    }
}

#[tool_handler]
impl ServerHandler for BrowserServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("browser-mcp-rs", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Browser automation backed by the OS-native webview (WKWebView on macOS) instead of a \
                 bundled Chromium binary — no second browser process, just the WebKit engine already \
                 resident on the system. macOS only for now.",
            )
    }
}

fn main() -> anyhow::Result<()> {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("browser-mcp-rs")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)?;

    let webview = WebViewBuilder::new()
        .with_url("about:blank")
        .build(&window)?;

    // The MCP stdio server runs on its own thread with its own tokio runtime, since the
    // webview + tao event loop must stay on the process's main thread.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(async move {
            let server = BrowserServer { browser: BrowserHandle { proxy } };
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
                *control_flow = ControlFlow::Exit;
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
            _ => {}
        }
    })
}
