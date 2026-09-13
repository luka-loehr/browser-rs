"""browser-rs vs @playwright/mcp: the same MCP calls on the same pages, measured the same way.

Each server navigates four real pages three times (the first navigation, which includes the
browser launch, is reported separately), then takes one snapshot. RSS is summed over the whole
process tree (server plus every browser process) after a 3 s settle.

    python3 scripts/bench_browser.py            # both
    python3 scripts/bench_browser.py rust       # only browser-rs
"""
import json, os, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bench import total_rss_mb

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PAGES = [
    "https://example.com/",
    "https://en.wikipedia.org/wiki/Rust_(programming_language)",
    "https://news.ycombinator.com/",
    "https://github.com/rust-lang/rust",
]


class Client:
    def __init__(self, cmd):
        t0 = time.time()
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.id = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "1"}})
        self.ready = time.time() - t0
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.id:
                return msg

    def call(self, name, args):
        t0 = time.time()
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        text = "".join(c.get("text", "") for c in r.get("result", {}).get("content", []))
        return time.time() - t0, text, bool(r.get("result", {}).get("isError")) or "error" in r

    def close(self):
        self.p.terminate()
        try:
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def bench(name, cmd):
    c = Client(cmd)
    tools = c.rpc("tools/list", {})["result"]["tools"]
    first_nav = None
    times = {url: [] for url in PAGES}
    sizes = {}
    for round_ in range(3):
        for url in PAGES:
            dt, text, err = c.call("browser_navigate", {"url": url})
            if err:
                print(f"  {url}: ERROR {text[:200]}")
            if first_nav is None:
                first_nav = dt
            else:
                times[url].append(dt)
            sizes.setdefault(url, len(text))
    dt_snap, snap, _ = c.call("browser_snapshot", {})
    time.sleep(3)
    rss, nproc = total_rss_mb(c.p.pid)
    c.close()
    time.sleep(1)
    print(f"\n## {name}")
    print(f"ready {c.ready*1000:.0f}ms | {len(tools)} tools, schemas {len(json.dumps(tools))} chars | first navigate (incl. browser launch) {first_nav*1000:.0f}ms")
    for url in PAGES:
        t = sorted(times[url])
        print(f"  navigate {url:<60} median {t[len(t)//2]*1000:6.0f}ms  reply {sizes[url]:6d} chars")
    print(f"  snapshot (last page) {dt_snap*1000:.0f}ms, {len(snap)} chars")
    print(f"  RSS after navigating: {rss:.0f} MB across {nproc} processes")


if __name__ == "__main__":
    which = sys.argv[1:] or ["rust", "playwright"]
    if "rust" in which:
        bench("browser-rs", [os.path.join(ROOT, "target/release/browser-rs"), "--isolated"])
    if "playwright" in which:
        bench("@playwright/mcp", ["npx", "-y", "@playwright/mcp@0.0.80", "--headless", "--isolated", "--browser", "chromium"])
