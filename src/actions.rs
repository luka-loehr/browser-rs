//! Everything a tool does to a page, as methods on `Browser`: trusted input through
//! `Input.dispatch*`, element resolution and actionability through the injected runtime, and
//! the capture/storage/network features built on plain CDP domains.

use crate::browser::{Browser, PageRef, Route, Video};
use crate::keys;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub struct Shot {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub path: PathBuf,
}

fn modifier_mask(mods: &[String]) -> Result<(i64, Vec<String>)> {
    let mut mask = 0;
    let mut names = Vec::new();
    for m in mods {
        let combo = keys::parse(&format!("{m}+a")).map_err(|e| anyhow!(e))?;
        mask |= combo.mask;
        names.extend(combo.modifiers.into_iter().map(|k| k.key));
    }
    Ok((mask, names))
}

fn button_mask(button: &str) -> i64 {
    match button {
        "left" => 1,
        "right" => 2,
        "middle" => 4,
        _ => 0,
    }
}

impl Browser {
    // ------------------------------------------------------------------ low-level input

    /// Input events block until the page handles them; if handling opens a dialog, the event
    /// has done its job, so a dialog is a success here rather than an error.
    async fn input(&self, page: &PageRef, method: &str, params: Value) -> Result<()> {
        match self.send_page(page, method, params, self.cfg.timeout_navigation).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("dialog") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn mouse_move(&self, page: &PageRef, x: f64, y: f64) -> Result<()> {
        // Moves must report held buttons, or Chrome never starts a drag.
        let buttons = self.state.lock().unwrap().buttons;
        let button = if buttons & 1 != 0 { "left" } else if buttons & 2 != 0 { "right" } else if buttons & 4 != 0 { "middle" } else { "none" };
        self.input(page, "Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y, "button": button, "buttons": buttons })).await?;
        self.state.lock().unwrap().mouse = (x, y);
        Ok(())
    }

    pub async fn mouse_button(&self, page: &PageRef, down: bool, button: &str, count: i64, modifiers: i64) -> Result<()> {
        let (x, y) = self.state.lock().unwrap().mouse;
        let kind = if down { "mousePressed" } else { "mouseReleased" };
        let buttons = {
            let mut st = self.state.lock().unwrap();
            if down {
                st.buttons |= button_mask(button);
            } else {
                st.buttons &= !button_mask(button);
            }
            st.buttons
        };
        self.input(
            page,
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": count, "modifiers": modifiers }),
        )
        .await
    }

    pub async fn click_xy(&self, page: &PageRef, x: f64, y: f64, button: &str, count: i64, modifiers: i64, delay_ms: u64) -> Result<()> {
        self.mouse_move(page, x, y).await?;
        for i in 1..=count.max(1) {
            self.mouse_button(page, true, button, i, modifiers).await?;
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            self.mouse_button(page, false, button, i, modifiers).await?;
        }
        Ok(())
    }

    pub async fn wheel(&self, page: &PageRef, dx: f64, dy: f64) -> Result<()> {
        let (x, y) = self.state.lock().unwrap().mouse;
        self.input(page, "Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy })).await
    }

    async fn key_event(&self, page: &PageRef, down: bool, key: &keys::KeyDef, mask: i64, commands: Vec<&str>) -> Result<()> {
        let mut p = json!({
            "type": if down { if key.text.is_some() { "keyDown" } else { "rawKeyDown" } } else { "keyUp" },
            // nativeVirtualKeyCode is deliberately omitted: on macOS it is a Carbon key code, where
            // the Windows code 91 (Meta) means keypad 8 and would type "8"s.
            "key": key.key, "code": key.code, "windowsVirtualKeyCode": key.key_code,
            "modifiers": mask, "location": key.location,
        });
        if down {
            if let Some(t) = &key.text {
                p["text"] = json!(t);
                p["unmodifiedText"] = json!(t);
            }
            if !commands.is_empty() {
                p["commands"] = json!(commands);
            }
        }
        self.input(page, "Input.dispatchKeyEvent", p).await
    }

    pub async fn press(&self, page: &PageRef, combo: &str) -> Result<()> {
        let c = keys::parse(combo).map_err(|e| anyhow!(e))?;
        let mut mask = 0;
        for m in &c.modifiers {
            mask |= match m.key.as_str() {
                "Alt" => keys::ALT,
                "Control" => keys::CONTROL,
                "Meta" => keys::META,
                _ => keys::SHIFT,
            };
            self.key_event(page, true, m, mask, vec![]).await?;
        }
        let commands = keys::mac_commands(c.mask, &c.key.key);
        self.key_event(page, true, &c.key, c.mask, commands).await?;
        self.key_event(page, false, &c.key, c.mask, vec![]).await?;
        for m in c.modifiers.iter().rev() {
            mask &= !match m.key.as_str() {
                "Alt" => keys::ALT,
                "Control" => keys::CONTROL,
                "Meta" => keys::META,
                _ => keys::SHIFT,
            };
            self.key_event(page, false, m, mask, vec![]).await?;
        }
        Ok(())
    }

    async fn hold_modifiers(&self, page: &PageRef, names: &[String], down: bool) -> Result<()> {
        let list: Vec<&String> = if down { names.iter().collect() } else { names.iter().rev().collect() };
        for n in list {
            let k = keys::parse(n).map_err(|e| anyhow!(e))?.key;
            self.key_event(page, down, &k, 0, vec![]).await?;
        }
        Ok(())
    }

    pub async fn insert_text(&self, page: &PageRef, text: &str) -> Result<()> {
        self.input(page, "Input.insertText", json!({ "text": text })).await
    }

    pub async fn type_slowly(&self, page: &PageRef, text: &str) -> Result<()> {
        for ch in text.chars() {
            let k = keys::char_key(ch);
            if k.code.is_empty() && k.key_code == 0 {
                // Characters without a US-layout key (emoji, CJK...) go through the IME path.
                self.insert_text(page, &ch.to_string()).await?;
                continue;
            }
            let mask = if ch.is_ascii_uppercase() { keys::SHIFT } else { 0 };
            self.key_event(page, true, &k, mask, vec![]).await?;
            self.key_event(page, false, &k, mask, vec![]).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ elements

    pub async fn element(&self, page: &PageRef, target: &str) -> Result<String> {
        self.eval_world_handle(page, &format!("__bmcp.resolve({})", js(target))).await
    }

    async fn prepare(&self, page: &PageRef, obj: &str, enabled: bool, hit_test: bool) -> Result<(f64, f64)> {
        let opts = json!({ "timeout": self.cfg.timeout_action.as_millis() as u64, "enabled": enabled, "hitTest": hit_test });
        let v = self.call_on(page, obj, "function(o) { return __bmcp.prepare(this, o); }", vec![opts]).await?;
        Ok((v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0)))
    }

    async fn show_action(&self, page: &PageRef, obj: &str, label: &str) {
        let Some(ms) = self.state.lock().unwrap().show_actions else { return };
        let f = format!(
            "function() {{ __bmcp.highlight(this, 'outline-color:#0a84ff;background:rgba(10,132,255,.12)', 'action'); \
             setTimeout(() => __bmcp.removeHighlight('action'), {ms}); return {}; }}",
            js(label)
        );
        let _ = self.call_on(page, obj, &f, vec![]).await;
    }

    pub async fn click(&self, page: &PageRef, target: &str, double: bool, button: &str, modifiers: &[String]) -> Result<()> {
        let obj = self.element(page, target).await?;
        let (x, y) = self.prepare(page, &obj, true, true).await?;
        self.show_action(page, &obj, "click").await;
        let (mask, names) = modifier_mask(modifiers)?;
        self.hold_modifiers(page, &names, true).await?;
        let r = self.click_xy(page, x, y, button, if double { 2 } else { 1 }, mask, 0).await;
        self.hold_modifiers(page, &names, false).await?;
        r
    }

    pub async fn hover(&self, page: &PageRef, target: &str) -> Result<()> {
        let obj = self.element(page, target).await?;
        let (x, y) = self.prepare(page, &obj, false, true).await?;
        self.mouse_move(page, x, y).await
    }

    pub async fn fill(&self, page: &PageRef, target: &str, text: &str) -> Result<()> {
        let obj = self.element(page, target).await?;
        self.prepare(page, &obj, true, false).await?;
        self.show_action(page, &obj, "fill").await;
        let mode = self.call_on(page, &obj, "function(v) { return __bmcp.beginFill(this, v); }", vec![json!(text)]).await?;
        if mode == "insert" {
            self.insert_text(page, text).await?;
        }
        Ok(())
    }

    pub async fn type_into(&self, page: &PageRef, target: &str, text: &str, slowly: bool, submit: bool) -> Result<()> {
        if slowly {
            let obj = self.element(page, target).await?;
            self.prepare(page, &obj, true, false).await?;
            self.call_on(page, &obj, "function() { __bmcp.focusForTyping(this); }", vec![]).await?;
            self.type_slowly(page, text).await?;
        } else {
            self.fill(page, target, text).await?;
        }
        if submit {
            self.press(page, "Enter").await?;
        }
        Ok(())
    }

    pub async fn select_option(&self, page: &PageRef, target: &str, values: &[String]) -> Result<Value> {
        let obj = self.element(page, target).await?;
        self.prepare(page, &obj, true, false).await?;
        self.call_on(page, &obj, "function(v) { return __bmcp.selectOptions(this, v); }", vec![json!(values)]).await
    }

    pub async fn set_checked(&self, page: &PageRef, target: &str, checked: bool) -> Result<()> {
        let obj = self.element(page, target).await?;
        let state = self.call_on(page, &obj, "function() { return __bmcp.checkedState(this); }", vec![]).await?;
        if state.as_bool() == Some(checked) {
            return Ok(());
        }
        self.click(page, target, false, "left", &[]).await?;
        let after = self.call_on(page, &obj, "function() { return __bmcp.checkedState(this); }", vec![]).await?;
        if after.as_bool() != Some(checked) {
            bail!("clicking {target} did not {} it", if checked { "check" } else { "uncheck" });
        }
        Ok(())
    }

    pub async fn drag(&self, page: &PageRef, from: &str, to: &str) -> Result<()> {
        let src = self.element(page, from).await?;
        let (sx, sy) = self.prepare(page, &src, true, true).await?;
        self.mouse_move(page, sx, sy).await?;
        page.cdp.send_session(&page.session, "Input.setInterceptDrags", json!({ "enabled": true })).await.map_err(|e| anyhow!(e))?;
        let session = page.session.clone();
        let mut intercepted = page.cdp.wait_for(move |e| e.method == "Input.dragIntercepted" && e.session_id.as_deref() == Some(&session));
        let result = async {
            self.mouse_button(page, true, "left", 1, 0).await?;
            let dst = self.element(page, to).await?;
            let (dx, dy) = self.prepare(page, &dst, false, false).await?;
            let mut drag_data = None;
            for step in 1..=5 {
                let f = step as f64 / 5.0;
                self.mouse_move(page, sx + (dx - sx) * f, sy + (dy - sy) * f).await?;
                if drag_data.is_none() {
                    if let Ok(ev) = intercepted.try_recv() {
                        drag_data = Some(ev.params["data"].clone());
                    }
                }
            }
            if drag_data.is_none() {
                if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(50), &mut intercepted).await {
                    drag_data = Some(ev.params["data"].clone());
                }
            }
            if let Some(data) = drag_data {
                // HTML5 drag and drop: replay the intercepted drag onto the target.
                for kind in ["dragEnter", "dragOver", "drop"] {
                    self.input(page, "Input.dispatchDragEvent", json!({ "type": kind, "x": dx, "y": dy, "data": data })).await?;
                }
            }
            self.mouse_button(page, false, "left", 1, 0).await
        }
        .await;
        let _ = page.cdp.send_session(&page.session, "Input.setInterceptDrags", json!({ "enabled": false })).await;
        result
    }

    pub async fn drop_data(&self, page: &PageRef, target: &str, paths: &[String], data: &serde_json::Map<String, Value>) -> Result<()> {
        for p in paths {
            if !Path::new(p).is_file() {
                bail!("file not found: {p}");
            }
        }
        let obj = self.element(page, target).await?;
        let (x, y) = self.prepare(page, &obj, false, false).await?;
        let items: Vec<Value> = data.iter().map(|(mime, v)| json!({ "mimeType": mime, "data": v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()) })).collect();
        let drag = json!({ "items": items, "files": paths, "dragOperationsMask": 1 | 2 | 16 });
        for kind in ["dragEnter", "dragOver", "drop"] {
            self.input(page, "Input.dispatchDragEvent", json!({ "type": kind, "x": x, "y": y, "data": drag })).await?;
        }
        Ok(())
    }

    pub async fn upload(&self, page: &PageRef, paths: &[String]) -> Result<String> {
        let chooser = self.tab_write(page, |t| t.file_chooser.take())?;
        let Some((node, mode)) = chooser else {
            bail!("no file chooser is open; click the file input (or the button that opens it) first");
        };
        if paths.is_empty() {
            return Ok("file chooser cancelled".into());
        }
        if mode == "selectSingle" && paths.len() > 1 {
            bail!("this file input accepts a single file");
        }
        for p in paths {
            if !Path::new(p).is_file() {
                bail!("file not found: {p}");
            }
        }
        self.send_page(page, "DOM.setFileInputFiles", json!({ "files": paths, "backendNodeId": node }), self.cfg.timeout_action).await?;
        Ok(format!("uploaded {} file(s)", paths.len()))
    }

    pub async fn handle_dialog(&self, page: &PageRef, accept: bool, prompt: Option<&str>) -> Result<String> {
        let dialog = self.tab_read(page, |t| t.dialog.clone())?.ok_or_else(|| anyhow!("no dialog is open"))?;
        let mut p = json!({ "accept": accept });
        if let Some(t) = prompt {
            p["promptText"] = json!(t);
        } else if dialog.kind == "prompt" && accept {
            p["promptText"] = json!(dialog.default_prompt);
        }
        page.cdp.send_session(&page.session, "Page.handleJavaScriptDialog", p).await.map_err(|e| anyhow!(e))?;
        self.tab_write(page, |t| t.dialog = None)?;
        Ok(format!("{} \"{}\" dialog", if accept { "accepted" } else { "dismissed" }, dialog.kind))
    }

    /// Runs page JavaScript in the page's own world (so it sees the page's globals), optionally
    /// with a resolved element as the function's argument.
    pub async fn evaluate(&self, page: &PageRef, function: &str, target: Option<&str>) -> Result<Value> {
        let f = function.trim();
        // "() => x" and "function () {}" are called; anything else ("document.title",
        // "(() => 1)()") is evaluated as an expression.
        let fn_re = regex::Regex::new(r"^(async\s+)?(function\b|\([^()]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)").unwrap();
        let is_fn = fn_re.is_match(f) && !f.ends_with(")()");
        let res = match target {
            Some(t) => {
                let obj = self.element(page, t).await?;
                let desc = self.send_page(page, "DOM.describeNode", json!({ "objectId": obj }), self.cfg.timeout_action).await?;
                let backend = desc["node"]["backendNodeId"].clone();
                let main = self.send_page(page, "DOM.resolveNode", json!({ "backendNodeId": backend }), self.cfg.timeout_action).await?;
                let main_obj = main["object"]["objectId"].as_str().ok_or_else(|| anyhow!("could not resolve element in page world"))?.to_string();
                let decl = if is_fn { f.to_string() } else { format!("function() {{ return ({f}); }}") };
                self.send_page(
                    page,
                    "Runtime.callFunctionOn",
                    json!({ "functionDeclaration": decl, "objectId": main_obj, "arguments": [{ "objectId": main_obj }], "returnByValue": true, "awaitPromise": true, "userGesture": true }),
                    self.cfg.timeout_navigation,
                )
                .await?
            }
            None => {
                let expr = if is_fn { format!("({f})()") } else { f.to_string() };
                self.send_page(
                    page,
                    "Runtime.evaluate",
                    json!({ "expression": expr, "returnByValue": true, "awaitPromise": true, "userGesture": true, "replMode": true }),
                    self.cfg.timeout_navigation,
                )
                .await?
            }
        };
        if let Some(ex) = res.get("exceptionDetails") {
            let msg = ex["exception"]["description"].as_str().or_else(|| ex["text"].as_str()).unwrap_or("exception");
            bail!("{msg}");
        }
        let r = &res["result"];
        Ok(match r.get("value") {
            Some(v) => v.clone(),
            None => json!(r["unserializableValue"].as_str().or_else(|| r["description"].as_str()).unwrap_or("undefined")),
        })
    }

    // ------------------------------------------------------------------ reading

    pub async fn snapshot(&self, page: &PageRef, target: Option<&str>, depth: Option<u32>, boxes: bool) -> Result<String> {
        let opts = json!({ "target": target, "depth": depth, "boxes": boxes });
        let v = self.eval_world(page, &format!("__bmcp.snapshot({opts})")).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    pub async fn find(&self, page: &PageRef, text: Option<&str>, regex: Option<&str>) -> Result<String> {
        let snap = self.snapshot(page, None, None, false).await?;
        let re = match (text, regex) {
            (_, Some(r)) => regex::Regex::new(r).context("invalid regex")?,
            (Some(t), None) => regex::RegexBuilder::new(&regex::escape(t)).case_insensitive(true).build()?,
            _ => bail!("pass text or regex"),
        };
        let lines: Vec<&str> = snap.lines().collect();
        let indent = |l: &str| l.len() - l.trim_start().len();
        let mut keep = vec![false; lines.len()];
        let mut hits = 0;
        for (i, l) in lines.iter().enumerate() {
            if !re.is_match(l) {
                continue;
            }
            hits += 1;
            keep[i] = true;
            let mut level = indent(l);
            for j in (0..i).rev() {
                if indent(lines[j]) < level {
                    keep[j] = true;
                    level = indent(lines[j]);
                }
            }
        }
        if hits == 0 {
            return Ok("No matches".into());
        }
        let out: Vec<&str> = lines.iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| *l).collect();
        Ok(format!("{hits} match(es):\n```yaml\n{}\n```", out.join("\n")))
    }

    pub async fn wait_for(&self, page: &PageRef, time: Option<f64>, text: Option<&str>, gone: Option<&str>) -> Result<String> {
        if let Some(t) = time {
            tokio::time::sleep(Duration::from_secs_f64(t.clamp(0.0, 30.0))).await;
        }
        let timeout = self.cfg.timeout_navigation.as_millis().min(30_000);
        if let Some(t) = gone {
            self.eval_world(page, &format!("__bmcp.waitForText({}, true, {timeout})", js(t))).await?;
        }
        if let Some(t) = text {
            self.eval_world(page, &format!("__bmcp.waitForText({}, false, {timeout})", js(t))).await?;
        }
        Ok(match (time, text, gone) {
            (_, Some(t), _) => format!("Text \"{t}\" appeared"),
            (_, _, Some(t)) => format!("Text \"{t}\" disappeared"),
            (Some(s), _, _) => format!("Waited {s}s"),
            _ => bail!("pass time, text or textGone"),
        })
    }

    // ------------------------------------------------------------------ capture

    pub fn output_path(&self, filename: Option<&str>, default_stem: &str, ext: &str) -> Result<PathBuf> {
        let path = match filename {
            Some(f) if Path::new(f).is_absolute() => PathBuf::from(f),
            Some(f) => self.cfg.output_dir.join(f),
            None => {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
                self.cfg.output_dir.join(format!("{default_stem}-{ts}.{ext}"))
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(path)
    }

    pub async fn screenshot(&self, page: &PageRef, kind: &str, full_page: bool, target: Option<&str>, css_scale: bool, filename: Option<&str>) -> Result<Shot> {
        let (format, mime) = match kind {
            "jpeg" | "jpg" => ("jpeg", "image/jpeg"),
            "webp" => ("webp", "image/webp"),
            _ => ("png", "image/png"),
        };
        let metrics = self.send_page(page, "Page.getLayoutMetrics", json!({}), self.cfg.timeout_action).await?;
        let vv = &metrics["cssVisualViewport"];
        let dpr = {
            let v = self.eval_world(page, "devicePixelRatio").await.ok().and_then(|v| v.as_f64()).unwrap_or(1.0);
            if v > 0.0 { v } else { 1.0 }
        };
        let scale = if css_scale { 1.0 / dpr } else { 1.0 };
        let clip = if let Some(t) = target {
            let obj = self.element(page, t).await?;
            let r = self
                .call_on(
                    page,
                    &obj,
                    "function() { this.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' }); const r = this.getBoundingClientRect(); return { x: r.left + scrollX, y: r.top + scrollY, w: r.width, h: r.height }; }",
                    vec![],
                )
                .await?;
            if r["w"].as_f64().unwrap_or(0.0) <= 0.0 {
                bail!("element has no size; it cannot be captured");
            }
            json!({ "x": r["x"], "y": r["y"], "width": r["w"], "height": r["h"], "scale": scale })
        } else if full_page {
            let size = &metrics["cssContentSize"];
            json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": scale })
        } else {
            json!({ "x": vv["pageX"], "y": vv["pageY"], "width": vv["clientWidth"], "height": vv["clientHeight"], "scale": scale })
        };
        let mut params = json!({ "format": format, "clip": clip, "captureBeyondViewport": full_page || target.is_some(), "optimizeForSpeed": true });
        if format != "png" {
            params["quality"] = json!(80);
        }
        let res = self.send_page(page, "Page.captureScreenshot", params, self.cfg.timeout_navigation).await?;
        let data = B64.decode(res["data"].as_str().unwrap_or_default())?;
        let path = self.output_path(filename, "page", format)?;
        std::fs::write(&path, &data)?;
        Ok(Shot { data, mime, path })
    }

    pub async fn pdf(&self, page: &PageRef, filename: Option<&str>) -> Result<PathBuf> {
        let res = self
            .send_page(page, "Page.printToPDF", json!({ "printBackground": true, "preferCSSPageSize": true }), Duration::from_secs(120))
            .await?;
        let path = self.output_path(filename, "page", "pdf")?;
        std::fs::write(&path, B64.decode(res["data"].as_str().unwrap_or_default())?)?;
        Ok(path)
    }

    pub async fn resize(&self, page: &PageRef, width: u32, height: u32) -> Result<()> {
        self.state.lock().unwrap().viewport = Some((width, height));
        let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        for s in sessions {
            page.cdp
                .send_session(&s, "Emulation.setDeviceMetricsOverride", json!({ "width": width, "height": height, "deviceScaleFactor": 0, "mobile": false }))
                .await
                .map_err(|e| anyhow!(e))?;
        }
        if let Ok(win) = page.cdp.send("Browser.getWindowForTarget", json!({ "targetId": page.target_id })).await {
            let chrome_ui = if self.is_headless().await { 0 } else { 87 };
            let _ = page
                .cdp
                .send("Browser.setWindowBounds", json!({ "windowId": win["windowId"], "bounds": { "windowState": "normal" } }))
                .await;
            let _ = page
                .cdp
                .send("Browser.setWindowBounds", json!({ "windowId": win["windowId"], "bounds": { "width": width, "height": height + chrome_ui } }))
                .await;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ tabs

    pub async fn tabs_list(&self) -> String {
        let st = self.state.lock().unwrap();
        if st.tabs.is_empty() {
            return "No open tabs".into();
        }
        st.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| format!("- {i}:{} [{}]({})", if i == st.current { " (current)" } else { "" }, t.title, t.url))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn tab_select(&self, index: usize) -> Result<()> {
        let (session, len) = {
            let mut st = self.state.lock().unwrap();
            let len = st.tabs.len();
            if index >= len {
                bail!("tab {index} does not exist ({len} open)");
            }
            st.current = index;
            (st.tabs[index].session_id.clone(), len)
        };
        let _ = len;
        let cdp = self.cdp().await?;
        let _ = cdp.send_session(&session, "Page.bringToFront", json!({})).await;
        Ok(())
    }

    pub async fn tab_close(&self, index: Option<usize>) -> Result<()> {
        let target = {
            let st = self.state.lock().unwrap();
            let i = index.unwrap_or(st.current);
            st.tabs.get(i).map(|t| t.target_id.clone()).ok_or_else(|| anyhow!("tab {i} does not exist"))?
        };
        let cdp = self.cdp().await?;
        let session = self.state.lock().unwrap().tabs.iter().find(|t| t.target_id == target).map(|t| t.session_id.clone());
        let gone = cdp.wait_for(move |e| e.method == "Target.detachedFromTarget" && e.params["sessionId"].as_str() == session.as_deref());
        cdp.send("Target.closeTarget", json!({ "targetId": target })).await.map_err(|e| anyhow!(e))?;
        let _ = tokio::time::timeout(Duration::from_secs(5), gone).await;
        Ok(())
    }

    pub async fn go_back(&self, page: &PageRef) -> Result<bool> {
        let h = self.send_page(page, "Page.getNavigationHistory", json!({}), self.cfg.timeout_action).await?;
        let idx = h["currentIndex"].as_i64().unwrap_or(0);
        if idx <= 0 {
            return Ok(false);
        }
        let entry = h["entries"][(idx - 1) as usize]["id"].clone();
        self.settle(page, async {
            self.send_page(page, "Page.navigateToHistoryEntry", json!({ "entryId": entry }), self.cfg.timeout_action).await?;
            Ok(())
        })
        .await?;
        Ok(true)
    }

    // ------------------------------------------------------------------ console & network

    pub fn console_text(&self, page: &PageRef, level: &str, all: bool) -> Result<String> {
        let rank = |l: &str| match l {
            "error" => 0,
            "warning" => 1,
            "info" => 2,
            _ => 3,
        };
        let max = rank(level);
        self.tab_read(page, |t| {
            let start = if all { 0 } else { t.console_nav_start };
            let lines: Vec<String> = t.console[start..]
                .iter()
                .filter(|m| rank(m.level) <= max)
                .map(|m| {
                    if m.location.is_empty() {
                        format!("[{}] {}", m.level.to_uppercase(), m.text)
                    } else {
                        format!("[{}] {} @ {}", m.level.to_uppercase(), m.text, m.location)
                    }
                })
                .collect();
            if lines.is_empty() { "No console messages".to_string() } else { lines.join("\n") }
        })
    }

    pub fn network_text(&self, page: &PageRef, include_static: bool, filter: Option<&str>) -> Result<String> {
        let re = filter.map(regex::Regex::new).transpose().context("invalid filter regex")?;
        self.tab_read(page, |t| {
            let mut out = Vec::new();
            for (i, r) in t.requests.iter().enumerate().skip(t.requests_nav_start) {
                let ok = r.status.is_some_and(|s| s < 400) && r.failure.is_none();
                let is_static = matches!(r.resource_type.as_str(), "Image" | "Font" | "Stylesheet" | "Script" | "Media" | "Manifest");
                if is_static && ok && !include_static {
                    continue;
                }
                if re.as_ref().is_some_and(|re| !re.is_match(&r.url)) {
                    continue;
                }
                let status = match (&r.failure, r.status) {
                    (Some(f), _) => format!("[FAILED] {f}"),
                    (None, Some(s)) => format!("[{s}] {}", r.status_text),
                    _ => "[pending]".into(),
                };
                out.push(format!("{}. [{}] {} => {status}{}", i - t.requests_nav_start + 1, r.method, r.url, if r.from_route { " (mocked)" } else { "" }));
            }
            if out.is_empty() { "No network requests".to_string() } else { out.join("\n") }
        })
    }

    pub async fn network_request(&self, page: &PageRef, index: usize, part: Option<&str>) -> Result<String> {
        let req = self.tab_read(page, |t| t.requests.get(t.requests_nav_start + index.saturating_sub(1)).cloned())?;
        let req = req.ok_or_else(|| anyhow!("request #{index} does not exist"))?;
        let fmt_headers = |h: &Value| h.as_object().map(|o| o.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        let mut request_body = req.post_data.clone();
        if request_body.is_none() && req.method != "GET" {
            request_body = self
                .send_page(page, "Network.getRequestPostData", json!({ "requestId": req.id }), self.cfg.timeout_action)
                .await
                .ok()
                .and_then(|v| v["postData"].as_str().map(str::to_string));
        }
        let response_body = if matches!(part, None | Some("response-body")) && req.finished {
            match self.send_page(page, "Network.getResponseBody", json!({ "requestId": req.id }), self.cfg.timeout_action).await {
                Ok(v) if v["base64Encoded"] == true => {
                    let len = B64.decode(v["body"].as_str().unwrap_or_default()).map(|b| b.len()).unwrap_or(0);
                    format!("<binary, {len} bytes, {}>", req.mime)
                }
                Ok(v) => v["body"].as_str().unwrap_or_default().to_string(),
                Err(e) => format!("<unavailable: {e}>"),
            }
        } else {
            String::new()
        };
        let sections = [
            ("request-headers", format!("{} {}\n{}", req.method, req.url, fmt_headers(&req.request_headers))),
            ("request-body", request_body.unwrap_or_default()),
            ("response-headers", format!("{} {}\n{}", req.status.map(|s| s.to_string()).unwrap_or_else(|| "pending".into()), req.status_text, fmt_headers(&req.response_headers))),
            ("response-body", response_body),
        ];
        Ok(sections
            .iter()
            .filter(|(name, _)| part.is_none_or(|p| p == *name))
            .map(|(name, body)| format!("#### {name}\n{body}"))
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    pub async fn set_offline(&self, offline: bool) -> Result<()> {
        self.state.lock().unwrap().offline = offline;
        let cdp = self.cdp().await?;
        let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        for s in sessions {
            cdp.send_session(&s, "Network.emulateNetworkConditions", json!({ "offline": offline, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }))
                .await
                .map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    pub async fn add_route(&self, route: Route) {
        {
            let mut st = self.state.lock().unwrap();
            st.routes.retain(|r| r.pattern != route.pattern);
            st.routes.push(route);
        }
        self.sync_fetch().await;
    }

    pub async fn remove_routes(&self, pattern: Option<&str>) -> usize {
        let removed = {
            let mut st = self.state.lock().unwrap();
            let before = st.routes.len();
            match pattern {
                Some(p) => st.routes.retain(|r| r.pattern != p),
                None => st.routes.clear(),
            }
            before - st.routes.len()
        };
        self.sync_fetch().await;
        removed
    }

    // ------------------------------------------------------------------ storage

    pub async fn cookies(&self) -> Result<Vec<Value>> {
        let cdp = self.cdp().await?;
        Ok(cdp.send("Storage.getCookies", json!({})).await.map_err(|e| anyhow!(e))?["cookies"].as_array().cloned().unwrap_or_default())
    }

    pub async fn web_storage(&self, page: &PageRef, local: bool, op: &str, key: Option<&str>, value: Option<&str>) -> Result<String> {
        let store = if local { "localStorage" } else { "sessionStorage" };
        let k = key.map(js).unwrap_or_default();
        let expr = match op {
            "list" => format!("JSON.stringify(Object.fromEntries(Object.entries({store})), null, 1)"),
            "get" => format!("(() => {{ const v = {store}.getItem({k}); return v === null ? 'Key not found' : v; }})()"),
            "set" => format!("({store}.setItem({k}, {}), 'ok')", js(value.unwrap_or_default())),
            "delete" => format!("({store}.removeItem({k}), 'ok')"),
            _ => format!("({store}.clear(), 'ok')"),
        };
        let v = self.evaluate(page, &expr, None).await?;
        Ok(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
    }

    // ------------------------------------------------------------------ devtools

    pub async fn start_tracing(&self) -> Result<()> {
        let cdp = self.cdp().await?;
        cdp.send(
            "Tracing.start",
            json!({
                "transferMode": "ReturnAsStream",
                "traceConfig": { "includedCategories": [
                    "devtools.timeline", "disabled-by-default-devtools.timeline", "disabled-by-default-devtools.timeline.frame",
                    "disabled-by-default-devtools.screenshot", "blink.user_timing", "loading", "latencyInfo", "v8.execute",
                    "disabled-by-default-v8.cpu_profiler",
                ] }
            }),
        )
        .await
        .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    pub async fn stop_tracing(&self) -> Result<PathBuf> {
        let cdp = self.cdp().await?;
        let complete = cdp.wait_for(|e| e.method == "Tracing.tracingComplete");
        cdp.send("Tracing.end", json!({})).await.map_err(|e| anyhow!(e))?;
        let ev = tokio::time::timeout(Duration::from_secs(60), complete).await.map_err(|_| anyhow!("trace did not complete"))?.map_err(|_| anyhow!("browser closed"))?;
        let stream = ev.params["stream"].as_str().ok_or_else(|| anyhow!("no trace stream"))?.to_string();
        let path = self.output_path(None, "trace", "json")?;
        let mut file = std::fs::File::create(&path)?;
        loop {
            let chunk = cdp.send("IO.read", json!({ "handle": stream, "size": 1 << 20 })).await.map_err(|e| anyhow!(e))?;
            let data = chunk["data"].as_str().unwrap_or_default();
            if chunk["base64Encoded"] == true {
                std::io::Write::write_all(&mut file, &B64.decode(data)?)?;
            } else {
                std::io::Write::write_all(&mut file, data.as_bytes())?;
            }
            if chunk["eof"] == true {
                break;
            }
        }
        let _ = cdp.send("IO.close", json!({ "handle": stream })).await;
        Ok(path)
    }

    pub async fn start_video(&self, page: &PageRef, filename: Option<&str>, size: Option<(u32, u32)>) -> Result<PathBuf> {
        if self.state.lock().unwrap().video.is_some() {
            bail!("a video is already recording; stop it first");
        }
        let filename = self.output_path(filename, "video", "webm")?;
        let dir = filename.with_extension("frames");
        std::fs::create_dir_all(&dir)?;
        self.state.lock().unwrap().video = Some(Video {
            dir,
            filename: filename.clone(),
            session: page.session.clone(),
            frames: Vec::new(),
            chapters: Vec::new(),
            started: Instant::now(),
        });
        let (w, h) = size.unwrap_or((1280, 800));
        self.send_page(page, "Page.startScreencast", json!({ "format": "jpeg", "quality": 80, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1 }), self.cfg.timeout_action)
            .await?;
        Ok(filename)
    }

    pub fn video_chapter(&self, title: &str, description: Option<&str>) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let v = st.video.as_mut().ok_or_else(|| anyhow!("no video is recording"))?;
        let t = v.started.elapsed().as_secs_f64();
        v.chapters.push((t, match description {
            Some(d) => format!("{title} — {d}"),
            None => title.to_string(),
        }));
        Ok(())
    }

    /// Stops the screencast and encodes the frames with ffmpeg when it is installed; otherwise
    /// leaves the timestamped JPEG frames on disk.
    pub async fn stop_video(&self) -> Result<String> {
        let video = self.state.lock().unwrap().video.take().ok_or_else(|| anyhow!("no video is recording"))?;
        if let Ok(cdp) = self.cdp().await {
            let _ = cdp.send_session(&video.session, "Page.stopScreencast", json!({})).await;
        }
        if video.frames.is_empty() {
            let _ = std::fs::remove_dir_all(&video.dir);
            bail!("no frames were captured (the page did not repaint)");
        }
        let total = video.started.elapsed().as_secs_f64();
        let mut concat = String::new();
        for (i, (t, path)) in video.frames.iter().enumerate() {
            let next = video.frames.get(i + 1).map(|f| f.0).unwrap_or(total);
            concat.push_str(&format!("file '{}'\nduration {:.3}\n", path.display(), (next - t).max(0.001)));
        }
        if let Some((_, last)) = video.frames.last() {
            concat.push_str(&format!("file '{}'\n", last.display()));
        }
        let list = video.dir.join("frames.txt");
        std::fs::write(&list, concat)?;
        let chapters: String = video.chapters.iter().map(|(t, c)| format!("{:>8.2}s  {c}\n", t)).collect();
        if !chapters.is_empty() {
            std::fs::write(video.filename.with_extension("chapters.txt"), &chapters)?;
        }
        let status = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i"])
            .arg(&list)
            .args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2", "-c:v", "libvpx-vp9", "-b:v", "1M", "-pix_fmt", "yuv420p"])
            .arg(&video.filename)
            .status()
            .await;
        match status {
            Ok(s) if s.success() => {
                let _ = std::fs::remove_dir_all(&video.dir);
                Ok(format!("Video saved to {} ({} frames, {total:.1}s)", video.filename.display(), video.frames.len()))
            }
            _ => Ok(format!(
                "ffmpeg is not available, so the {} frames were kept as JPEGs in {} (frames.txt is an ffmpeg concat list)",
                video.frames.len(),
                video.dir.display()
            )),
        }
    }

    pub async fn highlight(&self, page: &PageRef, target: &str, style: Option<&str>) -> Result<()> {
        let obj = self.element(page, target).await?;
        self.call_on(page, &obj, "function(s, k) { __bmcp.highlight(this, s, k); }", vec![json!(style.unwrap_or("")), json!(target)]).await?;
        Ok(())
    }

    pub async fn hide_highlight(&self, page: &PageRef, target: Option<&str>) -> Result<()> {
        self.eval_world(page, &format!("__bmcp.removeHighlight({})", target.map(js).unwrap_or_else(|| "undefined".into()))).await?;
        Ok(())
    }

    pub async fn set_recording(&self, on: bool) -> Result<Option<Vec<Value>>> {
        let out = {
            let mut st = self.state.lock().unwrap();
            if on {
                st.recording = Some(Vec::new());
                None
            } else {
                st.recording.take()
            }
        };
        let cdp = self.cdp().await?;
        let tabs: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        for s in tabs {
            let page = PageRef { cdp: cdp.clone(), session: s.clone(), target_id: String::new() };
            let _ = self.eval_world(&page, &format!("__bmcp.setRecording({on})")).await;
            if on {
                let _ = cdp
                    .send_session(&s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": "__bmcp.setRecording(true)", "worldName": crate::browser::WORLD }))
                    .await
                    .map(|v| self.tab_write(&page, |t| t.handoff_scripts.push(format!("rec:{}", v["identifier"].as_str().unwrap_or_default()))));
            } else {
                let ids: Vec<String> = self
                    .tab_write(&page, |t| {
                        let (rec, rest): (Vec<String>, Vec<String>) = t.handoff_scripts.drain(..).partition(|s| s.starts_with("rec:"));
                        t.handoff_scripts = rest;
                        rec
                    })
                    .unwrap_or_default();
                for id in ids {
                    let _ = cdp.send_session(&s, "Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": &id[4..] })).await;
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ testing

    pub async fn locator_for(&self, page: &PageRef, target: &str) -> Result<String> {
        let obj = self.element(page, target).await?;
        let v = self.call_on(page, &obj, "function(a) { return __bmcp.generateLocator(this, a); }", vec![json!(self.cfg.test_id_attribute)]).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    // ------------------------------------------------------------------ human hand-off

    /// Switches to a visible window, shows a banner, and blocks until the human clicks "Done"
    /// (or closes the window, or the timeout passes). Optionally goes headless again afterwards.
    pub async fn hand_off(&self, message: &str, timeout: Duration, return_headless: bool) -> Result<String> {
        let was_headless = self.is_headless().await;
        self.set_headless(false).await?;
        let page = self.page().await?;
        let cdp = page.cdp.clone();
        let banner = format!("__bmcp.showHandoff({})", js(message));

        let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
        let mut scripts = Vec::new();
        for s in &sessions {
            if let Ok(v) = cdp
                .send_session(s, "Page.addScriptToEvaluateOnNewDocument", json!({ "source": format!("addEventListener('DOMContentLoaded', () => {banner})"), "worldName": crate::browser::WORLD }))
                .await
            {
                scripts.push((s.clone(), v["identifier"].as_str().unwrap_or_default().to_string()));
            }
            let p = PageRef { cdp: cdp.clone(), session: s.clone(), target_id: String::new() };
            let _ = self.eval_world(&p, &banner).await;
        }
        // Tabs opened while the human works need the banner too, so poll for them.
        let done = cdp.wait_for(|e| e.method == "Runtime.bindingCalled" && e.params["name"] == "__bmcpHandoff");
        let started = Instant::now();
        tokio::pin!(done);
        let outcome = loop {
            tokio::select! {
                r = &mut done => break if r.is_ok() { "done" } else { "closed" },
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if cdp.is_closed() { break "closed"; }
                    if started.elapsed() > timeout { break "timeout"; }
                }
            }
        };

        if outcome != "closed" {
            for (s, id) in &scripts {
                let _ = cdp.send_session(s, "Page.removeScriptToEvaluateOnNewDocument", json!({ "identifier": id })).await;
            }
            let sessions: Vec<String> = self.state.lock().unwrap().tabs.iter().map(|t| t.session_id.clone()).collect();
            for s in sessions {
                let p = PageRef { cdp: cdp.clone(), session: s, target_id: String::new() };
                let _ = self.eval_world(&p, "__bmcp.hideHandoff()").await;
            }
        }
        let mut msg = match outcome {
            "done" => "The user finished and handed control back.".to_string(),
            "timeout" => format!("The user did not click Done within {}s; continuing.", timeout.as_secs()),
            _ => "The user closed the browser window. Cookies saved to the profile are kept; the browser restarts on the next call.".to_string(),
        };
        if outcome != "closed" && return_headless && was_headless {
            self.set_headless(true).await?;
            msg.push_str(" Back in headless mode.");
        }
        Ok(msg)
    }
}

/// Playwright URL glob: `**` any characters, `*` any characters except `/`, `?` one character,
/// `{a,b}` alternatives. A pattern without wildcards matches as a substring of the URL.
pub fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    if !glob.contains(['*', '?', '{']) {
        return Ok(regex::Regex::new(&regex::escape(glob))?);
    }
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    let mut in_group = false;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push('.'),
            '{' => {
                in_group = true;
                re.push_str("(?:");
            }
            '}' if in_group => {
                in_group = false;
                re.push(')');
            }
            ',' if in_group => re.push('|'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Ok(regex::Regex::new(&re)?)
}
