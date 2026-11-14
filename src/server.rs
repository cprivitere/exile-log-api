use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{
        broadcast::{Receiver, error::RecvError},
        mpsc,
    },
};
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::info;

/// Events the tray status state machine consumes. Sent from async tasks,
/// applied on the single winit event-loop thread (no races, no flapping).
#[derive(Debug, Clone, Copy)]
pub enum StatusEvent {
    /// A leveling tab completed the WS handshake.
    ClientConnected,
    /// A connected tab went away (close / timeout / error).
    ClientDisconnected,
    /// The log tail failed this tick (locked / rotated / deleted).
    LogFailed,
    /// The log tail read cleanly this tick.
    LogHealthy,
    /// 1s housekeeping tick (drives the idle grace countdown).
    Tick,
}

/// Shared client counter + status sink. Cloned into every connection task.
#[derive(Debug, Clone)]
pub struct Status {
    clients: Arc<AtomicUsize>,
    tx: mpsc::UnboundedSender<StatusEvent>,
}

impl Status {
    pub fn new(tx: mpsc::UnboundedSender<StatusEvent>) -> Self {
        Self {
            clients: Arc::new(AtomicUsize::new(0)),
            tx,
        }
    }


    pub fn send(&self, ev: StatusEvent) {
        let _ = self.tx.send(ev);
    }
}
pub async fn listen(
    addr: String,
    rx: Receiver<Vec<Message>>,
    timeout: Duration,
    status: Status,
) -> anyhow::Result<()> {
    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket?;
    info!("Listening on: {}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let rx = rx.resubscribe();
        let status = status.clone();
        tokio::spawn(async move {
            if let Err(err) = accept_connection(stream, rx, timeout, &status).await {
                info!("Client failed to connect: {}", err);
            }
            status
                .clients
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .ok();
            status.send(StatusEvent::ClientDisconnected);
        });
    }
}

async fn accept_connection(
    stream: TcpStream,
    mut rx: Receiver<Vec<Message>>,
    timeout: Duration,
    status: &Status,
) -> anyhow::Result<()> {
    let addr = stream
        .peer_addr()
        .context("Connected streams should have a peer address")?;

    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .context("Error during the websocket handshake occurred")?;

    info!("Connected: {}", addr);
    status.clients.fetch_add(1, Ordering::Relaxed);
    status.send(StatusEvent::ClientConnected);

    let (mut write, mut read) = ws_stream.split();

    let read_handle = async {
        loop {
            let timeout = tokio::time::timeout(timeout, read.next());
            let message = timeout.await.context("Client timeout")?;
            let message = message.ok_or(tungstenite::Error::ConnectionClosed)??;
            match message {
                Message::Close(_) => Err(tungstenite::Error::ConnectionClosed),
                _ => Ok(()),
            }?;
        }
    };

    let write_handle = async {
        loop {
            match rx.recv().await {
                Ok(message_batch) => {
                    for message in message_batch {
                        write.feed(message).await?;
                    }
                    write.flush().await?;
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let res: anyhow::Result<()> = tokio::select! {
        res = read_handle => res,
        res = write_handle => res,
    };

    if let Err(err) = res {
        info!("Disconnected: {}", err);
    }

    Ok(())
}
