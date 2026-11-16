// dsh-hyprland WebView 宿主
// ---------------------------------------------------------------------------
// 这一层只做三件事：
//   1. 把 Hyprland + NVIDIA(Wayland) 下 Chromium 该开的开关开对（P4）
//   2. 把渲染进程的帧预算管住：失焦降帧、长列表 content-visibility（P3）
//   3. 长跑兜底：渲染进程工作集超过阈值就重载（P5 的止损版）
// dsh 的服务端（host）由 Rust 外壳拉起，这里不碰。
'use strict'

const { app, BrowserWindow, session } = require('electron')

const fs = require('node:fs')
const path = require('node:path')

// 日志落盘：从桌面入口启动时 stdout 没人接，出问题只能靠猜。写到
// $XDG_STATE_HOME/dsh-hyprland/webview.log，同时保留终端输出。
const LOG_DIR = path.join(process.env.XDG_STATE_HOME || path.join(process.env.HOME || '.', '.local', 'state'), 'dsh-hyprland')
const LOG_FILE = path.join(LOG_DIR, 'webview.log')
function logLine(...parts) {
  const line = new Date().toISOString() + ' ' + parts.map((v) => (typeof v === 'string' ? v : JSON.stringify(v))).join(' ')
  try { fs.mkdirSync(LOG_DIR, { recursive: true }); fs.appendFileSync(LOG_FILE, line + '\n') } catch {}
  try { process.stdout.write(line + '\n') } catch {}
}
const console = { log: logLine, warn: logLine, error: logLine, info: logLine, debug: () => {} }

const URL_IN = process.env.DSH_URL || ''
console.log('[webview] boot pid=' + process.pid + ' url=' + URL_IN)
const IDLE_FPS = Number(process.env.DSH_IDLE_FPS || 15)
const WATCH_RSS_MB = Number(process.env.DSH_WATCH_RSS_MB || 900)
const WATCH_INTERVAL_MS = Number(process.env.DSH_WATCH_INTERVAL_MS || 60_000)
// 用户最后一次按键/输入的时间（主进程视角）。看门狗重载、崩溃重载都要避开它：
// 重载会把没提交的输入清掉，表现就是“打着字突然断了”。
let lastInputAt = 0
const WATCH_IDLE_MS = Number(process.env.DSH_WATCH_IDLE_MS || 30_000)
// rAF 治理的输入宽限：刚有输入（含输入法组合中）的这段时间不压帧。
const INPUT_GRACE_MS = Number(process.env.DSH_INPUT_GRACE_MS || 2_000)

// ── P4：Hyprland / Wayland / NVIDIA ─────────────────────────────────────────
app.commandLine.appendSwitch('ozone-platform-hint', 'auto')
app.commandLine.appendSwitch('enable-features', 'WaylandLinuxDrmSyncobj,CanvasOopRasterization')
app.commandLine.appendSwitch('disable-features', 'CalculateNativeWinOcclusion,HardwareMediaKeyHandling')
// Electron 44 的 GL 走 ANGLE：`--use-gl=egl` 会报 "not found in allowed implementations"，
// GPU 进程直接退出 → 整窗掉到软件光栅。桌面 GL 必须写成 angle + gl。
app.commandLine.appendSwitch('use-gl', 'angle')
app.commandLine.appendSwitch('use-angle', 'gl')
app.commandLine.appendSwitch('enable-zero-copy')
app.commandLine.appendSwitch('force_high_performance_gpu')
if (process.env.DSH_RENDER_NODE) app.commandLine.appendSwitch('render-node-override', process.env.DSH_RENDER_NODE)
app.commandLine.appendSwitch('disable-breakpad')
// Wayland 文本输入：不开这个，Electron 不接 text-input-v3，fcitx5 的中文输入会
// 断字、候选窗不跟光标、上屏丢字符。GTK_IM_MODULE 在 Hyprland 会话里常常没导出，
// 所以这条不能指望环境变量。
app.commandLine.appendSwitch('enable-wayland-ime')
app.commandLine.appendSwitch('wayland-text-input-version', '3')
// 调试口：DSH_DEBUG_PORT=9222 时才开，验收脚本（CDP）靠它注入测帧 snippet。
if (process.env.DSH_DEBUG_PORT) {
  app.commandLine.appendSwitch('remote-debugging-port', process.env.DSH_DEBUG_PORT)
  app.commandLine.appendSwitch('remote-allow-origins', '*')
}

let win = null
let reloads = 0

function isLocal(url) {
  try {
    const u = new URL(url)
    return (u.protocol === 'http:' || u.protocol === 'https:') &&
      ['127.0.0.1', 'localhost', '[::1]'].includes(u.hostname)
  } catch {
    return false
  }
}

// ── P3：失焦降帧 + 长列表跳过离屏渲染 ────────────────────────────────────────
const FRAME_BUDGET_CSS = `
[data-turn]:not(:last-child) { content-visibility: auto; contain-intrinsic-size: auto 220px; }
html.dsh-unfocused *, html.dsh-unfocused *::before, html.dsh-unfocused *::after {
  animation-play-state: paused !important;
}
`

const MEDIA_GUARD = `(() => {
  if (window.__dshMediaGuard) return
  window.__dshMediaGuard = true
  window.__dshBlockedMedia = new Set()
  // 失焦只 pause 一次不够：SPA 之后还会渲染新的 <video> 并 autoplay。
  // 抓 play 事件，在失焦状态下把新起的播放按回去。
  document.addEventListener('play', (e) => {
    const v = e.target
    if (!v || v.tagName !== 'VIDEO') return
    if (window.__dshFocused === false) {
      try { v.pause() } catch {}
      window.__dshBlockedMedia.add(v)
    }
  }, true)
  window.__dshResumeMedia = () => {
    for (const v of window.__dshBlockedMedia) { try { v.play() } catch {} }
    window.__dshBlockedMedia.clear()
  }
})()`

const RAF_GOVERNOR = `(() => {
  if (window.__dshRafGovernor) return
  window.__dshRafGovernor = true
  const nat = window.requestAnimationFrame.bind(window)
  const idleMs = 1000 / ${Number.isFinite(IDLE_FPS) && IDLE_FPS > 0 ? IDLE_FPS : 15}
  let last = 0
  let lastInput = -1e9
  const mark = () => { lastInput = performance.now() }
  for (const ev of ['keydown', 'keyup', 'input', 'compositionstart', 'compositionupdate',
                    'compositionend', 'pointerdown', 'pointermove', 'wheel'])
    addEventListener(ev, mark, { capture: true, passive: true })
  window.requestAnimationFrame = (cb) => nat((t) => {
    const unfocused = document.hidden || window.__dshFocused === false
    if (!unfocused) return cb(t)
    // 刚有输入就不要压帧：压到 15fps 时输入法组合、光标跟随会看起来“卡住/断字”。
    if (t - lastInput < ${INPUT_GRACE_MS}) return cb(t)
    if (t - last < idleMs) return
    last = t
    cb(t)
  })
})()`

async function inject(contents) {
  try {
    await contents.insertCSS(FRAME_BUDGET_CSS, { cssOrigin: 'user' })
    // DSH_IDLE_FPS=0 → 完全放开 rAF（用来做对照实验，确认降帧治理的贡献）
    if (IDLE_FPS > 0) await contents.executeJavaScript(RAF_GOVERNOR, true)
    await contents.executeJavaScript(MEDIA_GUARD, true)
  } catch (e) {
    console.warn('[webview] inject failed:', e.message)
  }
}

function applyFocus(contents, focused) {
  // 主进程看到的焦点才是准的：文档里的 document.hasFocus() 在 Electron 里
  // 会因为窗口从未拿到键盘焦点、或 DevTools 会话接管而一直是 false，
  // 那样 rAF 治理会永远停在“失焦”档，用户真在用的时候反而被压到 15fps。
  const js = `(window.__focusLog = window.__focusLog || []).push([${focused}, Date.now()]);
    window.__dshFocused = ${focused};
    document.documentElement.classList.toggle('dsh-unfocused', ${!focused});
    try {
      if (!${focused}) {
        // 会话里的视频/动图会一直解码（Media + VideoFrameCompositor 线程），
        // 窗口不在前台时没有理由继续烧：暂停，回到前台再接着放。
        window.__dshBlockedMedia = window.__dshBlockedMedia || new Set();
        for (const v of document.querySelectorAll('video')) {
          if (!v.paused) { try { v.pause() } catch {}; window.__dshBlockedMedia.add(v) }
        }
      } else if (window.__dshResumeMedia) {
        window.__dshResumeMedia();
      }
    } catch {}`
  contents.executeJavaScript(js, true).catch((e) => console.warn('[webview] applyFocus failed:', e && e.message))
}

// ── 窗口 ────────────────────────────────────────────────────────────────────
function createWindow(url) {
  win = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 900,
    minHeight: 600,
    show: false,
    autoHideMenuBar: true,
    backgroundColor: '#0F1115',
    title: 'DeepSeek Harness',
    webPreferences: {
      backgroundThrottling: true,   // 失焦时 Chromium 自己也要降频
      spellcheck: false,
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: require('node:path').join(__dirname, 'preload.cjs'),
    },
  })

  win.once('ready-to-show', () => win.show())
  // 自己记焦点：Wayland 下 win.isFocused() 会在窗口真被聚焦后仍返回 false，
  // 用它给 did-finish-load 收尾会把刚设好的“聚焦”又盖回“失焦”。
  let focusedNow = false
  win.on('focus', () => { focusedNow = true; console.log('[webview] focus=true'); applyFocus(win.webContents, true) })
  win.on('blur', () => { focusedNow = false; console.log('[webview] focus=false'); applyFocus(win.webContents, false) })
  win.on('closed', () => { win = null })

  const { webContents } = win
  webContents.on('did-finish-load', async () => {
    inject(webContents)
    applyFocus(webContents, focusedNow)
    try {
      const probe = await webContents.executeJavaScript(
        `({ title: document.title, href: location.href, rootKids: document.querySelector('#root')?.childElementCount ?? -1, mods: Object.keys(window.__ModuleLoader__ ? {m:1} : {}) .length })`, true)
      console.log('[webview] loaded', JSON.stringify(probe))
    } catch (e) {
      console.warn('[webview] probe failed:', e.message)
    }
  })
  // 主进程侧记录输入活动：键盘事件在这里一定能看到（页面里的监听可能被框架吞掉）。
  webContents.on('before-input-event', () => { lastInputAt = Date.now() })
  webContents.on('did-fail-load', (_e, code, desc, url) => {
    console.error(`[webview] did-fail-load ${code} ${desc} ${url}`)
  })
  webContents.on('render-process-gone', (_e, details) => {
    console.error('[webview] renderer gone:', details.reason)
    if (reloads >= 3) { console.error('[webview] reload budget exhausted'); return }
    // 崩溃必须重载，但如果刚刚还在打字，等静默期结束再重载，别把输入吞掉。
    const wait = Math.max(0, WATCH_IDLE_MS - (Date.now() - lastInputAt))
    reloads++
    setTimeout(() => {
      console.error('[webview] reloading after renderer gone, reason=' + details.reason)
      if (win && !win.isDestroyed()) win.webContents.reload()
    }, Math.min(wait, 10_000))
  })
  webContents.setWindowOpenHandler(({ url: target }) => {
    if (!isLocal(target)) return { action: 'deny' }
    return { action: 'allow' }
  })

  win.loadURL(url)
}

// ── P5：长跑看门狗 ──────────────────────────────────────────────────────────
function startWatchdog() {
  setInterval(() => {
    if (!win || win.isDestroyed()) return
    try {
      const tabs = app.getAppMetrics().filter((m) => m.type === 'Tab')
      for (const tab of tabs) {
        const mb = Math.round((tab.memory?.workingSetSize || 0) / 1024)
        const idleFor = Date.now() - lastInputAt
        if (mb > WATCH_RSS_MB && reloads < 5 && idleFor > WATCH_IDLE_MS) {
          console.warn(`[webview] renderer ${mb}MB > ${WATCH_RSS_MB}MB, idle ${Math.round(idleFor / 1000)}s → reload`)
          reloads++
          win.webContents.reload()
          break
        } else if (mb > WATCH_RSS_MB) {
          console.warn(`[webview] renderer ${mb}MB over budget but input ${Math.round(idleFor / 1000)}s ago, deferring reload`)
        }
      }
    } catch (e) {
      console.warn('[webview] watchdog:', e.message)
    }
  }, WATCH_INTERVAL_MS).unref?.()
}

// ── 启动 ────────────────────────────────────────────────────────────────────
if (!isLocal(URL_IN)) {
  console.error(`[webview] DSH_URL 不是本机地址: ${JSON.stringify(URL_IN)}`)
  app.exit(2)
}

app.on('window-all-closed', () => app.quit())
app.on('web-contents-created', (_e, contents) => {
  contents.on('will-navigate', (event, target) => {
    if (!isLocal(target)) event.preventDefault()
  })
})

app.whenReady().then(() => {
  console.log('[webview] ready, creating window')
  // 本地 web UI 需要剪贴板、通知、麦克风（语音输入）；其余一律拒
  const ALLOW = new Set(['clipboard-read', 'clipboard-sanitized-write', 'notifications', 'media', 'fullscreen'])
  session.defaultSession.setPermissionRequestHandler((_wc, permission, cb) => cb(ALLOW.has(permission)))
  session.defaultSession.setPermissionCheckHandler((_wc, permission) => ALLOW.has(permission))

  try {
    createWindow(URL_IN)
    console.log('[webview] window created, id=' + (win && win.id))
  } catch (e) {
    console.error('[webview] createWindow failed:', e && e.stack)
    app.exit(3)
  }
  startWatchdog()
}).catch((e) => { console.error('[webview] whenReady failed:', e && e.stack); app.exit(4) })

process.on('uncaughtException', (e) => console.error('[webview] uncaught:', e && e.stack))
