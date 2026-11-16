// dsh-hyprland WebView 宿主
// ---------------------------------------------------------------------------
// 这一层只做三件事：
//   1. 把 Hyprland + NVIDIA(Wayland) 下 Chromium 该开的开关开对（P4）
//   2. 把渲染进程的帧预算管住：失焦降帧、长列表 content-visibility（P3）
//   3. 长跑兜底：渲染进程工作集超过阈值就重载（P5 的止损版）
// dsh 的服务端（host）由 Rust 外壳拉起，这里不碰。
'use strict'

const { app, BrowserWindow, session } = require('electron')

const URL_IN = process.env.DSH_URL || ''
console.log('[webview] boot pid=' + process.pid + ' url=' + URL_IN)
const IDLE_FPS = Number(process.env.DSH_IDLE_FPS || 15)
const WATCH_RSS_MB = Number(process.env.DSH_WATCH_RSS_MB || 900)
const WATCH_INTERVAL_MS = Number(process.env.DSH_WATCH_INTERVAL_MS || 60_000)

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

const RAF_GOVERNOR = `(() => {
  if (window.__dshRafGovernor) return
  window.__dshRafGovernor = true
  const nat = window.requestAnimationFrame.bind(window)
  const idleMs = 1000 / ${Number.isFinite(IDLE_FPS) && IDLE_FPS > 0 ? IDLE_FPS : 15}
  let last = 0
  window.requestAnimationFrame = (cb) => nat((t) => {
    const lenient = document.hidden || !document.hasFocus()
    if (!lenient) return cb(t)
    if (t - last < idleMs) return
    last = t
    cb(t)
  })
})()`

async function inject(contents) {
  try {
    await contents.insertCSS(FRAME_BUDGET_CSS, { cssOrigin: 'user' })
    await contents.executeJavaScript(RAF_GOVERNOR, true)
  } catch (e) {
    console.warn('[webview] inject failed:', e.message)
  }
}

function applyFocus(contents, focused) {
  const js = `document.documentElement.classList.toggle('dsh-unfocused', ${!focused})`
  contents.executeJavaScript(js, true).catch(() => {})
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
  win.on('focus', () => applyFocus(win.webContents, true))
  win.on('blur', () => applyFocus(win.webContents, false))
  win.on('closed', () => { win = null })

  const { webContents } = win
  webContents.on('did-finish-load', async () => {
    inject(webContents)
    applyFocus(webContents, win.isFocused())
    try {
      const probe = await webContents.executeJavaScript(
        `({ title: document.title, href: location.href, rootKids: document.querySelector('#root')?.childElementCount ?? -1, mods: Object.keys(window.__ModuleLoader__ ? {m:1} : {}) .length })`, true)
      console.log('[webview] loaded', JSON.stringify(probe))
    } catch (e) {
      console.warn('[webview] probe failed:', e.message)
    }
  })
  webContents.on('did-fail-load', (_e, code, desc, url) => {
    console.error(`[webview] did-fail-load ${code} ${desc} ${url}`)
  })
  webContents.on('render-process-gone', (_e, details) => {
    console.error('[webview] renderer gone:', details.reason)
    if (reloads++ < 3) win?.reload()
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
        if (mb > WATCH_RSS_MB && reloads < 5) {
          console.warn(`[webview] renderer ${mb}MB > ${WATCH_RSS_MB}MB → reload`)
          reloads++
          win.webContents.reload()
          break
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
