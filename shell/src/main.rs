//! dsh-shell-hyprland —— Tauri 式 Rust 外壳。
//!
//! 职责分工（和 DSH 官方 nightly 的 tauri_runtime_electron 一致）：
//!   Rust 外壳   = 单实例锁、解析 payload 布局、拉起 dsh host、等 ready、拉起 WebView、退出清理
//!   Electron    = 只当 WebView 宿主（`shell/webview/main.cjs`），带 Hyprland/NVIDIA 的 Chromium 参数
//!   dsh host    = 真正的服务端（Node），cli 不变
//!
//! 为什么 WebView 仍然用 Electron 而不是 WebKitGTK：渲染引擎不变，前端 43 个 client bundle
//! 的兼容性风险为零；外壳这一层换语言只省下 Electron 主进程早期那 ~0.6s，真正的启动收益来自
//! host 侧（V8 编译缓存）与帧预算（见 webview/main.cjs）。
//!
//! 用法：
//!   dsh-shell-hyprland [--app DIR] [--profile desktop] [--headless] [--port N] [--replace]

use std::env;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HOST_ENTRY_SUFFIX: &str = "node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js";
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// 裸端口兜底的等待窗口：短于这个时间只认 IPC（带 token 的地址）
const PORT_FALLBACK_AFTER: Duration = Duration::from_secs(60);

// ─── 极薄的 libc 绑定（不引 crate，外壳零依赖） ───────────────────────────────
mod sys {
    extern "C" {
        /// AF_UNIX / SOCK_STREAM 的 socketpair —— Node 的 IPC 通道必须是 socket，
        /// 普通 pipe 上 process.send 会 write EINVAL。
        pub fn socketpair(domain: i32, kind: i32, protocol: i32, sv: *mut i32) -> i32;
        pub fn dup2(from: i32, to: i32) -> i32;
        pub fn close(fd: i32) -> i32;
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
    pub const AF_UNIX: i32 = 1;
    pub const SOCK_STREAM: i32 = 1;
    pub const SIGTERM: i32 = 15;
    pub const SIGKILL: i32 = 9;
}

// ─── 参数 ────────────────────────────────────────────────────────────────────
struct Args {
    app: Option<PathBuf>,
    profile: String,
    headless: bool,
    port: Option<u16>,
    replace: bool,
}

fn parse_args() -> Args {
    let mut a = Args { app: None, profile: "desktop".into(), headless: false, port: None, replace: false };
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--app" => a.app = it.next().map(PathBuf::from),
            "--profile" => a.profile = it.next().unwrap_or_else(|| "desktop".into()),
            "--headless" => a.headless = true,
            "--replace" => a.replace = true,
            "--port" => a.port = it.next().and_then(|v| v.parse().ok()),
            "-h" | "--help" => {
                println!("用法: dsh-shell-hyprland [--app DIR] [--profile NAME] [--headless] [--port N] [--replace]");
                std::process::exit(0);
            }
            other => eprintln!("[shell] 忽略未知参数: {other}"),
        }
    }
    a
}

// ─── 布局：把 Electron 包与 Tauri 包两种安装形态都认出来 ──────────────────────
struct Layout {
    /// Electron 运行时二进制（既当 node 用，也当 WebView 宿主）
    electron: PathBuf,
    /// dsh runtime 根：app.asar 或 deepseek-harness-pkg-linux.asar（Electron fs 能当目录读）
    runtime: PathBuf,
    host_entry: PathBuf,
    profile_dir: PathBuf,
    /// primary runtime 目录（捆绑的 node/pnpm/python 载荷所在），可空
    primary: Option<PathBuf>,
    pnpm: Option<PathBuf>,
    /// 未打包的 Electron，只用来当 WebView 宿主
    webview_electron: Option<PathBuf>,
}

fn exe_dir() -> PathBuf {
    env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

/// 找 Electron 运行时二进制。三种布局都要认：
///   发行版：`<app>/deepseek-harness`（228MB，直接在根）
///   nightly：`<app>/runtime/linux-x64/<品牌名>`
///   旧版：`<app>/resources/runtime/linux-x64/…`
fn find_electron(app: &Path) -> Option<PathBuf> {
    for rel in [".", "runtime/linux-x64", "resources/runtime/linux-x64"] {
        let dir = app.join(rel);
        if !dir.is_dir() {
            continue;
        }
        let mut best: Option<(u64, PathBuf)> = None;
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                // Electron 运行时本体是最大的那个可执行、无扩展名文件
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.contains('.') || name == "chrome-sandbox" || name == "chrome_crashpad_handler" {
                    continue;
                }
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(s, _)| size > *s).unwrap_or(true) {
                    best = Some((size, p));
                }
            }
        }
        if let Some((_, p)) = best {
            return Some(p);
        }
    }
    None
}

/// WebView 宿主必须是「未打包」的 Electron：`resources/` 下只有 default_app.asar。
/// 打包版（resources/app.asar）会无视 argv 里的 app 目录、永远跑它自己那份 main.js，
/// 所以我们不能拿主程序二进制来当 WebView 宿主。
fn find_bare_electron(app: &Path) -> Option<PathBuf> {
    if let Some(p) = env::var_os("DSH_WEBVIEW_ELECTRON").map(PathBuf::from) {
        if p.exists() {
            return Some(p);
        }
    }
    // “裸” Electron 的判据：同目录下有 resources/default_app.asar，且没有 resources/app.asar。
    // 打包版把自己的 main 放在 app.asar 里，arg 给的 app 目录会被它无视（进程秒退、RC=0、无输出）。
    let dirs = [
        app.join("runtime/linux-x64"),
        app.join("webview-host"),
        app.join("resources/webview-host"),
    ];
    for dir in dirs {
        if dir.join("resources/app.asar").exists() || !dir.join("resources/default_app.asar").exists() {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut best: Option<(u64, PathBuf)> = None;
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !p.is_file() || name.contains('.') || name.starts_with("chrome") {
                continue;
            }
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if size < 1_000_000 {
                continue;
            }
            if best.as_ref().map(|(s, _)| size > *s).unwrap_or(true) {
                best = Some((size, p));
            }
        }
        if let Some((_, p)) = best {
            return Some(p);
        }
    }
    // 系统装了 electron 包（Arch: /usr/lib/electron、/usr/lib/electron42…）也能用。
    for root in ["/usr/lib", "/usr/lib64"] {
        let Ok(rd) = fs::read_dir(root) else { continue };
        let mut found: Vec<PathBuf> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("electron") {
                continue;
            }
            let base = e.path();
            let bin = base.join("electron");
            if bin.exists() && base.join("resources/default_app.asar").exists() {
                found.push(bin);
            }
        }
        found.sort(); // electron < electron42：优先不带版本号的
        if let Some(p) = found.pop() {
            return Some(p);
        }
    }
    None
}

fn find_runtime(app: &Path) -> Option<PathBuf> {
    let cands = [
        app.join("resources/app.asar"),
        app.join("dependencies/dsh/deepseek-harness-pkg-linux.asar"),
    ];
    cands.into_iter().find(|p| p.exists())
}

fn discover(args: &Args) -> Result<Layout, String> {
    let app = args
        .app
        .clone()
        .or_else(|| env::var_os("DSH_APP_DIR").map(PathBuf::from))
        .unwrap_or_else(|| exe_dir().join(".."));

    let electron = find_electron(&app).ok_or_else(|| format!("在 {} 下找不到 Electron 运行时", app.display()))?;
    let asar = find_runtime(&app).ok_or_else(|| format!("在 {} 下找不到 dsh runtime asar", app.display()))?;
    // runtimeDir 的约定因布局而异：
    //   app.asar（Electron 应用）    → runtimeDir = <asar>/dsh
    //   deepseek-harness-pkg-*.asar  → runtimeDir = <asar>
    let runtime = match env::var_os("DSH_RUNTIME_DIR") {
        Some(v) => PathBuf::from(v),
        None if asar.ends_with("app.asar") => asar.join("dsh"),
        None => asar.clone(),
    };
    let host_entry = runtime.join(HOST_ENTRY_SUFFIX);
    // asar 是单文件，路径 exists() 永远 false —— 只有真实目录才做存在性检查
    if asar.is_dir() && !host_entry.exists() {
        return Err(format!("host 入口不存在: {}", host_entry.display()));
    }

    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let dsh_home = env::var_os("DSH_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".dsh"));
    let profile_dir = dsh_home.join("profiles").join(&args.profile);

    let primary = [app.join("resources/runtime/primary-runtime"), app.join("dependencies/primary-runtime")]
        .into_iter()
        .find(|p| p.is_dir());
    let pnpm = [app.join("resources/runtime/pnpm/bin/pnpm.cjs"), app.join("dependencies/pnpm/bin/pnpm.cjs")]
        .into_iter()
        .find(|p| p.exists());

    let webview_electron = find_bare_electron(&app);
    Ok(Layout { electron, runtime, host_entry, profile_dir, primary, pnpm, webview_electron })
}

// ─── 单实例 ──────────────────────────────────────────────────────────────────
fn acquire_lock(replace: bool) -> Result<PathBuf, String> {
    let dir = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let lock = dir.join("dsh-shell-hyprland.lock");
    if let Ok(existing) = fs::read_to_string(&lock) {
        if let Ok(pid) = existing.trim().parse::<i32>() {
            let alive = Path::new(&format!("/proc/{pid}")).exists();
            if alive && !replace {
                return Err(format!("已有实例在跑 (pid={pid})；要替换就加 --replace"));
            }
            if alive {
                unsafe {
                    sys::kill(pid, sys::SIGTERM);
                }
                thread::sleep(Duration::from_millis(600));
            }
        }
    }
    fs::write(&lock, std::process::id().to_string()).map_err(|e| e.to_string())?;
    Ok(lock)
}

// ─── ready 判定：看 host 有没有在听、能不能应答 HTTP ─────────────────────────
fn spawn_host(layout: &Layout, _args: &Args) -> Result<(Child, File), String> {
    let mut sv = [0i32; 2];
    if unsafe { sys::socketpair(sys::AF_UNIX, sys::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
        return Err("socketpair() 失败".into());
    }
    let (parent_sock, child_sock) = (sv[0], sv[1]);

    let cache_dir = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("dsh-v8-compile-cache");
    let _ = fs::create_dir_all(&cache_dir);

    let mut cmd = Command::new(&layout.electron);
    cmd.arg("--expose-internals")
        .arg("--no-warnings")
        .arg(&layout.host_entry)
        .arg(&layout.runtime)
        .arg(&layout.profile_dir);
    match &layout.primary {
        Some(p) => {
            cmd.arg(p);
        }
        None => {
            cmd.arg("");
        }
    }
    if let Some(pnpm) = &layout.pnpm {
        cmd.arg(pnpm);
        cmd.arg(layout.electron.parent().unwrap_or(Path::new("/")));
    }

    cmd.env("ELECTRON_RUN_AS_NODE", "1")
        .env("NODE_COMPILE_CACHE", &cache_dir)
        .env("NODE_OPTIONS", merge_node_options())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);

    cmd.env("NODE_CHANNEL_FD", "3");

    let cs = child_sock;
    unsafe {
        cmd.pre_exec(move || {
            if sys::dup2(cs, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            sys::close(cs);
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|e| format!("拉起 host 失败: {e}"))?;
    unsafe {
        sys::close(child_sock);
    }
    let sock = unsafe { File::from_raw_fd(parent_sock) };
    Ok((child, sock))
}

fn merge_node_options() -> String {
    let inherited = env::var("NODE_OPTIONS").unwrap_or_default();
    let ours = "--max-semi-space-size=64";
    if inherited.is_empty() {
        ours.to_string()
    } else if inherited.contains("max-semi-space-size") {
        inherited
    } else {
        format!("{inherited} {ours}")
    }
}

/// 从 /proc/<pid>/fd 拿 socket inode，再去 /proc/net/tcp{,6} 找 LISTEN 的本地端口。
/// 不走 Node IPC：那条 fd-3 通道是 libuv 专用的，手搓容易踩 EINVAL。
fn listening_port(pid: i32) -> Option<u16> {
    let fd_dir = format!("/proc/{pid}/fd");
    let mut inodes: Vec<String> = Vec::new();
    for entry in fs::read_dir(&fd_dir).ok()?.flatten() {
        if let Ok(target) = fs::read_link(entry.path()) {
            let t = target.to_string_lossy();
            if let Some(rest) = t.strip_prefix("socket:[") {
                inodes.push(rest.trim_end_matches(']').to_string());
            }
        }
    }
    if inodes.is_empty() {
        return None;
    }
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = fs::read_to_string(table) else { continue };
        for line in content.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            if !inodes.iter().any(|i| i == cols[9]) {
                continue;
            }
            let local = cols[1];
            if let Some(hex) = local.rsplit(':').next() {
                if let Ok(port) = u16::from_str_radix(hex, 16) {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// IPC 行里的 `{"type":"ready","url":"…"}`（JSON 序列化模式，一行一条）
fn url_from_line(line: &str) -> Option<String> {
    if !line.contains("\"ready\"") {
        return None;
    }
    let start = line.find("\"url\":\"")? + 7;
    let rest = &line[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn wait_ready(child: &mut Child, sock: File) -> Result<String, String> {
    let start = Instant::now();
    let deadline = start + READY_TIMEOUT;
    let pid = child.id() as i32;
    let (tx, rx) = mpsc::channel::<String>();
    // 读线程：host 只写不读，读端不会阻塞在这里
    // 读线程要活到进程结束：父端 socket 一关，host 的 process.connected 就变 false，
    // 它的 operation-diagnostics 会抛 INACTIVE_EFFECT 把自己拆掉。
    thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(sock).lines().map_while(Result::ok) {
            if !sent {
                if let Some(url) = url_from_line(&line) {
                    let _ = tx.send(url);
                    sent = true;
                }
            }
        }
    });

    loop {
        if Instant::now() > deadline {
            return Err(format!("等 ready 超时（{}s）", READY_TIMEOUT.as_secs()));
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("host 提前退出: {status}"));
        }
        // 主路径：IPC 报的 url
        if let Ok(url) = rx.recv_timeout(Duration::from_millis(300)) {
            return Ok(url);
        }
        // 兜底只在 IPC 长时间没动静时才用：host 的裸端口要鉴权（401），
        // 真正能打开的是 ready 里带 token 的 authenticatedUrl。
        if deadline.saturating_duration_since(Instant::now()).is_zero() { /* unreachable */ }
        if start.elapsed() > PORT_FALLBACK_AFTER {
            if let Some(port) = listening_port(pid) {
                if http_alive(port) {
                    eprintln!("[shell] 警告：IPC 未报 ready，退回裸端口 {port}（可能要手动带 token）");
                    return Ok(format!("http://127.0.0.1:{port}/"));
                }
            }
        }
    }
}

/// 最朴素的 HTTP 探活：发一个 GET /，能读到 "HTTP/" 就算通。
fn http_alive(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration as D;
    let Ok(mut s) = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        D::from_millis(400),
    ) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(D::from_millis(800)));
    let req = format!("GET / HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if std::io::Write::write_all(&mut s, req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) if n >= 5 => buf.starts_with(b"HTTP/"),
        _ => false,
    }
}

// ─── WebView ─────────────────────────────────────────────────────────────────
fn launch_webview(layout: &Layout, url: &str, headless: bool, port: Option<u16>) -> Result<Option<Child>, String> {
    if headless {
        println!("{url}");
        return Ok(None);
    }
    let exe = exe_dir();
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = env::var_os("DSH_WEBVIEW_DIR") {
        cands.push(PathBuf::from(p));
    }
    cands.push(exe.join("../share/dsh-hyprland/webview")); // 安装布局
    cands.push(exe.join("webview"));                       // 同目录摆放
    cands.push(PathBuf::from("/usr/share/dsh-hyprland/webview"));
    cands.push(exe.join("../../shell/webview"));           // 仓库内 target/release
    cands.push(PathBuf::from("shell/webview"));
    let webview = cands
        .into_iter()
        .find(|p| p.join("main.cjs").exists() && p.join("package.json").exists())
        .ok_or_else(|| format!("找不到 webview 目录（找 main.cjs+package.json），试 DSH_WEBVIEW_DIR=…（当前 exe={}）", exe.display()))?;
    let webview = fs::canonicalize(&webview).unwrap_or(webview);
    let host_bin = layout
        .webview_electron
        .as_ref()
        .ok_or_else(|| "找不到未打包的 Electron 当 WebView 宿主（设 DSH_WEBVIEW_ELECTRON 指向它，或把 default_app.asar 布局放到 <app>/runtime/linux-x64）".to_string())?;
    let mut cmd = Command::new(host_bin);
    cmd.arg(&webview)
        // 外层环境（例如从 DSH 的 bash 工具里启动）常常带着这个变量，
        // 会让 Electron 退化成 node 去跑 main.cjs —— 必须显式摘掉
        .env_remove("ELECTRON_RUN_AS_NODE")
        .env("DSH_URL", url)
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .env("ELECTRON_DISABLE_SECURITY_WARNINGS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    if let Some(p) = port {
        cmd.env("DSH_PORT", p.to_string());
    }
    let child = cmd.spawn().map_err(|e| format!("拉起 WebView 失败: {e}"))?;
    eprintln!("[shell] webview pid={} dir={}", child.id(), webview.display());
    Ok(Some(child))
}

fn terminate(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        // 整个进程组一起收（负 pid）
        sys::kill(-pid, sys::SIGTERM);
    }
    thread::sleep(Duration::from_millis(800));
    if child.try_wait().ok().flatten().is_none() {
        unsafe {
            sys::kill(-pid, sys::SIGKILL);
        }
        let _ = child.wait();
    }
}

fn main() {
    let t0 = Instant::now();
    let args = parse_args();
    let lock = match acquire_lock(args.replace) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[shell] {e}");
            std::process::exit(1);
        }
    };
    let layout = match discover(&args) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[shell] {e}");
            let _ = fs::remove_file(&lock);
            std::process::exit(1);
        }
    };
    eprintln!(
        "[shell] electron={} runtime={} profile={}",
        layout.electron.display(),
        layout.runtime.display(),
        layout.profile_dir.display()
    );

    let (mut host, host_sock) = match spawn_host(&layout, &args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[shell] {e}");
            let _ = fs::remove_file(&lock);
            std::process::exit(1);
        }
    };
    let url = match wait_ready(&mut host, host_sock) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[shell] {e}");
            terminate(&mut host);
            let _ = fs::remove_file(&lock);
            std::process::exit(1);
        }
    };
    eprintln!("[shell] host ready: {url} (+{}ms)", t0.elapsed().as_millis());

    match launch_webview(&layout, &url, args.headless, args.port) {
        Ok(Some(mut view)) => {
            let _ = view.wait();
            terminate(&mut host);
        }
        Ok(None) => {
            // headless：前台占着，Ctrl-C 时一起收
            let _ = host.wait();
        }
        Err(e) => {
            eprintln!("[shell] {e}");
            terminate(&mut host);
            let _ = fs::remove_file(&lock);
            std::process::exit(1);
        }
    }
    let _ = fs::remove_file(&lock);
}
