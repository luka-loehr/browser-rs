# browser-mcp-rs

A browser-automation MCP server that does **not** bundle or launch a separate Chromium
binary. It drives the OS's native webview instead — `WKWebView` on macOS, via
[`wry`](https://github.com/tauri-apps/wry) (the same crate Tauri uses) + `tao` for windowing.
Since WebKit is already resident system-wide (Safari, Mail, every other app using a webview),
the marginal cost is just one small window, not a second browser process tree — see
[BENCHMARKS.md](../BENCHMARKS.md) for real numbers against `@playwright/mcp`.

**macOS only** for now — Windows (WebView2) and Linux (WebKitGTK) would need their own code
paths behind `wry`'s cross-platform API; not implemented here yet.

This is **not a drop-in replacement for `@playwright/mcp`** — different tool names, and a
smaller feature set (see below). It's additive: run it alongside Playwright's MCP, not instead
of it, unless you've confirmed it covers what you actually use.

## Tools

- `navigate(url)` — load a URL
- `click(selector)` — click the first element matching a CSS selector
- `type(selector, text)` — set `.value` and fire `input`/`change` events (so React/Vue-style
  controlled inputs pick it up), not just set the DOM attribute
- `get_text()` — `document.body.innerText`
- `get_html()` — `document.documentElement.outerHTML`
- `eval_js(code)` — evaluate arbitrary JavaScript in the page, return its value
- `screenshot()` — **not implemented in this version.** WKWebView's
  `takeSnapshot:completionHandler:` needs direct `objc2`/WebKit FFI bridging that wasn't
  verifiable without iterating against a real display; the tool exists and returns a clear
  error rather than a broken result. Tracked as a known gap — a real contribution target if
  you want to pick it up.

Not implemented at all (vs. Playwright's MCP): accessibility-tree snapshot mode, multi-tab,
dialog handling, file upload, network interception, drag/drop.

## How it's built

A visible native window (`tao`) owns the process's main thread — `WKWebView` requires a real
window, it can't run fully headless the way this version is structured. The MCP stdio server
runs on a background thread with its own `tokio` runtime, and sends commands to the window's
event loop over a channel (`click`/`type`/`get_text`/`get_html` are all implemented as small,
purpose-built JS snippets run through the same `eval_js` path).

## Run

```sh
cargo build --release -p browser-mcp-rs
./target/release/browser-mcp-rs
```

A window titled "browser-mcp-rs" opens and stays open for the life of the server.
