# browser-mcp-rs

Browser automation for agents: a Rust MCP server that drives a **bundled, pinned Chromium** over
the Chrome DevTools Protocol, with **`@playwright/mcp`'s tool names and parameters**. No Node, no
Playwright library, no driver process: one small binary talking to Chromium over two pipes.

- **Headless by default.** When a human has to act (log in, 2FA, a CAPTCHA, payment details),
  the agent calls `browser_handoff`: the browser reopens in a visible window with a banner, waits
  until the person clicks **Done**, then goes back to headless. Cookies, localStorage,
  sessionStorage and open tabs carry over.
- **Trusted input.** Clicks, typing, keys, drags and drops go through `Input.dispatch*`, so the
  page sees real events (`isTrusted === true`), not JavaScript imitations.
- **Fewer tokens per step.** After the first snapshot of a page, action replies contain only the
  snapshot lines that changed, and `browser_batch` runs several steps in one call.

Measured against `@playwright/mcp` on the same four real pages (see
[BENCHMARKS.md](../docs/BENCHMARKS.md)): **535 MB across 5 processes vs 1,496 MB across 9**,
navigations 20–35% faster, server ready in 3–6 ms.

## Chromium

Two Chrome for Testing builds of the same pinned version (`153.0.8010.36`), downloaded once into
`~/Library/Caches/browser-mcp-rs/chromium` (Linux: `~/.cache/browser-mcp-rs`):

| Build | Used for | Download |
|---|---|---|
| `chrome-headless-shell` | headless mode (default) | ~99 MB, on the first tool call |
| `chrome` (full browser) | headed mode and `browser_handoff` | ~191 MB, only the first time a window is needed |

`browser-mcp-rs install` downloads both ahead of time. `--executable-path` (or
`BROWSER_MCP_EXECUTABLE`) uses your own Chromium or Chrome instead. Chromium starts lazily on the
first tool call and stops when the MCP connection closes.

The profile lives in `<cache>/profile` and persists logins between sessions. If another session
already holds it, the server falls back to a temporary profile. `--isolated` always uses a
temporary profile, and `--user-data-dir` picks your own.

## Tools

Same names and parameters as `@playwright/mcp`. Element parameters (`target`) take a ref from the
snapshot (`e12`), a CSS selector, `text=…` or `role=button[name="…"]`; `ref` is accepted as an
alias.

**Core (always on):** `browser_navigate`, `browser_navigate_back`, `browser_snapshot`,
`browser_find`, `browser_click`, `browser_hover`, `browser_type`, `browser_fill_form`,
`browser_select_option`, `browser_press_key`, `browser_drag`, `browser_drop`,
`browser_file_upload`, `browser_handle_dialog`, `browser_evaluate`, `browser_wait_for`,
`browser_take_screenshot`, `browser_resize`, `browser_tabs`, `browser_console_messages`,
`browser_network_requests`, `browser_network_request`, `browser_close`, `browser_install`

**Additions:**

| Tool | What it does |
|---|---|
| `browser_handoff` | Hand the browser to a human: visible window, banner with your message, waits for **Done** (or a timeout), then back to headless |
| `browser_set_mode` | Switch between `headless` and `headed` explicitly |
| `browser_batch` | Run several tools in order in one call and get one snapshot at the end |

**Opt-in with `--caps`** (`--caps all` enables everything, 72 tools):

| Capability | Tools |
|---|---|
| `vision` | `browser_mouse_click_xy`, `browser_mouse_move_xy`, `browser_mouse_drag_xy`, `browser_mouse_down`, `browser_mouse_up`, `browser_mouse_wheel` |
| `pdf` | `browser_pdf_save` |
| `network` | `browser_route`, `browser_route_list`, `browser_unroute`, `browser_network_state_set` |
| `storage` | `browser_cookie_*`, `browser_localstorage_*`, `browser_sessionstorage_*`, `browser_storage_state`, `browser_set_storage_state` |
| `devtools` | `browser_highlight`, `browser_hide_highlight`, `browser_start_tracing`, `browser_stop_tracing`, `browser_start_video`, `browser_stop_video`, `browser_video_chapter`, `browser_video_show_actions`, `browser_video_hide_actions`, `browser_start_recording`, `browser_stop_recording` |
| `testing` | `browser_generate_locator`, `browser_verify_element_visible`, `browser_verify_text_visible`, `browser_verify_list_visible`, `browser_verify_value` |
| `config` | `browser_get_config` |

## Replies

Replies use Playwright MCP's sections (`### Result`, `### Page`, `### Modal state`,
`### Snapshot`, new console errors and warnings, events such as opened tabs or finished downloads).
Two differences:

- **Snapshots are inline and incremental.** An action on a page already snapshotted returns only
  the changed lines, with their ancestors marked `# unchanged` for context
  (`--snapshot-mode incremental|full|none`). Playwright 0.0.80 instead writes each snapshot to a
  file the agent has to read.
- **Large pages are capped.** Action replies inline at most 16,000 characters of snapshot and
  `browser_snapshot` at most `--snapshot-max-chars` (default 50,000). Anything longer is saved to a
  file in the output directory, and the reply points to `browser_find` or `target`/`depth`.

## Options

Same names as `@playwright/mcp` where they exist:

```
--headless / --headed           start headless (default) or with a visible window
--caps <list>                   vision,pdf,network,storage,devtools,testing,config or all
--executable-path <path>        --user-data-dir <path>        --isolated
--storage-state <path>          --viewport-size <WxH>         --user-agent <ua>
--proxy-server <url>            --proxy-bypass <domains>      --ignore-https-errors
--block-service-workers         --allowed-origins <a;b>       --blocked-origins <a;b>
--init-script <path>            --output-dir <path>           --test-id-attribute <name>
--snapshot-mode <mode>          --snapshot-max-chars <n>
--timeout-action <ms> (5000)    --timeout-navigation <ms> (60000)    --timeout-settle <ms> (500)
```

## How it works

| File | Role |
|---|---|
| `src/cdp.rs` | DevTools Protocol client on `--remote-debugging-pipe` (fd 3/4, NUL-delimited JSON) |
| `src/install.rs` | Downloads and locates the pinned Chrome for Testing builds |
| `src/browser.rs` | Launch, per-tab state from CDP events, headless ↔ headed relaunch |
| `src/actions.rs` | Trusted input, screenshots, PDF, storage, routes, tracing, video, hand-off |
| `src/injected.js` | Runs in an isolated world of every page: snapshot and refs, actionability checks, the hand-off banner |
| `src/response.rs` | Reply formatting and snapshot diffs |
| `src/tools.rs` | The MCP tools |

Before every click, the injected runtime waits until the element is attached, visible, stable,
enabled and actually receives the pointer at its center. If something covers it, the error names
the covering element.

Chromium cannot switch between headless and headed while running, so a mode switch closes
Chromium and relaunches it on the same profile. It copies cookies (session cookies too) and
sessionStorage across and reopens the tabs. **Pages reload, so unsaved in-page state such as a
half-filled form is lost.**

## Not implemented

- `browser_run_code_unsafe` (runs Playwright code in Playwright's Node process),
  `browser_annotate` and `browser_resume` (Playwright Dashboard / test runner features).
- The `--device`, `--mobile`, `--init-page`, `--save-session`, `--output-max-size` and
  `--config` options.
- Cross-origin iframes are not part of the snapshot (same-origin iframes are).
- `browser_stop_video` needs `ffmpeg` on `PATH` to produce a `.webm`; without it the frames stay
  on disk as JPEGs.
- Recording (`browser_start_recording`) captures clicks, fills, checks, selections and
  Enter/Escape/Tab presses.
- macOS only is tested. Linux uses the same code path but is untested; Windows is not supported
  (the pipe transport is Unix-only).

## Run

```sh
cargo build --release -p browser-mcp-rs
claude mcp add browser-rs "$PWD/target/release/browser-mcp-rs"
```

Set `BROWSER_MCP_DEBUG=1` to log target attachments to stderr.
