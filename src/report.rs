//! 报告生成：report.html + 三个 CSV。

use std::sync::Arc;

use serde_json::Value;

use crate::state::AppState;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn csv_cell(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn n(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Number(num)) => num.to_string(),
        Some(Value::String(t)) => t.clone(),
        _ => String::new(),
    }
}

pub fn write_reports(state: &Arc<AppState>) -> Result<(), String> {
    let rows = state.derive_rows();
    let stale_days = state.cfg().stale_days;

    let mut stale: Vec<&Value> = rows.iter().filter(|r| r["status"] == "stale").collect();
    stale.sort_by(|a, b| {
        b.get("days").and_then(Value::as_i64).unwrap_or(0)
            .cmp(&a.get("days").and_then(Value::as_i64).unwrap_or(0))
    });
    let errors: Vec<&Value> = rows.iter().filter(|r| r["status"] == "error").collect();
    let active_count = rows.iter().filter(|r| r["status"] == "active").count();
    let pending_count = rows.iter().filter(|r| r["status"] == "pending").count();

    // ---- CSV ----
    let mut csv = String::from("\u{feff}账号名,账号ID,账号URL,最后更新日期,距今天数,作品数\n");
    for r in &stale {
        csv.push_str(&format!(
            "{},{},{},{},{},{}\n",
            csv_cell(&s(r, "nickname")),
            csv_cell(&if s(r, "uid").is_empty() { s(r, "secUid") } else { s(r, "uid") }),
            csv_cell(&s(r, "homeUrl")),
            csv_cell(&s(r, "lastPostDate")),
            csv_cell(&n(r, "days")),
            csv_cell(&n(r, "awemeCount")),
        ));
    }
    std::fs::write(state.cfg().out_dir.join("stale_accounts.csv"), csv).map_err(|e| e.to_string())?;

    let mut csv = String::from("\u{feff}账号名,账号ID,账号URL,异常原因\n");
    for r in &errors {
        csv.push_str(&format!(
            "{},{},{},{}\n",
            csv_cell(&s(r, "nickname")),
            csv_cell(&if s(r, "uid").is_empty() { s(r, "secUid") } else { s(r, "uid") }),
            csv_cell(&s(r, "homeUrl")),
            csv_cell(&s(r, "note")),
        ));
    }
    std::fs::write(state.cfg().out_dir.join("error_accounts.csv"), csv).map_err(|e| e.to_string())?;

    let mut csv = String::from("\u{feff}账号名,账号ID,账号URL,最后更新日期,距今天数,状态,作品数\n");
    for r in &rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            csv_cell(&s(r, "nickname")),
            csv_cell(&if s(r, "uid").is_empty() { s(r, "secUid") } else { s(r, "uid") }),
            csv_cell(&s(r, "homeUrl")),
            csv_cell(&s(r, "lastPostDate")),
            csv_cell(&n(r, "days")),
            csv_cell(&s(r, "status")),
            csv_cell(&n(r, "awemeCount")),
        ));
    }
    std::fs::write(state.cfg().out_dir.join("all_accounts.csv"), csv).map_err(|e| e.to_string())?;

    // ---- HTML ----
    let tr_stale: String = stale
        .iter()
        .map(|a| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
                 <td><a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">打开主页</a></td></tr>",
                esc(&s(a, "nickname")),
                esc(&if s(a, "uid").is_empty() { s(a, "secUid") } else { s(a, "uid") }),
                esc(&s(a, "lastPostDate")),
                n(a, "days"),
                n(a, "awemeCount"),
                esc(&s(a, "homeUrl")),
            )
        })
        .collect();

    let tr_err: String = errors
        .iter()
        .map(|a| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td>\
                 <td><a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">打开主页</a></td></tr>",
                esc(&s(a, "nickname")),
                esc(&if s(a, "uid").is_empty() { s(a, "secUid") } else { s(a, "uid") }),
                esc(&s(a, "note")),
                esc(&s(a, "homeUrl")),
            )
        })
        .collect();

    let html = format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0"><title>抖音关注账号活跃度报告</title>
<style>body{{font-family:-apple-system,"Segoe UI","Microsoft YaHei",sans-serif;background:#f5f6f8;color:#1f2328;margin:0;padding:28px}}
.wrap{{max-width:1000px;margin:0 auto}}h1{{font-size:22px}}h2{{font-size:16px;margin:26px 0 10px}}
.cards{{display:flex;gap:14px;margin:18px 0;flex-wrap:wrap}}
.card{{background:#fff;border:1px solid #e6e8eb;border-radius:12px;padding:16px 20px;flex:1;min-width:150px}}
.card .k{{color:#6b7280;font-size:12px}}.card .v{{font-size:26px;font-weight:700}}
.card.s .v{{color:#e5484d}}.card.a .v{{color:#16a34a}}
table{{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e6e8eb;border-radius:12px;overflow:hidden}}
th,td{{padding:10px 14px;text-align:left;border-bottom:1px solid #f1f2f4;font-size:13.5px}}
th{{background:#fafbfc;color:#6b7280;font-weight:600}}.num{{font-variant-numeric:tabular-nums}}
a{{color:#2f6feb;text-decoration:none}}tr:last-child td{{border-bottom:none}}</style></head><body><div class="wrap">
<h1>抖音关注账号活跃度报告</h1>
<p style="color:#6b7280">判定标准：最新作品距今超过 {stale_days} 天 ｜ 共 {total} 个关注账号</p>
<div class="cards">
<div class="card"><div class="k">关注账号总数</div><div class="v">{total}</div></div>
<div class="card s"><div class="k">超过 {stale_days} 天未更新</div><div class="v">{n_stale}</div></div>
<div class="card a"><div class="k">正常更新</div><div class="v">{n_active}</div></div>
<div class="card"><div class="k">异常 / 无法获取</div><div class="v">{n_err}</div></div>
<div class="card"><div class="k">未检查</div><div class="v">{n_pending}</div></div></div>
<h2>一、超过 {stale_days} 天未更新的账号（{n_stale}）</h2>
<table><thead><tr><th>账号名</th><th>账号ID</th><th>最后更新</th><th>距今天数</th><th>作品数</th><th>主页</th></tr></thead>
<tbody>{tr_stale}</tbody></table>
<h2>二、异常 / 无法获取的账号（{n_err}）</h2>
<table><thead><tr><th>账号名</th><th>账号ID</th><th>异常原因</th><th>主页</th></tr></thead>
<tbody>{tr_err}</tbody></table>
</div></body></html>"#,
        stale_days = stale_days,
        total = rows.len(),
        n_stale = stale.len(),
        n_active = active_count,
        n_err = errors.len(),
        n_pending = pending_count,
        tr_stale = if tr_stale.is_empty() { "<tr><td colspan=\"6\" style=\"text-align:center;color:#6b7280\">无</td></tr>".to_string() } else { tr_stale },
        tr_err = if tr_err.is_empty() { "<tr><td colspan=\"4\" style=\"text-align:center;color:#6b7280\">无</td></tr>".to_string() } else { tr_err },
    );

    std::fs::write(state.cfg().out_dir.join("report.html"), html).map_err(|e| e.to_string())?;
    Ok(())
}
