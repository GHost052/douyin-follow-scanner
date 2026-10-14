//! 抖音关注清理助手 —— 轻量原生桌面应用。
//!
//! 架构：Rust 单进程。
//!   · 后台线程：本地 HTTP 服务（给界面提供 API）+ 扫描引擎 + 浏览器自动化（原生 CDP）
//!   · 主线程  ：原生窗口（系统自带 WebView2 内核，不捆绑浏览器）
//! 产物为单个可执行文件，双击即用，全程不经过浏览器。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cdp;
mod config;
mod db;
mod douyin;
mod report;
mod scanner;
mod server;
mod state;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
#[cfg(windows)]
use wry::WebViewBuilderExtWindows;

fn main() {
    let cfg = config::Config::load();
    if std::fs::create_dir_all(&cfg.out_dir).is_err() {
        eprintln!("无法创建数据目录：{}", cfg.out_dir.display());
    }

    let db_path = cfg.out_dir.join("douyin_accounts.db");
    let db = match db::Db::open(&db_path.to_string_lossy()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("打开数据库失败：{e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(state::AppState::new(cfg, db));
    state.load_settings();
    state.log("抖音关注清理助手已启动", "ok");
    state.log(&format!("数据目录：{}", state.cfg().out_dir.display()), "");

    // 先把 HTTP 服务绑好端口，再打开窗口，避免窗口抢先加载导致白屏
    let port = server::start(Arc::clone(&state));
    if port == 0 {
        state.log("界面服务启动失败，退出", "err");
        return;
    }

    run_window(state, port);
}

fn run_window(state: Arc<AppState>, port: u16) {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("抖音关注清理助手")
        .with_inner_size(LogicalSize::new(1320.0, 860.0))
        .with_min_inner_size(LogicalSize::new(960.0, 640.0))
        .build(&event_loop)
        .expect("创建窗口失败");

    let url = format!("http://127.0.0.1:{port}/");

    // WebView2 需要一个可写的用户数据目录。默认位置在某些环境下不可用（会静默失败导致白屏），
    // 因此显式指定到数据目录下。
    let webview_data = state.cfg().out_dir.join("webview2");
    if let Err(e) = std::fs::create_dir_all(&webview_data) {
        state.log(&format!("创建 WebView2 数据目录失败：{e}"), "err");
    }
    state.log(&format!("正在打开窗口：{url}"), "");

    // WebView2 需要一个可写的用户数据目录。默认位置在某些环境下不可用（会静默失败导致白屏），
    // 因此显式指定到数据目录下。WebContext 必须比 WebView 活得久。
    let mut web_context = wry::WebContext::new(Some(webview_data));

    let mut builder = WebViewBuilder::new_with_web_context(&mut web_context).with_url(&url);

    // 排障用：设置 DYC_DEBUG_PORT 后可通过 CDP 检查页面（默认不开启）
    if let Ok(debug_port) = std::env::var("DYC_DEBUG_PORT") {
        builder = builder.with_additional_browser_args(format!("--remote-debugging-port={debug_port}"));
        state.log(&format!("已开启调试端口 {debug_port}"), "warn");
    }

    let built = builder.build(&window);

    if let Err(e) = built {
        state.log(&format!("创建 WebView 失败：{e}"), "err");
        eprintln!("创建 WebView 失败：{e}");
        eprintln!("请确认系统已安装 Microsoft Edge WebView2 运行时（Windows 10/11 通常已内置）。");
        std::process::exit(1);
    }
    state.log("窗口已创建", "ok");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _ = &web_context; // 保持 WebContext 存活
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            // 关窗即停：让扫描线程尽快退出，并释放浏览器
            state.stopped.store(true, Ordering::SeqCst);
            state.paused.store(false, Ordering::SeqCst);
            *control_flow = ControlFlow::Exit;
        }
    });
}

use state::AppState;
