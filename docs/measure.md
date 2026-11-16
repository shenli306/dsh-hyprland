# 验收怎么测（含实测数字）

四条指标。测之前先 `pkill -x dsh-shell-hypri`，并确认没有残留 webview（外壳死了它不一定跟着死，见文末坑 1）。

## 1. 启动 → 实测 3.1–3.5s

```bash
cd ~/devel/dsh-hyprland
./shell/target/release/dsh-shell-hyprland --app /usr/lib/deepseek-harness-desktop 2>&1 | ts '%H:%M:%.S'
# 或者只测启动不出窗口
./bin/dsh-hyprland --headless
```

看的行：`[shell] host ready: … (+3471ms)`。这个数字是**外壳开始 → 拿到带 token 的可用 URL**，不含前端挂载。

| 实测 | 值 |
|---|---|
| 六次运行 | 3120 / 3163 / 3231 / 3266 / 3335 / 3471 ms |
| 修复前基线 | 6519 / 66109 / 72204 ms（`ensure_internal_plugins` 必败重装） |

判据：≤4500ms 通过。超过 6000ms 先查是不是又出现了必败重装：

```bash
grep 'STARTUP_PHASE: name=ensure_internal_plugins' ~/.local/share/dsh-tauri*/logs/desktop.log | tail -3
grep -c 'INTERNAL_PLUGIN_NEEDS_REINSTALL' ~/.local/share/dsh-tauri*/logs/desktop.log
```

## 2. 帧率与掉帧 → 实测 58.4fps / 0.2%

窗口以 `DSH_DEBUG_PORT=9222` 启动，然后用脚本量，不需要人肉滚动：

```bash
DSH_DEBUG_PORT=9222 ./bin/dsh-hyprland &
./tools/measure-frames.py                 # 焦点态空转 600 帧
./tools/measure-frames.py --scroll        # 焦点态连续滚轮（CDP 发 mouseWheel）
```

脚本会先 `Emulation.setFocusEmulationEnabled`：**窗口没被 WM 聚焦时 Chromium 停发 BeginFrame，rAF 一次都不跑**（采到 0 帧就是这个原因），所以必须仿真焦点才测得到渲染成本。

| 场景 | frames | fps | median | p95 | p99 | worst | drops |
|---|---|---|---|---|---|---|---|
| 焦点态空转 | 600 | 58.6 | 16.7ms | 17.9ms | 18.3ms | 34.0ms | **0.3%** |
| 焦点态滚动（346 次滚轮） | 600 | 58.4 | 16.7ms | 18.1ms | 18.4ms | 34.2ms | **0.2%** |

判据：drops <5%、p99 <25ms → 通过。58fps 而不是 60，是 60Hz 输出上的采样抖动，不是掉帧。

主线程是否有 JS 热点：

```bash
./tools/profile-cpu.py 9222 6
# samples=24010 over 6.0s → 99.0% 在 (idle)
```

结论：**渲染器主线程几乎全空**，卡顿不可能来自页面 JS。

## 3. 焦点 / 失焦 CPU → 失焦空闲 1.2–2.8%

```bash
W=$(grep -a 'webview pid=' /tmp/one.log | sed 's/.*pid=\([0-9]*\).*/\1/')
./tools/measure-cpu.py "$W" 10      # 按进程
```

按线程看：`./tools/measure-cpu.py` 只给进程级；线程级用 `/proc/<pid>/task/*/stat` 差分，热点会显示在
`Media`、`VideoFrameCompositor`（视频解码）与 `VizCompositor`（合成）。

| 状态 | 合计 CPU | 说明 |
|---|---|---|
| 失焦 + 会话里有视频自动播放 | 18.6%（渲染 11.9%） | 复现用户“开久了卡”的场景 |
| 失焦 + 视频被按停（当前实现） | **6.0% → 空闲 1.2–2.8%** | 60 分钟 62 个样本，中位 15.2%（有人在用时会高） |
| 焦点态（视频在放 + 消息在流） | 14–29% | 正常使用态，不是泄漏 |
| 对照：原 Electron 客户端 | 8.9% 单核常驻（3h22m 均值） | 常驻动画插件 + rAF 不限速 |

**根因不是外壳**：CPU 大头是会话里 `<video>` 的自动播放，它与窗口焦点无关。
现在失焦会把 `video` 按停、回前台恢复（`__dshBlockedMedia`），并用捕获阶段监听 `play`
事件来按住在 SPA 之后才渲染出来的新视频——只在 blur 那一刻 `pause()` 是不够的。

## 4. 两小时 RSS → 446 → 486MB，末段持平

```bash
# 采样器（每分钟一行，同时记 CPU）
/tmp/rss2h.sh            # 见 docs/measure.md 附的脚本，或自己写一行
tail -5 /tmp/rss-2h.log
```

| 项 | 值 |
|---|---|
| 起始 | 446MB（20:54） |
| 60 分钟 | 483MB |
| 66 分钟（末样本） | 486MB，区间最大 489MB |
| 末 20 分钟 | 483 → 486，**持平** |
| 看门狗触发 | 0 次（阈值 `DSH_WATCH_RSS_MB=900`） |

判据：2h 后 <900MB 且曲线不单调上升 → **通过（工作集增长 40MB 后走平，不是泄漏）**。

## 5. GPU 有没有真的在干活

```bash
nvidia-smi --query-compute-apps=pid,used_memory --format=csv
grep -c 'Exiting GPU process' /tmp/*.log    # 应为 0
```

`use-gl=egl` 在 Electron 44 上会让 GPU 进程直接退出（日志 `Requested GL implementation (gl=egl-gles2,angle=none) not found`），
整窗转软件光栅。正确写法是 `use-gl=angle` + `use-angle=gl`，改完 `Exiting GPU process` 计数归 0。

## 踩过的坑（测的时候会遇到）

1. **杀掉外壳不会带走 webview 窗口**：`pkill -x dsh-shell-hypri` 之后窗口还在，反复测就堆一屏。要么 `pkill -f 'electron42/electron /home/elysia/devel'`，要么用带 `[e]` 的写法避免匹配到自己那条 bash 命令行（**不带括号会把执行命令的 shell 一起杀掉**）。
2. **`pkill -f 'dsh-shell-hyprland --app'` 会自杀**：它的命令行本身就包含这个串。用 `pkill -x dsh-shell-hypri`（进程名被内核截到 15 字符）。
3. **`$XDG_RUNTIME_DIR/dsh-shell-hyprland.lock` 里的 pid 会被复用**：重启后报“另一个实例在跑”，删锁或加 `--replace`。
4. **窗口没聚焦时 rAF 不跑**：CDP 采样必须开焦点仿真，否则采到 0 帧（不是 bug，是 Chromium 的 BeginFrame 节流）。
5. **`document.hasFocus()` 在 Electron 里恒为 false**，`win.isFocused()` 在 Wayland 下也不可信（日志 `focus=true`、页面侧仍是失焦）。所以焦点状态由主进程的 `focus`/`blur` 事件维护成 `window.__dshFocused`，页面只读这个标志——早期版本读 `document.hasFocus()`，结果是 rAF 治理**永远停在失焦档**，用户真在使用时被压到 15fps。
6. `grim -g "<x>,<y> <w>x<h>"` 跨输出截图会拍到别的窗口；判断渲染是否成功用 `[webview] loaded` 探针（含 `rootKids`、`mods`）比截图可靠。
