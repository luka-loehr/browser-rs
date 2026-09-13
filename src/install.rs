//! The bundled Chromium: pinned Chrome for Testing builds, downloaded once into the user cache and
//! shared by every session. Two builds of the same version:
//! - `chrome-headless-shell` runs headless mode. It has no browser UI and no GPU process, so it
//!   starts faster and uses much less memory.
//! - `chrome` (the full browser) runs headed mode, and is only downloaded the first time a visible
//!   window is needed. Same version, so both open the same profile.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const CHROME_VERSION: &str = "153.0.8010.36";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Product {
    HeadlessShell,
    Chrome,
}

impl Product {
    fn name(self) -> &'static str {
        match self {
            Product::HeadlessShell => "chrome-headless-shell",
            Product::Chrome => "chrome",
        }
    }
}

fn platform() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linux-arm64",
        (os, arch) => bail!("no bundled Chromium for {os}/{arch}; pass --executable-path"),
    })
}

pub fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("BROWSER_MCP_CACHE") {
        return PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/browser-mcp-rs")
    } else if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("browser-mcp-rs")
    } else {
        home.join(".cache/browser-mcp-rs")
    }
}

fn version_dir() -> PathBuf {
    cache_root().join("chromium").join(CHROME_VERSION)
}

/// Name of the archive's top-level folder, e.g. `chrome-headless-shell-mac-arm64`.
fn folder(product: Product, platform: &str) -> String {
    format!("{}-{platform}", product.name())
}

fn executable_in(root: &Path, product: Product, platform: &str) -> PathBuf {
    let dir = root.join(folder(product, platform));
    match product {
        Product::HeadlessShell => dir.join("chrome-headless-shell"),
        Product::Chrome if platform.starts_with("mac") => {
            dir.join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
        }
        Product::Chrome => dir.join("chrome"),
    }
}

/// The bundled build of `product`, downloading it first if this machine does not have it yet.
pub async fn ensure(product: Product) -> Result<PathBuf> {
    let platform = platform()?;
    let dir = version_dir();
    let exe = executable_in(&dir, product, platform);
    if exe.exists() {
        return Ok(exe);
    }

    let top = folder(product, platform);
    let url = format!("https://storage.googleapis.com/chrome-for-testing-public/{CHROME_VERSION}/{platform}/{top}.zip");
    eprintln!("browser-mcp-rs: downloading {} {CHROME_VERSION} once into {}", product.name(), dir.display());
    std::fs::create_dir_all(&dir)?;
    // Unique staging paths so two sessions installing at once never see each other's half-written files.
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    );
    let zip_path = dir.join(format!(".download-{stamp}.zip"));
    let staging = dir.join(format!(".staging-{stamp}"));

    let result = async {
        download(&url, &zip_path).await?;
        std::fs::create_dir_all(&staging)?;
        extract(&zip_path, &staging)?;
        if !executable_in(&staging, product, platform).exists() {
            bail!("archive did not contain the expected executable");
        }
        if std::fs::rename(staging.join(&top), dir.join(&top)).is_err() && !exe.exists() {
            bail!("could not move {} into {}", product.name(), dir.display());
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&zip_path);
    let _ = std::fs::remove_dir_all(&staging);
    result?;
    eprintln!("browser-mcp-rs: {} installed", product.name());
    Ok(exe)
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    let resp = reqwest::get(url).await?.error_for_status().with_context(|| format!("GET {url}"))?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let (mut done, mut last_pct) = (0u64, 0u64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        done += chunk.len() as u64;
        if total > 0 {
            let pct = done * 100 / total;
            if pct >= last_pct + 10 {
                last_pct = pct;
                eprintln!("browser-mcp-rs: {pct}% of {} MB", total / 1_000_000);
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(())
}

/// The macOS archive holds an app bundle full of framework symlinks, which `ditto` preserves
/// exactly; `unzip` does the same on Linux.
fn extract(zip: &Path, dest: &Path) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        Command::new("ditto").arg("-x").arg("-k").arg(zip).arg(dest).status()
    } else {
        Command::new("unzip").arg("-q").arg(zip).arg("-d").arg(dest).status()
    }
    .map_err(|e| anyhow!("could not run the archive extractor: {e}"))?;
    if !status.success() {
        bail!("extracting {} failed ({status})", zip.display());
    }
    Ok(())
}
