// 只暴露一个标记，供前端判断"跑在自己的外壳里"。
// DOM 的帧预算与失焦降帧在 main.cjs / 注入脚本里做，preload 保持最小面。
'use strict'

const { contextBridge } = require('electron')

contextBridge.exposeInMainWorld('__dshShell', {
  kind: 'dsh-hyprland',
  version: process.env.DSH_SHELL_VERSION || '0.1.0',
  ozone: process.env.ELECTRON_OZONE_PLATFORM_HINT || 'auto',
})
