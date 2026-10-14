//! 本地 HTTP 服务：给界面提供 API，并托管页面。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Speed;
use crate::douyin;
use crate::report;
use crate::scanner;
use crate::state::AppState;

const APP_HTML: &str = include_str!("../assets/app.html");

/// 注入桌面模式标记，界面据此改用本地接口（而不是浏览器那套下载 / 新标签页）
/// 注入桌面标记与接口令牌。
/// 令牌放在 HTML 里就够了：跨站页面受同源策略限制读不到响应，
/// 因此拿不到令牌，也就无法伪造请求。
fn app_html(token: &str) -> &'static str {
    static HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HTML.get_or_init(|| {
        APP_HTML.replace(
            "<head>",
            &format!(
                "<head>\n<script>window.__DESKTOP__ = true; window.__TOKEN__ = {token:?};</script>"
            ),
        )
    })
}

/// 用系统默认程序打开链接或文件
fn open_with_os(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(cmd)
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// 在文件管理器中定位文件
fn reveal_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let dir = path.parent().unwrap_or(path);
        open_with_os(&dir.to_string_lossy())
    }
}

/// 只允许打开抖音主页与本机地址，避免被当成任意命令执行
fn is_allowed_url(url: &str) -> bool {
    url.starts_with("https://www.douyin.com/") || url.starts_with("http://127.0.0.1:")
}

/// 启动界面服务，返回实际绑定的端口（0 表示失败）。
///
/// 端口被占用时依次往后试，而不是直接失败——之前固定 8799，
/// 冲突时窗口已经弹出来了却直接报错退出，体验很差。
pub fn start(state: Arc<AppState>) -> u16 {
    let want = state.cfg().port;
    let mut bound = None;
    for p in want..want.saturating_add(20) {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
            bound = Some((l, p));
            break;
        }
    }
    let Some((listener, port)) = bound else {
        let m = format!("端口 {want}~{} 都被占用，界面服务无法启动", want.saturating_add(19));
        state.log(&m, "err");
        state.notify("err", &m);
        return 0;
    };
    if port != want {
        state.log(&format!("端口 {want} 被占用，已自动改用 {port}"), "warn");
    }
    // 提前把界面构建好（注入令牌），避免并发首次请求时重复构建
    let _ = app_html(&state.token);
    state.log(&format!("界面服务已启动：http://127.0.0.1:{port}/"), "");

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let st = Arc::clone(&state);
            thread::spawn(move || {
                let _ = handle(st, stream);
            });
        }
    });
    port
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: String,
}

fn handle(state: Arc<AppState>, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    let raw = String::from_utf8_lossy(&buf[..n]).to_string();

    let mut lines = raw.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.clone(), Vec::new()),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for l in lines {
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

    // /api/* 一律要求令牌，挡住跨站请求与同机其它程序的误调
    if path.starts_with("/api/") {
        let tok = headers
            .iter()
            .find(|(k, _)| k == "x-token")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if tok != state.token {
            let mut out = String::new();
            out.push_str("HTTP/1.1 403 Forbidden\r\n");
            out.push_str("Content-Type: application/json; charset=utf-8\r\n");
            out.push_str("Cache-Control: no-store\r\n");
            out.push_str("Connection: close\r\n");
            let b = r#"{"ok":false,"message":"缺少或错误的接口令牌"}"#;
            out.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            out.push_str(b);
            let _ = stream.write_all(out.as_bytes());
            return Ok(());
        }
    }

    let req = Request { method, path, query, body };

    match route(&state, &req) {
        Reply::Json(v) => send_json(&mut stream, &v),
        Reply::Html(h) => send_html(&mut stream, &h),
        Reply::Empty => send_json(&mut stream, &json!({ "ok": true })),
    }
    Ok(())
}

enum Reply {
    Json(Value),
    Html(String),
    Empty,
}

fn q<'a>(req: &'a Request, key: &str) -> String {
    req.query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn route(state: &Arc<AppState>, req: &Request) -> Reply {
    // 记录请求，便于排查「窗口白屏」这类问题：能看到请求就说明页面已加载
    {
        use std::sync::atomic::AtomicU64;
        static HITS: AtomicU64 = AtomicU64::new(0);
        let n = HITS.fetch_add(1, Ordering::Relaxed) + 1;
        if req.path != "/api/state" || n <= 3 || n % 40 == 0 {
            state.log(&format!("[HTTP] {} {} (第 {n} 次)", req.method, req.path), "");
        }
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => Reply::Html(app_html(&state.token).to_string()),

        ("GET", "/report") => {
            let p = state.cfg().out_dir.join("report.html");
            Reply::Html(std::fs::read_to_string(p).unwrap_or_else(|_| "<h1>报告尚未生成</h1>".into()))
        }

        ("GET", "/api/state") => Reply::Json(state.snapshot()),

        ("POST", "/api/login") => {
            let st = Arc::clone(state);
            if !st.running.load(Ordering::SeqCst) {
                thread::spawn(move || {
                    st.log("正在打开登录窗口，请扫码…", "");
                    match douyin::login_once(&st.cfg(), |m, l| st.log(m, l), Duration::from_secs(1200)) {
                        Ok(sec) => {
                            *st.logged_in.lock().unwrap() = Some(true);
                            *st.self_sec_uid.lock().unwrap() = sec;
                            st.log("登录成功", "ok");
                        }
                        Err(e) => {
                            *st.logged_in.lock().unwrap() = Some(false);
                            st.log(&format!("登录失败：{e}"), "err");
                        }
                    }
                });
            }
            Reply::Empty
        }

        ("POST", "/api/scan/start") => {
            let st = Arc::clone(state);
            if st.running.swap(true, Ordering::SeqCst) {
                return Reply::Json(json!({ "ok": false, "error": "已有扫描在运行" }));
            }
            st.stopped.store(false, Ordering::SeqCst);
            st.paused.store(false, Ordering::SeqCst);
            st.update(|p| {
                *p = crate::state::ScanProgress::default();
                p.status = "running".into();
                p.phase = "准备中".into();
                p.started_at = Some(crate::state::now_sec() * 1000);
            });
            st.log("开始扫描", "");
            thread::spawn(move || {
                match douyin::open_session(&st.cfg(), true, |m, l| st.log(m, l), Duration::from_secs(60)) {
                    Ok(session) => {
                        *st.logged_in.lock().unwrap() = Some(true);
                        *st.self_sec_uid.lock().unwrap() = session.self_sec_uid.clone();
                        scanner::run_scan(Arc::clone(&st), session);
                    }
                    Err(e) if e == "NEED_LOGIN" => {
                        *st.logged_in.lock().unwrap() = Some(false);
                        st.update(|p| {
                            p.status = "need_login".into();
                            p.wait_text = "尚未登录，请点击「登录抖音」".into();
                        });
                        st.log("尚未登录，请先点击「登录抖音」", "warn");
                        st.notify("warn", "尚未登录，请先点击「登录抖音」");
                    }
                    Err(e) => {
                        st.update(|p| p.status = "error".into());
                        st.log(&format!("扫描出错：{e}"), "err");
                        st.notify("err", &format!("扫描出错：{e}"));
                    }
                }
                st.running.store(false, Ordering::SeqCst);
                let _ = report::write_reports(&st);
            });
            Reply::Empty
        }

        ("POST", "/api/scan/pause") => {
            state.paused.store(true, Ordering::SeqCst);
            Reply::Empty
        }
        ("POST", "/api/scan/resume") => {
            state.paused.store(false, Ordering::SeqCst);
            Reply::Empty
        }
        ("POST", "/api/scan/stop") => {
            state.stopped.store(true, Ordering::SeqCst);
            state.paused.store(false, Ordering::SeqCst);
            // 看门狗：扫描线程若异常退出（例如 panic），running 会永久卡在 true，
            // 之后取关等操作会被「扫描进行中」一直挡住，而且点停止也救不回来。
            // 停止后 30 秒仍未复位就强制复位。
            let st = Arc::clone(state);
            thread::spawn(move || {
                for _ in 0..60 {
                    thread::sleep(Duration::from_millis(500));
                    if !st.running.load(Ordering::SeqCst) {
                        return;
                    }
                }
                if st.running.load(Ordering::SeqCst) {
                    st.running.store(false, Ordering::SeqCst);
                    st.log("扫描线程未正常退出，已强制复位运行状态", "warn");
                }
            });
            Reply::Empty
        }

        ("GET", "/api/results") => {
            let kind = q(req, "type");
            let kw = q(req, "q").to_lowercase();
            let mut rows = state.derive_rows();
            if kind != "all" {
                rows.retain(|r| r["status"] == kind.as_str());
            }
            if !kw.is_empty() {
                rows.retain(|r| {
                    let nick = r["nickname"].as_str().unwrap_or("").to_lowercase();
                    let uid = r["uid"].as_str().unwrap_or("").to_string();
                    nick.contains(&kw) || uid.contains(&kw)
                });
            }
            rows.sort_by(|a, b| {
                b.get("days").and_then(Value::as_i64).unwrap_or(-1)
                    .cmp(&a.get("days").and_then(Value::as_i64).unwrap_or(-1))
            });
            let total = rows.len();
            rows.truncate(1000);
            Reply::Json(json!({ "total": total, "rows": rows }))
        }

        // 只返回主页地址，供「一键打开全部主页」使用。
        // 不走 /api/results 是因为那边为了渲染做了 1000 条截断。
        ("GET", "/api/homepages") => {
            let kind = q(req, "type");
            let kw = q(req, "q").to_lowercase();
            let mut rows = state.derive_rows();
            if kind != "all" {
                rows.retain(|r| r["status"] == kind.as_str());
            }
            if !kw.is_empty() {
                rows.retain(|r| {
                    let nick = r["nickname"].as_str().unwrap_or("").to_lowercase();
                    let uid = r["uid"].as_str().unwrap_or("").to_string();
                    nick.contains(&kw) || uid.contains(&kw)
                });
            }
            let urls: Vec<String> = rows
                .iter()
                .map(|r| r["homeUrl"].as_str().unwrap_or("").to_string())
                .filter(|u| !u.is_empty())
                .collect();
            Reply::Json(json!({ "total": urls.len(), "urls": urls }))
        }

        ("GET", "/api/export") => {
            let kind = q(req, "type");
            let mut rows = state.derive_rows();
            if kind != "all" {
                rows.retain(|r| r["status"] == kind.as_str());
            }
            let head = if kind == "error" {
                "\u{feff}账号名,账号ID,账号URL,异常原因\n"
            } else {
                "\u{feff}账号名,账号ID,账号URL,最后更新日期,距今天数,状态,作品数\n"
            };
            let mut csv = String::from(head);
            for r in &rows {
                let cell = |k: &str| {
                    let v = match r.get(k) {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        _ => String::new(),
                    };
                    format!("\"{}\"", v.replace('"', "\"\""))
                };
                if kind == "error" {
                    csv.push_str(&format!("{},{},{},{}\n", cell("nickname"), cell("uid"), cell("homeUrl"), cell("note")));
                } else {
                    csv.push_str(&format!(
                        "{},{},{},{},{},{},{}\n",
                        cell("nickname"), cell("uid"), cell("homeUrl"),
                        cell("lastPostDate"), cell("days"), cell("status"), cell("awemeCount")
                    ));
                }
            }

            // 桌面应用没有浏览器的下载机制，改为直接写入数据目录并定位到该文件
            let file = state.cfg().out_dir.join(format!("{kind}_accounts.csv"));
            if let Err(e) = std::fs::write(&file, csv) {
                return Reply::Json(json!({ "ok": false, "message": format!("写入失败：{e}") }));
            }
            let _ = reveal_in_file_manager(&file);
            state.log(&format!("已导出 CSV：{}", file.display()), "ok");
            Reply::Json(json!({
                "ok": true,
                "message": format!("已导出 {} 行\n文件位置：{}", rows.len(), file.display()),
                "path": file.to_string_lossy(),
            }))
        }

        ("GET", "/api/open-report") => {
            let file = state.cfg().out_dir.join("report.html");
            if !file.exists() {
                return Reply::Json(json!({ "ok": false, "message": "报告尚未生成，请先扫描一次" }));
            }
            match open_with_os(&file.to_string_lossy()) {
                Ok(_) => Reply::Json(json!({ "ok": true, "message": "已用默认程序打开报告" })),
                Err(e) => Reply::Json(json!({ "ok": false, "message": format!("打开失败：{e}") })),
            }
        }

        ("GET", "/api/open") => {
            let url = q(req, "url");
            if !is_allowed_url(&url) {
                return Reply::Json(json!({ "ok": false, "message": "不允许打开该地址" }));
            }
            match open_with_os(&url) {
                Ok(_) => Reply::Json(json!({ "ok": true })),
                Err(e) => Reply::Json(json!({ "ok": false, "message": format!("打开失败：{e}") })),
            }
        }

        ("POST", "/api/settings") => {
            if let Ok(v) = serde_json::from_str::<Value>(&req.body) {
                state.update_cfg(|cfg| {
                    let days = v
                        .get("staleDays")
                        .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
                    if let Some(n) = days {
                        cfg.stale_days = n.max(1);
                    }
                    if let Some(s) = v.get("speed").and_then(Value::as_str) {
                        cfg.apply_speed(Speed::parse(s));
                    }
                });
                state.save_settings();
                let cfg = state.cfg();
                state.log(
                    &format!("设置已保存：阈值 {} 天，速度档 {}", cfg.stale_days, cfg.speed.label()),
                    "",
                );
            }
            Reply::Json(json!({ "ok": true, "settings": state.settings() }))
        }

        ("POST", "/api/data/clear") => {
            if state.running.load(Ordering::SeqCst) {
                return Reply::Json(json!({ "ok": false, "message": "扫描进行中，请先停止再清空" }));
            }
            let body: Value = serde_json::from_str(&req.body).unwrap_or(json!({}));
            let also_login = body.get("alsoLogin").and_then(Value::as_bool).unwrap_or(false);
            let out = state.cfg().out_dir.clone();

            // 1) 清空数据库
            {
                let db = state.db.lock().unwrap();
                let _ = db.exec("DELETE FROM accounts; DELETE FROM runs;");
                let _ = db.exec("VACUUM;");
            }
            // 2) 删除所有生成的文件
            for f in [
                "following_all.json", "following_meta.json", "report.html",
                "stale_accounts.csv", "stale_accounts.json", "error_accounts.csv",
                "all_accounts.csv", "app-settings.json", "app.log",
            ] {
                let _ = std::fs::remove_file(out.join(f));
            }
            // 3) 可选：清除登录状态（需要重新扫码）
            if also_login {
                let _ = std::fs::remove_dir_all(out.join("profile"));
            }
            // 4) 重置内存状态与设置
            state.update(|p| *p = crate::state::ScanProgress::default());
            state.logs.lock().unwrap().clear();
            if also_login {
                *state.logged_in.lock().unwrap() = None;
                state.self_sec_uid.lock().unwrap().clear();
            }
            state.update_cfg(|cfg| {
                cfg.stale_days = 60;
                cfg.apply_speed(Speed::Standard); // 默认档位
            });

            state.log(
                if also_login {
                    "已从零开始：账号数据、报告、列表缓存、登录状态全部清除，请重新登录抖音"
                } else {
                    "已从零开始：账号数据、报告、列表缓存全部清除（登录状态保留）"
                },
                "warn",
            );
            Reply::Json(json!({
                "ok": true,
                "alsoLogin": also_login,
                "message": if also_login {
                    "已清空所有数据，并清除了登录状态。\n下次使用请重新点击「登录抖音」扫码。"
                } else {
                    "已清空所有数据，回到初始状态。\n（登录状态已保留，无需重新扫码）"
                },
            }))
        }

        // ---------------- 取关（不可逆写操作） ----------------
        ("POST", "/api/unfollow/capture") => {
            let sample = serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|v| v.get("secUid").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            if sample.is_empty() {
                return Reply::Json(json!({ "ok": false, "message": "缺少用于录制的账号" }));
            }
            let st = Arc::clone(state);
            st.update_unfollow(|p| {
                p.status = "capturing".into();
                p.wait_text = "正在录制取关请求，请在弹出的浏览器里点一次「取消关注」".into();
            });
            thread::spawn(move || {
                let cfg = st.cfg();
                match douyin::capture_unfollow(&cfg, &sample, |m, l| st.log(m, l), Duration::from_secs(300))
                {
                    Ok(tpl) => match douyin::save_template(&cfg.out_dir, &tpl) {
                        Ok(_) => {
                            st.log("取关请求录制完成，可以执行取关了", "ok");
                            st.update_unfollow(|p| {
                                p.status = "ready".into();
                                p.wait_text.clear();
                            });
                        }
                        Err(e) => {
                            st.log(&format!("保存取关模板失败：{e}"), "err");
                            st.notify("err", &format!("保存取关模板失败：{e}"));
                            st.update_unfollow(|p| {
                                p.status = "idle".into();
                                p.wait_text.clear();
                            });
                        }
                    },
                    Err(e) => {
                        st.log(&format!("录制失败：{e}"), "err");
                        st.notify("err", &format!("录制取关请求失败：{e}"));
                        st.update_unfollow(|p| {
                            p.status = "idle".into();
                            p.wait_text.clear();
                        });
                    }
                }
            });
            Reply::Empty
        }

        ("POST", "/api/unfollow/start") => {
            let body: Value = serde_json::from_str(&req.body).unwrap_or(json!({}));
            // 需要同时拿到数字 ID 与 sec_uid：实测取关请求用的是 body 里的 user_id
            let targets: Vec<(String, String)> = body
                .get("targets")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let uid = x.get("uid").and_then(Value::as_str).unwrap_or("").to_string();
                            let sec = x.get("secUid").and_then(Value::as_str).unwrap_or("").to_string();
                            if uid.is_empty() && sec.is_empty() { None } else { Some((uid, sec)) }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if targets.is_empty() {
                state.log("取关请求被拒绝：没有选中任何账号", "warn");
                return Reply::Json(json!({ "ok": false, "message": "没有选中任何账号" }));
            }
            if !state.unfollow_ready() {
                state.log("取关请求被拒绝：尚未录制取关请求", "warn");
                return Reply::Json(json!({
                    "ok": false,
                    "message": "尚未录制取关请求。请先点「录制取关请求」，在浏览器中手动取消关注一次。"
                }));
            }
            if state.running.load(Ordering::SeqCst) {
                state.log("取关请求被拒绝：扫描仍在运行中（若扫描已结束，请点一次「停止扫描」复位）", "warn");
                return Reply::Json(json!({ "ok": false, "message": "扫描进行中，请先停止扫描" }));
            }
            {
                let st_locked = state.unfollow.lock().unwrap();
                if st_locked.status == "running" || st_locked.status == "capturing" {
                    let msg = format!("取关请求被拒绝：当前取关状态为 {}", st_locked.status);
                    drop(st_locked);
                    state.log(&msg, "warn");
                    return Reply::Json(json!({ "ok": false, "message": "取关正在执行中" }));
                }
            }

            let st = Arc::clone(state);
            st.unfollow_stop.store(false, Ordering::SeqCst);
            st.update_unfollow(|p| {
                *p = crate::state::UnfollowProgress::default();
                p.status = "running".into();
                p.total = targets.len() as i64;
            });
            st.log(&format!("开始取关：本次 {} 个账号", targets.len()), "warn");
            thread::spawn(move || run_unfollow(st, targets));
            Reply::Empty
        }

        ("POST", "/api/notices/ack") => {
            let id = serde_json::from_str::<Value>(&req.body)
                .ok()
                .and_then(|x| x.get("id").and_then(Value::as_i64))
                .unwrap_or(0);
            state.ack_notices(id);
            Reply::Empty
        }

        ("POST", "/api/unfollow/stop") => {
            state.unfollow_stop.store(true, Ordering::SeqCst);
            Reply::Empty
        }

        _ => Reply::Json(json!({ "ok": false, "error": "not found" })),
    }
}

/// 轻量伪随机（够用即可，不为此引入 rand 依赖）
fn pseudo_rand(max: u64) -> u64 {
    static SEED: AtomicU64 = AtomicU64::new(0);
    let mut x = SEED.load(Ordering::Relaxed);
    if x == 0 {
        x = (crate::state::now_sec() as u64) | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    SEED.store(x, Ordering::Relaxed);
    x % max
}

/// 容易被中断的等待（取关用）
fn wait_ms(state: &Arc<AppState>, ms: u64) {
    for _ in 0..(ms / 200).max(1) {
        if state.unfollow_stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// 执行取关：逐个调用录制的请求，命中风控立即熔断
fn run_unfollow(state: Arc<AppState>, targets: Vec<(String, String)>) {
    let cfg = state.cfg();
    let mut session = match douyin::open_session(&cfg, true, |m, l| state.log(m, l), Duration::from_secs(60)) {
        Ok(s) => s,
        Err(e) => {
            state.log(&format!("取关失败：{e}"), "err");
            state.notify("err", &format!("取关失败：{e}"));
            state.update_unfollow(|p| p.status = "error".into());
            return;
        }
    };
    let Some(tpl) = douyin::load_template(&cfg.out_dir) else {
        state.log("没有可用的取关模板", "err");
        state.notify("err", "没有可用的取关模板，请到「设置」里重新录制");
        state.update_unfollow(|p| p.status = "error".into());
        session.close();
        return;
    };

    let mut ok = 0i64;
    let mut bad = 0i64;

    for (i, (uid, sec)) in targets.iter().enumerate() {
        if state.unfollow_stop.load(Ordering::SeqCst) {
            break;
        }
        state.update_unfollow(|p| p.current = sec.clone());

        match douyin::do_unfollow(&session.page, &tpl, uid, sec) {
            Ok(v) => {
                if v.get("blocked").and_then(Value::as_bool).unwrap_or(false) {
                    bad += 1;
                    state.log(&format!("触发风控，立即停止取关（已完成 {i} 个）"), "err");
                    state.notify("err", &format!("取关触发风控，已立即停止（完成 {i} 个）"));
                    state.unfollow_stop.store(true, Ordering::SeqCst);
                    break;
                }
                let sc = v.get("status_code").and_then(Value::as_i64).unwrap_or(-1);
                if sc == 0 {
                    ok += 1;
                    state.log(&format!("[{}/{}] 取关成功", i + 1, targets.len()), "ok");
                    // 记下「已取关」，界面上会在勾选框旁打标记
                    {
                        let db = state.db.lock().unwrap();
                        db.mark_unfollowed(sec, crate::state::now_sec());
                    }
                } else {
                    bad += 1;
                    state.log(&format!("[{}/{}] 取关失败（接口码 {sc}）", i + 1, targets.len()), "warn");
                }
            }
            Err(e) => {
                bad += 1;
                state.log(&format!("[{}/{}] 取关出错：{e}", i + 1, targets.len()), "warn");
            }
        }

        state.update_unfollow(|p| {
            p.done = (i + 1) as i64;
            p.success = ok;
            p.failed = bad;
        });

        // 基础间隔 3~8 秒；每满 10 个再额外停 10~20 秒
        if i + 1 < targets.len() {
            let base = 3000 + pseudo_rand(5001);
            let extra = if (i + 1) % 10 == 0 { 10_000 + pseudo_rand(10_001) } else { 0 };
            let wait = base + extra;
            state.update_unfollow(|p| {
                p.wait_text = if extra > 0 {
                    format!("已完成 {} 个，长休息 {} 秒…", i + 1, wait / 1000)
                } else {
                    format!("间隔 {} 秒后继续…", wait / 1000)
                };
            });
            wait_ms(&state, wait);
            state.update_unfollow(|p| p.wait_text.clear());
        }
    }

    session.close();
    state.update_unfollow(|p| {
        p.status = "done".into();
        p.current.clear();
        p.wait_text.clear();
    });
    state.log(&format!("取关结束：成功 {ok}，失败 {bad}"), if bad > 0 { "warn" } else { "ok" });
    if bad > 0 && ok == 0 {
        state.notify("err", &format!("取关全部失败（{bad} 个）。若反复失败，可能是录制到的请求已失效，请到「设置」重新录制。"));
    } else if bad > 0 {
        state.notify("warn", &format!("取关结束：成功 {ok} 个，失败 {bad} 个"));
    }
}

fn parse_query(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), url_decode(v)))
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn send_json(stream: &mut TcpStream, v: &Value) {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
}

fn send_html(stream: &mut TcpStream, html: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.as_bytes().len(),
        html
    );
}

