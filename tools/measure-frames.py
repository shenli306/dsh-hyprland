#!/usr/bin/env python3
"""用 CDP 量渲染器的帧间隔与掉帧率，不需要人肉滚动。

前提：窗口以 DSH_DEBUG_PORT=9222 启动。
  DSH_DEBUG_PORT=9222 ./bin/dsh-hyprland

用法：
  ./tools/measure-frames.py                 # 焦点态空转 10s
  ./tools/measure-frames.py --scroll        # 焦点态连续滚轮 10s
  ./tools/measure-frames.py --frames 1200 --seconds 20
"""
import argparse
import asyncio
import json
import time
import urllib.request

import websockets

SAMPLER = """
(() => {
  const cap = %d, deadline = performance.now() + %d;
  const t = []; let last = performance.now(); let n = 0;
  function step(now) {
    t.push(now - last); last = now; n++;
    if (n < cap && now < deadline) requestAnimationFrame(step);
    else window.__frameStats = { t, done: true, at: Date.now() };
  }
  window.__frameStats = null;
  requestAnimationFrame(step);
  return true;
})()
"""


def target_list(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5) as r:
        return json.load(r)


def pick_target(items):
    pages = [t for t in items if t.get("type") == "page" and "127.0.0.1" in t.get("url", "")]
    if not pages:
        pages = [t for t in items if t.get("type") == "page"]
    return pages[0] if pages else None


async def run(port, cap_frames, max_seconds, scroll):
    tgt = pick_target(target_list(port))
    if not tgt:
        raise SystemExit("没有 page target，窗口是不是没起来？")
    print(f"target: {tgt['url'][:80]}")

    async with websockets.connect(tgt["webSocketDebuggerUrl"], max_size=32 * 1024 * 1024) as ws:
        mid = 0

        async def call(method, params=None):
            nonlocal mid
            mid += 1
            await ws.send(json.dumps({"id": mid, "method": method, "params": params or {}}))
            while True:
                msg = json.loads(await ws.recv())
                if msg.get("id") == mid:
                    if "error" in msg:
                        raise RuntimeError(f"{method}: {msg['error']}")
                    return msg.get("result", {})

        await call("Runtime.enable")
        vis = await call(
            "Runtime.evaluate",
            {"expression": "JSON.stringify({v: document.visibilityState, f: document.hasFocus()})", "returnByValue": True},
        )
        print("page:", vis["result"]["value"])
        # 窗口没被 WM 聚焦时 Chromium 会停发 BeginFrame，rAF 一次都不跑（采到 0 帧）。
        # 用 CDP 的焦点仿真把 rAF 唤醒，这样测的是“焦点态”的渲染成本。
        try:
            await call("Emulation.setFocusEmulationEnabled", {"enabled": True})
            vis = await call(
                "Runtime.evaluate",
                {"expression": "JSON.stringify({v: document.visibilityState, f: document.hasFocus()})", "returnByValue": True},
            )
            print("page(focus-emulated):", vis["result"]["value"])
        except Exception as e:
            print("focus emulation failed:", e)
        await call(
            "Runtime.evaluate",
            {"expression": SAMPLER % (cap_frames, int(max_seconds * 1000)), "returnByValue": True},
        )

        t0 = time.time()
        sent = 0
        while time.time() - t0 < max_seconds:
            if scroll:
                await call(
                    "Input.dispatchMouseEvent",
                    {"type": "mouseWheel", "x": 640, "y": 500, "deltaX": 0, "deltaY": 420, "modifiers": 0},
                )
                sent += 1
            await asyncio.sleep(0.016 if scroll else 0.25)
            st = await call(
                "Runtime.evaluate",
                {"expression": "window.__frameStats && window.__frameStats.done", "returnByValue": True},
            )
            if st.get("result", {}).get("value"):
                break

        # 采样器的 deadline 与这里的循环同时到点，收尾时多等几秒
        for _ in range(int(5 / 0.2)):
            st = await call(
                "Runtime.evaluate",
                {"expression": "window.__frameStats && window.__frameStats.done", "returnByValue": True},
            )
            if st.get("result", {}).get("value"):
                break
            await asyncio.sleep(0.2)

        raw = await call(
            "Runtime.evaluate",
            {"expression": "JSON.stringify(window.__frameStats ? window.__frameStats.t : [])", "returnByValue": True},
        )

    t = json.loads(raw["result"]["value"])
    if not t:
        raise SystemExit("没采到帧")
    t = [x for x in t if x > 0]
    t_sorted = sorted(t)

    def q(p):
        return t_sorted[min(len(t_sorted) - 1, int(len(t_sorted) * p))]

    drops = sum(1 for x in t if x > 20)
    span = sum(t)
    print(
        f"frames={len(t)} wheels={sent} fps={len(t) / (span / 1000):.1f} median={q(0.5):.1f}ms "
        f"p95={q(0.95):.1f}ms p99={q(0.99):.1f}ms worst={t_sorted[-1]:.1f}ms drops={drops / len(t) * 100:.1f}%"
    )
    if drops:
        print(f"  >20ms 的帧: {drops} 个；>33ms: {sum(1 for x in t if x > 33)} 个")
    return drops / len(t) * 100


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=9222)
    ap.add_argument("--frames", type=int, default=600)
    ap.add_argument("--seconds", type=float, default=12.0)
    ap.add_argument("--scroll", action="store_true")
    a = ap.parse_args()
    asyncio.run(run(a.port, a.frames, a.seconds, a.scroll))
