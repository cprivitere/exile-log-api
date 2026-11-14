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

/// Tray dot colors. Amber = waiting, green = live, red = log unreadable.
fn dot_icon(r: u8, g: u8, b: u8) -> tray_icon::Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let inside = dx * dx + dy * dy <= 30.0;
            if inside {
                rgba.extend_from_slice(&[r, g, b, 0xFF]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayStatus {
    /// No client yet (or none for a while): amber.
    Waiting,
    /// Tab connected, log reads healthy: green.
    Live,
    /// Log unreadable, retrying: red (latches until a healthy read).
    Degraded,
}

impl TrayStatus {
    fn icon(self) -> tray_icon::Icon {
        match self {
            TrayStatus::Waiting => dot_icon(0xF5, 0xA6, 0x23),
            TrayStatus::Live => dot_icon(0x3D, 0xD6, 0x6E),
            TrayStatus::Degraded => dot_icon(0xE5, 0x4E, 0x4E),
        }
    }

    fn tooltip(self, clients: usize) -> String {
        match self {
            TrayStatus::Waiting => "exile-log-api — waiting for client…".to_string(),
            TrayStatus::Live => format!("exile-log-api — live ({clients} client{})", if clients == 1 { "" } else { "s" }),
            TrayStatus::Degraded => "exile-log-api — log unreadable, retrying…".to_string(),
        }
    }
}

enum AppEvent {
    TrayMenu(muda::MenuEvent),
    ServerDone(ExitCode),
    Status(server::StatusEvent),
}

struct App {
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    tray: Option<tray_icon::TrayIcon>,
    quit_id: muda::MenuId,
    runtime: Option<tokio::runtime::Runtime>,
    args: Option<Args>,
    status: TrayStatus,
    clients: usize,
    /// Ticks since the last connected client went away. Green latches until
    /// this exceeds the grace period, so tab reloads don't flap amber/green.
    idle_ticks: u32,
    exit_code: ExitCode,
}

/// Heartbeats to wait with zero clients before leaving Live (10s at 1s/tick).
const IDLE_GRACE_TICKS: u32 = 10;

impl App {
    fn new(args: Args, proxy: winit::event_loop::EventLoopProxy<AppEvent>) -> Self {
        Self {
            proxy,
            tray: None,
            quit_id: muda::MenuId::new("quit-placeholder"),
            runtime: None,
            args: Some(args),
            status: TrayStatus::Waiting,
            clients: 0,
            idle_ticks: 0,
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
        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
        let status = server::Status::new(status_tx);
        // Bridge async status events onto the winit event loop, which owns
        // the tray icon. A 1s poll also drives the idle grace countdown.
        runtime.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    ev = status_rx.recv() => {
                        let Some(ev) = ev else { break };
                        if proxy.send_event(AppEvent::Status(ev)).is_err() {
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        if proxy.send_event(AppEvent::Status(server::StatusEvent::Tick)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let proxy = self.proxy.clone();
        runtime.spawn(async move {
            let code = run_server(args, status).await;
            let _ = proxy.send_event(AppEvent::ServerDone(code));
        });
        self.runtime = Some(runtime);
    }

    /// Apply one status event. The icon/tooltip only change on transitions.
    fn apply_status(&mut self, ev: server::StatusEvent) {
        use server::StatusEvent as E;
        match ev {
            E::ClientConnected => {
                self.idle_ticks = 0;
            }
            E::ClientDisconnected => {}
            E::Tick => {
                if self.clients == 0 {
                    self.idle_ticks = self.idle_ticks.saturating_add(1);
                }
            }
            E::LogFailed => {
                self.set_status(TrayStatus::Degraded);
                return;
            }
            E::LogHealthy => {
                if self.status == TrayStatus::Degraded {
                    self.set_status(if self.clients > 0 {
                        TrayStatus::Live
                    } else {
                        TrayStatus::Waiting
                    });
                }
                return;
            }
        }
        if self.status == TrayStatus::Degraded {
            return;
        }
        if self.clients > 0 {
            self.idle_ticks = 0;
            self.set_status(TrayStatus::Live);
        } else if self.idle_ticks > IDLE_GRACE_TICKS {
            self.set_status(TrayStatus::Waiting);
        }
    }

    fn set_status(&mut self, next: TrayStatus) {
        if next == self.status {
            if next == TrayStatus::Live {
                self.refresh_tooltip();
            }
            return;
        }
        self.status = next;
        if next == TrayStatus::Live {
            self.idle_ticks = 0;
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(Some(next.icon()));
            let _ = tray.set_tooltip(Some(next.tooltip(self.clients)));
        }
    }

    fn refresh_tooltip(&mut self) {
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(self.status.tooltip(self.clients)));
        }
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
            .with_tooltip(TrayStatus::Waiting.tooltip(0))
            .with_icon(TrayStatus::Waiting.icon())
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
            AppEvent::Status(ev) => {
                match ev {
                    server::StatusEvent::ClientConnected => {
                        self.clients = self.clients.saturating_add(1);
                    }
                    server::StatusEvent::ClientDisconnected => {
                        self.clients = self.clients.saturating_sub(1);
                    }
                    _ => {}
                }
                self.apply_status(ev);
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

async fn run_server(args: Args, status: server::Status) -> ExitCode {
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

    let listen = server::listen(
        args.address,
        rx,
        Duration::from_secs(args.client_timeout),
        status.clone(),
    );
    let log = async move {
        let patterns = RegexSet::new([r"Generating level \d+ area"]).unwrap();
        let mut reader: Option<log_reader::Reader> = None;
        let mut interval = tokio::time::interval(Duration::from_secs(args.heart_beat));
        loop {
            let mut messages = vec![Message::Ping(Default::default())];
            if reader.is_none() {
                match log_reader::build(&log_path, patterns.clone()) {
                    Ok(built) => {
                        reader = Some(built);
                        status.send(server::StatusEvent::LogHealthy);
                    }
                    Err(e) => {
                        error!("log read failed: {e}");
                        status.send(server::StatusEvent::LogFailed);
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
                status.send(server::StatusEvent::LogFailed);
                reader = None;
            } else {
                status.send(server::StatusEvent::LogHealthy);
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
