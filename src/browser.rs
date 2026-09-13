//! Browser lifecycle and page state: launching Chromium on a pipe, tracking every tab through
//! CDP events, and relaunching between headless and headed mode without losing the session.

use crate::cdp::{Cdp, Event};
use crate::install;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const WORLD: &str = "__bmcp";
pub const INJECTED: &str = include_str!("injected.js");

#[derive(Debug, Clone)]
pub struct Config {
    pub executable: Option<PathBuf>,
    pub headless: bool,
    pub user_data_dir: Option<PathBuf>,
    pub isolated: bool,
    pub viewport: Option<(u32, u32)>,
    pub user_agent: Option<String>,
    pub proxy_server: Option<String>,
    pub proxy_bypass: Option<String>,
    pub ignore_https_errors: bool,
    pub block_service_workers: bool,
    pub init_scripts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub blocked_origins: Vec<String>,
    pub output_dir: PathBuf,
    pub timeout_action: Duration,
    pub timeout_navigation: Duration,
    pub timeout_settle: Duration,
    pub test_id_attribute: String,
    pub storage_state: Option<PathBuf>,
    pub snapshot_mode: String,
    /// Snapshots longer than this are saved to a file and truncated in the reply.
    pub snapshot_max_chars: usize,
    pub caps: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct ConsoleMsg {
    pub level: &'static str,
    pub text: String,
    pub location: String,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub url: String,
    pub resource_type: String,
    pub request_headers: Value,
    pub post_data: Option<String>,
    pub status: Option<i64>,
    pub status_text: String,
    pub response_headers: Value,
    pub mime: String,
    pub failure: Option<String>,
    pub finished: bool,
    pub from_route: bool,
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_prompt: String,
}

#[derive(Debug, Clone)]
pub struct Download {
    pub guid: String,
    pub url: String,
    pub filename: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub pattern: String,
    pub regex: regex::Regex,
    pub status: Option<i64>,
    pub body: Option<String>,
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    pub remove_headers: Vec<String>,
}

pub struct Tab {
    pub target_id: String,
    pub session_id: String,
    pub url: String,
    pub title: String,
    pub main_frame: String,
    pub world_ctx: Option<i64>,
    pub loader_id: String,
    pub loading: watch::Sender<bool>,
    pub ready: watch::Sender<bool>,
    pub console: Vec<ConsoleMsg>,
    pub console_nav_start: usize,
    pub console_reported: usize,
    pub requests: Vec<Request>,
    pub request_index: HashMap<String, usize>,
    pub requests_nav_start: usize,
    pub inflight: HashSet<String>,
    pub dialog: Option<Dialog>,
    pub file_chooser: Option<(i64, String)>,
    pub last_snapshot: Option<String>,
    pub nav_seq: u64,
    pub handoff_scripts: Vec<String>,
}

pub struct Video {
    pub dir: PathBuf,
    pub filename: PathBuf,
    pub session: String,
    pub frames: Vec<(f64, PathBuf)>,
    pub chapters: Vec<(f64, String)>,
    pub started: Instant,
}

#[derive(Default)]
pub struct State {
    pub tabs: Vec<Tab>,
    pub current: usize,
    pub routes: Vec<Route>,
    pub downloads: Vec<Download>,
    pub recording: Option<Vec<Value>>,
    pub offline: bool,
    pub notices: Vec<String>,
    pub viewport: Option<(u32, u32)>,
    pub mouse: (f64, f64),
    /// Mouse buttons currently held, as a CDP `buttons` bitmask.
    pub buttons: i64,
    pub video: Option<Video>,
    pub show_actions: Option<u64>,
    /// Session most recently brought to front.
    pub front: Option<String>,
}

impl State {
    pub fn tab_by_session(&mut self, session: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.session_id == session)
    }
}

struct Running {
    cdp: Arc<Cdp>,
    child: Child,
    headless: bool,
    profile: PathBuf,
    temp_profile: bool,
}

pub struct Browser {
    pub cfg: Config,
    pub state: Arc<Mutex<State>>,
    running: tokio::sync::Mutex<Option<Running>>,
    /// Mode the next launch uses; flips on set_headless.
    want_headless: Mutex<bool>,
    rt: tokio::runtime::Handle,
}

/// What a tool needs to talk to the current tab.
#[derive(Clone)]
pub struct PageRef {
    pub cdp: Arc<Cdp>,
    pub session: String,
    pub target_id: String,
}

impl Browser {
    pub fn new(cfg: Config) -> Arc<Self> {
        let headless = cfg.headless;
        let state = State { viewport: cfg.viewport, ..Default::default() };
        Arc::new(Self {
            cfg,
            state: Arc::new(Mutex::new(state)),
            running: tokio::sync::Mutex::new(None),
            want_headless: Mutex::new(headless),
            rt: tokio::runtime::Handle::current(),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.try_lock().map(|r| r.as_ref().is_some_and(|r| !r.cdp.is_closed())).unwrap_or(true)
    }

    pub async fn is_headless(&self) -> bool {
        match &*self.running.lock().await {
            Some(r) if !r.cdp.is_closed() => r.headless,
            _ => *self.want_headless.lock().unwrap(),
        }
    }

    /// The browser connection, launching Chromium on first use or after it was closed.
    pub async fn cdp(&self) -> Result<Arc<Cdp>> {
        let mut guard = self.running.lock().await;
        if let Some(r) = guard.as_mut() {
            if !r.cdp.is_closed() {
                return Ok(r.cdp.clone());
            }
            let _ = r.child.wait();
            if r.temp_profile {
                let _ = std::fs::remove_dir_all(&r.profile);
            }
            *guard = None;
        }
        let headless = *self.want_headless.lock().unwrap();
        let running = self.launch(headless).await?;
        let cdp = running.cdp.clone();
        *guard = Some(running);
        drop(guard);
        if let Some(path) = &self.cfg.storage_state {
            let path = path.clone();
            self.set_storage_state(&cdp, &path).await.with_context(|| format!("loading --storage-state {}", path.display()))?;
        }
        Ok(cdp)
    }

    pub async fn page(&self) -> Result<PageRef> {
        let cdp = self.cdp().await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (found, ready_rx) = {
                let st = self.state.lock().unwrap();
                match st.tabs.get(st.current) {
                    Some(t) => (Some((t.session_id.clone(), t.target_id.clone())), Some(t.ready.subscribe())),
                    None => (None, None),
                }
            };
            match (found, ready_rx) {
                (Some((session, target_id)), Some(mut rx)) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    tokio::time::timeout(wait, rx.wait_for(|r| *r)).await.map_err(|_| anyhow!("tab did not become ready"))?.ok();
                    // Only the foreground tab renders (timers, rAF, screencast), so keep the
                    // current tab in front whenever it changes.
                    let front = self.state.lock().unwrap().front.clone();
                    if front.as_deref() != Some(session.as_str()) {
                        let _ = cdp.send_session(&session, "Page.bringToFront", json!({})).await;
                        self.state.lock().unwrap().front = Some(session.clone());
                    }
                    return Ok(PageRef { cdp, session, target_id });
                }
                _ => {
                    if Instant::now() > deadline {
                        bail!("browser has no open page");
                    }
                    // Every tab was closed: open a fresh one, like Playwright does on the next call.
                    self.create_blank_tab(&cdp).await?;
                }
            }
        }
    }

    // ------------------------------------------------------------------ launch

    async fn launch(&self, headless: bool) -> Result<Running> {
        let exe = match &self.cfg.executable {
            Some(p) => p.clone(),
            None if headless => install::ensure(install::Product::HeadlessShell).await?,
            None => install::ensure(install::Product::Chrome).await?,
        };
        let (profile, temp_profile) = self.profile_dir()?;
        std::fs::create_dir_all(&profile)?;
        // Chrome would otherwise reopen the previous run's tabs next to the ones we restore.
        // Cookies and storage live elsewhere in the profile and are unaffected.
        let _ = std::fs::remove_dir_all(profile.join("Default").join("Sessions"));
        let downloads = self.cfg.output_dir.join("downloads");
        std::fs::create_dir_all(&downloads)?;

        let (w, h) = self.cfg.viewport.unwrap_or((1280, 800));
        let mut args: Vec<String> = vec![
            "--remote-debugging-pipe".into(),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-background-networking".into(),
            "--disable-background-timer-throttling".into(),
            "--disable-backgrounding-occluded-windows".into(),
            "--disable-renderer-backgrounding".into(),
            "--disable-breakpad".into(),
            // Pages left behind by navigation would otherwise stay alive in the back/forward cache,
            // and component extensions run background workers nobody uses here.
            "--disable-back-forward-cache".into(),
            "--disable-component-extensions-with-background-pages".into(),
            "--disable-field-trial-config".into(),
            "--disable-updater-scheduler".into(),
            "--allow-pre-commit-input".into(),
            "--force-color-profile=srgb".into(),
            "--disable-infobars".into(),
            "--disable-client-side-phishing-detection".into(),
            "--disable-component-update".into(),
            "--disable-default-apps".into(),
            "--disable-dev-shm-usage".into(),
            "--disable-extensions".into(),
            "--disable-hang-monitor".into(),
            "--disable-ipc-flooding-protection".into(),
            "--disable-popup-blocking".into(),
            "--disable-prompt-on-repost".into(),
            "--disable-sync".into(),
            "--disable-search-engine-choice-screen".into(),
            "--disable-blink-features=AutomationControlled".into(),
            "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,AutofillServerCommunication,CertificateTransparencyComponentUpdater,InterestFeedContentSuggestions,PrivacySandboxSettings4,AcceptCHFrame,LensOverlay".into(),
            "--metrics-recording-only".into(),
            "--password-store=basic".into(),
            "--use-mock-keychain".into(),
            "--no-service-autorun".into(),
            "--export-tagged-pdf".into(),
            format!("--window-size={w},{}", if headless { h } else { h + 87 }),
        ];
        if headless {
            args.extend(["--headless".into(), "--hide-scrollbars".into(), "--mute-audio".into()]);
        }
        if let Some(ua) = &self.cfg.user_agent {
            args.push(format!("--user-agent={ua}"));
        }
        if let Some(p) = &self.cfg.proxy_server {
            args.push(format!("--proxy-server={p}"));
        }
        if let Some(b) = &self.cfg.proxy_bypass {
            args.push(format!("--proxy-bypass-list={b}"));
        }
        if self.cfg.ignore_https_errors {
            args.push("--ignore-certificate-errors".into());
        }
        args.push("about:blank".into());

        // Two pipes: we write into Chrome's fd 3 and read from its fd 4.
        let (to_chrome_r, to_chrome_w) = pipe()?;
        let (from_chrome_r, from_chrome_w) = pipe()?;
        let mut cmd = Command::new(&exe);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(to_chrome_r, 3) < 0 || libc::dup2(from_chrome_w, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("launching {}", exe.display()))?;
        unsafe {
            libc::close(to_chrome_r);
            libc::close(from_chrome_w);
        }
        let to_chrome = unsafe { File::from_raw_fd(to_chrome_w) };
        let from_chrome = unsafe { File::from_raw_fd(from_chrome_r) };

        let state = self.state.clone();
        let rt = self.rt.clone();
        let cfg = self.cfg.clone();
        let cdp_slot: Arc<Mutex<Option<Arc<Cdp>>>> = Arc::new(Mutex::new(None));
        let slot = cdp_slot.clone();
        let cdp = Cdp::new(
            to_chrome,
            from_chrome,
            Box::new(move |ev| {
                let cdp = slot.lock().unwrap().clone();
                on_event(ev, &state, cdp, &rt, &cfg);
            }),
        );
        *cdp_slot.lock().unwrap() = Some(cdp.clone());
        {
            let mut st = self.state.lock().unwrap();
            st.tabs.clear();
            st.current = 0;
        }

        cdp.send("Target.setDiscoverTargets", json!({ "discover": true })).await.map_err(|e| anyhow!("browser did not start: {e}"))?;
        cdp.send("Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }))
            .await
            .map_err(|e| anyhow!(e))?;
        cdp.send(
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allowAndName", "downloadPath": downloads, "eventsEnabled": true }),
        )
        .await
        .map_err(|e| anyhow!(e))?;

        // Wait for the startup tab to attach and initialize.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ready = {
                let st = self.state.lock().unwrap();
                st.tabs.first().map(|t| *t.ready.borrow())
            };
            if ready == Some(true) {
                break;
            }
            if Instant::now() > deadline {
                if ready.is_none() {
                    self.create_blank_tab(&cdp).await?;
                    break;
                }
                bail!("browser started but its first page never became ready");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(Running { cdp, child, headless, profile, temp_profile })
    }

    /// The persistent profile, unless another live browser holds it (then a throwaway one).
    fn profile_dir(&self) -> Result<(PathBuf, bool)> {
        let temp = || -> Result<(PathBuf, bool)> {
            let dir = std::env::temp_dir().join(format!("browser-mcp-rs-profile-{}", std::process::id()));
            Ok((dir, true))
        };
        if self.cfg.isolated {
            return temp();
        }
        let dir = self.cfg.user_data_dir.clone().unwrap_or_else(|| install::cache_root().join("profile"));
        if let Ok(target) = std::fs::read_link(dir.join("SingletonLock")) {
            let pid: i32 = target.to_string_lossy().rsplit('-').next().and_then(|p| p.parse().ok()).unwrap_or(0);
            let ours = self.running.try_lock().ok().and_then(|r| r.as_ref().map(|r| r.child.id() as i32)) == Some(pid);
            if pid > 0 && !ours && unsafe { libc::kill(pid, 0) } == 0 {
                if self.cfg.user_data_dir.is_some() {
                    bail!("profile {} is in use by another browser (pid {pid})", dir.display());
                }
                eprintln!("browser-mcp-rs: default profile in use by pid {pid}; using a temporary profile");
                return temp();
            }
        }
        Ok((dir, false))
    }

    pub async fn close(&self) -> Result<bool> {
        let mut guard = self.running.lock().await;
        let Some(mut r) = guard.take() else { return Ok(false) };
        shutdown(&mut r).await;
        self.state.lock().unwrap().tabs.clear();
        Ok(true)
    }

    // ------------------------------------------------------------------ tabs

    /// Opens an about:blank tab, waits until it is initialized, and makes it current.
    async fn create_blank_tab(&self, cdp: &Arc<Cdp>) -> Result<String> {
        let res = cdp.send("Target.createTarget", json!({ "url": "about:blank" })).await.map_err(|e| anyhow!(e))?;
        let target_id = res["targetId"].as_str().unwrap_or_default().to_string();
        self.wait_tab_ready(&target_id).await?;
        let mut st = self.state.lock().unwrap();
        if let Some(i) = st.tabs.iter().position(|t| t.target_id == target_id) {
            st.current = i;
        }
        Ok(target_id)
    }

    pub async fn new_tab_raw(&self, cdp: &Arc<Cdp>, url: &str) -> Result<String> {
        let target_id = self.create_blank_tab(cdp).await?;
        if url != "about:blank" {
            let page = self.page().await?;
            self.navigate(&page, url).await?;
        }
        Ok(target_id)
    }

    // ------------------------------------------------------------------ navigation

    pub async fn navigate(&self, page: &PageRef, url: &str) -> Result<()> {
        let url = normalize_url(url);
        self.check_origin(&url)?;
        let mut loading = self.tab_watch(page, |t| t.loading.subscribe())?;
        let res = page
            .cdp
            .send_full(Some(&page.session), "Page.navigate", json!({ "url": url }), self.cfg.timeout_navigation)
            .await
            .map_err(|e| anyhow!(e))?;
        if let Some(err) = res["errorText"].as_str().filter(|e| !e.is_empty()) {
            // Navigating to a file download aborts the navigation but starts a download.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let st = self.state.lock().unwrap();
            if err == "net::ERR_ABORTED" && st.downloads.iter().any(|d| d.url == url) {
                return Ok(());
            }
            bail!("navigation to {url} failed: {err}");
        }
        let Some(loader) = res["loaderId"].as_str().map(str::to_string) else {
            return Ok(()); // same-document navigation
        };
        let deadline = Instant::now() + self.cfg.timeout_navigation;
        loop {
            let done = {
                let st = self.state.lock().unwrap();
                st.tabs.iter().find(|t| t.session_id == page.session).map(|t| t.loader_id == loader && !*t.loading.borrow())
            };
            match done {
                Some(true) => return Ok(()),
                None => bail!("tab closed during navigation"),
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("navigation to {url} timed out after {}ms", self.cfg.timeout_navigation.as_millis());
            }
            let _ = tokio::time::timeout(left.min(Duration::from_millis(250)), loading.changed()).await;
        }
    }

    pub fn check_origin(&self, url: &str) -> Result<()> {
        if let Some(reason) = origin_blocked(url, &self.cfg) {
            bail!("{reason}");
        }
        Ok(())
    }

    /// Runs an action, then waits for what it set off: a navigation to finish loading, or
    /// fetch/XHR requests it started to settle (bounded by --timeout-settle).
    pub async fn settle<T>(&self, page: &PageRef, action: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let before = self.tab_read(page, |t| (t.nav_seq, t.requests.len()))?;
        let out = action.await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        loop {
            let snap = {
                let st = self.state.lock().unwrap();
                st.tabs.iter().find(|t| t.session_id == page.session).map(|t| {
                    let new_xhr = t.requests[before.1.min(t.requests.len())..]
                        .iter()
                        .any(|r| !r.finished && matches!(r.resource_type.as_str(), "Fetch" | "XHR" | "Document"));
                    (t.nav_seq != before.0, *t.loading.borrow(), new_xhr, t.dialog.is_some())
                })
            };
            let Some((navigated, loading, busy, dialog)) = snap else { return Ok(out) };
            if dialog {
                return Ok(out);
            }
            let limit = if navigated { self.cfg.timeout_navigation } else { self.cfg.timeout_settle };
            if !(loading && navigated) && !busy {
                return Ok(out);
            }
            if started.elapsed() > limit {
                return Ok(out);
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    pub fn tab_read<T>(&self, page: &PageRef, f: impl FnOnce(&Tab) -> T) -> Result<T> {
        let st = self.state.lock().unwrap();
        st.tabs.iter().find(|t| t.session_id == page.session).map(f).ok_or_else(|| anyhow!("the tab was closed"))
    }

    pub fn tab_write<T>(&self, page: &PageRef, f: impl FnOnce(&mut Tab) -> T) -> Result<T> {
        let mut st = self.state.lock().unwrap();
        st.tabs.iter_mut().find(|t| t.session_id == page.session).map(f).ok_or_else(|| anyhow!("the tab was closed"))
    }

    fn tab_watch<T>(&self, page: &PageRef, f: impl FnOnce(&Tab) -> T) -> Result<T> {
        self.tab_read(page, f)
    }

    // ------------------------------------------------------------------ evaluation

    /// Evaluates in the runtime's isolated world of the main frame, creating the world if this
    /// document does not have it yet. Fails fast when a dialog blocks the page.
    pub async fn eval_world(&self, page: &PageRef, expr: &str) -> Result<Value> {
        let res = self.eval_raw(page, expr, true, true).await?;
        Ok(res["result"]["value"].clone())
    }

    pub async fn eval_world_handle(&self, page: &PageRef, expr: &str) -> Result<String> {
        let res = self.eval_raw(page, expr, false, false).await?;
        res["result"]["objectId"].as_str().map(str::to_string).ok_or_else(|| anyhow!("expression did not produce an object"))
    }

    async fn eval_raw(&self, page: &PageRef, expr: &str, by_value: bool, await_promise: bool) -> Result<Value> {
        for attempt in 0..3 {
            let ctx = self.world_context(page).await?;
            let params = json!({
                "expression": expr, "contextId": ctx, "returnByValue": by_value,
                "awaitPromise": await_promise, "userGesture": true,
            });
            match self.send_page(page, "Runtime.evaluate", params, self.cfg.timeout_navigation).await {
                Ok(res) => return exception_to_err(res),
                Err(e) if attempt < 2 && is_context_gone(&e.to_string()) => {
                    self.tab_write(page, |t| t.world_ctx = None)?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    pub async fn call_on(&self, page: &PageRef, object_id: &str, func: &str, args: Vec<Value>) -> Result<Value> {
        let params = json!({
            "functionDeclaration": func, "objectId": object_id,
            "arguments": args.into_iter().map(|v| json!({ "value": v })).collect::<Vec<_>>(),
            "returnByValue": true, "awaitPromise": true, "userGesture": true,
        });
        let res = exception_to_err(self.send_page(page, "Runtime.callFunctionOn", params, self.cfg.timeout_navigation).await?)?;
        Ok(res["result"]["value"].clone())
    }

    /// Sends a session command, but bails out if a JavaScript dialog opens meanwhile (a pending
    /// alert() blocks Runtime calls until it is handled, which would otherwise hang the tool).
    pub async fn send_page(&self, page: &PageRef, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if let Some(d) = self.tab_read(page, |t| t.dialog.clone())? {
            bail!("a \"{}\" dialog is open ({}); handle it with browser_handle_dialog first", d.kind, d.message);
        }
        let session = page.session.clone();
        let dialog = page.cdp.wait_for(move |e| e.method == "Page.javascriptDialogOpening" && e.session_id.as_deref() == Some(&session));
        tokio::select! {
            r = page.cdp.send_full(Some(&page.session), method, params, timeout) => r.map_err(|e| anyhow!(e)),
            Ok(ev) = dialog => bail!(
                "the action opened a \"{}\" dialog ({}); handle it with browser_handle_dialog",
                ev.params["type"].as_str().unwrap_or("?"), ev.params["message"].as_str().unwrap_or("")
            ),
        }
    }

    async fn world_context(&self, page: &PageRef) -> Result<i64> {
        if let Some(ctx) = self.tab_read(page, |t| t.world_ctx)? {
            return Ok(ctx);
        }
        // The addScriptToEvaluateOnNewDocument world usually shows up with the document; give it a beat.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(ctx) = self.tab_read(page, |t| t.world_ctx)? {
                return Ok(ctx);
            }
        }
        let frame = self.tab_read(page, |t| t.main_frame.clone())?;
        let res = page
            .cdp
            .send_session(&page.session, "Page.createIsolatedWorld", json!({ "frameId": frame, "worldName": WORLD, "grantUniveralAccess": true }))
            .await
            .map_err(|e| anyhow!(e))?;
        let ctx = res["executionContextId"].as_i64().ok_or_else(|| anyhow!("no isolated world"))?;
        page.cdp
            .send_session(&page.session, "Runtime.evaluate", json!({ "expression": INJECTED, "contextId": ctx }))
            .await
            .map_err(|e| anyhow!(e))?;
        self.tab_write(page, |t| t.world_ctx = Some(ctx))?;
        Ok(ctx)
    }

    // ------------------------------------------------------------------ headless <-> headed

    /// Relaunches Chromium in the other mode on the same profile and restores tabs, cookies
    /// (including session cookies) and sessionStorage. In-page JavaScript state does not survive.
    pub async fn set_headless(&self, headless: bool) -> Result<String> {
        *self.want_headless.lock().unwrap() = headless;
        let mut guard = self.running.lock().await;
        let Some(r) = guard.as_mut().filter(|r| !r.cdp.is_closed()) else {
            return Ok(format!("browser will start {}", if headless { "headless" } else { "headed" }));
        };
        if r.headless == headless {
            if !headless {
                self.bring_to_front(&r.cdp).await;
            }
            return Ok(format!("already {}", if headless { "headless" } else { "headed" }));
        }
        let cdp = r.cdp.clone();

        let cookies = cdp.send("Storage.getCookies", json!({})).await.map(|v| v["cookies"].clone()).unwrap_or(json!([]));
        let tabs: Vec<(String, String)> = {
            let st = self.state.lock().unwrap();
            st.tabs.iter().map(|t| (t.session_id.clone(), t.url.clone())).collect()
        };
        let current = self.state.lock().unwrap().current;
        let mut saved = Vec::new();
        for (session, url) in &tabs {
            let storage = cdp
                .send_full(
                    Some(session),
                    "Runtime.evaluate",
                    json!({ "expression": "JSON.stringify(Object.entries(sessionStorage))", "returnByValue": true }),
                    Duration::from_secs(2),
                )
                .await
                .ok()
                .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
                .unwrap_or_else(|| "[]".into());
            saved.push((url.clone(), storage));
        }

        let mut old = guard.take().unwrap();
        let profile_was_temp = old.temp_profile;
        let profile = old.profile.clone();
        // Keep a temporary profile on disk across the relaunch; it is removed on final close.
        old.temp_profile = false;
        shutdown(&mut old).await;
        let mut running = self.launch(headless).await?;
        running.temp_profile = profile_was_temp;
        running.profile = profile;
        let cdp = running.cdp.clone();
        *guard = Some(running);
        drop(guard);

        if cookies.as_array().is_some_and(|c| !c.is_empty()) {
            let _ = cdp.send("Storage.setCookies", json!({ "cookies": cookies })).await;
        }
        for (i, (url, storage)) in saved.iter().enumerate() {
            if i > 0 {
                let res = cdp.send("Target.createTarget", json!({ "url": "about:blank" })).await.map_err(|e| anyhow!(e))?;
                let id = res["targetId"].as_str().unwrap_or_default().to_string();
                self.wait_tab_ready(&id).await?;
            }
            let session = self.state.lock().unwrap().tabs.get(i).map(|t| t.session_id.clone());
            let Some(session) = session else { continue };
            if storage != "[]" {
                let script = format!(
                    "(() => {{ try {{ if (!sessionStorage.length && location.href === {u}) for (const [k, v] of {s}) sessionStorage.setItem(k, v); }} catch {{}} }})()",
                    u = serde_json::to_string(url).unwrap(),
                    s = storage
                );
                let _ = cdp.send_session(&session, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": script })).await;
            }
            if url != "about:blank" && !url.is_empty() {
                let _ = cdp.send_session(&session, "Page.navigate", json!({ "url": url })).await;
            }
        }
        {
            let mut st = self.state.lock().unwrap();
            st.current = current.min(st.tabs.len().saturating_sub(1));
        }
        let page = self.page().await?;
        let _ = self.wait_loaded(&page, self.cfg.timeout_navigation).await;
        if !headless {
            self.bring_to_front(&cdp).await;
        }
        Ok(format!("switched to {} mode ({} tab(s) restored)", if headless { "headless" } else { "headed" }, saved.len()))
    }

    async fn wait_tab_ready(&self, target_id: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rx = self.state.lock().unwrap().tabs.iter().find(|t| t.target_id == target_id).map(|t| t.ready.subscribe());
            if let Some(mut rx) = rx {
                let _ = tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|r| *r)).await;
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("tab did not attach");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn wait_loaded(&self, page: &PageRef, timeout: Duration) -> Result<()> {
        let mut rx = self.tab_read(page, |t| t.loading.subscribe())?;
        tokio::time::timeout(timeout, rx.wait_for(|l| !*l)).await.map_err(|_| anyhow!("page did not finish loading"))?.ok();
        Ok(())
    }

    async fn bring_to_front(&self, cdp: &Arc<Cdp>) {
        let session = {
            let st = self.state.lock().unwrap();
            st.tabs.get(st.current).map(|t| t.session_id.clone())
        };
        if let Some(s) = session {
            let _ = cdp.send_session(&s, "Page.bringToFront", json!({})).await;
        }
        // Chromium launched from a background process does not take focus on its own, so ask the app
        // to activate (no System Events / accessibility permission involved). `activate` launches
        // the app if it is not running, which once left a stray default-profile browser behind after
        // a quick headed -> headless switch: only activate a running app, and wait for osascript so
        // a shutdown can never overlap it.
        #[cfg(target_os = "macos")]
        {
            let script = "if application id \"com.google.chrome.for.testing\" is running then \
                          tell application id \"com.google.chrome.for.testing\" to activate";
            let child = tokio::process::Command::new("osascript")
                .args(["-e", script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status();
            let _ = tokio::time::timeout(Duration::from_secs(2), child).await;
        }
    }

    // ------------------------------------------------------------------ storage state

    pub async fn storage_state(&self, cdp: &Arc<Cdp>) -> Result<Value> {
        let cookies = cdp.send("Storage.getCookies", json!({})).await.map_err(|e| anyhow!(e))?["cookies"].clone();
        let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        let mut origins: Vec<Value> = Vec::new();
        let mut seen = HashSet::new();
        for s in sessions {
            let res = cdp
                .send_session(
                    &s,
                    "Runtime.evaluate",
                    json!({ "expression": "({ origin: location.origin, localStorage: Object.entries(localStorage).map(([name, value]) => ({ name, value })) })", "returnByValue": true }),
                )
                .await;
            if let Ok(v) = res {
                let v = v["result"]["value"].clone();
                let origin = v["origin"].as_str().unwrap_or("null").to_string();
                if origin != "null" && seen.insert(origin) {
                    origins.push(v);
                }
            }
        }
        Ok(json!({ "cookies": cookies, "origins": origins }))
    }

    /// Loads a Playwright storage-state file: cookies directly, localStorage by opening each origin
    /// in a scratch tab whose document request is fulfilled locally (no network round trip).
    pub async fn set_storage_state(&self, cdp: &Arc<Cdp>, path: &std::path::Path) -> Result<()> {
        let data: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if let Some(cookies) = data["cookies"].as_array().filter(|c| !c.is_empty()) {
            let cookies: Vec<Value> = cookies
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    if c["expires"].as_f64().is_some_and(|e| e < 0.0) {
                        c.as_object_mut().unwrap().remove("expires");
                    }
                    c
                })
                .collect();
            cdp.send("Storage.setCookies", json!({ "cookies": cookies })).await.map_err(|e| anyhow!(e))?;
        }
        for origin in data["origins"].as_array().cloned().unwrap_or_default() {
            let Some(o) = origin["origin"].as_str() else { continue };
            let items = origin["localStorage"].clone();
            let res = cdp.send("Target.createTarget", json!({ "url": "about:blank", "background": true })).await.map_err(|e| anyhow!(e))?;
            let target = res["targetId"].as_str().unwrap_or_default().to_string();
            self.wait_tab_ready(&target).await?;
            let session = self.state.lock().unwrap().tabs.iter().find(|t| t.target_id == target).map(|t| t.session_id.clone());
            if let Some(session) = session {
                let _ = cdp.send_session(&session, "Fetch.enable", json!({ "patterns": [{ "urlPattern": format!("{o}/*"), "requestStage": "Request" }] })).await;
                let paused = cdp.wait_for({
                    let s = session.clone();
                    move |e| e.method == "Fetch.requestPaused" && e.session_id.as_deref() == Some(&s)
                });
                let _ = cdp.send_session(&session, "Page.navigate", json!({ "url": format!("{o}/") })).await;
                if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), paused).await {
                    let _ = cdp
                        .send_session(&session, "Fetch.fulfillRequest", json!({ "requestId": ev.params["requestId"], "responseCode": 200, "body": "" }))
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                let script = format!("for (const {{name, value}} of {items}) localStorage.setItem(name, value)");
                let _ = cdp.send_session(&session, "Runtime.evaluate", json!({ "expression": script })).await;
            }
            let _ = cdp.send("Target.closeTarget", json!({ "targetId": target })).await;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ routes

    pub async fn sync_fetch(&self) {
        let enable = {
            let st = self.state.lock().unwrap();
            !st.routes.is_empty() || !self.cfg.allowed_origins.is_empty() || !self.cfg.blocked_origins.is_empty()
        };
        let Ok(cdp) = self.cdp().await else { return };
        let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        for s in sessions {
            let _ = if enable {
                cdp.send_session(&s, "Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] })).await
            } else {
                cdp.send_session(&s, "Fetch.disable", json!({})).await
            };
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(mut g) = self.running.try_lock() {
            if let Some(r) = g.as_mut() {
                let _ = r.child.kill();
                let _ = r.child.wait();
                if r.temp_profile {
                    let _ = std::fs::remove_dir_all(&r.profile);
                }
            }
        }
    }
}

async fn shutdown(r: &mut Running) {
    let _ = r.cdp.send_full(None, "Browser.close", json!({}), Duration::from_secs(3)).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(r.child.try_wait(), Ok(Some(_))) {
            break;
        }
        if Instant::now() > deadline {
            let _ = r.child.kill();
            let _ = r.child.wait();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if r.temp_profile {
        let _ = std::fs::remove_dir_all(&r.profile);
    }
}

fn pipe() -> Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    // Only the dup2'ed copies (fd 3/4) may reach Chrome.
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((fds[0], fds[1]))
}

pub fn normalize_url(url: &str) -> String {
    let u = url.trim();
    if u.contains("://") || ["about:", "data:", "javascript:", "file:", "chrome:", "blob:"].iter().any(|p| u.starts_with(p)) {
        return u.to_string();
    }
    if u.starts_with("localhost") || u.starts_with("127.0.0.1") || u.starts_with("[::1]") {
        return format!("http://{u}");
    }
    format!("https://{u}")
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(format!("{scheme}://{host}"))
}

fn origin_matches(origin: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    if pattern.contains("://") {
        return origin == pattern;
    }
    let host = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin);
    let host = host.split(':').next().unwrap_or(host);
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

pub fn origin_blocked(url: &str, cfg: &Config) -> Option<String> {
    let origin = origin_of(url)?;
    if !origin.starts_with("http") {
        return None;
    }
    if cfg.blocked_origins.iter().any(|p| origin_matches(&origin, p)) {
        return Some(format!("access to {origin} is blocked by --blocked-origins"));
    }
    if !cfg.allowed_origins.is_empty() && !cfg.allowed_origins.iter().any(|p| origin_matches(&origin, p)) {
        return Some(format!("access to {origin} is not in --allowed-origins"));
    }
    None
}

fn exception_to_err(res: Value) -> Result<Value> {
    if let Some(ex) = res.get("exceptionDetails") {
        let msg = ex["exception"]["description"]
            .as_str()
            .or_else(|| ex["exception"]["value"].as_str())
            .or_else(|| ex["text"].as_str())
            .unwrap_or("JavaScript exception");
        // Keep just the message, not the injected runtime's stack.
        let first = msg.lines().next().unwrap_or(msg).trim_start_matches("Error: ");
        bail!("{first}");
    }
    Ok(res)
}

fn is_context_gone(e: &str) -> bool {
    e.contains("Cannot find context") || e.contains("Execution context was destroyed") || e.contains("Inspected target navigated")
}

// ---------------------------------------------------------------------- events

fn level_of_console(t: &str) -> &'static str {
    match t {
        "error" | "assert" => "error",
        "warning" => "warning",
        "debug" | "verbose" => "debug",
        _ => "info",
    }
}

fn format_remote(arg: &Value) -> String {
    if let Some(v) = arg.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(u) = arg.get("unserializableValue").and_then(Value::as_str) {
        return u.to_string();
    }
    if let Some(p) = arg.get("preview") {
        let props: Vec<String> = p["properties"]
            .as_array()
            .map(|a| a.iter().map(|x| format!("{}: {}", x["name"].as_str().unwrap_or(""), x["value"].as_str().unwrap_or(""))).collect())
            .unwrap_or_default();
        if p["subtype"] == "array" {
            return format!("[{}]", props.iter().map(|s| s.split_once(": ").map(|x| x.1).unwrap_or(s)).collect::<Vec<_>>().join(", "));
        }
        return format!("{{{}}}", props.join(", "));
    }
    arg.get("description").and_then(Value::as_str).unwrap_or("undefined").to_string()
}

fn on_event(ev: &Event, state: &Arc<Mutex<State>>, cdp: Option<Arc<Cdp>>, rt: &tokio::runtime::Handle, cfg: &Config) {
    let p = &ev.params;
    let session = ev.session_id.as_deref();
    match ev.method.as_str() {
        "Target.attachedToTarget" => {
            let Some(cdp) = cdp else { return };
            let child = p["sessionId"].as_str().unwrap_or_default().to_string();
            let info = &p["targetInfo"];
            if std::env::var_os("BROWSER_MCP_DEBUG").is_some() {
                eprintln!("[attach] session={} parent={:?} {}", child, session, info);
            }
            if info["type"] != "page" {
                rt.spawn(async move {
                    let _ = cdp.send_session(&child, "Runtime.runIfWaitingForDebugger", json!({})).await;
                });
                return;
            }
            let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
            let opener = info["openerId"].as_str().is_some();
            {
                let mut st = state.lock().unwrap();
                st.tabs.push(Tab {
                    target_id,
                    session_id: child.clone(),
                    url: info["url"].as_str().unwrap_or_default().to_string(),
                    title: info["title"].as_str().unwrap_or_default().to_string(),
                    main_frame: String::new(),
                    world_ctx: None,
                    loader_id: String::new(),
                    loading: watch::channel(false).0,
                    ready: watch::channel(false).0,
                    console: Vec::new(),
                    console_nav_start: 0,
                    console_reported: 0,
                    requests: Vec::new(),
                    request_index: HashMap::new(),
                    requests_nav_start: 0,
                    inflight: HashSet::new(),
                    dialog: None,
                    file_chooser: None,
                    last_snapshot: None,
                    nav_seq: 0,
                    handoff_scripts: Vec::new(),
                });
                if opener {
                    let n = st.tabs.len();
                    st.notices.push(format!("A new tab was opened by the page (tab {}); use browser_tabs to switch to it", n - 1));
                }
            }
            let state = state.clone();
            let cfg = cfg.clone();
            rt.spawn(async move { init_session(cdp, state, child, cfg).await });
        }
        "Target.detachedFromTarget" => {
            let Some(s) = p["sessionId"].as_str() else { return };
            let mut st = state.lock().unwrap();
            if let Some(i) = st.tabs.iter().position(|t| t.session_id == s) {
                st.tabs.remove(i);
                if st.current > i || st.current >= st.tabs.len() {
                    st.current = st.current.saturating_sub(1);
                }
            }
        }
        "Target.targetInfoChanged" => {
            let info = &p["targetInfo"];
            let mut st = state.lock().unwrap();
            if let Some(t) = st.tabs.iter_mut().find(|t| info["targetId"] == t.target_id.as_str()) {
                if let Some(u) = info["url"].as_str() {
                    t.url = u.to_string();
                }
                if let Some(title) = info["title"].as_str() {
                    t.title = title.to_string();
                }
            }
        }
        "Browser.downloadWillBegin" => {
            let mut st = state.lock().unwrap();
            st.downloads.push(Download {
                guid: p["guid"].as_str().unwrap_or_default().into(),
                url: p["url"].as_str().unwrap_or_default().into(),
                filename: p["suggestedFilename"].as_str().unwrap_or_default().into(),
                state: "inProgress".into(),
            });
        }
        "Browser.downloadProgress" => {
            let st_name = p["state"].as_str().unwrap_or_default();
            if st_name == "inProgress" {
                return;
            }
            let mut st = state.lock().unwrap();
            let dir = cfg.output_dir.join("downloads");
            let mut notice = None;
            if let Some(d) = st.downloads.iter_mut().find(|d| p["guid"] == d.guid.as_str()) {
                d.state = st_name.to_string();
                if st_name == "completed" {
                    // allowAndName stores the file under its guid; give it its real name.
                    let target = unique_path(dir.join(&d.filename));
                    let _ = std::fs::rename(dir.join(&d.guid), &target);
                    notice = Some(format!("Downloaded \"{}\" to {}", d.filename, target.display()));
                } else {
                    notice = Some(format!("Download of \"{}\" {}", d.filename, st_name));
                }
            }
            if let Some(n) = notice {
                st.notices.push(n);
            }
        }
        "Runtime.bindingCalled" if p["name"] == "__bmcpRecord" => {
            let mut st = state.lock().unwrap();
            if let (Some(rec), Some(payload)) = (st.recording.as_mut(), p["payload"].as_str()) {
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    rec.push(v);
                }
            }
        }
        "Fetch.requestPaused" => {
            let (Some(cdp), Some(s)) = (cdp, session) else { return };
            let s = s.to_string();
            let url = p["request"]["url"].as_str().unwrap_or_default().to_string();
            let request_id = p["requestId"].clone();
            let (route, blocked) = {
                let st = state.lock().unwrap();
                (st.routes.iter().rev().find(|r| r.regex.is_match(&url)).cloned(), origin_blocked(&url, cfg))
            };
            let headers = p["request"]["headers"].clone();
            let network_id = p["networkId"].as_str().map(str::to_string);
            if route.is_some() {
                if let Some(id) = &network_id {
                    let mut st = state.lock().unwrap();
                    if let Some(t) = st.tab_by_session(&s) {
                        if let Some(&i) = t.request_index.get(id) {
                            t.requests[i].from_route = true;
                        }
                    }
                }
            }
            rt.spawn(async move {
                let _ = if blocked.is_some() {
                    cdp.send_session(&s, "Fetch.failRequest", json!({ "requestId": request_id, "errorReason": "BlockedByClient" })).await
                } else if let Some(r) = route {
                    fulfill_route(&cdp, &s, request_id, &r, &headers).await
                } else {
                    cdp.send_session(&s, "Fetch.continueRequest", json!({ "requestId": request_id })).await
                };
            });
        }
        "Page.screencastFrame" => {
            let (Some(cdp), Some(s)) = (cdp, session) else { return };
            let s = s.to_string();
            let ack = p["sessionId"].clone();
            {
                let mut st = state.lock().unwrap();
                if let Some(v) = st.video.as_mut().filter(|v| v.session == s) {
                    use base64::Engine as _;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(p["data"].as_str().unwrap_or_default()) {
                        let path = v.dir.join(format!("frame-{:06}.jpg", v.frames.len()));
                        if std::fs::write(&path, bytes).is_ok() {
                            v.frames.push((v.started.elapsed().as_secs_f64(), path));
                        }
                    }
                }
            }
            rt.spawn(async move {
                let _ = cdp.send_session(&s, "Page.screencastFrameAck", json!({ "sessionId": ack })).await;
            });
        }
        "__closed" => {
            let mut st = state.lock().unwrap();
            st.tabs.clear();
        }
        _ => {
            let Some(s) = session else { return };
            let mut st = state.lock().unwrap();
            let Some(tab) = st.tab_by_session(s) else { return };
            on_page_event(ev, tab);
        }
    }
}

fn unique_path(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..).map(|i| p.with_file_name(format!("{stem} ({i}){ext}"))).find(|c| !c.exists()).unwrap()
}

async fn fulfill_route(cdp: &Cdp, session: &str, request_id: Value, r: &Route, req_headers: &Value) -> std::result::Result<Value, String> {
    if r.status.is_none() && r.body.is_none() {
        // Header-only route: modify the outgoing request.
        let mut headers: Vec<Value> = req_headers
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| !r.remove_headers.iter().any(|h| h.eq_ignore_ascii_case(k)))
                    .map(|(k, v)| json!({ "name": k, "value": v }))
                    .collect()
            })
            .unwrap_or_default();
        for (k, v) in &r.headers {
            headers.retain(|h| !h["name"].as_str().unwrap_or("").eq_ignore_ascii_case(k));
            headers.push(json!({ "name": k, "value": v }));
        }
        return cdp.send_session(session, "Fetch.continueRequest", json!({ "requestId": request_id, "headers": headers })).await;
    }
    use base64::Engine as _;
    let mut headers: Vec<Value> = r.headers.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect();
    let body = r.body.clone().unwrap_or_default();
    let content_type = r.content_type.clone().unwrap_or_else(|| {
        if serde_json::from_str::<Value>(&body).is_ok_and(|v| v.is_object() || v.is_array()) { "application/json".into() } else { "text/plain".into() }
    });
    headers.push(json!({ "name": "Content-Type", "value": content_type }));
    headers.push(json!({ "name": "Access-Control-Allow-Origin", "value": "*" }));
    cdp.send_session(
        session,
        "Fetch.fulfillRequest",
        json!({
            "requestId": request_id, "responseCode": r.status.unwrap_or(200), "responseHeaders": headers,
            "body": base64::engine::general_purpose::STANDARD.encode(body),
        }),
    )
    .await
}

fn on_page_event(ev: &Event, tab: &mut Tab) {
    let p = &ev.params;
    match ev.method.as_str() {
        "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
            let f = &p["frame"];
            tab.main_frame = f["id"].as_str().unwrap_or_default().into();
            tab.url = f["url"].as_str().unwrap_or_default().to_string() + f["urlFragment"].as_str().unwrap_or("");
            tab.loader_id = f["loaderId"].as_str().unwrap_or_default().into();
            tab.console_nav_start = tab.console.len();
            tab.console_reported = tab.console.len();
            tab.last_snapshot = None;
        }
        "Page.navigatedWithinDocument" if p["frameId"] == tab.main_frame.as_str() => {
            tab.url = p["url"].as_str().unwrap_or_default().into();
        }
        "Page.frameRequestedNavigation" | "Page.frameStartedNavigating" if p["frameId"] == tab.main_frame.as_str() => {
            tab.nav_seq += 1;
        }
        "Page.frameStartedLoading" if p["frameId"] == tab.main_frame.as_str() || tab.main_frame.is_empty() => {
            tab.nav_seq += 1;
            tab.loading.send_replace(true);
        }
        "Page.frameStoppedLoading" if p["frameId"] == tab.main_frame.as_str() => {
            tab.loading.send_replace(false);
        }
        "Page.lifecycleEvent" if p["frameId"] == tab.main_frame.as_str() && p["name"] == "load" => {
            tab.loading.send_replace(false);
        }
        "Page.javascriptDialogOpening" => {
            tab.dialog = Some(Dialog {
                kind: p["type"].as_str().unwrap_or_default().into(),
                message: p["message"].as_str().unwrap_or_default().into(),
                default_prompt: p["defaultPrompt"].as_str().unwrap_or_default().into(),
            });
        }
        "Page.javascriptDialogClosed" => tab.dialog = None,
        "Page.fileChooserOpened" => {
            tab.file_chooser = Some((p["backendNodeId"].as_i64().unwrap_or(0), p["mode"].as_str().unwrap_or("selectSingle").into()));
        }
        "Runtime.executionContextCreated" => {
            let c = &p["context"];
            if c["name"] == WORLD && c["auxData"]["frameId"] == tab.main_frame.as_str() {
                tab.world_ctx = c["id"].as_i64();
            }
        }
        "Runtime.executionContextDestroyed" => {
            if tab.world_ctx.is_some() && p["executionContextId"].as_i64() == tab.world_ctx {
                tab.world_ctx = None;
            }
        }
        "Runtime.executionContextsCleared" => tab.world_ctx = None,
        "Runtime.consoleAPICalled" => {
            let args = p["args"].as_array().map(|a| a.iter().map(format_remote).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            let frame = &p["stackTrace"]["callFrames"][0];
            let location = frame["url"].as_str().filter(|u| !u.is_empty()).map(|u| format!("{u}:{}", frame["lineNumber"].as_i64().unwrap_or(0) + 1)).unwrap_or_default();
            tab.console.push(ConsoleMsg { level: level_of_console(p["type"].as_str().unwrap_or("log")), text: args, location });
        }
        "Runtime.exceptionThrown" => {
            let d = &p["exceptionDetails"];
            let text = d["exception"]["description"].as_str().or_else(|| d["text"].as_str()).unwrap_or("Uncaught exception").to_string();
            tab.console.push(ConsoleMsg { level: "error", text, location: d["url"].as_str().unwrap_or_default().into() });
        }
        "Log.entryAdded" => {
            let e = &p["entry"];
            tab.console.push(ConsoleMsg {
                level: level_of_console(e["level"].as_str().unwrap_or("info")),
                text: e["text"].as_str().unwrap_or_default().into(),
                location: e["url"].as_str().unwrap_or_default().into(),
            });
        }
        "Network.requestWillBeSent" => {
            let id = p["requestId"].as_str().unwrap_or_default().to_string();
            if let (Some(redirect), Some(&i)) = (p.get("redirectResponse"), tab.request_index.get(&id)) {
                let r = &mut tab.requests[i];
                r.status = redirect["status"].as_i64();
                r.status_text = redirect["statusText"].as_str().unwrap_or_default().into();
                r.response_headers = redirect["headers"].clone();
                r.finished = true;
            }
            if p["type"] == "Document" && p["frameId"] == tab.main_frame.as_str() && p.get("redirectResponse").is_none() {
                tab.requests_nav_start = tab.requests.len();
            }
            let req = &p["request"];
            tab.request_index.insert(id.clone(), tab.requests.len());
            tab.inflight.insert(id.clone());
            tab.requests.push(Request {
                id,
                method: req["method"].as_str().unwrap_or("GET").into(),
                url: req["url"].as_str().unwrap_or_default().to_string() + req["urlFragment"].as_str().unwrap_or(""),
                resource_type: p["type"].as_str().unwrap_or("Other").into(),
                request_headers: req["headers"].clone(),
                post_data: req["postData"].as_str().map(str::to_string),
                ..Default::default()
            });
            // Keep memory bounded on long-running pages.
            if tab.requests.len() > 5000 {
                tab.requests.drain(..1000);
                tab.requests_nav_start = tab.requests_nav_start.saturating_sub(1000);
                tab.request_index = tab.requests.iter().enumerate().map(|(i, r)| (r.id.clone(), i)).collect();
            }
        }
        "Network.responseReceived" => {
            if let Some(&i) = p["requestId"].as_str().and_then(|id| tab.request_index.get(id)) {
                let r = &mut tab.requests[i];
                let resp = &p["response"];
                r.status = resp["status"].as_i64();
                r.status_text = resp["statusText"].as_str().unwrap_or_default().into();
                r.response_headers = resp["headers"].clone();
                r.mime = resp["mimeType"].as_str().unwrap_or_default().into();
            }
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            let id = p["requestId"].as_str().unwrap_or_default();
            tab.inflight.remove(id);
            if let Some(&i) = tab.request_index.get(id) {
                let r = &mut tab.requests[i];
                r.finished = true;
                if ev.method == "Network.loadingFailed" {
                    r.failure = Some(p["errorText"].as_str().unwrap_or("failed").into());
                }
            }
        }
        _ => {}
    }
}

async fn init_session(cdp: Arc<Cdp>, state: Arc<Mutex<State>>, session: String, cfg: Config) {
    let s = session.as_str();
    let (routes_active, offline, viewport) = {
        let st = state.lock().unwrap();
        (!st.routes.is_empty() || !cfg.allowed_origins.is_empty() || !cfg.blocked_origins.is_empty(), st.offline, st.viewport)
    };
    let mut calls = vec![
        cdp.send_session(s, "Page.enable", json!({})),
        cdp.send_session(s, "Page.setLifecycleEventsEnabled", json!({ "enabled": true })),
        cdp.send_session(s, "Runtime.enable", json!({})),
        cdp.send_session(s, "Network.enable", json!({ "maxPostDataSize": 65536 })),
        cdp.send_session(s, "Log.enable", json!({})),
        cdp.send_session(s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": INJECTED, "worldName": WORLD, "runImmediately": true })),
        cdp.send_session(s, "Runtime.addBinding", json!({ "name": "__bmcpHandoff", "executionContextName": WORLD })),
        cdp.send_session(s, "Runtime.addBinding", json!({ "name": "__bmcpRecord", "executionContextName": WORLD })),
        cdp.send_session(s, "Page.setInterceptFileChooserDialog", json!({ "enabled": true })),
    ];
    for script in &cfg.init_scripts {
        calls.push(cdp.send_session(s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": script, "runImmediately": true })));
    }
    if let Some((w, h)) = viewport {
        calls.push(cdp.send_session(s, "Emulation.setDeviceMetricsOverride", json!({ "width": w, "height": h, "deviceScaleFactor": 0, "mobile": false })));
    }
    if cfg.block_service_workers {
        calls.push(cdp.send_session(s, "Network.setBypassServiceWorker", json!({ "bypass": true })));
    }
    if routes_active {
        calls.push(cdp.send_session(s, "Fetch.enable", json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] })));
    }
    if offline {
        calls.push(cdp.send_session(
            s,
            "Network.emulateNetworkConditions",
            json!({ "offline": true, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }),
        ));
    }
    futures_util::future::join_all(calls).await;
    let tree = cdp.send_session(s, "Page.getFrameTree", json!({})).await.ok();
    let _ = cdp.send_session(s, "Runtime.runIfWaitingForDebugger", json!({})).await;
    let mut st = state.lock().unwrap();
    if let Some(tab) = st.tab_by_session(s) {
        if let Some(tree) = tree {
            let f = &tree["frameTree"]["frame"];
            if tab.main_frame.is_empty() {
                tab.main_frame = f["id"].as_str().unwrap_or_default().into();
            }
            if tab.loader_id.is_empty() {
                tab.loader_id = f["loaderId"].as_str().unwrap_or_default().into();
            }
        }
        tab.ready.send_replace(true);
    }
}
