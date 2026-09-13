//! MCP tool surface: the @playwright/mcp tool set with the same names and parameters, plus
//! browser_handoff / browser_set_mode (headless <-> headed) and browser_batch.

use crate::actions::glob_to_regex;
use crate::browser::{Browser, Route};
use crate::response::Reply;
use anyhow::{anyhow, bail, Result};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------- parameters

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NavigateParams {
    /// The URL to navigate to
    url: String,
    /// Snapshot in the reply: "diff" (default, changed lines only), "main" (main content only), "full", or "none" when you do not need to see the page
    #[allow(dead_code)]
    snapshot: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClickParams {
    /// Human-readable element description used to obtain permission to interact with the element
    element: Option<String>,
    /// Element ref from a snapshot or browser_links (e.g. "e12"), a CSS selector (the first visible match wins; "iframe#x >> body" enters an iframe or shadow root), "text=…", "role=button[name=\"…\"]", "link=<regex>" for a visible link by text, or "href=/wiki/C++" for a link by URL (raw or encoded)
    #[serde(alias = "ref")]
    target: String,
    /// Whether to perform a double click instead of a single click
    double_click: Option<bool>,
    /// Button to click, defaults to left
    button: Option<String>,
    /// Modifier keys to press: Alt, Control, ControlOrMeta, Meta, Shift
    modifiers: Option<Vec<String>>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for the element to become visible, stable and clickable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetParams {
    /// Human-readable element description used to obtain permission to interact with the element
    element: Option<String>,
    /// Exact target element reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: String,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for the element to become actionable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptTargetParams {
    /// Human-readable element description
    element: Option<String>,
    /// Element reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DragParams {
    /// Human-readable source element description
    start_element: Option<String>,
    /// Exact source element reference from the page snapshot, or a selector
    #[serde(alias = "startRef")]
    start_target: String,
    /// Human-readable target element description
    end_element: Option<String>,
    /// Exact target element reference from the page snapshot, or a selector
    #[serde(alias = "endRef")]
    end_target: String,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for the elements to become actionable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DropParams {
    /// Human-readable element description
    element: Option<String>,
    /// Exact target element reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: String,
    /// Absolute paths to files to drop onto the element
    paths: Option<Vec<String>>,
    /// Data to drop, as a map of MIME type to string value, e.g. {"text/plain": "hello"}
    data: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateParams {
    /// Human-readable element description
    #[allow(dead_code)]
    element: Option<String>,
    /// Element reference or selector; when given, the element is passed as the function's argument
    #[serde(alias = "ref")]
    target: Option<String>,
    /// JavaScript function, e.g. "() => document.title" or "(element) => element.textContent"
    function: String,
    /// Save the result to this file instead of returning it
    filename: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UploadParams {
    /// Absolute paths to the files to upload. Omit or pass an empty list to cancel the file chooser
    paths: Option<Vec<String>>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FormField {
    /// Human-readable field name
    name: String,
    /// Type of the field: textbox, checkbox, radio, combobox or slider
    #[serde(rename = "type")]
    kind: String,
    /// Exact target field reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: String,
    /// Value to fill. For checkboxes "true" or "false"; for comboboxes the option label or value
    value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FillFormParams {
    /// Fields to fill in
    fields: Vec<FormField>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for each field to become actionable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindParams {
    /// Plain text to search for in the snapshot (case-insensitive)
    text: Option<String>,
    /// Regular expression to search for in the snapshot
    regex: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialogParams {
    /// Whether to accept the dialog
    accept: bool,
    /// The text of the prompt in case of a prompt dialog
    prompt_text: Option<String>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NetworkRequestParams {
    /// 1-based index of the request, as listed by browser_network_requests
    index: usize,
    /// Return only this part: request-headers, request-body, response-headers or response-body
    part: Option<String>,
    /// Save the result to this file instead of returning it
    filename: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NetworkRequestsParams {
    /// Whether to include successful static resources like images, fonts, scripts and stylesheets
    #[serde(rename = "static", default)]
    include_static: bool,
    /// Only return requests whose URL matches this regular expression
    filter: Option<String>,
    /// Only failed requests: HTTP status >= 400 or network errors (static resources included)
    failed: Option<bool>,
    /// Save the list to this file instead of returning it
    filename: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConsoleParams {
    /// Level of console messages to return; each level includes the more severe ones: error, warning, info, debug
    level: Option<String>,
    /// Return all messages since the session started, not just since the last navigation
    all: Option<bool>,
    /// Save the messages to this file instead of returning them
    filename: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// Name of the key to press or a character to generate, such as "ArrowLeft", "a" or "Control+Shift+K"
    key: String,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResizeParams {
    /// Width of the viewport in CSS pixels
    width: f64,
    /// Height of the viewport in CSS pixels
    height: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectParams {
    /// Human-readable element description
    element: Option<String>,
    /// Exact target element reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: String,
    /// Values (or labels) of the options to select
    values: Vec<String>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for the element to become actionable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SnapshotParams {
    /// Only snapshot the subtree of this element reference or selector
    #[serde(alias = "ref")]
    target: Option<String>,
    /// Save the snapshot to this markdown file instead of returning it
    filename: Option<String>,
    /// Limit the depth of the snapshot tree
    depth: Option<u32>,
    /// Include bounding boxes as [box=x,y,width,height]
    boxes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotParams {
    /// Human-readable element description
    element: Option<String>,
    /// Element reference or selector to capture instead of the viewport
    #[serde(alias = "ref")]
    target: Option<String>,
    /// Image format: png (default), jpeg or webp
    #[serde(rename = "type")]
    kind: Option<String>,
    /// File name to save the screenshot to; defaults to page-{timestamp}.{png|jpeg|webp} in the output directory
    filename: Option<String>,
    /// Capture the full scrollable page instead of the viewport
    full_page: Option<bool>,
    /// "css" for one image pixel per CSS pixel (smaller), or "device" for device pixels. Defaults to css
    scale: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeParams {
    /// Human-readable element description
    element: Option<String>,
    /// Exact target element reference from the page snapshot, or a selector
    #[serde(alias = "ref")]
    target: String,
    /// Text to type. Replaces the element's current value (React/Vue-safe); with slowly:true it is typed at the end instead
    text: String,
    /// Whether to press Enter after typing
    submit: Option<bool>,
    /// Type one character at a time with real key events (for pages with key handlers). Default replaces the value at once
    slowly: Option<bool>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait for the element to become actionable (default 5000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitParams {
    /// Time to wait in seconds
    time: Option<f64>,
    /// Text to wait for
    text: Option<String>,
    /// Text to wait for to disappear
    text_gone: Option<String>,
    /// Element ref or selector to wait for (see state)
    selector: Option<String>,
    /// State to wait for with selector: "visible" (default), "hidden", "attached" or "detached"
    state: Option<String>,
    /// Wait until the page URL matches this glob (e.g. "**/checkout-complete*") or contains this text
    url: Option<String>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Max ms to wait (default 30000)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TabsParams {
    /// Operation to perform: list, new, close or select
    action: String,
    /// Tab index, used by close and select. Close defaults to the current tab
    index: Option<usize>,
    /// URL to open in the new tab, used by new
    url: Option<String>,
    /// Snapshot in the reply: "diff" (default), "main", "full" or "none"
    #[allow(dead_code)]
    snapshot: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NetworkStateParams {
    /// "offline" or "online"
    state: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RouteParams {
    /// URL pattern to match, e.g. "**/api/users" or "https://example.com/*.png"
    pattern: String,
    /// HTTP status code to return (default 200)
    status: Option<i64>,
    /// Response body text or JSON string
    body: Option<String>,
    /// Content-Type header value
    content_type: Option<String>,
    /// Headers to add, in "Name: Value" format
    headers: Option<Vec<String>>,
    /// Comma-separated header names to remove from the request
    remove_headers: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnrouteParams {
    /// URL pattern to unroute; omit to remove all routes
    pattern: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NameParams {
    /// Cookie name
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CookieListParams {
    /// Only cookies whose domain contains this
    domain: Option<String>,
    /// Only cookies whose path starts with this
    path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CookieSetParams {
    /// Cookie name
    name: String,
    /// Cookie value
    value: String,
    /// Cookie domain; defaults to the current page's host
    domain: Option<String>,
    /// Cookie path
    path: Option<String>,
    /// Expiration as a Unix timestamp in seconds
    expires: Option<f64>,
    /// Whether the cookie is HTTP-only
    http_only: Option<bool>,
    /// Whether the cookie is secure
    secure: Option<bool>,
    /// SameSite attribute: Strict, Lax or None
    same_site: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyOnlyParams {
    /// Storage key
    key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyValueParams {
    /// Storage key
    key: String,
    /// Value to store
    value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FilenameParams {
    /// File name or absolute path
    filename: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OptFilenameParams {
    /// File name or absolute path; defaults to a timestamped file in the output directory
    filename: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HighlightParams {
    /// Human-readable element description
    element: Option<String>,
    /// Element reference or selector
    #[serde(alias = "ref")]
    target: String,
    /// Additional inline CSS for the overlay
    style: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VideoSize {
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VideoParams {
    /// File name to save the video to (webm)
    filename: Option<String>,
    /// Maximum video frame size
    size: Option<VideoSize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChapterParams {
    /// Chapter title
    title: String,
    /// Chapter description
    description: Option<String>,
    /// Duration in milliseconds (accepted for compatibility)
    duration: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShowActionsParams {
    /// How long each action annotation stays visible, in milliseconds (default 800)
    duration: Option<u64>,
    /// Placement of the annotation (accepted for compatibility)
    position: Option<String>,
    /// Cursor decoration: pointer or none (accepted for compatibility)
    cursor: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MouseClickParams {
    /// X coordinate in CSS pixels
    x: f64,
    /// Y coordinate in CSS pixels
    y: f64,
    /// Button to click, defaults to left
    button: Option<String>,
    /// Number of clicks, defaults to 1
    click_count: Option<i64>,
    /// Time between mouse down and up in milliseconds
    delay: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MouseButtonParams {
    /// Button to press, defaults to left
    button: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MouseMoveParams {
    /// X coordinate in CSS pixels
    x: f64,
    /// Y coordinate in CSS pixels
    y: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MouseDragParams {
    start_x: f64,
    start_y: f64,
    end_x: f64,
    end_y: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WheelParams {
    delta_x: f64,
    delta_y: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyElementParams {
    /// ROLE of the element, as shown in the snapshot
    role: String,
    /// ACCESSIBLE_NAME of the element, as shown in the snapshot
    accessible_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerifyTextParams {
    /// TEXT to verify, as shown in the snapshot
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerifyListParams {
    /// Human-readable list description
    element: String,
    /// Element reference pointing to the list
    #[serde(alias = "ref")]
    target: String,
    /// Items to verify
    items: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerifyValueParams {
    /// Type of the element: textbox, checkbox, radio, combobox or slider
    #[serde(rename = "type")]
    kind: String,
    /// Human-readable element description
    element: String,
    /// Element reference from the snapshot
    #[serde(alias = "ref")]
    target: String,
    /// Value to verify; "true" or "false" for checkboxes
    value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HandoffParams {
    /// What the human should do, shown at the top of the viewer (e.g. "Log in to GitHub, then click Done")
    message: String,
    /// How long to wait for the human, in seconds (default 600)
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModeParams {
    /// "headless" (no window) or "headed" (visible window)
    mode: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchStep {
    /// Tool name, e.g. "browser_click"
    tool: String,
    /// That tool's arguments. Common: browser_navigate {url}; browser_click {target}; browser_type {target, text, submit?}; browser_select_option {target, values}; browser_press_key {key}; browser_wait_for {text | textGone | selector, state? | url | time}; browser_read {target, prop?, all?, equals?, contains?}; browser_evaluate {function}; browser_text {heading? | target? | lead?}; browser_links {filter?, region?}; browser_handle_dialog {accept, promptText?}; browser_file_upload {paths}. Any step may add timeout (ms)
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchParams {
    /// Steps to run in order; the reply has each step's result and one snapshot at the end
    steps: Vec<BatchStep>,
    /// Stop at the first failing step (default true)
    stop_on_error: Option<bool>,
    /// Snapshot at the end: "diff" (default), "main", "full", or "none" when the steps already return what you need
    #[allow(dead_code)]
    snapshot: Option<String>,
    /// Default max ms each step waits for its element (a step's own "timeout" argument wins)
    #[allow(dead_code)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinksParams {
    /// Only links inside this element ref or selector
    scope: Option<String>,
    /// Case-insensitive regex matched against link text and URL
    filter: Option<String>,
    /// Only links in this page region: "main", "nav", "header", "footer", "aside", "dialog" or "page"
    region: Option<String>,
    /// Include hidden links too (default false)
    include_hidden: Option<bool>,
    /// Max links to return (default 200)
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextParams {
    /// Read only this element (ref or selector) instead of the main content
    #[serde(alias = "ref")]
    target: Option<String>,
    /// Read only the section under the first heading containing this text
    heading: Option<String>,
    /// Only the first N prose paragraphs (skips infoboxes, tables, figures); 1 or true for an article's opening
    #[serde(default, deserialize_with = "count_or_bool")]
    lead: Option<u32>,
    /// Keep links as [text](url) (default false)
    links: Option<bool>,
    /// Max characters to return (default 20000)
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TableParams {
    /// The table (or an element containing it) as ref or selector
    #[serde(alias = "ref")]
    target: String,
    /// "tsv" (default), "md" or "json" (array of objects keyed by the header row)
    format: Option<String>,
    /// Max data rows (default 200)
    max_rows: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtractParams {
    /// CSS selector matching each item, e.g. ".inventory_item"
    selector: String,
    /// Field name -> sub-selector inside the item: "css" for text, "css@attr" or "@attr" for an attribute (href/src are absolute). Omit for each item's text
    fields: Option<std::collections::BTreeMap<String, String>>,
    /// Max items (default 100)
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetchParams {
    /// URL to request (relative URLs resolve against the page)
    url: Option<String>,
    /// Several URLs, fetched in parallel with the same options
    urls: Option<Vec<String>>,
    /// For JSON: keep only these dot paths (e.g. ["number", "title", "user.login"]) in each object, or in each item of a wrapped list like {"items": [...]}
    fields: Option<Vec<String>>,
    /// JavaScript function applied to the parsed JSON (or text) before returning, e.g. "(d) => d.filter(p => p.merged_at).slice(0, 3).map(p => p.number)"
    transform: Option<String>,
    /// HTTP method (default GET)
    method: Option<String>,
    /// Request headers
    headers: Option<serde_json::Map<String, Value>>,
    /// Request body
    body: Option<String>,
    /// Max body characters to return (default 20000)
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ScrapeTarget {
    /// Just the URL; uses the shared function and maxChars
    Url(String),
    /// A URL with its own function and/or maxChars
    Spec {
        url: String,
        function: Option<String>,
        #[serde(rename = "maxChars")]
        max_chars: Option<usize>,
    },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeParams {
    /// URLs to load, each in its own background tab: plain strings, or {url, function, maxChars} to read a page its own way
    urls: Vec<ScrapeTarget>,
    /// JavaScript function run on each loaded page; its result is returned. Omit to get each page's main content as Markdown
    function: Option<String>,
    /// Max characters per page (default 4000)
    max_chars: Option<usize>,
    /// Pages loaded at the same time (default 4, max 8)
    concurrency: Option<usize>,
    /// For the Markdown reading: only metadata plus the first N paragraphs (1 or true for the opening)
    #[serde(default, deserialize_with = "count_or_bool")]
    lead: Option<u32>,
    /// Give up on a page after this many ms and report it as timed out (default 15000)
    page_timeout: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    /// Element ref or selector; with all=true, every element matching a CSS selector
    #[serde(alias = "ref")]
    target: String,
    /// What to read: "text" (default), "value", "checked", "html", "attr:NAME", or any property name
    prop: Option<String>,
    /// Read every matching element and return a JSON array
    all: Option<bool>,
    /// Fail (and stop a batch) unless the value equals this
    equals: Option<String>,
    /// Fail (and stop a batch) unless the value contains this
    contains: Option<String>,
}

// ---------------------------------------------------------------------- server

#[derive(Clone)]
pub struct BrowserServer {
    browser: Arc<Browser>,
    tool_router: ToolRouter<Self>,
    lock: Arc<tokio::sync::Mutex<()>>,
}

const CAPS: &[(&str, &[&str])] = &[
    ("vision", &["browser_mouse_click_xy", "browser_mouse_down", "browser_mouse_drag_xy", "browser_mouse_move_xy", "browser_mouse_up", "browser_mouse_wheel"]),
    ("pdf", &["browser_pdf_save"]),
    ("network", &["browser_network_state_set", "browser_route", "browser_route_list", "browser_unroute"]),
    (
        "storage",
        &[
            "browser_cookie_clear", "browser_cookie_delete", "browser_cookie_get", "browser_cookie_list", "browser_cookie_set",
            "browser_localstorage_clear", "browser_localstorage_delete", "browser_localstorage_get", "browser_localstorage_list",
            "browser_localstorage_set", "browser_sessionstorage_clear", "browser_sessionstorage_delete", "browser_sessionstorage_get",
            "browser_sessionstorage_list", "browser_sessionstorage_set", "browser_set_storage_state", "browser_storage_state",
        ],
    ),
    (
        "devtools",
        &[
            "browser_highlight", "browser_hide_highlight", "browser_start_tracing", "browser_stop_tracing", "browser_start_video",
            "browser_stop_video", "browser_video_chapter", "browser_video_show_actions", "browser_video_hide_actions",
            "browser_start_recording", "browser_stop_recording",
        ],
    ),
    ("testing", &["browser_generate_locator", "browser_verify_element_visible", "browser_verify_list_visible", "browser_verify_text_visible", "browser_verify_value"]),
    ("config", &["browser_get_config"]),
];

type R = Result<CallToolResult, McpError>;

/// `lead` as a count; `true` means 1, so agents need not guess the type.
fn count_or_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Bool(true)) => Some(1),
        Some(Value::Number(n)) => n.as_u64().map(|n| n as u32),
        _ => None,
    })
}

/// Keeps only `fields` (dot paths) of a JSON object, of each object in an array, or of the items of a
/// wrapper such as GitHub search's {"total_count": 142, "items": [...]}.
fn project(v: &Value, fields: &[String]) -> Value {
    let pointer = |f: &str| format!("/{}", f.replace('.', "/"));
    match v {
        Value::Array(items) => Value::Array(items.iter().map(|i| project(i, fields)).collect()),
        Value::Object(map) => {
            // Fields the object itself has stay at this level; the rest apply to the items of its list
            // (so ["total_count", "title"] on a search result keeps the count and each item's title).
            let (own, inner): (Vec<String>, Vec<String>) = fields.iter().cloned().partition(|f| v.pointer(&pointer(f)).is_some());
            if !inner.is_empty() {
                if let Some((key, list)) = map.iter().find(|(_, x)| x.is_array()) {
                    let mut out: serde_json::Map<String, Value> = if own.is_empty() {
                        map.iter().filter(|(_, x)| !x.is_array() && !x.is_object()).map(|(k, x)| (k.clone(), x.clone())).collect()
                    } else {
                        own.iter().map(|f| (f.clone(), v.pointer(&pointer(f)).cloned().unwrap_or(Value::Null))).collect()
                    };
                    out.insert(key.clone(), project(list, &inner));
                    return Value::Object(out);
                }
            }
            Value::Object(fields.iter().map(|f| (f.clone(), v.pointer(&pointer(f)).cloned().unwrap_or(Value::Null))).collect())
        }
        other => other.clone(),
    }
}

impl BrowserServer {
    pub fn new(browser: Arc<Browser>) -> Self {
        let mut tool_router = Self::tool_router();
        let all = browser.cfg.caps.contains("all");
        for (cap, tools) in CAPS {
            if !all && !browser.cfg.caps.contains(*cap) {
                for t in *tools {
                    tool_router.remove_route(t);
                }
            }
        }
        Self { browser, tool_router, lock: Arc::new(tokio::sync::Mutex::new(())) }
    }

    async fn respond(&self, r: Result<Reply>) -> R {
        let reply = r.unwrap_or_else(|e| Reply::error(format!("{e:#}")));
        Ok(self.browser.finish(reply).await)
    }

    fn save_or(&self, filename: Option<&str>, text: String, stem: &str, ext: &str) -> Result<Reply> {
        match filename {
            Some(f) => {
                let path = self.browser.output_path(Some(f), stem, ext)?;
                std::fs::write(&path, text)?;
                Ok(Reply::text(format!("Saved to {}", path.display())))
            }
            None => Ok(Reply::text(text)),
        }
    }

    /// Runs any tool by name with JSON arguments; used by browser_batch.
    async fn dispatch(&self, tool: &str, args: Value) -> Result<Reply> {
        fn p<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
            serde_json::from_value(if v.is_null() { json!({}) } else { v }).map_err(|e| anyhow!("invalid arguments: {e}"))
        }
        let enabled = self.tool_router.has_route(tool) && !self.tool_router.is_disabled(tool);
        if !enabled || matches!(tool, "browser_batch" | "browser_handoff") {
            bail!("{tool} cannot be used in a batch");
        }
        match tool {
            "browser_navigate" => self.navigate(p(args)?).await,
            "browser_navigate_back" => self.back().await,
            "browser_click" => self.click(p(args)?).await,
            "browser_hover" => self.hover(p(args)?).await,
            "browser_type" => self.type_text(p(args)?).await,
            "browser_fill_form" => self.fill_form(p(args)?).await,
            "browser_select_option" => self.select(p(args)?).await,
            "browser_press_key" => self.press_key(p(args)?).await,
            "browser_drag" => self.drag(p(args)?).await,
            "browser_drop" => self.drop_(p(args)?).await,
            "browser_wait_for" => self.wait(p(args)?).await,
            "browser_snapshot" => self.snapshot(p(args)?).await,
            "browser_find" => self.find(p(args)?).await,
            "browser_take_screenshot" => self.screenshot(p(args)?).await,
            "browser_evaluate" => self.evaluate(p(args)?).await,
            "browser_file_upload" => self.upload(p(args)?).await,
            "browser_handle_dialog" => self.dialog(p(args)?).await,
            "browser_tabs" => self.tabs(p(args)?).await,
            "browser_resize" => self.resize(p(args)?).await,
            "browser_console_messages" => self.console(p(args)?).await,
            "browser_network_requests" => self.network_requests(p(args)?).await,
            "browser_mouse_click_xy" => self.mouse_click(p(args)?).await,
            "browser_mouse_move_xy" => self.mouse_move(p(args)?).await,
            "browser_mouse_wheel" => self.mouse_wheel(p(args)?).await,
            "browser_set_mode" => self.set_mode(p(args)?).await,
            "browser_links" => self.links(p(args)?).await,
            "browser_read" => self.read(p(args)?).await,
            "browser_text" => self.text(p(args)?).await,
            "browser_table" => self.table(p(args)?).await,
            "browser_extract" => self.extract(p(args)?).await,
            "browser_fetch" => self.fetch(p(args)?).await,
            "browser_scrape" => self.scrape(p(args)?).await,
            "browser_network_request" => self.network_request(p(args)?).await,
            other => bail!("{other} is not supported in browser_batch"),
        }
    }

    // ------------------------------------------------------------------ implementations

    async fn navigate(&self, p: NavigateParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let status = self.browser.navigate(&page, &p.url).await?;
        let url = crate::browser::normalize_url(&p.url);
        Ok(Reply::action(match status {
            Some(s) if s >= 400 => format!("Navigated to {url}: HTTP {s}, the server returned an error page"),
            Some(s) => format!("Navigated to {url} (HTTP {s})"),
            None => format!("Navigated to {url}"),
        }))
    }

    async fn back(&self) -> Result<Reply> {
        let page = self.browser.page().await?;
        Ok(Reply::action(if self.browser.go_back(&page).await? { "Went back" } else { "No previous page in history" }))
    }

    async fn click(&self, p: ClickParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let button = p.button.as_deref().unwrap_or("left");
        let double = p.double_click.unwrap_or(false);
        let mods = p.modifiers.unwrap_or_default();
        self.browser.settle(&page, self.browser.click(&page, &p.target, double, button, &mods)).await?;
        Ok(Reply::action(format!("{} {}", if double { "Double-clicked" } else { "Clicked" }, p.element.unwrap_or(p.target))))
    }

    async fn hover(&self, p: TargetParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        self.browser.settle(&page, self.browser.hover(&page, &p.target)).await?;
        Ok(Reply::action(format!("Hovered {}", p.element.unwrap_or(p.target))))
    }

    async fn type_text(&self, p: TypeParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let (slowly, submit) = (p.slowly.unwrap_or(false), p.submit.unwrap_or(false));
        self.browser.settle(&page, self.browser.type_into(&page, &p.target, &p.text, slowly, submit)).await?;
        Ok(Reply::action(format!("Typed into {}{}", p.element.unwrap_or(p.target), if submit { " and pressed Enter" } else { "" })))
    }

    async fn fill_form(&self, p: FillFormParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        for f in &p.fields {
            let r = async {
                match f.kind.as_str() {
                    "checkbox" | "radio" | "switch" => self.browser.set_checked(&page, &f.target, f.value == "true").await,
                    "combobox" | "listbox" => {
                        let is_select = self.browser.evaluate(&page, "(e) => e.tagName === 'SELECT'", Some(&f.target)).await? == json!(true);
                        if is_select {
                            self.browser.select_option(&page, &f.target, std::slice::from_ref(&f.value)).await.map(|_| ())
                        } else {
                            self.browser.fill(&page, &f.target, &f.value).await
                        }
                    }
                    _ => self.browser.fill(&page, &f.target, &f.value).await,
                }
            }
            .await;
            r.map_err(|e| anyhow!("field \"{}\": {e}", f.name))?;
        }
        self.browser.settle(&page, async { Ok(()) }).await?;
        Ok(Reply::action(format!("Filled {} field(s)", p.fields.len())))
    }

    async fn select(&self, p: SelectParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let selected = self.browser.settle(&page, self.browser.select_option(&page, &p.target, &p.values)).await?;
        Ok(Reply::action(format!("Selected {selected} in {}", p.element.unwrap_or(p.target))))
    }

    async fn press_key(&self, p: KeyParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        self.browser.settle(&page, self.browser.press(&page, &p.key)).await?;
        Ok(Reply::action(format!("Pressed {}", p.key)))
    }

    async fn drag(&self, p: DragParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        self.browser.settle(&page, self.browser.drag(&page, &p.start_target, &p.end_target)).await?;
        Ok(Reply::action(format!(
            "Dragged {} to {}",
            p.start_element.unwrap_or(p.start_target),
            p.end_element.unwrap_or(p.end_target)
        )))
    }

    async fn drop_(&self, p: DropParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let paths = p.paths.unwrap_or_default();
        let data = p.data.unwrap_or_default();
        if paths.is_empty() && data.is_empty() {
            bail!("pass paths or data to drop");
        }
        self.browser.settle(&page, self.browser.drop_data(&page, &p.target, &paths, &data)).await?;
        Ok(Reply::action(format!("Dropped onto {}", p.element.unwrap_or(p.target))))
    }

    async fn wait(&self, p: WaitParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let mut done = Vec::new();
        if let Some(sel) = &p.selector {
            done.push(self.browser.wait_selector(&page, sel, p.state.as_deref().unwrap_or("visible")).await?);
        }
        if let Some(url) = &p.url {
            done.push(self.browser.wait_url(&page, url).await?);
        }
        if p.time.is_some() || p.text.is_some() || p.text_gone.is_some() || done.is_empty() {
            done.push(self.browser.wait_for(&page, p.time, p.text.as_deref(), p.text_gone.as_deref()).await?);
        }
        Ok(Reply::action(done.join("; ")))
    }

    async fn snapshot(&self, p: SnapshotParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let snap = self.browser.snapshot(&page, p.target.as_deref(), p.depth, p.boxes.unwrap_or(false)).await?;
        if p.target.is_none() && p.depth.is_none() {
            let s = snap.clone();
            self.browser.tab_write(&page, |t| t.last_snapshot = Some(s))?;
        }
        if let Some(f) = p.filename {
            let path = self.browser.output_path(Some(&f), "snapshot", "md")?;
            std::fs::write(&path, format!("```yaml\n{snap}\n```\n"))?;
            return Ok(Reply::text(format!("Snapshot saved to {}", path.display())));
        }
        Ok(Reply::text(self.browser.snapshot_block(&snap)))
    }

    async fn find(&self, p: FindParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        Ok(Reply::raw(self.browser.find(&page, p.text.as_deref(), p.regex.as_deref()).await?))
    }

    async fn screenshot(&self, p: ScreenshotParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let kind = p.kind.as_deref().unwrap_or("png");
        let css = p.scale.as_deref() != Some("device");
        let shot = self.browser.screenshot(&page, kind, p.full_page.unwrap_or(false), p.target.as_deref(), css, p.filename.as_deref()).await?;
        let what = p.element.or(p.target).unwrap_or_else(|| if p.full_page == Some(true) { "full page".into() } else { "viewport".into() });
        let mut r = Reply::raw(format!("Took a screenshot of the {what} and saved it as {}", shot.path.display()));
        r.images.push((shot.data, shot.mime));
        Ok(r)
    }

    async fn evaluate(&self, p: EvaluateParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let v = self.browser.settle(&page, self.browser.evaluate(&page, &p.function, p.target.as_deref())).await?;
        // Strings come back as-is, everything else as compact JSON: no quoting or escaping to wade through.
        let text = match v {
            Value::String(s) => s,
            other => serde_json::to_string(&other)?,
        };
        match p.filename {
            Some(f) => self.save_or(Some(&f), text, "evaluate", "json"),
            None => Ok(Reply::text(text)),
        }
    }

    async fn upload(&self, p: UploadParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let msg = self.browser.settle(&page, self.browser.upload(&page, &p.paths.unwrap_or_default())).await?;
        Ok(Reply::action(msg))
    }

    async fn dialog(&self, p: DialogParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let msg = self.browser.settle(&page, self.browser.handle_dialog(&page, p.accept, p.prompt_text.as_deref())).await?;
        Ok(Reply::action(msg))
    }

    async fn tabs(&self, p: TabsParams) -> Result<Reply> {
        match p.action.as_str() {
            "list" => {
                self.browser.cdp().await?;
                Ok(Reply::raw(self.browser.tabs_list().await))
            }
            "new" => {
                let cdp = self.browser.cdp().await?;
                self.browser.new_tab_raw(&cdp, p.url.as_deref().unwrap_or("about:blank")).await?;
                Ok(Reply::action(self.browser.tabs_list().await))
            }
            "select" => {
                self.browser.cdp().await?;
                self.browser.tab_select(p.index.ok_or_else(|| anyhow!("select needs an index"))?).await?;
                Ok(Reply::action(self.browser.tabs_list().await))
            }
            "close" => {
                self.browser.cdp().await?;
                self.browser.tab_close(p.index).await?;
                Ok(Reply::action(self.browser.tabs_list().await))
            }
            other => bail!("unknown tabs action \"{other}\"; use list, new, close or select"),
        }
    }

    async fn resize(&self, p: ResizeParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let (w, h) = (p.width.round() as u32, p.height.round() as u32);
        self.browser.resize(&page, w, h).await?;
        Ok(Reply::action(format!("Resized viewport to {w}x{h}")))
    }

    async fn console(&self, p: ConsoleParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let text = self.browser.console_text(&page, p.level.as_deref().unwrap_or("info"), p.all.unwrap_or(false))?;
        self.save_or(p.filename.as_deref(), text, "console", "log").map(|mut r| {
            r.page_state = false;
            r
        })
    }

    async fn network_requests(&self, p: NetworkRequestsParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let failed = p.failed.unwrap_or(false);
        let text = self.browser.network_text(&page, p.include_static || failed, p.filter.as_deref(), failed)?;
        self.save_or(p.filename.as_deref(), text, "network", "txt").map(|mut r| {
            r.page_state = false;
            r
        })
    }

    async fn network_request(&self, p: NetworkRequestParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let text = self.browser.network_request(&page, p.index, p.part.as_deref()).await?;
        self.save_or(p.filename.as_deref(), text, "request", "txt").map(|mut r| {
            r.page_state = false;
            r
        })
    }

    async fn mouse_click(&self, p: MouseClickParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let button = p.button.clone().unwrap_or_else(|| "left".into());
        self.browser
            .settle(&page, self.browser.click_xy(&page, p.x, p.y, &button, p.click_count.unwrap_or(1), 0, p.delay.unwrap_or(0)))
            .await?;
        Ok(Reply::action(format!("Clicked at ({}, {})", p.x, p.y)))
    }

    async fn mouse_move(&self, p: MouseMoveParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        self.browser.settle(&page, self.browser.mouse_move(&page, p.x, p.y)).await?;
        Ok(Reply::action(format!("Moved mouse to ({}, {})", p.x, p.y)))
    }

    async fn mouse_wheel(&self, p: WheelParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        self.browser.settle(&page, self.browser.wheel(&page, p.delta_x, p.delta_y)).await?;
        Ok(Reply::action(format!("Scrolled by ({}, {})", p.delta_x, p.delta_y)))
    }

    async fn set_mode(&self, p: ModeParams) -> Result<Reply> {
        let headless = match p.mode.as_str() {
            "headless" => true,
            "headed" => false,
            other => bail!("unknown mode \"{other}\"; use headless or headed"),
        };
        let msg = self.browser.set_headless(headless).await?;
        Ok(Reply::text(msg))
    }

    async fn storage_op(&self, local: bool, op: &str, key: Option<&str>, value: Option<&str>) -> Result<Reply> {
        let page = self.browser.page().await?;
        Ok(Reply::raw(self.browser.web_storage(&page, local, op, key, value).await?))
    }

    async fn links(&self, p: LinksParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let opts = json!({ "scope": p.scope, "filter": p.filter, "region": p.region, "visibleOnly": !p.include_hidden.unwrap_or(false), "limit": p.limit });
        let links = self.browser.eval_world(&page, &format!("__bmcp.links({opts})")).await?;
        let url = self.browser.tab_read(&page, |t| t.url.clone())?;
        let origin = url.split('/').take(3).collect::<Vec<_>>().join("/");
        let lines: Vec<String> = links
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|l| {
                        let href = l["href"].as_str().unwrap_or_default();
                        let href = href.strip_prefix(&origin).filter(|h| h.starts_with('/')).unwrap_or(href);
                        let text: String = l["text"].as_str().unwrap_or_default().chars().take(80).collect();
                        let hidden = if l["visible"] == false { " (hidden)" } else { "" };
                        format!("{} [{}] {text}{hidden} -> {href}", l["ref"].as_str().unwrap_or_default(), l["region"].as_str().unwrap_or_default())
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Reply::raw(if lines.is_empty() { "No matching links".into() } else { format!("{} links (refs work as targets; relative URLs are on {origin}):\n{}", lines.len(), lines.join("\n")) }))
    }

    async fn text(&self, p: TextParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let opts = json!({ "target": p.target, "heading": p.heading, "lead": p.lead, "links": p.links, "maxChars": p.max_chars });
        let text = self.browser.eval_world(&page, &format!("__bmcp.text({opts})")).await?;
        Ok(Reply::raw(text.as_str().unwrap_or_default()))
    }

    async fn table(&self, p: TableParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let opts = json!({ "format": p.format, "maxRows": p.max_rows });
        let expr = format!("__bmcp.tableText(__bmcp.resolve({}), {opts})", crate::actions::js(&p.target));
        let text = self.browser.eval_world(&page, &expr).await?;
        Ok(Reply::raw(text.as_str().unwrap_or_default()))
    }

    async fn extract(&self, p: ExtractParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let opts = json!({ "selector": p.selector, "fields": p.fields, "limit": p.limit });
        let items = self.browser.eval_world(&page, &format!("__bmcp.extract({opts})")).await?;
        let items = items.as_array().cloned().unwrap_or_default();
        let lines: Vec<String> = items.iter().map(Value::to_string).collect();
        Ok(Reply::raw(if lines.is_empty() { format!("No elements match {}", p.selector) } else { format!("{} items:\n{}", lines.len(), lines.join("\n")) }))
    }

    async fn fetch(&self, p: FetchParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let headers = p.headers.clone().unwrap_or_default();
        let mut urls = p.urls.clone().unwrap_or_default();
        if let Some(u) = &p.url {
            urls.insert(0, u.clone());
        }
        if urls.is_empty() {
            bail!("pass url or urls");
        }
        let max = p.max_chars.unwrap_or(20_000);
        let method = p.method.clone().unwrap_or_else(|| "GET".into());
        let results = futures_util::future::join_all(urls.iter().map(|u| self.browser.fetch_in_page(&page, u, &method, &headers, p.body.as_deref()))).await;
        let mut sections = Vec::new();
        for (u, r) in urls.iter().zip(results) {
            let section = match r {
                Ok(f) => {
                    let body = self.shape_body(&page, &f, p.transform.as_deref(), p.fields.as_deref()).await?;
                    format!("{}\n\n{}", f.head, crate::actions::truncate_chars(body, max))
                }
                Err(e) => format!("failed: {e:#}"),
            };
            sections.push(if urls.len() > 1 { format!("## {u}\n{section}") } else { section });
        }
        Ok(Reply::raw(sections.join("\n\n")))
    }

    /// Applies `transform` (a JS function, run in the page's isolated world) or `fields` to a fetched body.
    async fn shape_body(&self, page: &crate::browser::PageRef, f: &crate::actions::FetchResult, transform: Option<&str>, fields: Option<&[String]>) -> Result<String> {
        if let Some(t) = transform {
            let input = match &f.json {
                Some(v) => v.to_string(),
                None => serde_json::to_string(&f.text)?,
            };
            let v = self.browser.eval_world(page, &format!("({t})({input})")).await?;
            return Ok(match v {
                Value::String(s) => s,
                other => other.to_string(),
            });
        }
        Ok(match (&f.json, fields) {
            (Some(v), Some(fields)) => project(v, fields).to_string(),
            (Some(v), None) => v.to_string(),
            _ => f.text.clone(),
        })
    }

    async fn scrape(&self, p: ScrapeParams) -> Result<Reply> {
        if p.urls.is_empty() {
            bail!("pass at least one URL");
        }
        let default_max = p.max_chars.unwrap_or(4_000);
        let shared = p.function.clone();
        let targets: Vec<(String, Option<String>, usize)> = p
            .urls
            .into_iter()
            .map(|t| match t {
                ScrapeTarget::Url(u) => (u, shared.clone(), default_max),
                ScrapeTarget::Spec { url, function, max_chars } => (url, function.or_else(|| shared.clone()), max_chars.unwrap_or(default_max)),
            })
            .collect();
        let text = self.browser.scrape(&targets, p.concurrency.unwrap_or(4), p.lead, Duration::from_millis(p.page_timeout.unwrap_or(15_000))).await?;
        Ok(Reply::raw(text))
    }

    async fn read(&self, p: ReadParams) -> Result<Reply> {
        let page = self.browser.page().await?;
        let expr = format!(
            "__bmcp.read({}, {}, {})",
            crate::actions::js(&p.target),
            crate::actions::js(p.prop.as_deref().unwrap_or("text")),
            p.all.unwrap_or(false)
        );
        let v = self.browser.eval_world(&page, &expr).await?;
        let text = match &v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(want) = &p.equals {
            if &text != want {
                bail!("{} is \"{text}\", expected \"{want}\"", p.target);
            }
        }
        if let Some(want) = &p.contains {
            if !text.contains(want.as_str()) {
                bail!("{} is \"{text}\", which does not contain \"{want}\"", p.target);
            }
        }
        Ok(Reply::raw(text))
    }

    async fn verify(&self, ok: bool, what: String) -> Result<Reply> {
        if ok {
            Ok(Reply::raw(format!("Done: {what}")))
        } else {
            bail!("Verification failed: {what}")
        }
    }
}

#[tool_router]
impl BrowserServer {
    #[tool(description = "Navigate to a URL")]
    async fn browser_navigate(&self, Parameters(p): Parameters<NavigateParams>) -> R {
        self.respond(self.navigate(p).await).await
    }

    #[tool(description = "Go back to the previous page in the history")]
    async fn browser_navigate_back(&self) -> R {
        self.respond(self.back().await).await
    }

    #[tool(description = "Perform click on a web page")]
    async fn browser_click(&self, Parameters(p): Parameters<ClickParams>) -> R {
        self.respond(self.click(p).await).await
    }

    #[tool(description = "Close the browser")]
    async fn browser_close(&self) -> R {
        let closed = self.browser.close().await;
        self.respond(closed.map(|c| Reply::raw(if c { "Browser closed" } else { "Browser was not running" }))).await
    }

    #[tool(description = "Returns console messages of the current page since the last navigation")]
    async fn browser_console_messages(&self, Parameters(p): Parameters<ConsoleParams>) -> R {
        self.respond(self.console(p).await).await
    }

    #[tool(description = "Perform drag and drop between two elements")]
    async fn browser_drag(&self, Parameters(p): Parameters<DragParams>) -> R {
        self.respond(self.drag(p).await).await
    }

    #[tool(description = "Drop files or MIME-typed data onto an element, as if dragged from outside the page")]
    async fn browser_drop(&self, Parameters(p): Parameters<DropParams>) -> R {
        self.respond(self.drop_(p).await).await
    }

    #[tool(description = "Evaluate a JavaScript function on the page, or on an element when target is given, and return its result")]
    async fn browser_evaluate(&self, Parameters(p): Parameters<EvaluateParams>) -> R {
        self.respond(self.evaluate(p).await).await
    }

    #[tool(description = "Upload one or multiple files into the open file chooser. Omit paths to cancel it")]
    async fn browser_file_upload(&self, Parameters(p): Parameters<UploadParams>) -> R {
        self.respond(self.upload(p).await).await
    }

    #[tool(description = "Fill multiple form fields at once")]
    async fn browser_fill_form(&self, Parameters(p): Parameters<FillFormParams>) -> R {
        self.respond(self.fill_form(p).await).await
    }

    #[tool(description = "Search the accessibility snapshot for text or a regex; returns matching nodes with their ancestors")]
    async fn browser_find(&self, Parameters(p): Parameters<FindParams>) -> R {
        self.respond(self.find(p).await).await
    }

    #[tool(description = "Accept or dismiss the open JavaScript dialog (alert, confirm, prompt, beforeunload)")]
    async fn browser_handle_dialog(&self, Parameters(p): Parameters<DialogParams>) -> R {
        self.respond(self.dialog(p).await).await
    }

    #[tool(description = "Hover over an element on the page")]
    async fn browser_hover(&self, Parameters(p): Parameters<TargetParams>) -> R {
        self.respond(self.hover(p).await).await
    }

    #[tool(description = "Returns full details (headers and body) of one network request from browser_network_requests")]
    async fn browser_network_request(&self, Parameters(p): Parameters<NetworkRequestParams>) -> R {
        self.respond(self.network_request(p).await).await
    }

    #[tool(description = "Returns a numbered list of the network requests since the page loaded")]
    async fn browser_network_requests(&self, Parameters(p): Parameters<NetworkRequestsParams>) -> R {
        self.respond(self.network_requests(p).await).await
    }

    #[tool(description = "Press a key on the keyboard, e.g. \"Enter\", \"ArrowDown\", \"a\", \"Control+A\"")]
    async fn browser_press_key(&self, Parameters(p): Parameters<KeyParams>) -> R {
        self.respond(self.press_key(p).await).await
    }

    #[tool(description = "Resize the browser viewport")]
    async fn browser_resize(&self, Parameters(p): Parameters<ResizeParams>) -> R {
        self.respond(self.resize(p).await).await
    }

    #[tool(description = "Select an option in a dropdown")]
    async fn browser_select_option(&self, Parameters(p): Parameters<SelectParams>) -> R {
        self.respond(self.select(p).await).await
    }

    #[tool(description = "Capture an accessibility snapshot of the current page, with element refs for actions. Better than a screenshot")]
    async fn browser_snapshot(&self, Parameters(p): Parameters<SnapshotParams>) -> R {
        self.respond(self.snapshot(p).await).await
    }

    #[tool(description = "Take a screenshot of the current page. You can't perform actions based on the screenshot; use browser_snapshot for actions")]
    async fn browser_take_screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> R {
        self.respond(self.screenshot(p).await).await
    }

    #[tool(description = "Type text into an editable element")]
    async fn browser_type(&self, Parameters(p): Parameters<TypeParams>) -> R {
        self.respond(self.type_text(p).await).await
    }

    #[tool(description = "Wait for text to appear or disappear, or for a time to pass")]
    async fn browser_wait_for(&self, Parameters(p): Parameters<WaitParams>) -> R {
        self.respond(self.wait(p).await).await
    }

    #[tool(description = "List, create, close, or select a browser tab")]
    async fn browser_tabs(&self, Parameters(p): Parameters<TabsParams>) -> R {
        self.respond(self.tabs(p).await).await
    }

    #[tool(description = "Download and install the bundled Chromium (headless shell and full browser) if not installed yet")]
    async fn browser_install(&self) -> R {
        let r = async {
            let mut lines = Vec::new();
            for product in [crate::install::Product::HeadlessShell, crate::install::Product::Chrome] {
                let p = crate::install::ensure(product).await?;
                lines.push(format!("{product:?} {} is installed at {}", crate::install::CHROME_VERSION, p.display()));
            }
            Ok(Reply::raw(lines.join("\n")))
        }
        .await;
        self.respond(r).await
    }

    #[tool(
        description = "Hand the browser to the human in its exact current state: opens a live view of the current tab in a window, where they can click, type and paste, and waits until they click Done. Nothing reloads and the browser stays headless. Use when a person must act: log in, pass 2FA or a CAPTCHA, enter payment or other private details, or approve something"
    )]
    async fn browser_handoff(&self, Parameters(p): Parameters<HandoffParams>) -> R {
        let timeout = Duration::from_secs(p.timeout_seconds.unwrap_or(600));
        let r = self.browser.hand_off(&p.message, timeout).await.map(Reply::action);
        self.respond(r).await
    }

    #[tool(description = "Switch the browser between headless (no window) and headed (visible window) mode. Tabs, cookies and storage carry over; the pages reload")]
    async fn browser_set_mode(&self, Parameters(p): Parameters<ModeParams>) -> R {
        self.respond(self.set_mode(p).await).await
    }

    #[tool(
        description = "Run many browser tools in one call, in order: act, wait and read without round trips (e.g. navigate, type, click, wait_for {url}, read {target, equals}). Returns each step's result and one snapshot at the end (snapshot:\"none\" to skip it). Targets: ref, CSS (first visible match; \"a >> b\" enters iframes/shadow roots), text=…, role=…[name=\"…\"], link=<regex>"
    )]
    async fn browser_batch(&self, Parameters(p): Parameters<BatchParams>) -> R {
        let stop = p.stop_on_error.unwrap_or(true);
        let mut combined = Reply { page_state: true, ..Default::default() };
        let mut failed = false;
        let batch_timeout = self.browser.overrides.lock().unwrap().timeout;
        for (i, step) in p.steps.into_iter().enumerate() {
            let step_timeout = step.arguments.get("timeout").and_then(Value::as_u64).map(Duration::from_millis).or(batch_timeout);
            self.browser.overrides.lock().unwrap().timeout = step_timeout;
            match self.dispatch(&step.tool, step.arguments).await {
                Ok(r) => {
                    combined.result.push(format!("{}. {}: {}", i + 1, step.tool, r.result.join(" ").trim()));
                    combined.snapshot |= r.snapshot;
                    combined.images.extend(r.images);
                }
                Err(e) => {
                    combined.result.push(format!("{}. {}: ERROR {e:#}", i + 1, step.tool));
                    failed = true;
                    if stop {
                        break;
                    }
                }
            }
        }
        combined.error = failed && stop;
        combined.snapshot = true;
        if combined.error {
            // Still show where the page ended up, so the agent can recover without another call.
            let mut r = self.browser.finish(Reply { error: false, ..combined }).await;
            r.is_error = Some(true);
            return Ok(r);
        }
        Ok(self.browser.finish(combined).await)
    }

    // ----------------------------------------------------------------- reading without snapshots

    #[tool(description = "List links as compact lines: ref, page region (main/nav/header/footer/aside), text and URL. Filter by regex, region or scope. Far cheaper than a snapshot for choosing a link; the refs work as click targets")]
    async fn browser_links(&self, Parameters(p): Parameters<LinksParams>) -> R {
        self.respond(self.links(p).await).await
    }

    #[tool(description = "Read the page as Markdown: the article or main content by default (with title, URL, author, published date, description; site chrome, share boxes and infoboxes left out), one section by heading, one element, or just the first paragraphs with lead. Use it for reading instead of snapshots or custom JavaScript")]
    async fn browser_text(&self, Parameters(p): Parameters<TextParams>) -> R {
        self.respond(self.text(p).await).await
    }

    #[tool(description = "Read one value from the page: an element's text, value, checked state, HTML or attribute, or all CSS matches as a JSON array (searching inside open shadow roots too, e.g. code blocks). With equals/contains it becomes an assertion that fails the call or batch step")]
    async fn browser_read(&self, Parameters(p): Parameters<ReadParams>) -> R {
        self.respond(self.read(p).await).await
    }

    #[tool(description = "Read a table as TSV (default), Markdown or JSON")]
    async fn browser_table(&self, Parameters(p): Parameters<TableParams>) -> R {
        self.respond(self.table(p).await).await
    }

    #[tool(description = "Extract repeated items (products, rows, results) as one JSON object per line, mapping fields to sub-selectors")]
    async fn browser_extract(&self, Parameters(p): Parameters<ExtractParams>) -> R {
        self.respond(self.extract(p).await).await
    }

    #[tool(description = "HTTP request with the browser's cookies and user agent, sent directly (no CORS or CSP limits): returns status, content type and body, JSON compacted. Relative URLs resolve against the current page. Ideal for JSON APIs like api.github.com")]
    async fn browser_fetch(&self, Parameters(p): Parameters<FetchParams>) -> R {
        self.respond(self.fetch(p).await).await
    }

    #[tool(description = "Load several URLs at once in parallel background tabs and read each one (main content as Markdown, or the result of your JavaScript function), then close them. The current tab is untouched. Use for research across many pages")]
    async fn browser_scrape(&self, Parameters(p): Parameters<ScrapeParams>) -> R {
        self.respond(self.scrape(p).await).await
    }

    // ----------------------------------------------------------------- --caps=config

    #[tool(description = "Get the resolved configuration of this server")]
    async fn browser_get_config(&self) -> R {
        let cfg = &self.browser.cfg;
        let v = json!({
            "browser": { "engine": "chromium", "version": crate::install::CHROME_VERSION, "executablePath": cfg.executable, "headless": self.browser.is_headless().await,
                "userDataDir": cfg.user_data_dir, "isolated": cfg.isolated, "viewport": cfg.viewport, "userAgent": cfg.user_agent,
                "proxyServer": cfg.proxy_server, "proxyBypass": cfg.proxy_bypass, "ignoreHTTPSErrors": cfg.ignore_https_errors },
            "network": { "allowedOrigins": cfg.allowed_origins, "blockedOrigins": cfg.blocked_origins, "blockServiceWorkers": cfg.block_service_workers },
            "outputDir": cfg.output_dir, "snapshotMode": cfg.snapshot_mode, "snapshotMaxChars": cfg.snapshot_max_chars, "testIdAttribute": cfg.test_id_attribute,
            "timeouts": { "action": cfg.timeout_action.as_millis() as u64, "navigation": cfg.timeout_navigation.as_millis() as u64, "settle": cfg.timeout_settle.as_millis() as u64 },
            "capabilities": cfg.caps, "storageState": cfg.storage_state,
        });
        self.respond(Ok(Reply::raw(serde_json::to_string_pretty(&v).unwrap()))).await
    }

    // ----------------------------------------------------------------- --caps=network

    #[tool(description = "Set the browser network state to online or offline")]
    async fn browser_network_state_set(&self, Parameters(p): Parameters<NetworkStateParams>) -> R {
        let r = async {
            let offline = match p.state.as_str() {
                "offline" => true,
                "online" => false,
                other => bail!("unknown state \"{other}\""),
            };
            self.browser.set_offline(offline).await?;
            Ok(Reply::raw(format!("Network is {}", p.state)))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Mock network requests matching a URL pattern with a fixed response, or modify their headers")]
    async fn browser_route(&self, Parameters(p): Parameters<RouteParams>) -> R {
        let r = async {
            let headers = p
                .headers
                .unwrap_or_default()
                .iter()
                .map(|h| h.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).ok_or_else(|| anyhow!("header \"{h}\" is not \"Name: Value\"")))
                .collect::<Result<Vec<_>>>()?;
            let route = Route {
                regex: glob_to_regex(&p.pattern)?,
                pattern: p.pattern.clone(),
                status: p.status,
                body: p.body,
                content_type: p.content_type,
                headers,
                remove_headers: p.remove_headers.map(|s| s.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).collect()).unwrap_or_default(),
            };
            self.browser.add_route(route).await;
            Ok(Reply::raw(format!("Route added for {}", p.pattern)))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "List all active network routes")]
    async fn browser_route_list(&self) -> R {
        let routes: Vec<String> = self
            .browser
            .state
            .lock()
            .unwrap()
            .routes
            .iter()
            .map(|r| format!("- {} => {}", r.pattern, if r.body.is_some() || r.status.is_some() { format!("[{}] mocked response", r.status.unwrap_or(200)) } else { "modified headers".into() }))
            .collect();
        self.respond(Ok(Reply::raw(if routes.is_empty() { "No active routes".into() } else { routes.join("\n") }))).await
    }

    #[tool(description = "Remove network routes matching a pattern, or all routes")]
    async fn browser_unroute(&self, Parameters(p): Parameters<UnrouteParams>) -> R {
        let n = self.browser.remove_routes(p.pattern.as_deref()).await;
        self.respond(Ok(Reply::raw(format!("Removed {n} route(s)")))).await
    }

    // ----------------------------------------------------------------- --caps=storage

    #[tool(description = "Clear all cookies")]
    async fn browser_cookie_clear(&self) -> R {
        let r = async {
            let cdp = self.browser.cdp().await?;
            cdp.send("Storage.clearCookies", json!({})).await.map_err(|e| anyhow!(e))?;
            Ok(Reply::raw("Cleared all cookies"))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Delete a cookie by name")]
    async fn browser_cookie_delete(&self, Parameters(p): Parameters<NameParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let matching: Vec<Value> = self.browser.cookies().await?.into_iter().filter(|c| c["name"] == p.name.as_str()).collect();
            for c in &matching {
                page.cdp
                    .send_session(&page.session, "Network.deleteCookies", json!({ "name": c["name"], "domain": c["domain"], "path": c["path"] }))
                    .await
                    .map_err(|e| anyhow!(e))?;
            }
            Ok(Reply::raw(format!("Deleted {} cookie(s) named {}", matching.len(), p.name)))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Get a cookie by name")]
    async fn browser_cookie_get(&self, Parameters(p): Parameters<NameParams>) -> R {
        let r = async {
            let found: Vec<Value> = self.browser.cookies().await?.into_iter().filter(|c| c["name"] == p.name.as_str()).collect();
            Ok(Reply::raw(if found.is_empty() { format!("No cookie named {}", p.name) } else { serde_json::to_string_pretty(&found)? }))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "List all cookies, optionally filtered by domain and path")]
    async fn browser_cookie_list(&self, Parameters(p): Parameters<CookieListParams>) -> R {
        let r = async {
            let list: Vec<Value> = self
                .browser
                .cookies()
                .await?
                .into_iter()
                .filter(|c| p.domain.as_deref().is_none_or(|d| c["domain"].as_str().unwrap_or("").contains(d)))
                .filter(|c| p.path.as_deref().is_none_or(|x| c["path"].as_str().unwrap_or("").starts_with(x)))
                .collect();
            Ok(Reply::raw(if list.is_empty() { "No cookies".into() } else { serde_json::to_string_pretty(&list)? }))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Set a cookie")]
    async fn browser_cookie_set(&self, Parameters(p): Parameters<CookieSetParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let mut c = json!({ "name": p.name, "value": p.value, "path": p.path.unwrap_or_else(|| "/".into()) });
            match p.domain {
                Some(d) => c["domain"] = json!(d),
                None => {
                    let url = self.browser.tab_read(&page, |t| t.url.clone())?;
                    if !url.starts_with("http") {
                        bail!("pass a domain; the current page has no host");
                    }
                    c["url"] = json!(url);
                }
            }
            if let Some(e) = p.expires {
                c["expires"] = json!(e);
            }
            if let Some(h) = p.http_only {
                c["httpOnly"] = json!(h);
            }
            if let Some(s) = p.secure {
                c["secure"] = json!(s);
            }
            if let Some(s) = p.same_site {
                c["sameSite"] = json!(s);
            }
            page.cdp.send("Storage.setCookies", json!({ "cookies": [c] })).await.map_err(|e| anyhow!(e))?;
            Ok(Reply::raw(format!("Set cookie {}", p.name)))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Clear localStorage of the current page's origin")]
    async fn browser_localstorage_clear(&self) -> R {
        self.respond(self.storage_op(true, "clear", None, None).await).await
    }

    #[tool(description = "Delete a localStorage item")]
    async fn browser_localstorage_delete(&self, Parameters(p): Parameters<KeyOnlyParams>) -> R {
        self.respond(self.storage_op(true, "delete", Some(&p.key), None).await).await
    }

    #[tool(description = "Get a localStorage item")]
    async fn browser_localstorage_get(&self, Parameters(p): Parameters<KeyOnlyParams>) -> R {
        self.respond(self.storage_op(true, "get", Some(&p.key), None).await).await
    }

    #[tool(description = "List all localStorage items of the current page's origin")]
    async fn browser_localstorage_list(&self) -> R {
        self.respond(self.storage_op(true, "list", None, None).await).await
    }

    #[tool(description = "Set a localStorage item")]
    async fn browser_localstorage_set(&self, Parameters(p): Parameters<KeyValueParams>) -> R {
        self.respond(self.storage_op(true, "set", Some(&p.key), Some(&p.value)).await).await
    }

    #[tool(description = "Clear sessionStorage of the current page")]
    async fn browser_sessionstorage_clear(&self) -> R {
        self.respond(self.storage_op(false, "clear", None, None).await).await
    }

    #[tool(description = "Delete a sessionStorage item")]
    async fn browser_sessionstorage_delete(&self, Parameters(p): Parameters<KeyOnlyParams>) -> R {
        self.respond(self.storage_op(false, "delete", Some(&p.key), None).await).await
    }

    #[tool(description = "Get a sessionStorage item")]
    async fn browser_sessionstorage_get(&self, Parameters(p): Parameters<KeyOnlyParams>) -> R {
        self.respond(self.storage_op(false, "get", Some(&p.key), None).await).await
    }

    #[tool(description = "List all sessionStorage items of the current page")]
    async fn browser_sessionstorage_list(&self) -> R {
        self.respond(self.storage_op(false, "list", None, None).await).await
    }

    #[tool(description = "Set a sessionStorage item")]
    async fn browser_sessionstorage_set(&self, Parameters(p): Parameters<KeyValueParams>) -> R {
        self.respond(self.storage_op(false, "set", Some(&p.key), Some(&p.value)).await).await
    }

    #[tool(description = "Restore storage state (cookies and localStorage) from a Playwright storage-state file")]
    async fn browser_set_storage_state(&self, Parameters(p): Parameters<FilenameParams>) -> R {
        let r = async {
            let cdp = self.browser.cdp().await?;
            let path = self.browser.output_path(Some(&p.filename), "storage", "json")?;
            self.browser.set_storage_state(&cdp, &path).await?;
            Ok(Reply::raw(format!("Restored storage state from {}", path.display())))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Save storage state (cookies and localStorage of open tabs) to a Playwright-compatible file")]
    async fn browser_storage_state(&self, Parameters(p): Parameters<OptFilenameParams>) -> R {
        let r = async {
            let cdp = self.browser.cdp().await?;
            let state = self.browser.storage_state(&cdp).await?;
            let path = self.browser.output_path(p.filename.as_deref(), "storage-state", "json")?;
            std::fs::write(&path, serde_json::to_string_pretty(&state)?)?;
            Ok(Reply::raw(format!("Saved storage state to {}", path.display())))
        }
        .await;
        self.respond(r).await
    }

    // ----------------------------------------------------------------- --caps=vision

    #[tool(description = "Click a mouse button at a position in CSS pixels")]
    async fn browser_mouse_click_xy(&self, Parameters(p): Parameters<MouseClickParams>) -> R {
        self.respond(self.mouse_click(p).await).await
    }

    #[tool(description = "Press a mouse button down at the current position")]
    async fn browser_mouse_down(&self, Parameters(p): Parameters<MouseButtonParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            self.browser.mouse_button(&page, true, p.button.as_deref().unwrap_or("left"), 1, 0).await?;
            Ok(Reply::action("Mouse down"))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Drag with the left mouse button from one position to another")]
    async fn browser_mouse_drag_xy(&self, Parameters(p): Parameters<MouseDragParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let b = &self.browser;
            b.settle(&page, async {
                b.mouse_move(&page, p.start_x, p.start_y).await?;
                b.mouse_button(&page, true, "left", 1, 0).await?;
                for i in 1..=5 {
                    let f = i as f64 / 5.0;
                    b.mouse_move(&page, p.start_x + (p.end_x - p.start_x) * f, p.start_y + (p.end_y - p.start_y) * f).await?;
                }
                b.mouse_button(&page, false, "left", 1, 0).await
            })
            .await?;
            Ok(Reply::action(format!("Dragged from ({}, {}) to ({}, {})", p.start_x, p.start_y, p.end_x, p.end_y)))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Move the mouse to a position in CSS pixels")]
    async fn browser_mouse_move_xy(&self, Parameters(p): Parameters<MouseMoveParams>) -> R {
        self.respond(self.mouse_move(p).await).await
    }

    #[tool(description = "Release a mouse button at the current position")]
    async fn browser_mouse_up(&self, Parameters(p): Parameters<MouseButtonParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            self.browser.settle(&page, self.browser.mouse_button(&page, false, p.button.as_deref().unwrap_or("left"), 1, 0)).await?;
            Ok(Reply::action("Mouse up"))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Scroll with the mouse wheel at the current mouse position")]
    async fn browser_mouse_wheel(&self, Parameters(p): Parameters<WheelParams>) -> R {
        self.respond(self.mouse_wheel(p).await).await
    }

    // ----------------------------------------------------------------- --caps=pdf

    #[tool(description = "Save the current page as a PDF")]
    async fn browser_pdf_save(&self, Parameters(p): Parameters<OptFilenameParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let path = self.browser.pdf(&page, p.filename.as_deref()).await?;
            Ok(Reply::raw(format!("Saved page as {}", path.display())))
        }
        .await;
        self.respond(r).await
    }

    // ----------------------------------------------------------------- --caps=devtools

    #[tool(description = "Show a persistent highlight overlay around an element")]
    async fn browser_highlight(&self, Parameters(p): Parameters<HighlightParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            self.browser.highlight(&page, &p.target, p.style.as_deref()).await?;
            Ok(Reply::raw(format!("Highlighted {}", p.element.unwrap_or(p.target))))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Remove a highlight overlay added for an element, or all highlights")]
    async fn browser_hide_highlight(&self, Parameters(p): Parameters<OptTargetParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            self.browser.hide_highlight(&page, p.target.as_deref()).await?;
            Ok(Reply::raw(format!("Removed highlight {}", p.element.or(p.target).unwrap_or_else(|| "(all)".into()))))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Start recording a performance trace (Chrome DevTools format)")]
    async fn browser_start_tracing(&self) -> R {
        self.respond(self.browser.start_tracing().await.map(|_| Reply::raw("Tracing started"))).await
    }

    #[tool(description = "Stop the trace and save it; open it in Chrome DevTools' Performance panel")]
    async fn browser_stop_tracing(&self) -> R {
        self.respond(self.browser.stop_tracing().await.map(|p| Reply::raw(format!("Trace saved to {}", p.display())))).await
    }

    #[tool(description = "Start recording a video of the current tab")]
    async fn browser_start_video(&self, Parameters(p): Parameters<VideoParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let path = self.browser.start_video(&page, p.filename.as_deref(), p.size.map(|s| (s.width, s.height))).await?;
            Ok(Reply::raw(format!("Recording video to {}", path.display())))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Stop the video recording and save it")]
    async fn browser_stop_video(&self) -> R {
        self.respond(self.browser.stop_video().await.map(Reply::raw)).await
    }

    #[tool(description = "Add a chapter marker to the video recording")]
    async fn browser_video_chapter(&self, Parameters(p): Parameters<ChapterParams>) -> R {
        let _ = p.duration;
        self.respond(self.browser.video_chapter(&p.title, p.description.as_deref()).map(|_| Reply::raw(format!("Chapter \"{}\" added", p.title)))).await
    }

    #[tool(description = "Annotate subsequent actions with a highlight on the element they act on")]
    async fn browser_video_show_actions(&self, Parameters(p): Parameters<ShowActionsParams>) -> R {
        let _ = (p.position, p.cursor);
        self.browser.state.lock().unwrap().show_actions = Some(p.duration.unwrap_or(800));
        self.respond(Ok(Reply::raw("Actions will be annotated"))).await
    }

    #[tool(description = "Stop annotating actions")]
    async fn browser_video_hide_actions(&self) -> R {
        self.browser.state.lock().unwrap().show_actions = None;
        self.respond(Ok(Reply::raw("Actions will no longer be annotated"))).await
    }

    #[tool(description = "Start recording the actions the user performs in the browser (clicks, fills, selections, key presses)")]
    async fn browser_start_recording(&self) -> R {
        self.respond(self.browser.set_recording(true).await.map(|_| Reply::raw("Recording user actions"))).await
    }

    #[tool(description = "Stop recording and return the recorded actions as Playwright code")]
    async fn browser_stop_recording(&self) -> R {
        let r = self.browser.set_recording(false).await.map(|events| {
            let lines: Vec<String> = events
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    let loc = e["locator"].as_str().unwrap_or("locator('body')");
                    match e["action"].as_str().unwrap_or("") {
                        "click" => format!("await page.{loc}.click();"),
                        "fill" => format!("await page.{loc}.fill({});", e["value"]),
                        "check" => format!("await page.{loc}.check();"),
                        "uncheck" => format!("await page.{loc}.uncheck();"),
                        "selectOption" => format!("await page.{loc}.selectOption({});", e["value"]),
                        "press" => format!("await page.keyboard.press({});", e["key"]),
                        other => format!("// {other}"),
                    }
                })
                .collect();
            Reply::raw(if lines.is_empty() { "No actions were recorded".into() } else { format!("```js\n{}\n```", lines.join("\n")) })
        });
        self.respond(r).await
    }

    // ----------------------------------------------------------------- --caps=testing

    #[tool(description = "Generate a Playwright locator for an element, for use in tests")]
    async fn browser_generate_locator(&self, Parameters(p): Parameters<TargetParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            Ok(Reply::raw(self.browser.locator_for(&page, &p.target).await?))
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Verify an element with the given role and accessible name is visible on the page")]
    async fn browser_verify_element_visible(&self, Parameters(p): Parameters<VerifyElementParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let expr = format!("__bmcp.isRoleVisible({}, {})", crate::actions::js(&p.role), crate::actions::js(&p.accessible_name));
            let ok = self.browser.eval_world(&page, &expr).await? == json!(true);
            self.verify(ok, format!("{} \"{}\" is visible", p.role, p.accessible_name)).await
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Verify a list is visible and contains the given items. Prefer browser_verify_element_visible when possible")]
    async fn browser_verify_list_visible(&self, Parameters(p): Parameters<VerifyListParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let text = self.browser.evaluate(&page, "(e) => e.innerText", Some(&p.target)).await?;
            let text = text.as_str().unwrap_or_default().to_string();
            let missing: Vec<&String> = p.items.iter().filter(|i| !text.contains(i.as_str())).collect();
            self.verify(missing.is_empty(), format!("{} contains {:?}{}", p.element, p.items, if missing.is_empty() { String::new() } else { format!(" (missing {missing:?})") })).await
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Verify text is visible on the page. Prefer browser_verify_element_visible when possible")]
    async fn browser_verify_text_visible(&self, Parameters(p): Parameters<VerifyTextParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let text = self.browser.eval_world(&page, "__bmcp.pageText()").await?;
            self.verify(text.as_str().unwrap_or_default().contains(&p.text), format!("text \"{}\" is visible", p.text)).await
        }
        .await;
        self.respond(r).await
    }

    #[tool(description = "Verify an element's value; for checkboxes and radios pass \"true\" or \"false\"")]
    async fn browser_verify_value(&self, Parameters(p): Parameters<VerifyValueParams>) -> R {
        let r = async {
            let page = self.browser.page().await?;
            let f = match p.kind.as_str() {
                "checkbox" | "radio" => "(e) => String(e.checked ?? e.getAttribute('aria-checked') === 'true')",
                "combobox" => "(e) => e.tagName === 'SELECT' ? [...e.selectedOptions].map(o => o.label).join(', ') : (e.value ?? e.textContent)",
                _ => "(e) => e.value ?? e.textContent",
            };
            let actual = self.browser.evaluate(&page, f, Some(&p.target)).await?;
            let actual = actual.as_str().map(str::to_string).unwrap_or_else(|| actual.to_string());
            let ok = actual == p.value || (p.kind == "combobox" && actual.split(", ").any(|v| v == p.value));
            self.verify(ok, format!("{} has value \"{}\"{}", p.element, p.value, if ok { String::new() } else { format!(" (actual: \"{actual}\")") })).await
        }
        .await;
        self.respond(r).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BrowserServer {
    /// Every tool call, timed: the reply ends with `elapsed_ms` so agents can reason about speed with
    /// real numbers, and with BROWSER_MCP_TRACE=<file> each call is appended there as JSON.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let tool = request.name.to_string();
        let args_chars = request.arguments.as_ref().and_then(|a| serde_json::to_string(a).ok()).map(|s| s.len()).unwrap_or(0);
        // One call at a time: the browser has one current tab and per-call options live in one slot.
        let _serial = self.lock.lock().await;
        {
            let args = request.arguments.as_ref();
            *self.browser.overrides.lock().unwrap() = crate::browser::Overrides {
                snapshot: args.and_then(|a| a.get("snapshot")).and_then(Value::as_str).map(str::to_string),
                timeout: args.and_then(|a| a.get("timeout")).and_then(Value::as_u64).map(Duration::from_millis),
            };
        }
        let started = std::time::Instant::now();
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let mut response = self.tool_router.call(call).await;
        *self.browser.overrides.lock().unwrap() = crate::browser::Overrides::default();
        let ms = started.elapsed().as_millis() as u64;
        let (mut reply_chars, mut error) = (0, response.is_err());
        if let Ok(rmcp::model::CallToolResponse::Complete(result)) = &mut response {
            error = result.is_error == Some(true);
            reply_chars = serde_json::to_string(&result.content).map(|s| s.len()).unwrap_or(0);
            result.content.push(rmcp::model::ContentBlock::text(format!("\nelapsed_ms: {ms}")));
        }
        if let Some(path) = std::env::var_os("BROWSER_MCP_TRACE") {
            let line = json!({ "tool": tool, "ms": ms, "reply_chars": reply_chars, "args_chars": args_chars, "error": error });
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes());
            }
        }
        response
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("browser-mcp-rs", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Browser automation on a bundled Chromium, with @playwright/mcp's tool names plus agent-first tools. Headless. \
                 Fastest way to work: (1) read with browser_text (Markdown), browser_links, browser_table, browser_extract or \
                 browser_fetch instead of snapshots; (2) act with CSS/text selectors or refs, several steps per browser_batch; \
                 (3) pass snapshot:\"none\" on actions when you do not need to see the page (\"main\" for main content only, \
                 default \"diff\" shows changed lines); (4) use browser_scrape to read many URLs in parallel. browser_evaluate \
                 awaits async functions. Every reply ends with elapsed_ms. When a human must act (log in, 2FA, CAPTCHA, \
                 payment), call browser_handoff: they get a live view of the same page, nothing reloads.",
            )
    }
}
