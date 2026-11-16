# 验收怎么测

四条指标，全部可在本机复现。测之前先 `pkill -x dsh-shell-hypri`，并确认 `~/.dsh/profiles/desktop` 没有正在跑的 host（`pgrep -f dsh-desktop-host`）。

## 1. 启动

```bash
cd ~/devel/dsh-hyprland
./shell/target/release/dsh-shell-hyprland --app /usr/lib/deepseek-harness-desktop 2>&1 | ts '%H:%M:%.S'
```

看的行：`[shell] host ready: … (+3471ms)`。这个数字是**外壳进程开始 → 拿到带 token 的可用 URL**，不含前端挂载。

判据：

| 档位 | 阈值 |
|---|---|
| 通过 | ≤ 4500ms |
| 需要看 | 4500–6000ms |
| 不合格 | > 6000ms（大概率是 profile 里又出现了必败重装，见下） |

启动变慢时先查这一步有没有复活：

```bash
grep 'STARTUP_PHASE: name=ensure_internal_plugins' ~/.local/share/dsh-tauri*/logs/desktop.log | tail -3
grep -c 'INTERNAL_PLUGIN_NEEDS_REINSTALL' ~/.local/share/dsh-tauri*/logs/desktop.log
```

`ms` 上千就是 pnpm 又在重装内建插件，根因是 `pnpm-workspace.yaml` 的 `allowBuilds` key 与 `package.json` 里 git 依赖解析出的 commit 不一致。

## 2. 失焦态 CPU（这条最容易量化）

```bash
R=$(pgrep -f 'type=renderer' | head -1)
a=$(awk '{print $14+$15}' /proc/$R/stat); sleep 10
b=$(awk '{print $14+$15}' /proc/$R/stat); echo "$(( (b-a)/10 ))% of one core"
```

窗口切到别的应用后再测。判据：**≤ 2%**。基线：Electron 版常驻 8.9%（3h22m 均值），装 `dsh-perf-guard` 后 0%。

## 3. 滚动掉帧率

在窗口里开 DevTools（Ctrl+Shift+I）→ Console，粘：

```js
await new Promise(r => {
  const t = []; let last = performance.now(), n = 0;
  function f(now) { t.push(now - last); last = now; if (++n < 600) requestAnimationFrame(f); else r(t); }
  requestAnimationFrame(f);
}).then(t => {
  t.sort((a,b)=>a-b);
  const q = p => t[Math.floor(t.length*p)].toFixed(1);
  console.log(`median=${q(.5)} p95=${q(.95)} p99=${q(.99)} worst=${t.at(-1).toFixed(1)} drops=${(t.filter(x=>x>20).length/t.length*100).toFixed(1)}%`);
});
```

一屏消息里连续往上滚，滚到顶再往下滚，读数取两次平均。判据：**drops < 5%**，`p99 < 25ms`。

基线未测——这是这套东西唯一没有对照数字的一项；建议第一次先在旧 Electron 客户端上跑一遍同一条 snippet 再比。

## 4. 两小时 RSS

```bash
H=$(pgrep -f dsh-desktop-host | head -1)
while :; do
  printf '%s host=%sMB' "$(date +%H:%M)" "$(( $(awk '/VmRSS/{print $2}' /proc/$H/status) / 1024 ))"
  for p in $(pgrep -f 'type=renderer'); do
    printf ' r%s=%sMB' "$p" "$(( $(awk '/VmRSS/{print $2}' /proc/$p/status) / 1024 ))"
  done; echo; sleep 300
done
```

判据：2h 后 host < 900MB、renderer < 900MB，且**曲线不单调上升**（有回落即说明是工作集不是泄漏）。看门狗会在 Tab 进程超过 `DSH_WATCH_RSS_MB`（默认 900）时 autoc reload，最多 5 次；若 2h 内触发过 2 次以上，说明阈值设低了或确实有泄漏。

## 5. GPU 有没有真的在干活

```bash
nvidia-smi --query-compute-apps=pid,used_memory --format=csv
grep -c 'Exiting GPU process' /tmp/sh*.log    # 应为 0
```

`use-gl=egl` 在 Electron 44 上会让 GPU 进程直接退出（日志 `Requested GL implementation (gl=egl-gles2,angle=none) not found`），整窗转软件光栅，表现为"能用但拖窗口就糊"。正确写法是 `use-gl=angle` + `use-angle=gl`。

## 6. 三个一起测的最小脚本

```bash
cd ~/devel/dsh-hyprland
pkill -x dsh-shell-hypri; rm -f "${XDG_RUNTIME_DIR}/dsh-shell-hyprland.lock"
./bin/dsh-hyprland --headless    # 只测启动，打印 URL 就退
./bin/dsh-hyprland               # 起窗口，然后按 2/3/4 测
```
