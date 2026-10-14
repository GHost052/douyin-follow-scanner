//! 配置：默认值 + 可选的 config.json 覆盖 + 速度档位预设。

use std::path::PathBuf;

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    Conservative,
    Standard,
    Fast,
}

impl Speed {
    pub fn parse(s: &str) -> Speed {
        match s {
            "conservative" => Speed::Conservative,
            "fast" => Speed::Fast,
            _ => Speed::Standard,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Speed::Conservative => "保守（最慢，最安全）",
            Speed::Standard => "标准（推荐）",
            Speed::Fast => "快速（风险较高）",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub out_dir: PathBuf,
    pub stale_days: i64,
    pub channel: String,
    pub headless: bool,
    pub port: u16,
    pub speed: Speed,
    /// 已检查过的账号在多少小时内不重复检查（用于「停止后继续」）
    pub recheck_hours: u32,
    pub retry_max: u32,
    pub retry_wait_sec: u64,
    /// 关注列表翻页失败后的重试等待（秒）。列表接口的限流恢复期较长，
    /// 实测 2 分钟远远不够，因此默认等 30 分钟。
    pub list_retry_wait_sec: u64,
    /// 小批次：每 batch_min~batch_max 个账号，暂停 batch_pause_min~batch_pause_max 秒
    pub batch_min: u32,
    pub batch_max: u32,
    pub batch_pause_min: u32,
    pub batch_pause_max: u32,
    /// 预防性休息：每 rest_min~rest_max 个账号，主动停 rest_minutes_min~rest_minutes_max 分钟
    pub long_rest: bool,
    pub rest_min: u32,
    pub rest_max: u32,
    pub rest_minutes_min: u32,
    pub rest_minutes_max: u32,
    /// 每个账号请求之间的随机延迟（毫秒）
    pub delay_min: u64,
    pub delay_max: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            out_dir: PathBuf::from("./data"),
            stale_days: 60,
            channel: "msedge".into(),
            headless: true,
            port: 8799,
            speed: Speed::Standard,
            recheck_hours: 24,
            retry_max: 3,
            // 账号检查命中风控后的等待。风控恢复期通常远超 2 分钟，
            // 太短会白白浪费重试次数，所以和列表一样等 30 分钟。
            retry_wait_sec: 1800,
            list_retry_wait_sec: 1800,
            batch_min: 50,
            batch_max: 100,
            batch_pause_min: 1,
            batch_pause_max: 5,
            long_rest: true,
            rest_min: 800,
            rest_max: 950,
            rest_minutes_min: 50,
            rest_minutes_max: 75,
            delay_min: 150,
            delay_max: 400,
        }
    }
}

impl Config {
    /// 从命令行 / exe 所在目录 / 当前目录读取配置（均可选），并应用速度档位。
    /// 优先级：命令行 > config.json > 默认值
    pub fn load() -> Config {
        let mut cfg = Config::default();
        let mut out_dir_set = false;

        let candidates = [
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("config.json"))),
            Some(PathBuf::from("config.json")),
        ];
        for path in candidates.into_iter().flatten() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    cfg.apply_json(&v);
                    out_dir_set = v.get("outDir").is_some();
                    break;
                }
            }
        }

        // 命令行覆盖
        for arg in std::env::args().skip(1) {
            if let Some(v) = arg.strip_prefix("--out=") {
                cfg.out_dir = PathBuf::from(v);
                out_dir_set = true;
            } else if let Some(v) = arg.strip_prefix("--port=") {
                if let Ok(p) = v.parse::<u16>() {
                    cfg.port = p;
                }
            } else if let Some(v) = arg.strip_prefix("--speed=") {
                cfg.speed = Speed::parse(v);
            }
        }

        // 未显式指定时，数据目录放在 exe 同级，方便打包后双击即用
        if !out_dir_set {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    cfg.out_dir = dir.join("data");
                }
            }
        }
        cfg.out_dir = cfg.out_dir.clone();
        cfg.apply_speed(cfg.speed);
        cfg
    }

    fn apply_json(&mut self, v: &Value) {
        if let Some(s) = v.get("outDir").and_then(Value::as_str) {
            self.out_dir = PathBuf::from(s);
        }
        if let Some(n) = v.get("staleDays").and_then(Value::as_i64) {
            self.stale_days = n;
        }
        if let Some(n) = v.get("port").and_then(Value::as_u64) {
            self.port = n as u16;
        }
        if let Some(s) = v.get("speed").and_then(Value::as_str) {
            self.speed = Speed::parse(s);
        }
        if let Some(n) = v.get("recheckHours").and_then(Value::as_u64) {
            self.recheck_hours = n.max(1) as u32;
        }
        if let Some(b) = v.get("browser") {
            if let Some(s) = b.get("channel").and_then(Value::as_str) {
                self.channel = s.to_string();
            }
            if let Some(h) = b.get("headless").and_then(Value::as_bool) {
                self.headless = h;
            }
        }
        if let Some(r) = v.get("retry") {
            if let Some(n) = r.get("maxRetry").and_then(Value::as_u64) {
                self.retry_max = n as u32;
            }
            if let Some(n) = r.get("retryWaitSec").and_then(Value::as_u64) {
                self.retry_wait_sec = n;
            }
        }
    }

    pub fn apply_speed(&mut self, speed: Speed) {
        self.speed = speed;
        match speed {
            // 保守档：比标准档明显更慢——批次更小、批次间隔更长、
            // 预防性休息更频繁也更久，单个请求之间的间隔也翻倍。
            Speed::Conservative => {
                self.batch_min = 25; self.batch_max = 50;
                self.batch_pause_min = 3; self.batch_pause_max = 8;
                self.long_rest = true;
                self.rest_min = 500; self.rest_max = 650;
                self.rest_minutes_min = 60; self.rest_minutes_max = 90;
                self.delay_min = 300; self.delay_max = 800;
            }
            Speed::Standard => {
                self.batch_min = 50; self.batch_max = 100;
                self.batch_pause_min = 1; self.batch_pause_max = 3;
                self.long_rest = true;
                self.rest_min = 1500; self.rest_max = 2000;
                self.rest_minutes_min = 20; self.rest_minutes_max = 30;
                self.delay_min = 120; self.delay_max = 300;
            }
            Speed::Fast => {
                self.batch_min = 100; self.batch_max = 200;
                self.batch_pause_min = 0; self.batch_pause_max = 1;
                self.long_rest = false;
                self.rest_min = 800; self.rest_max = 950;
                self.rest_minutes_min = 10; self.rest_minutes_max = 20;
                self.delay_min = 50; self.delay_max = 150;
            }
        }
    }
}
