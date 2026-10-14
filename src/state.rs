//! 应用共享状态：进度、日志、控制开关、配置、数据库句柄。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};

use serde_json::{json, Value};

use crate::config::Config;
use crate::db::Db;

pub struct LogEntry {
    pub t: String,
    pub msg: String,
    pub level: String,
}

#[derive(Clone)]
pub struct ScanProgress {
    pub status: String,
    pub phase: String,
    pub total: i64,
    pub checked: i64,
    pub stale: i64,
    pub active: i64,
    pub error: i64,
    pub current_account: String,
    pub rate: f64,
    pub eta_seconds: Option<i64>,
    pub wait_text: String,
    pub started_at: Option<i64>,
    pub stale_list: Vec<Value>,
    pub error_list: Vec<Value>,
}

impl Default for ScanProgress {
    fn default() -> Self {
        ScanProgress {
            status: "idle".into(),
            phase: String::new(),
            total: 0,
            checked: 0,
            stale: 0,
            active: 0,
            error: 0,
            current_account: String::new(),
            rate: 0.0,
            eta_seconds: None,
            wait_text: String::new(),
            started_at: None,
            stale_list: Vec::new(),
            error_list: Vec::new(),
        }
    }
}

/// 取关进度（取关是不可逆的写操作，所以单独维护一套状态与停止开关）
#[derive(Clone)]
pub struct UnfollowProgress {
    /// idle（未录制）| ready（已录制）| capturing | running | done | error
    pub status: String,
    pub total: i64,
    pub done: i64,
    pub success: i64,
    pub failed: i64,
    pub current: String,
    pub wait_text: String,
    pub results: Vec<Value>,
}

impl Default for UnfollowProgress {
    fn default() -> Self {
        UnfollowProgress {
            status: "idle".into(),
            total: 0,
            done: 0,
            success: 0,
            failed: 0,
            current: String::new(),
            wait_text: String::new(),
            results: Vec::new(),
        }
    }
}

pub struct AppState {
    cfg: RwLock<Config>,
    pub db: Mutex<Db>,
    pub progress: Mutex<ScanProgress>,
    pub logs: Mutex<Vec<LogEntry>>,
    pub paused: AtomicBool,
    pub stopped: AtomicBool,
    pub running: AtomicBool,
    pub unfollow: Mutex<UnfollowProgress>,
    pub unfollow_stop: AtomicBool,
    /// 需要主动弹窗告知用户的消息（错误 / 重要提醒），前端按 id 增量取
    pub notices: Mutex<Vec<Value>>,
    pub notice_seq: AtomicI64,
    /// 前端已确认看到的通知 id，快照只返回比它新的
    pub notice_floor: AtomicI64,
    /// 本地接口令牌：随机生成、注入界面，请求须带 X-Token
    pub token: String,
    /// None = 未检测；Some(true/false) = 已检测
    pub logged_in: Mutex<Option<bool>>,
    pub self_sec_uid: Mutex<String>,
    pub settings_path: PathBuf,
    pub log_path: PathBuf,
}

/// 生成本地接口令牌。目的是挡住跨站请求（恶意网页读不到被注入的令牌），
/// 不需要密码学强度，够长不可猜即可。
fn gen_token() -> String {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32)
        ^ (&gen_token as *const _ as u64);
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> Self {
        let settings_path = cfg.out_dir.join("app-settings.json");
        let log_path = cfg.out_dir.join("app.log");
        AppState {
            cfg: RwLock::new(cfg),
            db: Mutex::new(db),
            progress: Mutex::new(ScanProgress::default()),
            logs: Mutex::new(Vec::new()),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            running: AtomicBool::new(false),
            logged_in: Mutex::new(None),
            self_sec_uid: Mutex::new(String::new()),
            settings_path,
            log_path,
            unfollow: Mutex::new(UnfollowProgress::default()),
            unfollow_stop: AtomicBool::new(false),
            notices: Mutex::new(Vec::new()),
            notice_seq: AtomicI64::new(0),
            notice_floor: AtomicI64::new(0),
            token: gen_token(),
        }
    }

    /// 取关是否已具备执行条件：录制过请求模板
    pub fn unfollow_ready(&self) -> bool {
        crate::douyin::load_template(&self.cfg().out_dir).is_some()
    }

    /// 记录一条需要弹窗告知用户的消息（错误 / 重要提醒）
    pub fn notify(&self, level: &str, msg: &str) {
        let id = self.notice_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut v = self.notices.lock().unwrap();
        v.push(json!({ "id": id, "level": level, "msg": msg, "at": now_sec() }));
        if v.len() > 50 {
            let n = v.len() - 50;
            v.drain(0..n);
        }
    }

    /// 前端已读到的最大通知 id
    pub fn ack_notices(&self, upto: i64) {
        let cur = self.notice_floor.load(Ordering::SeqCst);
        if upto > cur {
            self.notice_floor.store(upto, Ordering::SeqCst);
        }
    }

    pub fn update_unfollow<F: FnOnce(&mut UnfollowProgress)>(&self, f: F) {
        let mut p = self.unfollow.lock().unwrap();
        f(&mut p);
    }

    /// 取一份配置快照（Config 很小，克隆开销可忽略）
    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    pub fn update_cfg<F: FnOnce(&mut Config)>(&self, f: F) {
        let mut c = self.cfg.write().unwrap();
        f(&mut c);
    }

    pub fn log(&self, msg: &str, level: &str) {
        let t = local_hhmmss();
        println!("[{t}] {msg}");
        // release 版本没有控制台，同时写一份到文件方便排查
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "[{t}] {msg}");
        }
        let mut logs = self.logs.lock().unwrap();
        logs.push(LogEntry { t, msg: msg.to_string(), level: level.to_string() });
        if logs.len() > 400 {
            let drop = logs.len() - 400;
            logs.drain(0..drop);
        }
    }

    pub fn update<F: FnOnce(&mut ScanProgress)>(&self, f: F) {
        let mut p = self.progress.lock().unwrap();
        f(&mut p);
    }

    /// 由数据库实时派生统计（days 动态计算，因此结果不会过期）
    pub fn derive_rows(&self) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let rows = db
            .query(
                "SELECT sec_uid, uid, nickname, home_url, avatar, aweme_count, \
                 last_update_ts, last_update_date, note, last_checked_at, \
                 unfollowed_at FROM accounts",
                &[],
            )
            .unwrap_or_default();
        let now = now_sec();
        let stale_days = self.cfg().stale_days;
        rows.into_iter()
            .map(|r| {
                let get = |i: usize| r.get(i).and_then(Value::as_str).unwrap_or("").to_string();
                let ts = get(6).parse::<i64>().ok().filter(|v| *v > 0);
                let checked = get(9).parse::<i64>().ok().filter(|v| *v > 0);
                let days = ts.map(|t| (now - t) / 86400);
                // 拿不到时间戳时要区分两种情况：
                //   pending —— 还没检查过（例如列表拉到了但本轮没轮到）
                //   error   —— 检查过但确实取不到（无作品 / 私密 / 接口异常）
                let status = match days {
                    Some(d) if d > stale_days => "stale",
                    Some(_) => "active",
                    None if checked.is_none() => "pending",
                    None => "error",
                };
                json!({
                    "nickname": get(2),
                    "uid": get(1),
                    "secUid": get(0),
                    "homeUrl": get(3),
                    "avatar": get(4),
                    "awemeCount": get(5).parse::<i64>().unwrap_or(0),
                    "lastPostDate": get(7),
                    "days": days,
                    "status": status,
                    "note": get(8),
                    // 「已取关」持久化在数据库里；「已开过」只在本次会话记录，不落库
                    "unfollowedAt": get(10).parse::<i64>().ok().filter(|v| *v > 0),
                })
            })
            .collect()
    }

    pub fn summary(&self) -> Value {
        let rows = self.derive_rows();
        let count = |s: &str| rows.iter().filter(|r| r["status"] == s).count();
        json!({
            "total": rows.len(),
            "stale": count("stale"),
            "active": count("active"),
            "error": count("error"),
            "pending": count("pending"),
            "unfollowed": rows.iter().filter(|r| !r["unfollowedAt"].is_null()).count(),
            "lastScanAt": self.progress.lock().unwrap().started_at,
        })
    }

    pub fn settings(&self) -> Value {
        let cfg = self.cfg();
        json!({
            "staleDays": cfg.stale_days,
            "speed": speed_key(cfg.speed),
            "speedOptions": [
                {"key": "conservative", "label": "保守（最慢，最安全）"},
                {"key": "standard", "label": "标准（推荐）"},
                {"key": "fast", "label": "快速（风险较高）"},
            ],
            "outDir": cfg.out_dir.to_string_lossy(),
        })
    }

    pub fn save_settings(&self) {
        let cfg = self.cfg();
        let v = json!({ "staleDays": cfg.stale_days, "speed": speed_key(cfg.speed) });
        let _ = std::fs::write(&self.settings_path, serde_json::to_string_pretty(&v).unwrap_or_default());
    }

    pub fn load_settings(&self) {
        let Ok(text) = std::fs::read_to_string(&self.settings_path) else { return };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return };
        self.update_cfg(|cfg| {
            if let Some(n) = v.get("staleDays").and_then(Value::as_i64) {
                cfg.stale_days = n;
            }
            if let Some(s) = v.get("speed").and_then(Value::as_str) {
                let speed = crate::config::Speed::parse(s);
                cfg.apply_speed(speed);
            }
        });
    }

    /// 组装给界面的完整状态
    pub fn snapshot(&self) -> Value {
        let p = self.progress.lock().unwrap().clone();
        let u = self.unfollow.lock().unwrap().clone();
        let floor = self.notice_floor.load(Ordering::SeqCst);
        let logged = *self.logged_in.lock().unwrap();
        let logs: Vec<Value> = self
            .logs
            .lock()
            .unwrap()
            .iter()
            .map(|l| json!({ "t": l.t, "msg": l.msg, "level": l.level }))
            .collect();
        json!({
            "login": {
                "loggedIn": logged,
                "secUid": self.self_sec_uid.lock().unwrap().clone(),
                "logging": false
            },
            "scan": {
                "status": p.status,
                "phase": p.phase,
                "total": p.total,
                "checked": p.checked,
                "stale": p.stale,
                "active": p.active,
                "error": p.error,
                "currentAccount": p.current_account,
                "rate": p.rate,
                "etaSeconds": p.eta_seconds,
                "waitText": p.wait_text,
                "startedAt": p.started_at,
                "staleList": p.stale_list,
                "errorList": p.error_list,
            },
            "summary": self.summary(),
            "settings": self.settings(),
            "log": logs,
            "notices": self.notices.lock().unwrap().iter()
                .filter(|n| n.get("id").and_then(Value::as_i64).unwrap_or(0) > floor)
                .cloned().collect::<Vec<_>>(),
            "unfollow": {
                "ready": self.unfollow_ready(),
                "status": u.status,
                "total": u.total,
                "done": u.done,
                "success": u.success,
                "failed": u.failed,
                "current": u.current,
                "waitText": u.wait_text,
                "results": u.results,
                // 录制到的请求摘要，供用户核对（方法 / type / 用哪种 ID 定位目标）
                "template": crate::douyin::load_template(&self.cfg().out_dir)
                    .map(|t| crate::douyin::template_summary(&t)),
            },
        })
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

fn speed_key(s: crate::config::Speed) -> &'static str {
    match s {
        crate::config::Speed::Conservative => "conservative",
        crate::config::Speed::Standard => "standard",
        crate::config::Speed::Fast => "fast",
    }
}

pub fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn day_str(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant 的 civil_from_days 算法（避免引入日期库）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn local_hhmmss() -> String {
    let secs = now_sec();
    let h = ((secs / 3600) % 24 + 8) % 24;
    format!("{:02}:{:02}:{:02}", h, (secs / 60) % 60, secs % 60)
}
