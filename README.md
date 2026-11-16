# dsh-hyprland

给 DeepSeek Harness 做的**自用外壳 + 补丁集**：Rust 外壳做编排，Electron 只当 WebView 宿主，窗口参数按 Hyprland + NVIDIA(Wayland) 调优。

> 目标不是"把 Electron 换成 Rust"（那不会更快，见文末），而是把**启动慢**和**跑久了掉帧**这两件事在架构成因上掐掉。

## 已经落地并可复现的结果

| 指标 | 基线 | 本仓库 |
|---|---|---|
| 外壳启动 → host ready（带 token 的可用地址） | 7.9s（旧 Tauri nightly 端到端到 client-boot） | **3.47s** |
| 失焦态渲染器 CPU | 单核常驻 8.9%（Electron 版，实测 3h22m 均值） | 0%（`dsh-perf-guard` + 本外壳的帧预算） |
| GPU 进程 | `--use-gl=egl` 会让 Electron 44 的 GPU 进程退出，整窗掉软件光栅 | 用 `use-gl=angle` + `use-angle=gl`，GPU 进程存活 |

实测证据（本机，2026-11-16）：

```
[shell] host ready: http://127.0.0.1:36261/?token=… (+3471ms)
[shell] webview pid=905926
[webview] boot pid=905926 url=http://127.0.0.1:36261/?token=…
[webview] window created, id=1
[webview] loaded {"title":"DeepSeek Harness","href":"http://127.0.0.1:36261/","rootKids":1,"mods":1}
```

`hyprctl clients` 里出现 `('dsh-hyprland-webview', 'DeepSeek Harness')`，`#root` 有子节点、`__ModuleLoader__` 在，说明前端真的渲染出来了。

## 架构：谁干什么

```
dsh-hyprland (Rust, 368KB)
├─ 单实例锁        $XDG_RUNTIME_DIR/dsh-shell-hyprland.lock
├─ 解析 payload    认三种布局：发行版 / nightly / asar 包
├─ spawn host      Electron 二进制 + ELECTRON_RUN_AS_NODE=1 当 node
│                  args: --expose-internals --no-warnings <host-entry> <runtimeDir> <projectDir> <primaryRuntime> [pnpm, nodeBin]
│                  env : NODE_COMPILE_CACHE, NODE_OPTIONS=--max-semi-space-size=64, NODE_CHANNEL_FD=3
│                  IPC : AF_UNIX socketpair → fd 3（**不能用 pipe**，见踩坑记录）
├─ 等 ready        读 IPC 行 {"type":"ready","url":"…?token=…"}；60s 后才退回裸端口
├─ spawn WebView   未打包 Electron（resources/default_app.asar 那种）+ 本仓库 shell/webview
└─ 退出清理        杀整个进程组 SIGTERM → SIGKILL
```

**为什么 WebView 还是 Electron**：DSH 官方的"Tauri 外壳"其实也只链 `libgtk-3`（`ldd` 实测，不链 webkit），窗口由 `runtime/linux-x64/DSH Tauri Nightly` 这个 Electron 渲染。换 WebKitGTK 会把渲染引擎换掉，前端 43 个 client bundle 的兼容性风险全担上，而收益是负的（Linux 上 WebKitGTK 合成更慢）。

**WebView 宿主从哪来**（按顺序找，先命中先用）：

1. `DSH_WEBVIEW_ELECTRON` 指定的路径
2. `<app>/runtime/linux-x64`：目录里有 `resources/default_app.asar` 且**没有** `resources/app.asar`
3. `<app>/webview-host`、`<app>/resources/webview-host`（同上判据）
4. 系统 electron 包：`/usr/lib/electron*`、`/usr/lib64/electron*`（本机是 `/usr/lib/electron42`，**开箱即用，实测 3.23s**）

**为什么必须用"未打包"的 Electron 当宿主**：打包版（`resources/app.asar`）会无视 argv 里的 app 目录，永远跑它自己那份 `main.js`——你传什么都白搭，实测表现为"进程秒退，RC=0，一行日志都没有"。

## 目录

```
PKGBUILD                      Arch 打包（在 AUR 那份基础上加装外壳与 webview）
patches/0001-linux-desktop.patch   AUR 的 Linux 构建补丁（必需）
patches/0002-hide-menubar.patch    隐藏窗口内菜单栏
shell/Cargo.toml              Rust 外壳（零外部依赖）
shell/src/main.rs             编排逻辑
shell/webview/                Electron 侧：main.cjs（P3 帧预算 + P4 Chromium 参数 + P5 看门狗）
bin/dsh-hyprland              启动器
docs/measure.md               验收怎么测
```

## 构建

```bash
cd shell && cargo build --release        # 4s，产物 368KB
./bin/dsh-hyprland --headless            # 只起 host，打印 ready URL
./bin/dsh-hyprland                       # 起窗口
./bin/dsh-hyprland --replace             # 顶掉已有实例
```

可选环境变量：`DSH_APP_DIR`（payload 根）、`DSH_RUNTIME_DIR`、`DSH_WEBVIEW_DIR`、`DSH_WEBVIEW_ELECTRON`（未打包 Electron 的路径）、`DSH_RENDER_NODE`、`DSH_IDLE_FPS`（默认 15）、`DSH_WATCH_RSS_MB`（默认 900）。

## P3/P4/P5 具体在哪

- **P4 Chromium 参数**：`shell/webview/main.cjs` 顶部 `appendSwitch`
  `ozone-platform-hint=auto`、`enable-features=WaylandLinuxDrmSyncobj,CanvasOopRasterization`、`disable-features=CalculateNativeWinOcclusion,HardwareMediaKeyHandling`、`use-gl=angle` + `use-angle=gl`、`enable-zero-copy`、`force_high_performance_gpu`、`render-node-override`（由 `DSH_RENDER_NODE` 注入）。
- **P3 帧预算**：`insertCSS` 注入 `[data-turn]:not(:last-child){content-visibility:auto;contain-intrinsic-size:auto 220px}`；失焦时给 `<html>` 打 `dsh-unfocused` 暂停所有 CSS 动画；`requestAnimationFrame` 被包一层，失焦合并到 `DSH_IDLE_FPS`；`webPreferences.backgroundThrottling: true`。
- **P5 长跑**：每分钟查 `app.getAppMetrics()`，Tab 进程 workingSet 超过 `DSH_WATCH_RSS_MB` 就 `reload()`（最多 5 次）。
- **P2（客户端首屏懒加载）**：按你的决定不做。

## 踩坑记录（都实测过）

1. **Node IPC 必须是 socket，不能是 pipe**：`process.send` 在普通 pipe 上 `write EINVAL`。用 `socketpair(AF_UNIX, SOCK_STREAM)`，`dup2` 到 fd 3，`NODE_CHANNEL_FD=3`。Rust 侧已验证能收到 `{"type":"ready","url":"…"}\n`。
2. **父端 socket 不能关**：host 里有 `process.once("disconnect", () => stop())`，读端一关它就优雅自杀（表面症状是 `CordisError: cannot create effect on inactive context`）。读线程要活到进程结束。
3. **裸端口要鉴权**：`curl http://127.0.0.1:<port>/` → `401 dsh web authentication required`。可用地址是 ready 里的 `authenticatedUrl`（带 `?token=`，302/303 跳转）。
4. **`ELECTRON_RUN_AS_NODE=1` 会传染**：从 DSH 的 bash 工具里启动时环境里就有这个变量，Electron 会退化成 node 去跑 `main.cjs`（报 `Cannot find module 'electron'`）。拉起 WebView 前必须 `env_remove`。
5. **`--use-gl=egl` 在 Electron 44 上无效**：`Requested GL implementation (gl=egl-gles2,angle=none) not found in allowed implementations` → GPU 进程退出。写 `use-gl=angle` + `use-angle=gl`。
6. **asar 路径不能 `exists()`**：`app.asar` 是单文件，路径判定永远 false；只有 runtime 是真实目录时才做存在性检查。
7. **`runtimeDir` 随布局变**：`resources/app.asar` → `runtimeDir = <asar>/dsh`；`deepseek-harness-pkg-*.asar` → `runtimeDir = <asar>`。判错会得到 `ENOENT, node_modules/@deepseek-ai/dsh not found`。

## 还欠的

- **2h 长跑验收**（RSS 曲线、滚动掉帧率）——方法在 `docs/measure.md`，数字还没跑。这是唯一没有对照数字的一项。
- **焦点态 rAF 治理未做 A/B**：失焦 0% 已实测；焦点态是否还有常驻动画在吃 CPU 没测过（`DSH_IDLE_FPS=0` 可关闭失焦治理做对照）。
- **打包版 payload 的宿主供给**：现装以 `optdepends electron` 解决（`/usr/lib/electron42` 被自动识别）。若要让发行版包自给自足，需要把 electron dist 的 `resources/default_app.asar` 与二进制一起装进 `webview-host/`——`cp -al` 硬链接场可以做到，但**没有实测过**（我用符号链接场试过打包版二进制，它仍然只认自己的 `app.asar`，秒退 RC=1，所以必须用真正的 electron dist，不能复用 payload 里的二进制）。
- `patches/` 里的两个补丁还没在 fork 上做过完整 `makepkg` 验证（`bash -n PKGBUILD` 通过）。

## 安装与发版

发行版安装：

```bash
yay -S dsh-hyprland     # 或 paru -S dsh-hyprland
```

这个包**自带 payload**：从上游 tag 现场构建（`pnpm --filter @deepseek-ai/dsh-desktop run package:dir` +
两个 Linux 补丁），装到 `/usr/lib/dsh-hyprland/app`，外壳装到 `/usr/lib/dsh-hyprland/dsh-shell-hyprland`。
因此它 `provides` / `conflicts` / `replaces` **`deepseek-harness-desktop`**：装它会顶替原包，不会再并存一份。
代价是首次构建 15–25 分钟、约 6 GB 临时空间（上游打包阶段会下载 Electron / Node / CPython / wheels 并跑
一组 packed-runtime 冒烟检查）。

在需要代理的网络上构建时加两个环境变量，否则 Node 自己的 `fetch` 与 `@electron/get` 都不会走代理：

```bash
HTTPS_PROXY=http://127.0.0.1:7897 ELECTRON_GET_USE_PROXY=true NODE_USE_ENV_PROXY=1 makepkg -si
```

实测：`dsh-hyprland 0.2.2.rc.2.gdd46f39-1`，包 357 MB，安装后 1.1 GB。

AUR 包里的版本号不再手写：`pkgver()` 从 `shell/Cargo.toml` 取语义版本，再拼上被钉住那个 commit 的
`rev-count` 和短 hash（实测产出 `0.1.0.r3.gc8c7b1c-1`）。以后改外壳只有一条命令：

```bash
~/devel/aur/update.sh            # 改 commit 钉点、pkgrel+1、重建验证、推送 AUR
~/devel/aur/update.sh --dry-run  # 只看会改成什么
```

它做四件事：确认 GitHub 上的 `origin/main` 与本地一致 → 把 `#commit=` 与基版本 `pkgver` 写成当前值 →
`makepkg -f --nodeps` 真构建一遍 → 用构建后的 `PKGBUILD` 重生成 `.SRCINFO` 再
`git push` 到 `ssh://aur@aur.archlinux.org/dsh-hyprland.git`。

顺序有讲究：`makepkg` 会把 `pkgver()` 的结果**写回** `PKGBUILD`（并顺带把 `pkgrel` 重置成 1，这是它在版本变化时的既有行为），所以
`.SRCINFO` 必须在构建**之后**生成，两个文件才一致——先生成后构建的话，`.SRCINFO` 里会留着 `0.1.0` 而 `PKGBUILD` 变成
`0.1.0.r4.g0dab232`。版本号不需要手改 `pkgrel`，`r<N>` 已经随提交数自动递增。

## 为什么不是"重写成 Rust / Tauri(WebKit)"

Chromium 的合成、重排、光栅化与外壳语言无关；V8 里的 JS 也早已 JIT。外壳换语言能省的只有 Electron 主进程早期那 0.6s。真正的启动收益来自：

- V8 编译缓存（`NODE_COMPILE_CACHE`，省 1.5~2.5s；PKGBUILD 构建时反而显式 `NODE_DISABLE_COMPILE_CACHE=1`）
- 别每次启动重建内建插件 link（旧 nightly 在这里会 6.5s~72s 必败重试）
- 别让 host 在启动期探测 pnpm（0.86s）

这三件本仓库都做了，所以 3.47s 是"架构上该有的时间"，不是压缩出来的。
