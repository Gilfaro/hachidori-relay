// SPDX-License-Identifier: GPL-3.0-or-later
//! A listener that rebinds between this computer only and every interface.
//!
//! One task owns the listening socket and accepts connections from it. A rebind is
//! a command to that task. It drops the old socket first, because Linux refuses a
//! wildcard bind beside a loopback listener on the same port, and then binds the
//! new one. Connections that were already accepted are never touched by a rebind.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};

use crate::net;

/// What runs for each accepted connection. It must return quickly, typically by spawning a task.
pub(crate) type Handler = Arc<dyn Fn(TcpStream, SocketAddr) + Send + Sync>;

struct Rebind {
    network: bool,
    reply: oneshot::Sender<io::Result<()>>,
}

/// The control side of one listening port.
pub(crate) struct Listener {
    port: u16,
    network: AtomicBool,
    commands: mpsc::Sender<Rebind>,
}

impl Listener {
    /// Starts accepting on `listener`, which is bound to `127.0.0.1`.
    pub(crate) fn spawn(listener: TcpListener, handler: Handler, shutdown: watch::Receiver<bool>) -> io::Result<Self> {
        let port = listener.local_addr()?.port();
        let (commands, receiver) = mpsc::channel(1);
        tokio::spawn(accept_loop(Some(listener), port, receiver, handler, shutdown));
        Ok(Self { port, network: AtomicBool::new(false), commands })
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }

    /// Whether the listener is bound to every interface.
    pub(crate) fn network(&self) -> bool {
        self.network.load(Ordering::Acquire)
    }

    /// Binds afresh, to every interface or to this computer only. If it fails, nothing is
    /// listening until the next successful call.
    pub(crate) async fn open(&self, network: bool) -> io::Result<()> {
        let (reply, answer) = oneshot::channel();
        let stopped = || io::Error::other("the relay is shutting down");
        self.commands.send(Rebind { network, reply }).await.map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())??;
        self.network.store(network, Ordering::Release);
        Ok(())
    }
}

async fn accept_loop(
    mut listener: Option<TcpListener>,
    port: u16,
    mut commands: mpsc::Receiver<Rebind>,
    handler: Handler,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            () = crate::until_true(&mut shutdown) => return,
            command = commands.recv() => {
                let Some(Rebind { network, reply }) = command else { return };
                let rebinding = listener.take().is_some();
                let result = net::bind_retrying(net::listen_address(network, port), rebinding).await;
                let _ = reply.send(result.map(|bound| listener = Some(bound)));
            }
            accepted = accept(listener.as_ref()) => match accepted {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    handler(stream, peer);
                }
                Err(error) => {
                    // Usually out of file descriptors. Wait a moment rather than spin.
                    tracing::debug!(%error, port, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

async fn accept(listener: Option<&TcpListener>) -> io::Result<(TcpStream, SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}
