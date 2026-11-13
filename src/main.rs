#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::Parser;
use regex::RegexSet;
use sysinfo::System;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info};

mod log_reader;
mod server;

#[derive(Parser, Debug)]
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

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

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
                    }

                    interval.tick().await;
                }
            }
            .await
        }
    };

    info!("Log file: {}", log_path.display());

    let (tx, rx) = tokio::sync::broadcast::channel(1);

    let listen = server::listen(args.address, rx, Duration::from_secs(args.client_timeout));
    let log = async move {
        let mut reader = log_reader::build(
            log_path,
            RegexSet::new([r"Generating level \d+ area"]).unwrap(),
        )?;

        let mut interval = tokio::time::interval(Duration::from_secs(args.heart_beat));
        loop {
            let mut messages = vec![Message::Ping(Default::default())];
            reader.read_latest(|line| messages.push(line.into()))?;
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
