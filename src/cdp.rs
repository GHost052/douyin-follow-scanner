//! 极简 CDP（Chrome DevTools Protocol）客户端。
//!
//! 直接驱动系统已安装的 Edge / Chrome：不需要 Playwright，也不需要下载浏览器内核。
//! 连接建立后把 TCP 流一分为二，读线程与写线程各持一半，避免读写互相阻塞。

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{Message, Role, WebSocket};

type Handler = Arc<dyn Fn(Value, String) + Send + Sync>;

// --------------------------------------------------------------------------- //
// 浏览器定位
// --------------------------------------------------------------------------- //
pub fn find_browser(channel: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit {
        if std::path::Path::new(p).is_file() {
            return Some(p.to_string());
        }
    }
    let edge = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/microsoft-edge",
    ];
    let chrome = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
    ];
    let order: [&[&str]; 2] = if channel == "chrome" { [&chrome, &edge] } else { [&edge, &chrome] };
    for group in order {
        for p in group {
            if std::path::Path::new(p).is_file() {
                return Some(p.to_string());
            }
        }
    }
    None
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

fn port_alive(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(400),
    )
    .is_ok()
}

/// 按 DevTools 端口反查并强杀浏览器进程树。
///
/// 浏览器有时会"自我重启"，我们启动的那个进程会先退出、真正的浏览器还在跑，
/// 这时只能通过端口找到它。
pub fn force_kill_by_port(port: u16) {
    #[cfg(windows)]
    {
        let Ok(out) = Command::new("netstat").args(["-ano", "-p", "TCP"]).output() else { return };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut pids = Vec::new();
        for line in text.lines() {
            if line.contains(&format!(":{port}")) && line.to_uppercase().contains("LISTENING") {
                if let Some(pid) = line.split_whitespace().last() {
                    if pid.chars().all(|c| c.is_ascii_digit()) && pid != "0" {
                        pids.push(pid.to_string());
                    }
                }
            }
        }
        for pid in pids {
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid]).output();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill").arg("-f").arg(format!("remote-debugging-port={port}")).output();
    }
}

/// 清理上次异常退出留下的 profile 锁（仅在启动失败时调用）。
fn remove_stale_locks(dir: &str) {
    for name in ["lockfile", "SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = std::fs::remove_file(std::path::Path::new(dir).join(name));
    }
}

// --------------------------------------------------------------------------- //
// 端口记录：用于识别「上次遗留、仍在运行的浏览器」
//
// 应用被强杀时，浏览器进程会被遗留下来并占住 profile。此时再启动新实例，
// 浏览器只会委派给已有实例然后立刻退出，我们等待的新端口永远不会就绪。
// 因此把端口记下来，下次启动时先看看旧实例还在不在。
// --------------------------------------------------------------------------- //
fn port_file(user_data_dir: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(user_data_dir);
    dir.parent().unwrap_or(dir).join("browser.port")
}

fn read_saved_port(user_data_dir: &str) -> Option<u16> {
    std::fs::read_to_string(port_file(user_data_dir)).ok()?.trim().parse().ok()
}

fn save_port(user_data_dir: &str, port: u16) {
    let _ = std::fs::write(port_file(user_data_dir), port.to_string());
}

fn clear_port(user_data_dir: &str) {
    let _ = std::fs::remove_file(port_file(user_data_dir));
}

/// 如果 profile 正被某个浏览器实例占用，先把它清掉，避免白白等一次超时。
///
/// 判断方法：Chromium 会在数据目录写一个 lockfile 并持有它。
/// 如果这个文件能被改名，说明没人占用；改不动就是有进程在用。
fn ensure_profile_free(user_data_dir: &str) {
    let dir = std::path::Path::new(user_data_dir);
    let lock = dir.join("lockfile");
    if !lock.exists() {
        return;
    }
    let probe = dir.join("lockfile.probe");
    if std::fs::rename(&lock, &probe).is_ok() {
        let _ = std::fs::rename(&probe, &lock);
        return;
    }
    // 改不动 → 有浏览器占着这个 profile
    kill_browsers_using_profile(user_data_dir);
    thread::sleep(Duration::from_millis(800));
}

/// 杀掉所有正在使用指定 profile 的浏览器进程。
///
/// 仅在启动失败时作为兜底手段使用：应用被强杀会留下孤儿浏览器，
/// 它占着 profile 会让后续启动全部失败（新进程只会委派给旧实例然后退出）。
fn kill_browsers_using_profile(user_data_dir: &str) {
    #[cfg(windows)]
    {
        let escaped = user_data_dir.replace('\'', "''");
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"name='msedge.exe' or name='chrome.exe'\" \
             -ErrorAction SilentlyContinue | \
             Where-Object {{ $_.CommandLine -like '*{escaped}*' }} | \
             ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill").arg("-f").arg(user_data_dir).output();
    }
}

// --------------------------------------------------------------------------- //
// CDP 连接
// --------------------------------------------------------------------------- //
pub struct Cdp {
    tx: Sender<Message>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    handlers: Mutex<HashMap<(String, String), Vec<Handler>>>,
    closed: AtomicBool,
}

impl Cdp {
    fn connect(url: &str) -> Result<Arc<Self>, String> {
        // path 由 tungstenite 的 into_client_request 解析，这里只用到主机与端口
        let (host, port, _path) = parse_ws_url(url)?;
        let stream = TcpStream::connect((host.as_str(), port)).map_err(|e| format!("连接 CDP 失败: {e}"))?;
        stream.set_nodelay(true).ok();

        let request = url.into_client_request().map_err(|e| format!("构造握手请求失败: {e}"))?;
        let (ws, _resp) = tungstenite::client::client(request, stream)
            .map_err(|e| format!("CDP 握手失败: {e}"))?;

        // 握手完成后再拆分读写：此后两个方向互不影响。
        let raw = ws.into_inner();
        let read_half = raw.try_clone().map_err(|e| e.to_string())?;
        let reader = WebSocket::from_raw_socket(read_half, Role::Client, None);
        let writer = WebSocket::from_raw_socket(raw, Role::Client, None);

        let (tx, rx) = mpsc::channel::<Message>();
        let me = Arc::new(Cdp {
            tx,
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });

        // 写线程
        thread::spawn(move || {
            let mut writer = writer;
            while let Ok(msg) = rx.recv() {
                if writer.send(msg).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            let _ = writer.close(None);
        });

        // 读线程
        let reader_state = Arc::clone(&me);
        thread::spawn(move || {
            let mut reader = reader;
            loop {
                match reader.read() {
                    Ok(Message::Text(txt)) => reader_state.dispatch(txt.as_str()),
                    Ok(Message::Binary(bin)) => {
                        if let Ok(s) = std::str::from_utf8(&bin) {
                            reader_state.dispatch(s);
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ = reader_state.tx.send(Message::Pong(payload));
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            reader_state.closed.store(true, Ordering::SeqCst);
            // 唤醒所有等待者，避免调用方一直挂着
            for (_, tx) in reader_state.pending.lock().unwrap().drain() {
                let _ = tx.send(Value::Null);
            }
        });

        Ok(me)
    }

    fn dispatch(&self, raw: &str) {
        let Ok(v) = serde_json::from_str::<Value>(raw) else { return };

        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                let _ = tx.send(v);
            }
            return;
        }
        let Some(method) = v.get("method").and_then(Value::as_str) else { return };
        let session = v.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let params = v.get("params").cloned().unwrap_or(Value::Null);

        let handlers = self.handlers.lock().unwrap();
        for key in [(session, method.to_string()), ("*".to_string(), method.to_string())] {
            if let Some(list) = handlers.get(&key) {
                for f in list {
                    f(params.clone(), key.0.clone());
                }
            }
        }
    }

    pub fn on<F>(&self, method: &str, session: &str, f: F)
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        let handler: Handler = Arc::new(move |p, _s| f(p));
        self.handlers
            .lock()
            .unwrap()
            .entry((session.to_string(), method.to_string()))
            .or_default()
            .push(handler);
    }

    pub fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, String> {
        self.send_timeout(method, params, session, Duration::from_secs(30))
    }

    pub fn send_timeout(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("CDP 连接已断开".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        if self.tx.send(Message::text(msg.to_string())).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("CDP 写入失败".into());
        }

        match rx.recv_timeout(timeout) {
            Ok(v) if v.is_null() => Err("CDP 连接已关闭".into()),
            Ok(v) => {
                if let Some(err) = v.get("error") {
                    let m = err.get("message").and_then(Value::as_str).unwrap_or("CDP 错误");
                    return Err(m.to_string());
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("CDP 超时: {method}"))
            }
        }
    }
}

fn parse_ws_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url.strip_prefix("ws://").ok_or("只支持 ws:// 地址")?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (host_port.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

// --------------------------------------------------------------------------- //
// 页面
// --------------------------------------------------------------------------- //
pub struct Page {
    cdp: Arc<Cdp>,
    pub session_id: String,
}

impl Page {
    /// 在页面里执行 JS，返回可 JSON 化的结果。
    pub fn evaluate(&self, expression: &str) -> Result<Value, String> {
        let res = self.cdp.send_timeout(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "awaitPromise": true,
                "returnByValue": true,
                "allowUnsafeEvalBlockedByCSP": true,
            }),
            Some(&self.session_id),
            Duration::from_secs(40),
        )?;
        if let Some(detail) = res.get("exceptionDetails") {
            let desc = detail
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| detail.get("text").and_then(Value::as_str))
                .unwrap_or("页面脚本执行出错");
            return Err(desc.lines().next().unwrap_or("页面脚本执行出错").to_string());
        }
        Ok(res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }

    pub fn goto(&self, url: &str) {
        let done = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let d = Arc::clone(&done);
        self.cdp.on("Page.loadEventFired", &self.session_id, move |_| {
            let (m, c) = &*d;
            *m.lock().unwrap() = true;
            c.notify_all();
        });
        if self.cdp.send("Page.navigate", json!({ "url": url }), Some(&self.session_id)).is_err() {
            return;
        }
        let (m, c) = &*done;
        let guard = m.lock().unwrap();
        if !*guard {
            let _ = c.wait_timeout(guard, Duration::from_secs(30));
        }
    }
}

// --------------------------------------------------------------------------- //
// 浏览器
// --------------------------------------------------------------------------- //
pub struct Browser {
    proc: Option<Child>,
    pub cdp: Arc<Cdp>,
    pub port: u16,
    pub version: String,
    /// true 表示复用了上次遗留的浏览器实例（没有新起进程）
    pub reused: bool,
    user_data_dir: String,
}

impl Browser {
    /// 启动浏览器。
    ///
    /// 会先检查上次遗留的实例：能复用就复用，模式不符（比如需要弹窗但旧的是无窗口）
    /// 就先把它关掉再新起一个。失败时自动清理残留锁并重试一次。
    pub fn launch(
        channel: &str,
        executable: Option<&str>,
        headless: bool,
        user_data_dir: &str,
    ) -> Result<Self, String> {
        if let Some(b) = Self::try_reuse(user_data_dir, headless) {
            return Ok(b);
        }
        // 先确保 profile 没被遗留实例占用，否则新进程只会委派给它然后退出
        ensure_profile_free(user_data_dir);
        match Self::launch_once(channel, executable, headless, user_data_dir) {
            Ok(b) => Ok(b),
            Err(first_err) => {
                // 常见原因：上次遗留的浏览器实例还占着 profile（应用被强杀时会这样）。
                // 清掉锁和残留进程后重试一次。
                remove_stale_locks(user_data_dir);
                kill_browsers_using_profile(user_data_dir);
                thread::sleep(Duration::from_millis(800));
                Self::launch_once(channel, executable, headless, user_data_dir)
                    .map_err(|second_err| format!("{second_err}\n（首次尝试失败：{first_err}）"))
            }
        }
    }

    /// 尝试复用上次遗留的浏览器实例；模式不符则先关掉它。
    fn try_reuse(user_data_dir: &str, want_headless: bool) -> Option<Self> {
        let port = read_saved_port(user_data_dir)?;
        if !port_alive(port) {
            clear_port(user_data_dir);
            return None;
        }
        let body = http_get(&format!("http://127.0.0.1:{port}/json/version")).ok()?;
        let v: Value = serde_json::from_str(&body).ok()?;
        let ws = v.get("webSocketDebuggerUrl").and_then(Value::as_str)?.to_string();
        let ua = v.get("User-Agent").and_then(Value::as_str).unwrap_or("");
        let version = v.get("Browser").and_then(Value::as_str).unwrap_or("").to_string();
        // 无窗口模式下 User-Agent 会带 Headless
        let is_headless = ua.contains("Headless");

        if is_headless != want_headless {
            // 模式不符，先把这个旧实例关掉
            if let Ok(cdp) = Cdp::connect(&ws) {
                let _ = cdp.send_timeout("Browser.close", json!({}), None, Duration::from_secs(3));
            }
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(250));
                if !port_alive(port) {
                    break;
                }
            }
            if port_alive(port) {
                force_kill_by_port(port);
            }
            clear_port(user_data_dir);
            return None;
        }

        let cdp = Cdp::connect(&ws).ok()?;
        Some(Browser { proc: None, cdp, port, version, reused: true, user_data_dir: user_data_dir.to_string() })
    }

    fn launch_once(
        channel: &str,
        executable: Option<&str>,
        headless: bool,
        user_data_dir: &str,
    ) -> Result<Self, String> {
        let exe = find_browser(channel, executable)
            .ok_or("未找到 Edge 或 Chrome，请先安装其中之一")?;
        std::fs::create_dir_all(user_data_dir).map_err(|e| e.to_string())?;

        let port = free_port();
        let mut args: Vec<String> = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={user_data_dir}"),
            "--window-size=1280,860".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-blink-features=AutomationControlled".into(),
            "--disable-features=Translate,MediaRouter".into(),
            "--disable-background-networking".into(),
            "--disable-sync".into(),
            "--metrics-recording-only".into(),
            "--no-service-autorun".into(),
        ];
        if headless {
            args.push("--headless=new".into());
            args.push("--disable-gpu".into());
        } else {
            args.push("--start-maximized".into());
        }
        args.push("about:blank".into());

        let mut cmd = Command::new(&exe);
        cmd.args(&args).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut proc = cmd.spawn().map_err(|e| format!("启动浏览器失败: {e}"))?;

        // 后台收集浏览器输出，启动失败时能给出真实原因（否则只有一句笼统的提示）
        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        if let Some(err) = proc.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let mut reader = BufReader::new(err);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let mut b = buf.lock().unwrap();
                            b.push_str(&line);
                            if b.len() > 4000 {
                                let cut = b.len() - 4000;
                                b.drain(0..cut);
                            }
                        }
                    }
                }
            });
        }

        // 关键：浏览器可能"自我重启"，我们启动的进程会先退出。
        // 因此以「DevTools 端口是否就绪」判断成败，而不是看进程是否存活。
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut version = String::new();
        let mut ws_url = String::new();
        while Instant::now() < deadline {
            if let Ok(body) = http_get(&format!("http://127.0.0.1:{port}/json/version")) {
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    if let Some(u) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                        ws_url = u.to_string();
                        version = v.get("Browser").and_then(Value::as_str).unwrap_or("").to_string();
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
        if ws_url.is_empty() {
            force_kill_by_port(port);
            let collected = stderr_buf.lock().unwrap().clone();
            let lines: Vec<&str> = collected.lines().filter(|l| !l.trim().is_empty()).collect();
            let detail = lines.join(" ｜ ");
            let mut tail: String = detail.chars().rev().take(300).collect();
            tail = tail.chars().rev().collect();
            return Err(format!(
                "浏览器启动失败：{}",
                if tail.trim().is_empty() {
                    "DevTools 端口未就绪。浏览器可能已被其它程序占用该数据目录，或被安全软件拦截".to_string()
                } else {
                    format!("浏览器输出：{tail}")
                }
            ));
        }

        save_port(user_data_dir, port);
        let cdp = Cdp::connect(&ws_url)?;
        Ok(Browser { proc: Some(proc), cdp, port, version, reused: false, user_data_dir: user_data_dir.to_string() })
    }

    pub fn new_page(&self) -> Result<Page, String> {
        let target = self.cdp.send("Target.createTarget", json!({ "url": "about:blank" }), None)?;
        let target_id = target.get("targetId").and_then(Value::as_str).ok_or("创建页面失败")?.to_string();
        let session = self.cdp.send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
        )?;
        let session_id = session.get("sessionId").and_then(Value::as_str).ok_or("附加页面失败")?.to_string();

        for domain in ["Page", "Runtime", "Network"] {
            self.cdp.send(&format!("{domain}.enable"), json!({}), Some(&session_id))?;
        }

        let page = Page { cdp: Arc::clone(&self.cdp), session_id };
        Ok(page)
    }

    /// 监听页面请求（含请求方法与 POST 数据），用于捕获「取关」这类写操作请求。
    pub fn watch_requests_full(
        &self,
        page: &Page,
        f: impl Fn(String, String, String) + Send + Sync + 'static,
    ) {
        self.cdp.on("Network.requestWillBeSent", &page.session_id, move |p| {
            let req = p.get("request").cloned().unwrap_or(Value::Null);
            let method = req.get("method").and_then(Value::as_str).unwrap_or("GET").to_string();
            let Some(url) = req.get("url").and_then(Value::as_str).map(str::to_string) else {
                return;
            };
            let post = req.get("postData").and_then(Value::as_str).unwrap_or("").to_string();
            f(method, url, post);
        });
    }

    /// 监听某个页面上发出的所有请求 URL。
    pub fn watch_requests(&self, page: &Page, f: impl Fn(String) + Send + Sync + 'static) {
        self.cdp.on("Network.requestWillBeSent", &page.session_id, move |p| {
            if let Some(u) = p.get("request").and_then(|r| r.get("url")).and_then(Value::as_str) {
                f(u.to_string());
            }
        });
    }

    pub fn cookies(&self) -> Vec<Value> {
        self.cdp
            .send("Storage.getCookies", json!({}), None)
            .ok()
            .and_then(|r| r.get("cookies").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    }

    /// 彻底关闭：CDP 优雅关闭 → 等端口消失 → 按端口兜底强杀。
    pub fn close(&mut self) {
        let _ = self.cdp.send_timeout("Browser.close", json!({}), None, Duration::from_secs(3));
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(250));
            if !port_alive(self.port) {
                break;
            }
        }
        if port_alive(self.port) {
            force_kill_by_port(self.port);
        }
        if let Some(p) = self.proc.as_mut() {
            let _ = p.kill();
        }
        // 只有端口记录还指向自己时才清除，避免误删新实例的记录
        if read_saved_port(&self.user_data_dir) == Some(self.port) {
            clear_port(&self.user_data_dir);
        }
    }
}

/// 极简 HTTP GET（只用于读取本机 DevTools 端口信息）
pub fn http_get(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("只支持 http://")?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(host_port).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    use std::io::Read;
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(text.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}
