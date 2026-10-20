"""Smoke tests for browser-rs: a local HTTP server and an MCP client over stdio, no dependencies.

Covers the tool list, snapshots, clicks, batch, fetch and evaluate, navigation (slow subresources,
script redirects) and browser_frames (framesets, same-site and cross-site iframes). The browser
runs with --isolated; the only network access is one navigation to example.com.

    python3 scripts/smoke.py                         # all tests against target/release/browser-rs
    python3 scripts/smoke.py --bin <path> frames     # one group against another binary
    python3 scripts/smoke.py -- --executable-path <Chrome for Testing>
                                                     # full Chrome: cross-site iframes run out of
                                                     # process, which chrome-headless-shell never does
"""
import http.server, json, os, socketserver, subprocess, sys, threading, time

PAGES = {
    "/slow-img": b"<!doctype html><title>slow</title><h1>Slow image page</h1><p>Body text here.</p><img src='/sleep?s=12'>",
    "/redirect": b"<!doctype html><title>r</title><script>location.replace('/target')</script>",
    "/target": b"<!doctype html><title>target</title><h1>Target</h1><p>Arrived.</p>",
    "/frames": b"""<!doctype html><html><head><title>frames</title></head>
<frameset rows="50%,50%"><frame name="top" src="/frames-top"><frame name="bottom" src="/frame-b"></frameset></html>""",
    "/frames-top": b"""<html><frameset cols="50%,50%"><frame name="left" src="/frame-l"><frame name="right" src="/frame-r"></frameset></html>""",
    "/frame-l": b"<html><body><h2>Left</h2><p>LEFT text</p><a href='/target'>go</a></body></html>",
    "/frame-r": b"<html><body><p>RIGHT text</p><button>Press</button></body></html>",
    "/frame-b": b"<html><body><p>BOTTOM text</p></body></html>",
    "/sitey": b"""<!doctype html><title>sitey</title><p>Host text.</p><iframe name="sub" src="http://b.localhost:PORT/frame-r"></iframe>""",
    "/iframes": b"""<!doctype html><title>iframes</title><h1>Host</h1><p>Host text.</p>
<iframe id="same" name="same" src="/frame-l"></iframe>
<iframe id="cross" title="cross" src="http://localhost:PORT/frame-r"></iframe>""",
}


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/sleep":
            time.sleep(float(self.path.split("s=")[1]))
            self.send_response(200); self.send_header("Content-Type", "image/gif"); self.end_headers()
            self.wfile.write(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;")
            return
        body = PAGES.get(path)
        if body is None:
            self.send_response(404); self.end_headers(); return
        body = body.replace(b"PORT", str(PORT).encode())
        self.send_response(200); self.send_header("Content-Type", "text/html"); self.end_headers()
        self.wfile.write(body)


class TS(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


srv = TS(("127.0.0.1", 0), H)
PORT = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
BASE = f"http://127.0.0.1:{PORT}"


class Client:
    def __init__(self, cmd):
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
                                  env={**os.environ, "BROWSER_RS_NO_VIEWER": "1"})
        self.id = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "1"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"); self.p.stdin.flush()

    def rpc(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n"); self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server exited")
            msg = json.loads(line)
            if msg.get("id") == self.id:
                return msg

    def call(self, name, args):
        t0 = time.time()
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        text = "".join(c.get("text", "") for c in r.get("result", {}).get("content", []))
        return time.time() - t0, text, bool(r.get("result", {}).get("isError")) or "error" in r

    def close(self):
        try:
            self.call("browser_close", {})
        except Exception:
            pass
        self.p.terminate()
        try:
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


FAIL = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + ("" if cond else f"\n  {detail[:1500]}"))
    if not cond:
        FAIL.append(name)


def t_nav(c):
    c.call("browser_navigate", {"url": BASE + "/target", "snapshot": "none"})  # launch
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/slow-img", "snapshot": "none"})
    check("slow image does not hold navigation (<6s)", not err and dt < 6, f"{dt:.1f}s {text}")
    dt, text, err = c.call("browser_text", {})
    check("text readable after early return", "Body text here." in text, text)
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/redirect", "snapshot": "none"})
    dt2, text2, _ = c.call("browser_text", {})
    check("script redirect followed", not err and "Arrived." in text2, text + "\n" + text2)
    # A redirect can replace the document before the server's wait loop sees it commit.
    bad = []
    for _ in range(10):
        c.call("browser_navigate", {"url": BASE + "/frame-b", "snapshot": "none"})
        dt, text, err = c.call("browser_navigate", {"url": BASE + "/redirect", "snapshot": "none"})
        if err:
            bad.append(text)
    check("script redirect never times out (10 runs)", not bad, "\n".join(bad))
    dt, text, err = c.call("browser_navigate", {"url": "https://example.com/", "snapshot": "none"})
    check("real page navigates with status", not err and "HTTP 200" in text, text)


def t_frames(c):
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/frames", "snapshot": "full"})
    check("frameset snapshot shows frame content", "LEFT text" in text and "BOTTOM text" in text and "Press" in text, text)
    dt, text, err = c.call("browser_frames", {})
    check("browser_frames lists nested frames with text", not err and all(s in text for s in ["LEFT text", "RIGHT text", "BOTTOM text", "top > left", "top > right", "bottom"]), text)
    dt, text, err = c.call("browser_frames", {"frame": "right"})
    check("browser_frames one frame by name", not err and "RIGHT text" in text and "LEFT text" not in text, text)
    dt, text, err = c.call("browser_frames", {"frame": "nope"})
    check("browser_frames unknown frame errors with the list", err and "left" in text, text)
    dt, text, err = c.call("browser_click", {"target": "frame[name=top] >> frame[name=right] >> button", "snapshot": "none"})
    check("click inside nested frame by selector", not err, text)
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/iframes", "snapshot": "none"})
    dt, text, err = c.call("browser_frames", {})
    check("browser_frames reads cross-origin iframe", not err and "Host text." in text and "LEFT text" in text and "RIGHT text" in text, text)
    dt, text, err = c.call("browser_navigate", {"url": f"http://a.localhost:{PORT}/sitey", "snapshot": "none"})
    dt, text, err = c.call("browser_frames", {})
    check("browser_frames reads same-site cross-origin iframe", not err and "RIGHT text" in text and "cross-origin" in text, text)
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/iframes", "snapshot": "none"})
    dt, text, err = c.call("browser_batch", {"steps": [{"tool": "browser_frames", "arguments": {"frame": "same"}}]})
    check("browser_frames in batch", not err and "LEFT text" in text, text)


def t_basic(c):
    tools = c.rpc("tools/list", {})["result"]["tools"]
    names = [t["name"] for t in tools]
    print(f"  {len(tools)} tools")
    check("tool list has core tools", all(n in names for n in ["browser_navigate", "browser_text", "browser_batch", "browser_handoff"]), str(names))
    dt, text, err = c.call("browser_navigate", {"url": BASE + "/frame-l", "snapshot": "full"})
    check("snapshot shows heading and link", not err and 'heading "Left"' in text and 'link "go"' in text, text)
    dt, text, err = c.call("browser_click", {"target": "link=go", "snapshot": "none"})
    dt2, text2, _ = c.call("browser_text", {})
    check("click link navigates", not err and "Arrived." in text2, text + text2)
    dt, text, err = c.call("browser_batch", {"steps": [{"tool": "browser_navigate", "arguments": {"url": BASE + "/frame-r"}}, {"tool": "browser_click", "arguments": {"target": "role=button[name=\"Press\"]"}}, {"tool": "browser_text", "arguments": {}}]})
    check("batch runs three steps", not err and "RIGHT text" in text, text)
    dt, text, err = c.call("browser_fetch", {"url": BASE + "/target"})
    check("fetch returns body", not err and "Arrived." in text, text)
    dt, text, err = c.call("browser_evaluate", {"function": "() => 6 * 7"})
    check("evaluate", not err and "42" in text, text)


TESTS = {"basic": t_basic, "nav": t_nav, "frames": t_frames}

if __name__ == "__main__":
    args = sys.argv[1:]
    extra = args[args.index("--") + 1:] if "--" in args else []
    args = args[:args.index("--")] if "--" in args else args
    binary = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "browser-rs")
    if args[:1] == ["--bin"]:
        binary, args = args[1], args[2:]
    names = args or list(TESTS)
    c = Client([binary, "--isolated", "--timeout-navigation", "15000", *extra])
    try:
        for n in names:
            TESTS[n](c)
    finally:
        c.close()
    print("FAILED: " + ", ".join(FAIL) if FAIL else "ALL PASSED")
    sys.exit(1 if FAIL else 0)
