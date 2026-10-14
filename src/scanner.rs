//! 扫描引擎：拉关注列表（缓存 + 断点续传）→ 逐个查最新作品时间 → 生成报告。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::douyin::Session;
use crate::report;
use crate::state::{day_str, now_sec, AppState};

/// 轻量随机数（xorshift），避免为此引入 rand 依赖
fn rnd(min: u32, max: u32) -> u32 {
    if max <= min {
        return min;
    }
    use std::cell::Cell;
    thread_local! {
        static SEED: Cell<u64> = Cell::new(0);
    }
    SEED.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = (now_sec() as u64) ^ ((std::process::id() as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        min + (x % (max - min + 1) as u64) as u32
    })
}

/// 暂停时挂起；返回 false 表示被要求停止
fn wait_pause(state: &Arc<AppState>) -> bool {
    if !state.paused.load(Ordering::SeqCst) {
        return !state.is_stopped();
    }
    state.update(|p| {
        p.status = "paused".into();
        p.wait_text = "已暂停，点击「继续」恢复扫描".into();
    });
    while state.paused.load(Ordering::SeqCst) && !state.is_stopped() {
        thread::sleep(Duration::from_millis(300));
    }
    if !state.is_stopped() {
        state.update(|p| {
            p.status = "running".into();
            p.wait_text.clear();
        });
    }
    !state.is_stopped()
}

/// 可中断的等待；返回 false 表示被停止
fn wait_seconds(state: &Arc<AppState>, total: u64, tick: Option<Box<dyn Fn(u64)>>) -> bool {
    for s in (1..=total).rev() {
        if state.is_stopped() {
            return false;
        }
        if let Some(f) = &tick {
            f(s);
        }
        thread::sleep(Duration::from_secs(1));
    }
    !state.is_stopped()
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}时{m}分")
    } else if m > 0 {
        format!("{m}分{s}秒")
    } else {
        format!("{s}秒")
    }
}

// --------------------------------------------------------------------------- //
// 关注列表：缓存 + 断点续传
// --------------------------------------------------------------------------- //
fn list_file(out: &PathBuf) -> PathBuf {
    out.join("following_all.json")
}
fn meta_file(out: &PathBuf) -> PathBuf {
    out.join("following_meta.json")
}

struct Cached {
    accounts: Vec<crate::douyin::FollowAccount>,
    offset: i64,
    total: i64,
    done: bool,
    skip_fetch: bool,
    /// 是否从头完整读取（而不是复用缓存或断点续传）。
    /// 只有完整重读才做「新增 / 消失」的差异对比。
    full_read: bool,
}

fn load_cache(state: &Arc<AppState>) -> Cached {
    let mut out = Cached { accounts: Vec::new(), offset: 0, total: 0, done: false, skip_fetch: false, full_read: false };
    let (Ok(list_text), Ok(meta_text)) = (
        std::fs::read_to_string(list_file(&state.cfg().out_dir)),
        std::fs::read_to_string(meta_file(&state.cfg().out_dir)),
    ) else {
        return out;
    };
    let (Ok(list), Ok(meta)) = (
        serde_json::from_str::<Vec<Value>>(&list_text),
        serde_json::from_str::<Value>(&meta_text),
    ) else {
        return out;
    };

    out.accounts = list
        .into_iter()
        .map(|x| crate::douyin::FollowAccount {
            sec: x.get("sec").and_then(Value::as_str).unwrap_or("").to_string(),
            uid: x.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
            nick: x.get("nick").and_then(Value::as_str).unwrap_or("").to_string(),
            aweme_count: x.get("aweme_count").and_then(Value::as_i64).unwrap_or(0),
            avatar: x.get("avatar").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .filter(|a| !a.sec.is_empty())
        .collect();

    let age = now_sec() - meta.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
    let done = meta.get("done").and_then(Value::as_bool).unwrap_or(false);
    out.total = meta.get("total").and_then(Value::as_i64).unwrap_or(0);
    out.done = done;

    if done && age < 86400 {
        // 24 小时内且上次已读完：直接用缓存
        out.skip_fetch = true;
    } else if !done && age < 21600 {
        // 6 小时内的中断：接着上次的位置继续
        out.offset = meta.get("offset").and_then(Value::as_i64).unwrap_or(0);
    } else {
        // 距上次超过 24 小时（或上次没读完且已过期）：从头完整重读，
        // 读完后会与数据库做差异对比
        out.accounts.clear();
        out.offset = 0;
        out.full_read = true;
        state.log(
            &format!(
                "距上次读取关注列表已 {} 小时，重新完整读取",
                age.max(0) / 3600
            ),
            "",
        );
    }
    out
}

fn save_cache(state: &Arc<AppState>, accounts: &[crate::douyin::FollowAccount], offset: i64, total: i64, done: bool) {
    let list: Vec<Value> = accounts
        .iter()
        .map(|a| json!({ "sec": a.sec, "uid": a.uid, "nick": a.nick, "aweme_count": a.aweme_count, "avatar": a.avatar }))
        .collect();
    let _ = std::fs::write(list_file(&state.cfg().out_dir), serde_json::to_string(&list).unwrap_or_default());
    let meta = json!({ "offset": offset, "total": total, "done": done, "updatedAt": now_sec(), "count": accounts.len() });
    let _ = std::fs::write(meta_file(&state.cfg().out_dir), serde_json::to_string(&meta).unwrap_or_default());
}

/// 一次关注列表读取的结果。
struct ListFetch {
    accounts: Vec<crate::douyin::FollowAccount>,
    /// 接口报告的关注总数（用于校验是否读完整）
    total: i64,
    /// 是否确认已完整读取：读到 has_more=false，或数量已达到 total
    complete: bool,
    /// 是否属于「距上次超过 24 小时」的完整重读
    full_read: bool,
}

fn fetch_following_list(state: &Arc<AppState>, session: &Session) -> Result<ListFetch, String> {
    let mut cache = load_cache(state);
    let full_read = cache.full_read;
    let mut accounts = std::mem::take(&mut cache.accounts);

    if cache.skip_fetch {
        state.log(&format!("使用缓存的关注列表：{} 个", accounts.len()), "");
        return Ok(ListFetch { accounts, total: cache.total, complete: true, full_read: false });
    }

    let mut offset = cache.offset;
    let mut total = cache.total;
    let mut done = false;
    let mut page_no = offset / 20;
    let mut seen: std::collections::HashSet<String> = accounts.iter().map(|a| a.sec.clone()).collect();

    loop {
        if !wait_pause(state) {
            break;
        }
        page_no += 1;
        let mut r = session.list_page(offset);
        let mut attempt = 0;
        let max_retry = state.cfg().retry_max;
        let retry_wait = state.cfg().list_retry_wait_sec;
        while attempt < max_retry && (r.blocked || r.error.is_some() || r.sc != 0) {
            attempt += 1;
            state.log(
                &format!("关注列表被限流，第 {attempt}/{max_retry} 次重试（等待 {} 分钟）", retry_wait / 60),
                "warn",
            );
            state.update(|p| p.status = "waiting".into());
            let st = Arc::clone(state);
            let ok = wait_seconds(
                state,
                retry_wait,
                Some(Box::new(move |s| {
                    st.update(|p| {
                        p.wait_text = format!(
                            "关注列表被限流，第 {attempt}/{max_retry} 次重试｜{} 后自动重试",
                            fmt_duration(s)
                        );
                    });
                })),
            );
            state.update(|p| {
                p.wait_text.clear();
                p.status = "running".into();
            });
            if !ok {
                break;
            }
            r = session.list_page(offset);
        }
        if r.blocked || r.error.is_some() || r.sc != 0 {
            state.log(&format!("列表页重试 {} 次后仍失败，停止拉取", state.cfg().retry_max), "warn");
            break;
        }
        if r.total > 0 {
            total = r.total;
        }
        let n = r.list.len();
        for a in r.list {
            if seen.insert(a.sec.clone()) {
                accounts.push(a);
            }
        }
        state.update(|p| p.current_account = format!("获取关注列表中… {}/{}", accounts.len(), total));
        if page_no % 10 == 0 {
            state.log(&format!("已获取 {}/{}", accounts.len(), total), "");
            save_cache(state, &accounts, offset, total, false);
        }
        if !r.has_more {
            done = true;
            break;
        }
        offset += n.max(20) as i64;
        if total > 0 && accounts.len() as i64 >= total {
            done = true;
            break;
        }
        if page_no > 1500 {
            break;
        }
        thread::sleep(Duration::from_millis(rnd(1500, 3000) as u64));
    }

    save_cache(state, &accounts, offset, total, done);
    state.log(&format!("关注列表获取完成：累计 {} 个", accounts.len()), "");

    // 与关注数校验，确认确实读完整了（不完整就不做差异对比，避免误判）
    let complete = done && total > 0 && accounts.len() as i64 >= total;
    if done && total > 0 && !complete {
        state.log(
            &format!("列表数量与关注数不一致（{} / {}），本次不做差异对比", accounts.len(), total),
            "warn",
        );
    } else if complete {
        state.log(&format!("已与关注数校验一致（{} 个）", total), "ok");
    }

    Ok(ListFetch { accounts, total, complete, full_read })
}

// --------------------------------------------------------------------------- //
// 主流程
// --------------------------------------------------------------------------- //
pub fn run_scan(state: Arc<AppState>, session: Session) {
    state.update(|p| {
        p.status = "running".into();
        p.phase = "获取关注列表".into();
        p.started_at = Some(now_sec() * 1000);
    });

    let fetched = match fetch_following_list(&state, &session) {
        Ok(a) => a,
        Err(e) => {
            state.log(&format!("获取关注列表失败：{e}"), "err");
            state.notify("err", &format!("获取关注列表失败：{e}"));
            state.update(|p| p.status = "error".into());
            return;
        }
    };
    let accounts = fetched.accounts;
    if accounts.is_empty() {
        state.log("未获取到任何关注账号，请确认已登录", "err");
        state.notify("err", "未获取到任何关注账号，请确认已登录");
        state.update(|p| p.status = "error".into());
        return;
    }

    // 入库
    {
        let db = state.db.lock().unwrap();
        let now = now_sec();
        for a in &accounts {
            let _ = db.execute(
                "INSERT INTO accounts (sec_uid, uid, nickname, home_url, avatar, aweme_count, first_seen_at) \
                 VALUES (?,?,?,?,?,?,?) \
                 ON CONFLICT(sec_uid) DO UPDATE SET uid=excluded.uid, nickname=excluded.nickname, \
                 home_url=excluded.home_url, avatar=excluded.avatar, aweme_count=excluded.aweme_count",
                &[
                    json!(a.sec),
                    json!(a.uid),
                    json!(a.nick),
                    json!(format!("https://www.douyin.com/user/{}", a.sec)),
                    json!(a.avatar),
                    json!(a.aweme_count),
                    json!(now),
                ],
            );
        }
    }

    // 完整重读且已与关注数校验一致时，做差异对比：
    //   新增的 → 已在上面入库，last_checked_at 为空，即「未检查」
    //   消失的 → 标记「已取关」
    if fetched.full_read && fetched.complete {
        let db = state.db.lock().unwrap();
        let now = now_sec();

        let existing: Vec<String> = db
            .query("SELECT sec_uid FROM accounts", &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_string))
            .collect();
        let marked: std::collections::HashSet<String> = db
            .query("SELECT sec_uid FROM accounts WHERE unfollowed_at IS NOT NULL", &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_string))
            .collect();

        let live: std::collections::HashSet<&str> = accounts.iter().map(|a| a.sec.as_str()).collect();

        // 消失的：在库里但不在新列表里
        let mut gone = 0i64;
        for sec in &existing {
            if !live.contains(sec.as_str()) && !marked.contains(sec) {
                db.mark_unfollowed(sec, now);
                gone += 1;
            }
        }
        // 重新出现的：之前标过已取关，现在又在列表里 → 清掉标记
        let mut back = 0i64;
        for a in &accounts {
            if marked.contains(&a.sec) {
                db.clear_unfollowed(&a.sec);
                back += 1;
            }
        }

        let new_count = accounts.len() as i64 - (existing.len() as i64 - gone);
        state.log(
            &format!(
                "关注列表差异（共 {} 个关注）：新增 {} 个（按未检查处理），消失 {} 个（按已取关处理）",
                fetched.total,
                new_count.max(0),
                gone
            ),
            "ok",
        );
        if back > 0 {
            state.log(&format!("其中 {back} 个重新出现在列表里，已清除「已取关」标记"), "");
        }
    }

    let stale_sec = state.cfg().stale_days * 86400;
    let now = now_sec();
    // 待检查队列 = 停更 / 无作品时间的账号，**且最近没检查过**。
    //
    // 之前只按 last_update_ts 筛，导致停更账号和异常账号永远留在队列里：
    // 停止扫描后再点开始，这些账号会从头重查一遍、进度归零，
    // 表现就是「停止后不能继续」。加上 last_checked_at 窗口后，
    // 重新开始会跳过本轮已检查过的账号，真正接着往下走。
    let recheck_sec = state.cfg().recheck_hours.max(1) as i64 * 3600;
    let queue: Vec<Vec<Value>> = {
        let db = state.db.lock().unwrap();
        db.query(
            "SELECT sec_uid, uid, nickname, home_url, avatar, aweme_count FROM accounts \
             WHERE (last_update_ts IS NULL OR (? - last_update_ts) > ?) \
               AND (last_checked_at IS NULL OR (? - last_checked_at) > ?) \
             ORDER BY (last_checked_at IS NULL) DESC, last_checked_at ASC, \
                      (last_update_ts IS NULL) DESC, last_update_ts ASC",
            &[json!(now), json!(stale_sec), json!(now), json!(recheck_sec)],
        )
        .unwrap_or_default()
    };

    let db_total = {
        let db = state.db.lock().unwrap();
        db.query_int("SELECT COUNT(*) FROM accounts", &[])
    };
    state.log(
        &format!(
            "待检查 {} 个｜跳过 {} 个（近 {} 天内已更新，或 {} 小时内已检查过）",
            queue.len(),
            db_total - queue.len() as i64,
            state.cfg().stale_days,
            state.cfg().recheck_hours
        ),
        "",
    );
    state.update(|p| {
        p.phase = format!("检查最新更新时间（阈值 {} 天）", state.cfg().stale_days);
        p.total = queue.len() as i64;
        p.checked = 0;
        p.stale = 0;
        p.active = 0;
        p.error = 0;
    });

    // 逐个检查
    let mut batch_left = rnd(state.cfg().batch_min, state.cfg().batch_max);
    let mut rest_left = rnd(state.cfg().rest_min, state.cfg().rest_max);
    let mut rest_no = 0;
    if state.cfg().long_rest {
        state.log(
            &format!(
                "预防性休息：每 {}-{} 个账号，主动暂停 {}-{} 分钟",
                state.cfg().rest_min, state.cfg().rest_max, state.cfg().rest_minutes_min, state.cfg().rest_minutes_max
            ),
            "",
        );
    }

    let t0 = Instant::now();
    let mut i: i64 = 0;
    let mut s_stale = 0i64;
    let mut s_active = 0i64;
    let mut s_err = 0i64;
    let mut stale_list: Vec<Value> = Vec::new();
    let mut error_list: Vec<Value> = Vec::new();

    for row in &queue {
        if state.is_stopped() || !wait_pause(&state) {
            break;
        }
        i += 1;
        let sec = row.get(0).and_then(Value::as_str).unwrap_or("").to_string();
        let uid = row.get(1).and_then(Value::as_str).unwrap_or("").to_string();
        let nick = row.get(2).and_then(Value::as_str).unwrap_or("").to_string();
        let home = row.get(3).and_then(Value::as_str).unwrap_or("").to_string();
        let avatar = row.get(4).and_then(Value::as_str).unwrap_or("").to_string();
        let aweme_count = row.get(5).and_then(Value::as_str).unwrap_or("0").parse::<i64>().unwrap_or(0);

        state.update(|p| p.current_account = if nick.is_empty() { sec.clone() } else { nick.clone() });

        let mut res = session.fetch_latest(&sec);
        let mut block_retry = 0;
        while res.blocked {
            block_retry += 1;
            if block_retry > state.cfg().retry_max {
                let m = format!("触发风控且重试已满 {} 次，本轮已停止（进度已保存，可稍后继续）", state.cfg().retry_max);
                state.log(&m, "err");
                state.notify("err", &m);
                state.update(|p| {
                    p.status = "error".into();
                    p.wait_text.clear();
                });
                state.stopped.store(true, Ordering::SeqCst);
                break;
            }
            state.update(|p| p.status = "waiting".into());
            state.log(&format!("触发风控（{nick}），第 {block_retry}/{} 次等待重试…", state.cfg().retry_max), "warn");
            let wait = state.cfg().retry_wait_sec;
            let st = Arc::clone(&state);
            let ok = wait_seconds(
                &state,
                wait,
                Some(Box::new(move |s| {
                    st.update(|p| {
                        p.wait_text = format!("触发风控，第 {block_retry}/{} 次重试｜{} 后重试", st.cfg().retry_max, fmt_duration(s));
                    });
                })),
            );
            if !ok {
                break;
            }
            res = session.fetch_latest(&sec);
        }
        state.update(|p| p.wait_text.clear());
        if state.is_stopped() {
            break;
        }
        state.update(|p| p.status = "running".into());

        let (status, days, note, ts): (&str, Option<i64>, String, Option<i64>) = if let Some(e) = &res.error {
            ("error", None, e.clone(), None)
        } else if res.sc != 0 {
            ("error", None, format!("接口码{}", res.sc), None)
        } else if res.newest.is_none() {
            ("error", None, if res.count == 0 { "无作品/私密".into() } else { "无时间".into() }, None)
        } else {
            let t = res.newest.unwrap();
            let d = (now_sec() - t) / 86400;
            if d > state.cfg().stale_days { ("stale", Some(d), String::new(), Some(t)) } else { ("active", Some(d), String::new(), Some(t)) }
        };

        {
            let db = state.db.lock().unwrap();
            let _ = db.execute(
                "UPDATE accounts SET last_update_ts=?, last_update_date=?, status=?, note=?, last_checked_at=? WHERE sec_uid=?",
                &[
                    ts.map(|t| json!(t)).unwrap_or(Value::Null),
                    ts.map(|t| json!(day_str(t))).unwrap_or(Value::Null),
                    json!(status),
                    json!(note),
                    json!(now_sec()),
                    json!(sec),
                ],
            );
        }

        match status {
            "stale" => {
                s_stale += 1;
                stale_list.push(json!({
                    "nickname": nick, "uid": uid, "secUid": sec, "homeUrl": home, "avatar": avatar,
                    "awemeCount": aweme_count, "lastPostDate": ts.map(day_str).unwrap_or_default(), "days": days,
                }));
                stale_list.sort_by(|a, b| b["days"].as_i64().unwrap_or(0).cmp(&a["days"].as_i64().unwrap_or(0)));
                let sl = stale_list.clone();
                state.update(|p| p.stale_list = sl.clone());
            }
            "active" => s_active += 1,
            _ => {
                s_err += 1;
                error_list.push(json!({
                    "nickname": nick, "uid": uid, "secUid": sec, "homeUrl": home, "avatar": avatar, "note": note,
                }));
                let el = error_list.clone();
                state.update(|p| p.error_list = el.clone());
            }
        }

        let elapsed = t0.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 { i as f64 / elapsed } else { 0.0 };
        state.update(|p| {
            p.checked = i;
            p.stale = s_stale;
            p.active = s_active;
            p.error = s_err;
            p.rate = rate;
            p.eta_seconds = if rate > 0.0 { Some(((queue.len() as f64 - i as f64) / rate) as i64) } else { None };
        });
        if i % 25 == 0 {
            state.log(
                &format!("进度 {}/{}｜停更 {}｜正常 {}｜异常 {}｜{:.2}/s", i, queue.len(), s_stale, s_active, s_err, rate),
                "",
            );
        }

        // 小批次暂停
        batch_left = batch_left.saturating_sub(1);
        if batch_left == 0 {
            batch_left = rnd(state.cfg().batch_min, state.cfg().batch_max);
            let pause = rnd(state.cfg().batch_pause_min, state.cfg().batch_pause_max) as u64;
            if !wait_seconds(&state, pause, None) {
                break;
            }
        } else {
            thread::sleep(Duration::from_millis(rnd(state.cfg().delay_min as u32, state.cfg().delay_max as u32) as u64));
        }

        // 预防性休息
        if state.cfg().long_rest {
            rest_left = rest_left.saturating_sub(1);
            if rest_left == 0 {
                rest_no += 1;
                let wait_min = rnd(state.cfg().rest_minutes_min, state.cfg().rest_minutes_max);
                rest_left = rnd(state.cfg().rest_min, state.cfg().rest_max);
                state.log(&format!("预防性休息（第 {rest_no} 次）：主动暂停 {wait_min} 分钟，避免触发风控"), "warn");
                state.update(|p| p.status = "waiting".into());
                let st = Arc::clone(&state);
                let ok = wait_seconds(
                    &state,
                    wait_min as u64 * 60,
                    Some(Box::new(move |s| {
                        st.update(|p| {
                            p.wait_text = format!("预防性休息（第 {rest_no} 次）｜{} 后自动继续｜已检查 {i} 个", fmt_duration(s));
                        });
                    })),
                );
                state.update(|p| p.wait_text.clear());
                if !ok {
                    break;
                }
                state.update(|p| p.status = "running".into());
                state.log("预防性休息结束，继续扫描", "");
            }
        }
    }

    // 生成报告
    let _ = report::write_reports(&state);
    // 被停止时必须把状态从 running 里放出来，否则前端 busy 一直为真，
    // 「开始扫描」按钮永久禁用 —— 表现就是「停止后不能继续」。
    if state.is_stopped() {
        state.update(|p| p.status = "stopped".into());
    } else {
        state.update(|p| p.status = "done".into());
    }
    state.update(|p| p.current_account = "已完成".into());
    state.log(&format!("本轮结束：检查 {i} 个｜停更 {s_stale}｜正常 {s_active}｜异常 {s_err}"), "ok");
}
