import subprocess, json, time, sys

def descendant_pids(pid):
    out = subprocess.run(["ps", "-eo", "pid,ppid"], capture_output=True, text=True).stdout
    rows = [l.split() for l in out.splitlines()[1:] if l.strip()]
    children = {}
    for p, ppid in rows:
        children.setdefault(ppid, []).append(p)
    all_pids = [str(pid)]
    frontier = [str(pid)]
    while frontier:
        nxt = []
        for p in frontier:
            for c in children.get(p, []):
                all_pids.append(c)
                nxt.append(c)
        frontier = nxt
    return all_pids

def total_rss_mb(pid):
    pids = descendant_pids(pid)
    out = subprocess.run(["ps", "-o", "rss=", "-p", ",".join(pids)], capture_output=True, text=True).stdout
    total_kb = sum(int(x) for x in out.split())
    return total_kb / 1024, len(pids)

def run_bench(name, cmd, init_tool=None, init_args=None, settle=2.5):
    t0 = time.time()
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)

    def send(m): proc.stdin.write(json.dumps(m)+"\n"); proc.stdin.flush()
    def recv():
        l = proc.stdout.readline()
        return json.loads(l) if l else None

    send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bench","version":"1"}}})
    resp = recv()
    ready_time = time.time() - t0
    send({"jsonrpc":"2.0","method":"notifications/initialized"})

    if init_tool:
        send({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":init_tool,"arguments":init_args or {}}})
        recv()

    time.sleep(settle)
    rss_mb, n_pids = total_rss_mb(proc.pid)
    print(f"{name}: ready={ready_time:.2f}s rss={rss_mb:.1f}MB across {n_pids} process(es) init_ok={resp is not None}")

    proc.terminate()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()
    time.sleep(0.3)
    return rss_mb, ready_time

if __name__ == "__main__":
    pass
