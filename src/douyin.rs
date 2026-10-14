//! 抖音接口封装：打开会话、拉关注列表、查最新作品时间。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cdp::{Browser, Page};
use crate::config::Config;

const FOLLOW_PAGE: &str = "https://www.douyin.com/follow";
const FOLLOWING_API: &str = "/aweme/v1/web/user/following/list/";

const AWEME_QUERY: &str = "/aweme/v1/web/aweme/post/?device_platform=webapp&aid=6383&channel=channel_pc_web&sec_user_id=";
const AWEME_TAIL: &str = "&max_cursor=0&locate_query=false&show_live_replay_strategy=1&count=18\
&publish_video_strategy_type=2&pc_client_type=1&version_code=170400&version_name=17.4.0\
&cookie_enabled=true&screen_width=1366&screen_height=768&browser_language=zh-CN\
&browser_platform=Win32&browser_name=Edge&browser_online=true&engine_name=Blink\
&os_name=Windows&os_version=10&platform=PC";

pub struct Session {
    pub browser: Browser,
    pub page: Page,
    pub self_sec_uid: String,
    qs: Vec<(String, String)>,
}

pub struct FollowAccount {
    pub sec: String,
    pub uid: String,
    pub nick: String,
    pub aweme_count: i64,
    pub avatar: String,
}

pub struct ListPage {
    pub blocked: bool,
    pub error: Option<String>,
    pub sc: i64,
    pub has_more: bool,
    pub total: i64,
    pub list: Vec<FollowAccount>,
}

pub struct Latest {
    pub blocked: bool,
    pub error: Option<String>,
    pub sc: i64,
    pub count: i64,
    pub newest: Option<i64>,
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 打开抖音会话。headless=false 时会弹出窗口等待扫码登录。
pub fn open_session<F: Fn(&str, &str)>(
    cfg: &Config,
    headless: bool,
    log: F,
    wait_login: Duration,
) -> Result<Session, String> {
    let profile = cfg.out_dir.join("profile");
    let profile = profile.to_string_lossy().to_string();

    let browser = Browser::launch(&cfg.channel, None, headless, &profile)?;
    log(
        &format!(
            "浏览器已就绪（{}）{}",
            browser.version,
            if browser.reused { "，复用了上次遗留的实例" } else { "" }
        ),
        "",
    );

    let page = browser.new_page()?;

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let cap = Arc::clone(&captured);
        browser.watch_requests(&page, move |url| {
            if url.contains(FOLLOWING_API) {
                let mut c = cap.lock().unwrap();
                if c.is_none() {
                    *c = Some(url);
                }
            }
        });
    }

    let is_logged_in = |b: &Browser| -> bool {
        b.cookies().iter().any(|c| {
            c.get("name").and_then(Value::as_str) == Some("sessionid")
                && c.get("value").and_then(Value::as_str).map(|v| v.len() > 5).unwrap_or(false)
        })
    };

    page.goto(FOLLOW_PAGE);

    if !is_logged_in(&browser) {
        if headless {
            let mut b = browser;
            b.close();
            return Err("NEED_LOGIN".into());
        }
        log("请在弹出的浏览器窗口中扫码登录抖音…", "warn");
        let deadline = Instant::now() + wait_login;
        while Instant::now() < deadline && !is_logged_in(&browser) {
            thread::sleep(Duration::from_millis(1500));
        }
        if !is_logged_in(&browser) {
            let mut b = browser;
            b.close();
            return Err("登录超时，请重试".into());
        }
        log("登录成功", "ok");
        page.goto(FOLLOW_PAGE);
    }

    // 等待页面自身发出关注列表请求，从中取得完整参数（含自身 sec_user_id 与指纹参数）
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && captured.lock().unwrap().is_none() {
        thread::sleep(Duration::from_millis(500));
    }
    let url = captured.lock().unwrap().clone();
    let Some(url) = url else {
        let mut b = browser;
        b.close();
        return Err("未捕获到关注列表请求，请稍后重试".into());
    };

    let query = url.split('?').nth(1).unwrap_or("").to_string();
    let mut qs: Vec<(String, String)> = Vec::new();
    let mut self_sec_uid = String::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let val = urldecode(v);
            if k == "sec_user_id" {
                self_sec_uid = val.clone();
            }
            qs.push((k.to_string(), val));
        }
    }

    Ok(Session { browser, page, self_sec_uid, qs })
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
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

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Session {
    /// 拉取一页关注列表
    pub fn list_page(&self, offset: i64) -> ListPage {
        let mut parts: Vec<String> = Vec::new();
        for (k, v) in &self.qs {
            if k == "a_bogus" {
                continue;
            }
            parts.push(format!("{}={}", k, urlencode(v)));
        }
        parts.retain(|p| !p.starts_with("count=") && !p.starts_with("offset=")
            && !p.starts_with("max_time=") && !p.starts_with("min_time=") && !p.starts_with("is_top="));
        parts.push("count=20".into());
        parts.push(format!("offset={offset}"));
        parts.push("max_time=0".into());
        parts.push("min_time=0".into());
        parts.push("is_top=0".into());

        let url = format!("{FOLLOWING_API}?{}", parts.join("&"));
        let expr = format!(
            r#"(async () => {{
  const resp = await fetch({url}, {{ credentials: 'include' }});
  const text = await resp.text();
  let j; try {{ j = JSON.parse(text); }} catch (e) {{ return {{ blocked: true, len: text.length }}; }}
  return {{ sc: j.status_code, has_more: j.has_more, total: j.total,
    list: (j.followings || []).map(x => ({{ sec: x.sec_uid, uid: String(x.uid || ''), nick: x.nickname,
      aweme_count: x.aweme_count,
      avatar: (x.avatar_thumb && x.avatar_thumb.url_list && x.avatar_thumb.url_list[0]) || '' }})) }};
}})()"#,
            url = js_str(&url)
        );

        match self.page.evaluate(&expr) {
            Err(e) => ListPage { blocked: false, error: Some(e), sc: 0, has_more: false, total: 0, list: vec![] },
            Ok(Value::Null) => ListPage { blocked: false, error: Some("空响应".into()), sc: 0, has_more: false, total: 0, list: vec![] },
            Ok(v) => {
                if v.get("blocked").and_then(Value::as_bool).unwrap_or(false) {
                    return ListPage { blocked: true, error: None, sc: 0, has_more: false, total: 0, list: vec![] };
                }
                let list = v.get("list").and_then(Value::as_array).cloned().unwrap_or_default();
                ListPage {
                    blocked: false,
                    error: None,
                    sc: v.get("sc").and_then(Value::as_i64).unwrap_or(0),
                    has_more: v.get("has_more").and_then(Value::as_bool).unwrap_or(false),
                    total: v.get("total").and_then(Value::as_i64).unwrap_or(0),
                    list: list.into_iter().map(|x| FollowAccount {
                        sec: x.get("sec").and_then(Value::as_str).unwrap_or("").to_string(),
                        uid: x.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
                        nick: x.get("nick").and_then(Value::as_str).unwrap_or("").to_string(),
                        aweme_count: x.get("aweme_count").and_then(Value::as_i64).unwrap_or(0),
                        avatar: x.get("avatar").and_then(Value::as_str).unwrap_or("").to_string(),
                    }).filter(|a| !a.sec.is_empty()).collect(),
                }
            }
        }
    }

    /// 查询某账号最新作品的时间戳（取最近 18 条里最大的 create_time）
    pub fn fetch_latest(&self, sec_uid: &str) -> Latest {
        let url = format!("{AWEME_QUERY}{sec_uid}{AWEME_TAIL}");
        let expr = format!(
            r#"(async () => {{
  const resp = await fetch({url}, {{ credentials: 'include' }});
  const text = await resp.text();
  let j; try {{ j = JSON.parse(text); }} catch (e) {{ return {{ blocked: true, len: text.length }}; }}
  const arr = j.aweme_list || [];
  const ts = arr.map(a => a.create_time).filter(Boolean);
  return {{ sc: j.status_code, n: arr.length, newest: ts.length ? Math.max(...ts) : null }};
}})()"#,
            url = js_str(&url)
        );

        match self.page.evaluate(&expr) {
            Err(e) => Latest { blocked: false, error: Some(e), sc: 0, count: 0, newest: None },
            Ok(Value::Null) => Latest { blocked: false, error: Some("空响应".into()), sc: 0, count: 0, newest: None },
            Ok(v) => {
                if v.get("blocked").and_then(Value::as_bool).unwrap_or(false) {
                    return Latest { blocked: true, error: None, sc: 0, count: 0, newest: None };
                }
                Latest {
                    blocked: false,
                    error: None,
                    sc: v.get("sc").and_then(Value::as_i64).unwrap_or(0),
                    count: v.get("n").and_then(Value::as_i64).unwrap_or(0),
                    newest: v.get("newest").and_then(Value::as_i64),
                }
            }
        }
    }

    pub fn close(&mut self) {
        self.browser.close();
    }
}

/// 只做一件事：打开弹窗让用户扫码登录，成功后关闭浏览器（登录态已存盘）
pub fn login_once<F: Fn(&str, &str)>(cfg: &Config, log: F, timeout: Duration) -> Result<String, String> {
    let mut session = open_session(cfg, false, log, timeout)?;
    let sec = session.self_sec_uid.clone();
    session.close();
    Ok(sec)
}

// --------------------------------------------------------------------------- //
// 取关（真实写操作，不可逆）
//
// 采用「先录制、再重放」的方式：不猜接口，而是让用户在页面里手动点一次
// 「已关注 → 取消关注」，我们把这次真实请求（含全部指纹参数）录下来，
// 之后替换 sec_user_id 重放。和关注列表用同一套思路。
// --------------------------------------------------------------------------- //

const FOLLOW_COMMIT_HINT: &str = "/commit/follow/";

#[derive(Clone, Debug)]
pub struct UnfollowTemplate {
    pub method: String,
    pub url: String,
    pub post_data: String,
    pub captured_at: i64,
}

pub fn template_path(out_dir: &std::path::Path) -> std::path::PathBuf {
    out_dir.join("unfollow_template.json")
}

pub fn load_template(out_dir: &std::path::Path) -> Option<UnfollowTemplate> {
    let text = std::fs::read_to_string(template_path(out_dir)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(UnfollowTemplate {
        method: v.get("method").and_then(Value::as_str).unwrap_or("GET").to_string(),
        url: v.get("url").and_then(Value::as_str)?.to_string(),
        post_data: v.get("postData").and_then(Value::as_str).unwrap_or("").to_string(),
        captured_at: v.get("capturedAt").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn save_template(out_dir: &std::path::Path, tpl: &UnfollowTemplate) -> Result<(), String> {
    let v = json!({
        "method": tpl.method,
        "url": tpl.url,
        "postData": tpl.post_data,
        "capturedAt": tpl.captured_at,
    });
    std::fs::write(template_path(out_dir), serde_json::to_string_pretty(&v).unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// 打开一个账号主页，等用户手动点一次「取消关注」，把该请求录下来。
pub fn capture_unfollow<F: Fn(&str, &str)>(
    cfg: &Config,
    sample_sec: &str,
    log: F,
    timeout: Duration,
) -> Result<UnfollowTemplate, String> {
    let profile = cfg.out_dir.join("profile");
    let profile = profile.to_string_lossy().to_string();

    let mut browser = Browser::launch(&cfg.channel, None, false, &profile)?;
    let page = browser.new_page()?;

    let captured: Arc<Mutex<Option<(String, String, String)>>> = Arc::new(Mutex::new(None));
    {
        let cap = Arc::clone(&captured);
        browser.watch_requests_full(&page, move |method, url, post| {
            if url.contains(FOLLOW_COMMIT_HINT) {
                let mut c = cap.lock().unwrap();
                if c.is_none() {
                    *c = Some((method, url, post));
                }
            }
        });
    }

    let target = format!("https://www.douyin.com/user/{sample_sec}");
    log("已打开一个账号主页。请在弹出的浏览器里手动点击「已关注」→「取消关注」，程序会自动记录这次请求。", "warn");
    log("（录制只会记录请求格式，不会用于其它账号；录完会自动关闭浏览器）", "");
    page.goto(&target);

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && captured.lock().unwrap().is_none() {
        thread::sleep(Duration::from_millis(500));
    }
    let got = captured.lock().unwrap().clone();
    browser.close();

    match got {
        Some((method, url, post_data)) => {
            log(&format!("已录制取关请求（{method}）"), "ok");
            Ok(UnfollowTemplate { method, url, post_data, captured_at: crate::state::now_sec() })
        }
        None => Err("未捕获到取关请求。请在打开的页面里点一次「已关注」→「取消关注」后重试。".into()),
    }
}

/// 把目标账号替换进 URL / POST 数据，其余指纹参数原样保留。
///
/// 实测抖音的取关请求是 `POST /aweme/v1/web/commit/follow/user/?...`，
/// 目标放在 **POST body** 里：`type=0&user_id=<数字ID>`。
/// 不同版本也可能用 `sec_user_id`（放在 query 或 body），所以两种都处理。
fn replace_ids(s: &str, uid: &str, sec_uid: &str) -> String {
    let mut out = replace_param(s, "user_id", uid);
    out = replace_param(&out, "sec_user_id", sec_uid);
    out
}

/// 取参数的原始值（只认参数边界，避免匹配到 `pc_client_type=` 这类子串）
fn param_of(s: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let bytes = s.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = s[from..].find(&needle) {
        let i = from + rel;
        let boundary_ok = i == 0 || bytes[i - 1] == b'?' || bytes[i - 1] == b'&';
        if boundary_ok {
            let after = &s[i + needle.len()..];
            let end = after.find('&').unwrap_or(after.len());
            return Some(after[..end].to_string());
        }
        from = i + needle.len();
    }
    None
}

/// 录制结果的摘要，展示到界面上供用户核对
pub fn template_summary(tpl: &UnfollowTemplate) -> Value {
    let body = &tpl.post_data;
    json!({
        "method": tpl.method,
        "type": param_of(body, "type").or_else(|| param_of(&tpl.url, "type")),
        "byUserId": param_of(body, "user_id").is_some() || param_of(&tpl.url, "user_id").is_some(),
        "bySecUid": param_of(body, "sec_user_id").is_some() || param_of(&tpl.url, "sec_user_id").is_some(),
        "capturedAt": tpl.captured_at,
    })
}

/// 替换 `key=value` 形式的参数值；不存在时原样返回。
fn replace_param(s: &str, key: &str, value: &str) -> String {
    if value.is_empty() {
        return s.to_string();
    }
    let encoded = urlencode(value);
    let needle = format!("{key}=");
    let mut out = String::with_capacity(s.len() + 32);
    let mut rest = s;
    let mut hit = false;
    while let Some(i) = rest.find(&needle) {
        // 确保是参数边界（开头、? 或 & 之后）
        let boundary_ok = i == 0
            || rest.as_bytes()[i - 1] == b'?'
            || rest.as_bytes()[i - 1] == b'&';
        out.push_str(&rest[..i]);
        if boundary_ok {
            out.push_str(&needle);
            out.push_str(&encoded);
            let after = &rest[i + needle.len()..];
            let end = after.find('&').unwrap_or(after.len());
            rest = &after[end..];
            hit = true;
        } else {
            out.push_str(&needle);
            rest = &rest[i + needle.len()..];
        }
    }
    out.push_str(rest);
    let _ = hit;
    out
}

/// 对单个账号执行取关。返回接口状态码与原文，交由调用方判定成败。
pub fn do_unfollow(
    page: &Page,
    tpl: &UnfollowTemplate,
    uid: &str,
    sec_uid: &str,
) -> Result<Value, String> {
    let url = replace_ids(&tpl.url, uid, sec_uid);
    let body = if tpl.post_data.is_empty() {
        String::new()
    } else {
        replace_ids(&tpl.post_data, uid, sec_uid)
    };

    let init = if body.is_empty() {
        format!("{{ method: {}, credentials: 'include' }}", js_str(&tpl.method))
    } else {
        format!(
            "{{ method: {}, credentials: 'include', headers: {{ 'content-type': 'application/x-www-form-urlencoded' }}, body: {} }}",
            js_str(&tpl.method),
            js_str(&body)
        )
    };

    let expr = format!(
        r#"(async () => {{
  const resp = await fetch({url}, {init});
  const text = await resp.text();
  let j; try {{ j = JSON.parse(text); }} catch (e) {{ return {{ blocked: true, len: text.length }}; }}
  return j;
}})()"#,
        url = js_str(&url),
        init = init
    );

    page.evaluate(&expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 取自实测请求的结构（账号 ID 已替换为示例值）：目标在 POST body 里，参数名是 user_id
    const SAMPLE_BODY: &str = "type=0&user_id=1234567890123";
    const SAMPLE_URL: &str = "https://www.douyin.com/aweme/v1/web/commit/follow/user/?device_platform=webapp&pc_client_type=1&type=0&msToken=abc123&a_bogus=xyz";

    #[test]
    fn 替换_body_里的_user_id() {
        assert_eq!(replace_ids(SAMPLE_BODY, "999", "SEC"), "type=0&user_id=999");
    }

    #[test]
    fn 不误伤_其他参数里的同名前缀() {
        // pc_client_type=1 含 "type=" 子串；替换 type 时不能碰它
        let got = replace_param(SAMPLE_URL, "type", "9");
        assert!(got.contains("pc_client_type=1"), "误伤了 pc_client_type: {got}");
        assert!(got.contains("&type=9"), "没替换到 type: {got}");
    }

    #[test]
    fn 保留其余指纹参数() {
        let got = replace_param(SAMPLE_URL, "type", "1");
        assert!(got.contains("msToken=abc123"));
        assert!(got.contains("a_bogus=xyz"));
        assert!(got.contains("device_platform=webapp"));
    }

    #[test]
    fn 同时替换两种_id() {
        let s = "sec_user_id=OLD&user_id=111&x=1";
        assert_eq!(replace_ids(s, "222", "NEW"), "sec_user_id=NEW&user_id=222&x=1");
    }

    #[test]
    fn 值为空时原样返回() {
        assert_eq!(replace_ids(SAMPLE_BODY, "", ""), SAMPLE_BODY);
    }

    #[test]
    fn 参数取值要认边界() {
        assert_eq!(param_of(SAMPLE_BODY, "type").as_deref(), Some("0"));
        assert_eq!(param_of(SAMPLE_BODY, "user_id").as_deref(), Some("1234567890123"));
        // URL 里 type= 出现在 pc_client_type 中，但边界判断应取到真正的 type
        assert_eq!(param_of(SAMPLE_URL, "type").as_deref(), Some("0"));
        assert_eq!(param_of(SAMPLE_BODY, "nope"), None);
    }

    #[test]
    fn 摘要能看出录制到的请求特征() {
        let tpl = UnfollowTemplate {
            method: "POST".into(),
            url: SAMPLE_URL.into(),
            post_data: SAMPLE_BODY.into(),
            captured_at: 0,
        };
        let s = template_summary(&tpl);
        assert_eq!(s["method"], "POST");
        assert_eq!(s["type"], "0");
        assert_eq!(s["byUserId"], true);
        assert_eq!(s["bySecUid"], false);
    }
}


