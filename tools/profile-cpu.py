#!/usr/bin/env python3
"""用 CDP 的 Profiler 抓 5 秒 JS 采样，按自身耗时排序打印热点函数。
用法: ./tools/profile-cpu.py [端口] [秒数]
"""
import asyncio
import json
import sys
import urllib.request
from collections import Counter

import websockets


async def main(port, secs):
    items = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5))
    tgt = [t for t in items if t.get("type") == "page" and "127.0.0.1" in t.get("url", "")][0]
    async with websockets.connect(tgt["webSocketDebuggerUrl"], max_size=2**28) as ws:
        i = 0

        async def call(method, params=None):
            nonlocal i
            i += 1
            await ws.send(json.dumps({"id": i, "method": method, "params": params or {}}))
            while True:
                r = json.loads(await ws.recv())
                if r.get("id") == i:
                    if "error" in r:
                        raise RuntimeError(r["error"])
                    return r.get("result", {})

        await call("Profiler.enable")
        await call("Profiler.setSamplingInterval", {"interval": 200})
        await call("Profiler.start")
        await asyncio.sleep(secs)
        prof = await call("Profiler.stop")

    p = prof["profile"]
    nodes = {n["id"]: n for n in p["nodes"]}
    counts = Counter(p["samples"])

    def label(n):
        f = n.get("callFrame", {})
        url = f.get("url", "")
        name = f.get("functionName") or "(anonymous)"
        if url.startswith("file:") or "/" in url:
            url = url.rsplit("/", 1)[-1][:28]
        return f"{name} @ {url}" if url else name

    rows = []
    for nid, c in counts.items():
        n = nodes.get(nid)
        if n:
            rows.append((c, label(n)))
    rows.sort(reverse=True)
    total = sum(counts.values()) or 1
    print(f"samples={total} over {secs}s (1 sample ≈ 0.2ms CPU)")
    seen = Counter()
    for c, name in rows[:40]:
        seen[name] += c
    for name, c in seen.most_common(25):
        print(f"{c * 100 / total:5.1f}%  {c * 0.2 / secs * 100:5.1f}%cpu  {name}")


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1]) if len(sys.argv) > 1 else 9222, float(sys.argv[2]) if len(sys.argv) > 2 else 5))
