#!/usr/bin/env python3
"""量 Electron 进程树里各角色的 CPU 与 RSS。
用法: ./tools/measure-cpu.py <webview-pid> [秒数]
"""
import os
import sys
import time


def children(pid):
    out = [pid]
    try:
        for t in os.listdir(f"/proc/{pid}/task"):
            with open(f"/proc/{pid}/task/{t}/children") as f:
                for c in f.read().split():
                    out += children(int(c))
    except OSError:
        pass
    return out


def cpu_ticks(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().rsplit(")", 1)[1].split()
        return int(parts[11]) + int(parts[12])
    except (OSError, IndexError):
        return 0


def rss_mb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) // 1024
    except OSError:
        pass
    return 0


def role(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            args = f.read().decode("utf-8", "replace").split("\0")
    except OSError:
        return "gone"
    for a in args:
        if a.startswith("--type="):
            return a[7:]
    for a in args:
        if a.startswith("--utility-sub-type="):
            return "utility"
    return "browser"


def main():
    root = int(sys.argv[1])
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 10
    pids = sorted(set(children(root)))
    a = {p: cpu_ticks(p) for p in pids}
    time.sleep(secs)
    total = 0.0
    for p in pids:
        used = (cpu_ticks(p) - a[p]) / os.sysconf("SC_CLK_TCK") / secs * 100
        total += used
        print(f"{role(p):<16} pid={p:<8} cpu={used:5.1f}%  rss={rss_mb(p):4d}MB")
    print(f"{'TOTAL':<16} {'':<12} cpu={total:5.1f}%")


if __name__ == "__main__":
    main()
