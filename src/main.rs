#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::Parser;
use regex::RegexSet;
use sysinfo::System;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info};

mod log_reader;
mod server;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = { "127.0.0.1:6754".to_string() })]
    address: String,
    #[arg(long)]
    log_path: Option<PathBuf>,
    #[arg(long, default_value_t = 5)]
    client_timeout: u64,
    #[arg(long, default_value_t = 1)]
    heart_beat: u64,
    /// Suppress informational log output
    #[arg(long, short)]
    quiet: bool,
}

// 16x16 amber dot icon, RGBA, drawn once at startup.
fn tray_icon_data() -> tray_icon::Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let inside = dx * dx + dy * dy <= 30.0;
            if inside {
                rgba.extend_from_slice(&[0xF5, 0xA6, 0x23, 0xFF]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon")
}

enum AppEvent {
    TrayMenu(muda::MenuEvent),
    ServerDone(ExitCode),
}
struct App {
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    tray: Option<tray_icon::TrayIcon>,
    quit_id: muda::MenuId,
    runtime: Option<tokio::runtime::Runtime>,
    args: Option<Args>,
    exit_code: ExitCode,
}

impl App {
    fn new(args: Args, proxy: winit::event_loop::EventLoopProxy<AppEvent>) -> Self {
        Self {
            proxy,
            tray: None,
            quit_id: muda::MenuId::new("quit-placeholder"),
            runtime: None,
            args: Some(args),
            exit_code: ExitCode::SUCCESS,
        }
    }

    fn start_server(&mut self) {
        let args = self.args.take().expect("args taken once");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let proxy = self.proxy.clone();
        runtime.spawn(async move {
            let code = run_server(args).await;
            let _ = proxy.send_event(AppEvent::ServerDone(code));
        });
        self.runtime = Some(runtime);
    }
}

impl winit::application::ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let menu = muda::Menu::new();
        let quit = muda::MenuItem::new("Quit exile-log-api", true, None);
        menu.append(&quit).expect("menu item");
        self.quit_id = quit.id().clone();
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("exile-log-api")
            .with_icon(tray_icon_data())
            .build()
            .expect("tray icon");
        self.tray = Some(tray);
        self.start_server();
    }

    fn user_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        event: AppEvent,
    ) {
        match event {
            AppEvent::TrayMenu(ev) => {
                if ev.id == self.quit_id {
                    if let Some(rt) = self.runtime.take() {
                        rt.shutdown_background();
                    }
                    self.tray = None;
                    self.exit_code = ExitCode::SUCCESS;
                    event_loop.exit();
                }
            }
            AppEvent::ServerDone(code) => {
                self.exit_code = code;
                if let Some(rt) = self.runtime.take() {
                    rt.shutdown_background();
                }
                self.tray = None;
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Logging must init on the main thread before the runtime spawns workers.
    let max_level = if args.quiet {
        tracing::Level::ERROR
    } else {
        tracing::Level::INFO
    };
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_max_level(max_level)
        .with_ansi(false)
        .with_writer(non_blocking)
        .init();
    // Keep the non-blocking guard alive for the process lifetime.
    std::mem::forget(_guard);

    let event_loop = winit::event_loop::EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();
    muda::MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(AppEvent::TrayMenu(event));
    }));

    let mut app = App::new(args, event_loop.create_proxy());
    if let Err(err) = event_loop.run_app(&mut app) {
        error!("Event loop failed: {}", err);
        return ExitCode::FAILURE;
    }
    app.exit_code
}

async fn run_server(args: Args) -> ExitCode {
    let log_path = match args.log_path {
        Some(log_path) => log_path,
        _ => {
            info!("Waiting for PathOfExile client...");

            async {
                let mut interval = tokio::time::interval(Duration::from_secs(args.heart_beat));
                loop {
                    let Ok(mut sys) = System::new_all() else {
                        error!("PathOfExile auto-detect unsupported on this system; retrying...");
                        interval.tick().await;
                        continue;
                    };
                    sys.refresh_all();

                    for process in sys.processes_by_name("PathOfExile".as_ref()) {
                        if let Some(cwd) = process.cwd() {
                            let log_path = cwd.join("logs").join("LatestClient.txt");
                            if let Ok(true) = log_path.try_exists() {
                                return log_path;
                            }
                        }
                        if let Some(dir) = process.exe().and_then(|p| p.parent()) {
                            let log_path = dir.join("logs").join("LatestClient.txt");
                            if let Ok(true) = log_path.try_exists() {
                                return log_path;
                            }
                        }
                    }

                    interval.tick().await;
                }
            }
            .await
        }
    };

    info!("Log file: {}", log_path.display());

    let (tx, rx) = tokio::sync::broadcast::channel(16);

    let listen = server::listen(args.address, rx, Duration::from_secs(args.client_timeout));
    let log = async move {
        let patterns = RegexSet::new([r"Generating level \d+ area"]).unwrap();
        let mut reader: Option<log_reader::Reader> = None;
        let mut interval = tokio::time::interval(Duration::from_secs(args.heart_beat));
        loop {
            let mut messages = vec![Message::Ping(Default::default())];
            if reader.is_none() {
                match log_reader::build(&log_path, patterns.clone()) {
                    Ok(built) => reader = Some(built),
                    Err(e) => {
                        error!("log read failed: {e}");
                        let _ = tx.send(messages);
                        interval.tick().await;
                        continue;
                    }
                }
            }
            if let Some(r) = reader.as_mut()
                && let Err(e) = r.read_latest(|line| messages.push(line.into()))
            {
                error!("log read failed: {e}");
                reader = None;
            }
            let _ = tx.send(messages);
            interval.tick().await;
        }
    };

    let res = tokio::select! {
        res = listen => res,
        res = log => res,
    };

    match res {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{}", err);
            ExitCode::FAILURE
        }
    }
}
