//! Tool replies in Playwright MCP's markdown shape (Result / Page / Modal state / Snapshot),
//! with one difference that saves most of the tokens: after the first snapshot of a page,
//! actions return only the parts of the accessibility tree that changed.

use crate::browser::Browser;
use base64::Engine as _;
use rmcp::model::{CallToolResult, ContentBlock};

#[derive(Default)]
pub struct Reply {
    pub result: Vec<String>,
    pub images: Vec<(Vec<u8>, &'static str)>,
    pub snapshot: bool,
    pub page_state: bool,
    pub error: bool,
}

impl Reply {
    pub fn text(s: impl Into<String>) -> Self {
        Self { result: vec![s.into()], page_state: true, ..Default::default() }
    }

    /// Replies for actions that change the page carry a snapshot.
    pub fn action(s: impl Into<String>) -> Self {
        Self { result: vec![s.into()], page_state: true, snapshot: true, ..Default::default() }
    }

    pub fn raw(s: impl Into<String>) -> Self {
        Self { result: vec![s.into()], ..Default::default() }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self { result: vec![s.into()], error: true, page_state: true, ..Default::default() }
    }
}

const MAX_CHANGED_RATIO: f64 = 0.6;

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Lines of `new` that changed relative to `old`, with their ancestor lines for context.
/// Returns None when so much changed that the full snapshot is the better reply.
fn snapshot_delta(old: &str, new: &str) -> Option<String> {
    let new_lines: Vec<&str> = new.lines().collect();
    let diff = similar::TextDiff::from_lines(old, new);
    let mut changed = vec![false; new_lines.len()];
    let mut removed = 0usize;
    for op in diff.ops() {
        match op.tag() {
            similar::DiffTag::Equal => {}
            similar::DiffTag::Delete => removed += op.old_range().len(),
            similar::DiffTag::Insert | similar::DiffTag::Replace => {
                for i in op.new_range() {
                    changed[i] = true;
                }
                if op.tag() == similar::DiffTag::Replace {
                    removed += op.old_range().len().saturating_sub(op.new_range().len());
                }
            }
        }
    }
    let n_changed = changed.iter().filter(|c| **c).count();
    if n_changed == 0 && removed == 0 {
        return Some(String::new());
    }
    if new_lines.is_empty() || (n_changed as f64) > MAX_CHANGED_RATIO * new_lines.len() as f64 {
        return None;
    }
    let mut keep = changed.clone();
    for i in 0..new_lines.len() {
        if !changed[i] {
            continue;
        }
        let mut level = indent(new_lines[i]);
        for j in (0..i).rev() {
            if level == 0 {
                break;
            }
            if indent(new_lines[j]) < level {
                keep[j] = true;
                level = indent(new_lines[j]);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (i, l) in new_lines.iter().enumerate() {
        if keep[i] {
            out.push(if changed[i] { l.to_string() } else { format!("{l} # unchanged") });
        }
    }
    if removed > 0 {
        out.push(format!("# {removed} line(s) removed"));
    }
    Some(out.join("\n"))
}

impl Browser {
    /// A full snapshot as a yaml block, cut at a line boundary past --snapshot-max-chars with the
    /// complete snapshot saved to a file, so one huge page cannot flood the agent's context.
    pub fn snapshot_block(&self, snap: &str) -> String {
        self.snapshot_block_capped(snap, self.cfg.snapshot_max_chars)
    }

    /// Action replies carry at most this much snapshot inline; browser_snapshot gets the full limit.
    const ACTION_SNAPSHOT_CHARS: usize = 8_000;

    fn snapshot_block_capped(&self, snap: &str, max: usize) -> String {
        if max == 0 || snap.len() <= max {
            return format!("```yaml\n{snap}\n```");
        }
        let mut cut = max;
        while !snap.is_char_boundary(cut) {
            cut -= 1;
        }
        let head = &snap[..snap[..cut].rfind('\n').unwrap_or(cut)];
        let saved = self
            .output_path(None, "snapshot", "yaml")
            .and_then(|p| std::fs::write(&p, snap).map(|_| p).map_err(Into::into))
            .map(|p| format!("the full snapshot is in {}", p.display()))
            .unwrap_or_else(|e| format!("saving the full snapshot failed: {e}"));
        format!(
            "```yaml\n{head}\n```\nTruncated: showing {} of {} chars; {saved}. Use browser_find to locate elements, or browser_snapshot with target/depth for one part of the page.",
            head.len(),
            snap.len()
        )
    }

    pub async fn finish(&self, reply: Reply) -> CallToolResult {
        let mut out = String::new();
        if !reply.result.is_empty() {
            out.push_str(if reply.error { "### Error\n" } else { "### Result\n" });
            out.push_str(&reply.result.join("\n"));
            out.push('\n');
        }

        if reply.page_state && self.is_running() {
            if let Ok(page) = self.page().await {
                // Target title events lag behind the document; read the title directly.
                if self.tab_read(&page, |t| t.dialog.is_none()).unwrap_or(false) {
                    if let Ok(title) = self.eval_world(&page, "document.title").await {
                        if let Some(t) = title.as_str() {
                            let t = t.to_string();
                            let _ = self.tab_write(&page, |tab| tab.title = t);
                        }
                    }
                }
                let mut snapshot_text = None;
                // Per-call `snapshot` argument, else the --snapshot-mode default.
                let requested = self.overrides.lock().unwrap().snapshot.clone().unwrap_or_else(|| self.cfg.snapshot_mode.clone());
                let mode = match requested.as_str() {
                    "none" | "off" | "false" => "none",
                    "full" => "full",
                    "main" => "main",
                    _ => "diff",
                };
                if reply.snapshot && !reply.error && mode != "none" {
                    let has_dialog = self.tab_read(&page, |t| t.dialog.is_some()).unwrap_or(false);
                    if !has_dialog {
                        let snap = if mode == "main" {
                            match self.snapshot(&page, Some("main, [role=main], article"), None, false).await {
                                Ok(s) => Ok(s),
                                Err(_) => self.snapshot(&page, None, None, false).await,
                            }
                        } else {
                            self.snapshot(&page, None, None, false).await
                        };
                        if let Err(e) = &snap {
                            snapshot_text = Some(format!("### Snapshot\nCould not capture a snapshot: {e:#}\n"));
                        }
                        if let Ok(snap) = snap {
                            let prev = self.tab_read(&page, |t| t.last_snapshot.clone()).ok().flatten();
                            let delta = match (&prev, mode) {
                                (Some(prev), "diff") => snapshot_delta(prev, &snap),
                                _ => None,
                            };
                            snapshot_text = Some(match delta {
                                Some(d) if d.is_empty() => "### Snapshot\nNo changes since the last snapshot.\n".to_string(),
                                Some(d) => format!("### Snapshot (changes)\n```yaml\n{d}\n```\n"),
                                None => {
                                    let max = match (mode, self.cfg.snapshot_max_chars) {
                                        ("full", n) => n,
                                        (_, 0) => Self::ACTION_SNAPSHOT_CHARS,
                                        (_, n) => n.min(Self::ACTION_SNAPSHOT_CHARS),
                                    };
                                    format!("### Snapshot\n{}\n", self.snapshot_block_capped(&snap, max))
                                }
                            });
                            // A main-only snapshot is partial, so it must not become the diff baseline.
                            if mode != "main" {
                                let _ = self.tab_write(&page, |t| t.last_snapshot = Some(snap));
                            }
                        }
                    }
                }

                let (url, title, console, dialog, chooser, tabs, current) = {
                    let mut st = self.state.lock().unwrap();
                    let tabs = st.tabs.len();
                    let current = st.current;
                    let notices: Vec<String> = st.notices.drain(..).collect();
                    let Some(tab) = st.tabs.iter_mut().find(|t| t.session_id == page.session) else {
                        return CallToolResult::success(vec![ContentBlock::text(out)]);
                    };
                    let fresh: Vec<String> = tab.console[tab.console_reported.min(tab.console.len())..]
                        .iter()
                        // Failed resource loads are already in the network log; in every reply they are just noise.
                        .filter(|m| (m.level == "error" || m.level == "warning") && !m.text.starts_with("Failed to load resource"))
                        .map(|m| {
                            let line: String = m.text.lines().next().unwrap_or_default().chars().take(160).collect();
                            format!("- [{}] {line}", m.level.to_uppercase())
                        })
                        .collect();
                    tab.console_reported = tab.console.len();
                    if !notices.is_empty() {
                        out.push_str("### Events\n");
                        for n in notices {
                            out.push_str(&format!("- {n}\n"));
                        }
                    }
                    (tab.url.clone(), tab.title.clone(), fresh, tab.dialog.clone(), tab.file_chooser.clone(), tabs, current)
                };

                out.push_str(&format!("### Page\n- Page URL: {url}\n- Page Title: {title}\n"));
                if tabs > 1 {
                    out.push_str(&format!("- Tabs: {tabs} open, current is {current}\n"));
                }
                // Counts only: the messages themselves are rarely what the agent is after, and
                // browser_console_messages has them in full.
                if !console.is_empty() {
                    let errors = console.iter().filter(|l| l.starts_with("- [ERROR]")).count();
                    out.push_str(&format!(
                        "- Console: {errors} new error(s), {} warning(s) (browser_console_messages)\n",
                        console.len() - errors
                    ));
                }
                if let Some(d) = dialog {
                    out.push_str(&format!(
                        "### Modal state\n- [\"{}\" dialog with message \"{}\"]: can be handled by browser_handle_dialog\n",
                        d.kind, d.message
                    ));
                } else if chooser.is_some() {
                    out.push_str("### Modal state\n- [File chooser]: can be handled by browser_file_upload\n");
                }
                if let Some(s) = snapshot_text {
                    out.push_str(&s);
                }
            }
        }

        let mut content = vec![ContentBlock::text(out.trim_end().to_string())];
        for (data, mime) in reply.images {
            content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(data), mime));
        }
        if reply.error { CallToolResult::error(content) } else { CallToolResult::success(content) }
    }
}
