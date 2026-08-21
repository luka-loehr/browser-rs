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

A visible native window (`tao`) owns the process's main thread — `WKWebView` requires a real
window; it can't run fully headless the way this version is structured. The MCP stdio server
runs on a background thread with its own `tokio` runtime, and sends commands to the window's
event loop over a channel (`click`/`type`/`scroll`/`get_text`/`get_html` are all implemented as
small, purpose-built JS snippets run through the same `eval_js` path; `screenshot` and `zoom`
call native WKWebView/wry APIs directly).

## Run

```sh
cargo build --release -p browser-mcp-rs
./target/release/browser-mcp-rs
```

A window titled "browser-mcp-rs" opens and stays open for the life of the server.
