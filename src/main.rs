//! browser-mcp-rs: a Rust MCP server driving a bundled Chromium over the DevTools Protocol, with
//! the @playwright/mcp tool set. Headless by default; switches to a visible window when a human
//! has to take over (browser_handoff) and back again afterwards.

mod actions;
mod browser;
mod cdp;
mod install;
mod keys;
mod liveview;
mod response;
mod tools;

use anyhow::{anyhow, bail, Context, Result};
use browser::{Browser, Config};
use rmcp::{transport::stdio, ServiceExt};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
browser-mcp-rs — MCP browser automation on a bundled Chromium

USAGE:
    browser-mcp-rs [OPTIONS]        run the MCP server on stdio
    browser-mcp-rs install          download the bundled Chromium and exit

OPTIONS (same names as @playwright/mcp):
    --headless / --headed           start headless (default) or with a visible window
    --caps <list>                   extra tools: vision,pdf,network,storage,devtools,testing,config or all
    --executable-path <path>        use this Chromium/Chrome instead of the bundled one
    --user-data-dir <path>          persistent profile directory (default: <cache>/profile)
    --isolated                      throwaway profile, deleted on exit
    --storage-state <path>          load cookies/localStorage from a Playwright storage-state file
    --viewport-size <WxH>           e.g. 1280x720
    --user-agent <ua>
    --proxy-server <url>            --proxy-bypass <domains>
    --ignore-https-errors
    --block-service-workers
    --allowed-origins <a;b>         --blocked-origins <a;b>
    --init-script <path>            script added to every page (repeatable)
    --output-dir <path>             where screenshots, PDFs, traces and downloads go
    --snapshot-mode <mode>          incremental (default), full or none
    --snapshot-max-chars <n>        longer snapshots are saved to a file and truncated (default 50000, 0 = no limit)
    --timeout-action <ms>           default 5000
    --timeout-navigation <ms>       default 60000
    --timeout-settle <ms>           default 500
    --test-id-attribute <name>      default data-testid
";

fn parse_args() -> Result<Option<Config>> {
    let mut args = std::env::args().skip(1).peekable();
    let mut cfg = Config {
        executable: std::env::var_os("BROWSER_MCP_EXECUTABLE").map(PathBuf::from),
        headless: true,
        user_data_dir: None,
        isolated: false,
        viewport: None,
        user_agent: None,
        proxy_server: None,
        proxy_bypass: None,
        ignore_https_errors: false,
        block_service_workers: false,
        init_scripts: Vec::new(),
        allowed_origins: Vec::new(),
        blocked_origins: Vec::new(),
        output_dir: std::env::temp_dir().join("browser-mcp-rs"),
        timeout_action: Duration::from_millis(5000),
        timeout_navigation: Duration::from_millis(60000),
        timeout_settle: Duration::from_millis(500),
        test_id_attribute: "data-testid".into(),
        storage_state: None,
        snapshot_mode: "incremental".into(),
        snapshot_max_chars: 50_000,
        caps: HashSet::new(),
    };
    let list = |s: String| s.split([';', ',']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String> { inline.clone().or_else(|| args.next()).ok_or_else(|| anyhow!("{name} needs a value")) };
        let ms = |v: String| -> Result<Duration> { Ok(Duration::from_millis(v.parse().with_context(|| format!("{name}: not a number"))?)) };
        match name.as_str() {
            "install" => {
                for product in [install::Product::HeadlessShell, install::Product::Chrome] {
                    let path = install::ensure(product).await_blocking()?;
                    println!("{product:?} {} installed at {}", install::CHROME_VERSION, path.display());
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("browser-mcp-rs {} (Chromium {})", env!("CARGO_PKG_VERSION"), install::CHROME_VERSION);
                return Ok(None);
            }
            "--headless" => cfg.headless = true,
            "--headed" => cfg.headless = false,
            "--caps" | "--capabilities" => cfg.caps.extend(list(value()?)),
            "--executable-path" => cfg.executable = Some(value()?.into()),
            "--user-data-dir" => cfg.user_data_dir = Some(value()?.into()),
            "--isolated" => cfg.isolated = true,
            "--storage-state" => cfg.storage_state = Some(value()?.into()),
            "--viewport-size" => {
                let v = value()?;
                let (w, h) = v.split_once(['x', ',']).ok_or_else(|| anyhow!("--viewport-size must look like 1280x720"))?;
                cfg.viewport = Some((w.trim().parse()?, h.trim().parse()?));
            }
            "--user-agent" => cfg.user_agent = Some(value()?),
            "--proxy-server" => cfg.proxy_server = Some(value()?),
            "--proxy-bypass" => cfg.proxy_bypass = Some(value()?),
            "--ignore-https-errors" => cfg.ignore_https_errors = true,
            "--block-service-workers" => cfg.block_service_workers = true,
            "--allowed-origins" => cfg.allowed_origins = list(value()?),
            "--blocked-origins" => cfg.blocked_origins = list(value()?),
            "--init-script" => {
                let path = value()?;
                cfg.init_scripts.push(std::fs::read_to_string(&path).with_context(|| format!("reading --init-script {path}"))?);
            }
            "--output-dir" => cfg.output_dir = value()?.into(),
            "--snapshot-mode" => {
                let m = value()?;
                if !["incremental", "full", "none"].contains(&m.as_str()) {
                    bail!("--snapshot-mode must be incremental, full or none");
                }
                cfg.snapshot_mode = m;
            }
            "--snapshot-max-chars" => cfg.snapshot_max_chars = value()?.parse().context("--snapshot-max-chars: not a number")?,
            "--timeout-action" => cfg.timeout_action = ms(value()?)?,
            "--timeout-navigation" => cfg.timeout_navigation = ms(value()?)?,
            "--timeout-settle" => cfg.timeout_settle = ms(value()?)?,
            "--test-id-attribute" => cfg.test_id_attribute = value()?,
            "--browser" => {
                let b = value()?;
                if !["chromium", "chrome"].contains(&b.as_str()) {
                    bail!("only chromium is supported");
                }
            }
            other => bail!("unknown option {other}; see --help"),
        }
    }
    Ok(Some(cfg))
}

/// Lets the synchronous argument parser run the async installer.
trait AwaitBlocking: std::future::Future {
    fn await_blocking(self) -> Self::Output;
}
impl<F: std::future::Future> AwaitBlocking for F {
    fn await_blocking(self) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(self))
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--watchdog") {
        watchdog(args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0));
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

/// Runs as a separate tiny process next to every Chromium. Its stdin is a pipe held open by the
/// server; when the server dies for any reason, even SIGKILL, the pipe hits EOF and the watchdog
/// takes Chromium's whole process group down with it, since Chromium itself keeps running when its
/// DevTools pipe closes.
fn watchdog(pgid: i32) {
    use std::io::Read;
    if pgid <= 1 {
        return;
    }
    let mut buf = [0u8; 64];
    let mut stdin = std::io::stdin();
    while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if unsafe { libc::kill(-pgid, 0) } != 0 {
            return;
        }
    }
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

async fn run() -> Result<()> {
    let Some(cfg) = parse_args()? else { return Ok(()) };
    let browser = Browser::new(cfg);
    let server = tools::BrowserServer::new(browser.clone());
    let service = server.serve(stdio()).await?;
    tokio::select! {
        _ = service.waiting() => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => { s.recv().await; }
                Err(_) => std::future::pending::<()>().await,
            }
        } => {}
    }
    let _ = browser.close().await;
    Ok(())
}
