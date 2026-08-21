# browser-mcp-rs

A browser-automation MCP server that does **not** bundle or launch a separate Chromium
binary. It drives the OS's native webview instead — `WKWebView` on macOS, via
[`wry`](https://github.com/tauri-apps/wry) (the same crate Tauri uses) + `tao` for windowing.
Since WebKit is already resident system-wide (Safari, Mail, every other app using a webview),
the marginal cost is just one small window, not a second browser process tree — see
[BENCHMARKS.md](../BENCHMARKS.md) for real numbers against `@playwright/mcp`.

**macOS only** for now — Windows (WebView2) and Linux (WebKitGTK) would need their own code
paths behind `wry`'s cross-platform API; not implemented here yet.

This is **not a drop-in replacement for `@playwright/mcp`** — different tool names, no
accessibility-tree snapshot mode, no multi-tab, dialog handling, file upload, network
interception, or drag/drop. It's additive: run it alongside Playwright's MCP, not instead of
it, unless you've confirmed it covers what you actually use.

## Headless by default, same lifecycle model as `@playwright/mcp`

The window is created hidden (`with_visible(false)`) — WKWebView renders, and every tool
(`navigate`, `click`, `eval_js`, `screenshot`, ...) works identically whether or not its window
is on screen. Nothing pops up on the user's screen unless something explicitly calls
`open_window()`.

The server process's lifecycle is tied **only** to its MCP stdio connection, not to the
window: closing the window — by the user clicking its close button, or by calling
`close_window()` — just hides it. The process keeps running, state intact, until its MCP
client disconnects (same model `@playwright/mcp` uses: the browser is a resource the server
manages, not the server itself). Earlier versions got this backwards and exited the whole
process when the window closed.

## Tools

- `navigate(url)` — load a URL
- `click(selector)` — scroll the first element matching a CSS selector into view and click it
- `click_at(x, y)` — click at exact viewport coordinates (CSS pixels), regardless of what's there
- `type(selector, text)` — set `.value` and fire `input`/`change` events (so React/Vue-style
  controlled inputs pick it up), not just set the DOM attribute
- `scroll(x, y)` / `scroll_by(dx, dy)` — absolute or relative scrolling
- `resize(width, height)` — resize the window (logical/CSS pixels)
- `reset_size()` — back to the default 1280×800
- `zoom(scale)` — native zoom (1.0 = 100%), actual rendering scale, not a CSS transform
- `open_window()` / `close_window()` — show or hide the window so a human can watch (or stop
  watching). Purely visual, does not affect the server or any page state.
- `window_status()` — `{"visible": true|false}`
- `get_text()` — `document.body.innerText`
- `get_html()` — `document.documentElement.outerHTML`
- `eval_js(code)` — evaluate arbitrary JavaScript in the page, return its value
- `screenshot()` — a real PNG of the current viewport via WKWebView's native
  `takeSnapshotWithConfiguration:completionHandler:`, returned as an actual image content
  block (not base64 text dumped into a string) — an agent with vision sees it directly.
  Bridged through `objc2`/`objc2-app-kit`/`objc2-web-kit` (`NSImage` → `NSBitmapImageRep` → PNG
  `NSData`), see `src/main.rs`'s `macos_screenshot` module.
- `get_console_logs(limit?)` — JavaScript `console.log/warn/error/info/debug` calls captured
  since start or the last clear, as `{level, message}` JSON entries. Captured by overriding
  `console.*` via an injected script that still calls through to the original (so devtools
  keeps working too), forwarding to Rust over wry's IPC bridge.
- `clear_console_logs()` — reset the captured log buffer

## How it's built

A `tao` window (hidden by default) owns the process's main thread — `WKWebView` still needs a
real window object to attach to, it just doesn't need to be visible. The MCP stdio server runs
on a background thread with its own `tokio` runtime, and sends commands to the window's event
loop over a channel (`click`/`type`/`scroll`/`get_text`/`get_html` are all implemented as
small, purpose-built JS snippets run through the same `eval_js` path; `screenshot` and `zoom`
call native WKWebView/wry APIs directly).

## Run

```sh
cargo build --release -p browser-mcp-rs
./target/release/browser-mcp-rs
```

Runs headless — nothing appears on screen until something calls `open_window()`. The process
stays alive until its MCP connection closes, regardless of window state.
