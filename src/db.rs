//! SQLite 数据层。
//!
//! 直接调用 Windows 自带的 `winsqlite3.dll`（Win10 起内置）。
//! 采用**运行时动态加载**，因此不需要导入库、不需要 C 编译器，也不捆绑 SQLite 源码。

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;
use std::sync::OnceLock;

use serde_json::Value;

#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}
#[repr(C)]
struct Sqlite3Stmt {
    _private: [u8; 0],
}

type FnOpen = unsafe extern "C" fn(*const c_char, *mut *mut Sqlite3) -> c_int;
type FnClose = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
type FnExec = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    *const c_void,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;
type FnPrepare = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    c_int,
    *mut *mut Sqlite3Stmt,
    *mut *const c_char,
) -> c_int;
type FnStep = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type FnFinalize = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type FnBindText =
    unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, *const c_char, c_int, *const c_void) -> c_int;
type FnBindInt64 = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, i64) -> c_int;
type FnBindNull = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
type FnColumnText = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const c_uchar;
type FnColumnInt64 = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> i64;
type FnColumnType = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int;
type FnColumnCount = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type FnErrmsg = unsafe extern "C" fn(*mut Sqlite3) -> *const c_char;

struct Api {
    open: FnOpen,
    close: FnClose,
    exec: FnExec,
    prepare: FnPrepare,
    step: FnStep,
    finalize: FnFinalize,
    bind_text: FnBindText,
    bind_int64: FnBindInt64,
    bind_null: FnBindNull,
    column_text: FnColumnText,
    column_int64: FnColumnInt64,
    column_type: FnColumnType,
    column_count: FnColumnCount,
    errmsg: FnErrmsg,
}

unsafe extern "system" {
    fn LoadLibraryA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// 加载 winsqlite3.dll 并解析所需符号（只做一次）
fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load_api).as_ref().map_err(|e| e.clone())
}

fn load_api() -> Result<Api, String> {
    unsafe {
        let dll = CString::new("winsqlite3.dll").unwrap();
        let handle = LoadLibraryA(dll.as_ptr());
        if handle.is_null() {
            return Err("无法加载 winsqlite3.dll（Windows 10 及以上系统自带）".into());
        }
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let c = CString::new($name).unwrap();
                let p = GetProcAddress(handle, c.as_ptr());
                if p.is_null() {
                    return Err(format!("winsqlite3.dll 缺少符号：{}", $name));
                }
                std::mem::transmute::<*mut c_void, $ty>(p)
            }};
        }
        Ok(Api {
            open: sym!("sqlite3_open", FnOpen),
            close: sym!("sqlite3_close", FnClose),
            exec: sym!("sqlite3_exec", FnExec),
            prepare: sym!("sqlite3_prepare_v2", FnPrepare),
            step: sym!("sqlite3_step", FnStep),
            finalize: sym!("sqlite3_finalize", FnFinalize),
            bind_text: sym!("sqlite3_bind_text", FnBindText),
            bind_int64: sym!("sqlite3_bind_int64", FnBindInt64),
            bind_null: sym!("sqlite3_bind_null", FnBindNull),
            column_text: sym!("sqlite3_column_text", FnColumnText),
            column_int64: sym!("sqlite3_column_int64", FnColumnInt64),
            column_type: sym!("sqlite3_column_type", FnColumnType),
            column_count: sym!("sqlite3_column_count", FnColumnCount),
            errmsg: sym!("sqlite3_errmsg", FnErrmsg),
        })
    }
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;
/// SQLITE_TRANSIENT：让 SQLite 自己复制一份字符串
const TRANSIENT: *const c_void = -1isize as *const c_void;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
  sec_uid          TEXT PRIMARY KEY,
  uid              TEXT,
  nickname         TEXT,
  home_url         TEXT,
  avatar           TEXT,
  aweme_count      INTEGER,
  last_update_ts   INTEGER,
  last_update_date TEXT,
  status           TEXT,
  note             TEXT,
  first_seen_at    INTEGER,
  last_checked_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_accounts_last_update ON accounts(last_update_ts);
CREATE TABLE IF NOT EXISTS runs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at  INTEGER,
  finished_at INTEGER,
  queue       INTEGER,
  checked     INTEGER,
  stale       INTEGER,
  active      INTEGER,
  err         INTEGER
);
";

/// 增量迁移：老库补字段。已存在时 ALTER 会报错，忽略即可。
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE accounts ADD COLUMN unfollowed_at INTEGER",
];

pub struct Db {
    raw: *mut Sqlite3,
}

// SQLite 在串行模式下线程安全；外部用 Mutex 保证串行访问。
unsafe impl Send for Db {}

impl Db {
    pub fn open(path: &str) -> Result<Db, String> {
        let api = api()?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let c_path = CString::new(path).map_err(|e| e.to_string())?;
        let mut raw: *mut Sqlite3 = ptr::null_mut();
        let rc = unsafe { (api.open)(c_path.as_ptr(), &mut raw) };
        if rc != SQLITE_OK || raw.is_null() {
            return Err(format!("打开数据库失败（错误码 {rc}）"));
        }
        let db = Db { raw };
        db.exec("PRAGMA journal_mode=WAL;")?;
        db.exec(SCHEMA)?;
        for m in MIGRATIONS {
            let _ = db.exec(m); // 字段已存在时报错，忽略
        }
        Ok(db)
    }


    /// 记录「已取关」的时间
    pub fn mark_unfollowed(&self, sec_uid: &str, ts: i64) {
        let _ = self.execute(
            "UPDATE accounts SET unfollowed_at=? WHERE sec_uid=?",
            &[Value::from(ts), Value::from(sec_uid)],
        );
    }

    /// 清除「已取关」标记（账号重新出现在关注列表里，说明又关注回来了）
    pub fn clear_unfollowed(&self, sec_uid: &str) {
        let _ = self.execute(
            "UPDATE accounts SET unfollowed_at=NULL WHERE sec_uid=? AND unfollowed_at IS NOT NULL",
            &[Value::from(sec_uid)],
        );
    }

    fn last_error(&self) -> String {
        let Ok(api) = api() else { return "未知错误".into() };
        unsafe {
            let p = (api.errmsg)(self.raw);
            if p.is_null() {
                "未知错误".into()
            } else {
                CStr::from_ptr(p).to_string_lossy().to_string()
            }
        }
    }

    pub fn exec(&self, sql: &str) -> Result<(), String> {
        let api = api()?;
        let c_sql = CString::new(sql).map_err(|e| e.to_string())?;
        let rc = unsafe {
            (api.exec)(self.raw, c_sql.as_ptr(), ptr::null(), ptr::null_mut(), ptr::null_mut())
        };
        if rc != SQLITE_OK {
            return Err(self.last_error());
        }
        Ok(())
    }

    fn bind(api: &Api, stmt: *mut Sqlite3Stmt, params: &[Value]) -> Result<(), String> {
        for (i, v) in params.iter().enumerate() {
            let idx = (i + 1) as c_int;
            let rc = unsafe {
                match v {
                    Value::Null => (api.bind_null)(stmt, idx),
                    Value::Bool(b) => (api.bind_int64)(stmt, idx, if *b { 1 } else { 0 }),
                    Value::Number(n) => (api.bind_int64)(stmt, idx, n.as_i64().unwrap_or(0)),
                    Value::String(s) => {
                        let c = CString::new(s.as_str()).map_err(|e| e.to_string())?;
                        (api.bind_text)(stmt, idx, c.as_ptr(), -1, TRANSIENT)
                    }
                    other => {
                        let c = CString::new(other.to_string()).map_err(|e| e.to_string())?;
                        (api.bind_text)(stmt, idx, c.as_ptr(), -1, TRANSIENT)
                    }
                }
            };
            if rc != SQLITE_OK {
                return Err(format!("绑定参数失败（第 {idx} 个）"));
            }
        }
        Ok(())
    }

    /// 执行写语句（INSERT / UPDATE / DELETE）
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<(), String> {
        let api = api()?;
        let stmt = self.prepare(sql)?;
        let result = (|| -> Result<(), String> {
            Self::bind(api, stmt, params)?;
            let rc = unsafe { (api.step)(stmt) };
            if rc != SQLITE_DONE && rc != SQLITE_ROW {
                return Err(self.last_error());
            }
            Ok(())
        })();
        unsafe { (api.finalize)(stmt) };
        result
    }

    /// 查询，返回按列顺序排列的行
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        let api = api()?;
        let stmt = self.prepare(sql)?;
        let mut rows = Vec::new();
        let result = (|| -> Result<(), String> {
            Self::bind(api, stmt, params)?;
            loop {
                let rc = unsafe { (api.step)(stmt) };
                if rc == SQLITE_ROW {
                    let n = unsafe { (api.column_count)(stmt) };
                    let mut row = Vec::with_capacity(n as usize);
                    for c in 0..n {
                        let ty = unsafe { (api.column_type)(stmt, c) };
                        if ty == SQLITE_NULL {
                            row.push(Value::Null);
                        } else {
                            let p = unsafe { (api.column_text)(stmt, c) };
                            let s = if p.is_null() {
                                String::new()
                            } else {
                                unsafe { CStr::from_ptr(p as *const c_char) }
                                    .to_string_lossy()
                                    .to_string()
                            };
                            row.push(Value::String(s));
                        }
                    }
                    rows.push(row);
                } else if rc == SQLITE_DONE {
                    break;
                } else {
                    return Err(self.last_error());
                }
            }
            Ok(())
        })();
        unsafe { (api.finalize)(stmt) };
        result.map(|_| rows)
    }

    /// 查询单个整数（用于 COUNT）
    pub fn query_int(&self, sql: &str, params: &[Value]) -> i64 {
        let Ok(api) = api() else { return 0 };
        let Ok(stmt) = self.prepare(sql) else { return 0 };
        let mut out = 0i64;
        if Self::bind(api, stmt, params).is_ok() && unsafe { (api.step)(stmt) } == SQLITE_ROW {
            out = unsafe { (api.column_int64)(stmt, 0) };
        }
        unsafe { (api.finalize)(stmt) };
        out
    }

    fn prepare(&self, sql: &str) -> Result<*mut Sqlite3Stmt, String> {
        let api = api()?;
        let c_sql = CString::new(sql).map_err(|e| e.to_string())?;
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            (api.prepare)(self.raw, c_sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
        };
        if rc != SQLITE_OK {
            return Err(self.last_error());
        }
        Ok(stmt)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe {
                (api.close)(self.raw);
            }
        }
    }
}
