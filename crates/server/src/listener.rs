//! The loopback listener: it accepts until shutdown begins, and serves each connection's
//! requests in a task of its own until the connection ends or shutdown drops it.

use std::convert::Infallible;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use nsv_gateway::serve_connection;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::daemon::{App, StartError};
use crate::panic;

/// Where the daemon is in its shutdown, which only moves forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    Serving,
    /// Quit was chosen or the daemon was asked to stop; it serves until its owner closes it.
    StopAsked,
    /// No new connections or requests; each open connection finishes its response in progress
    /// and closes.
    Draining,
    /// Connections still open are dropped.
    Closed,
}

/// Moves `phase` forward to `next`; a later phase stays.
pub(crate) fn advance(phase: &watch::Sender<Phase>, next: Phase) {
    phase.send_if_modified(|current| {
        let forward = *current < next;
        if forward {
            *current = next;
        }
        forward
    });
}

pub(crate) fn bind(port: u16) -> Result<TcpListener, StartError> {
    let listener = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .map_err(|error| match error.kind() {
            io::ErrorKind::AddrInUse => StartError::PortInUse(port),
            _ => StartError::Listen(error),
        })?;
    listener.set_nonblocking(true).map_err(StartError::Listen)?;
    TcpListener::from_std(listener).map_err(StartError::Listen)
}

/// Accepts until shutdown begins; each connection serves its requests in a task of its own.
pub(crate) fn serve(app: &Arc<App>, listener: TcpListener, phase: &watch::Sender<Phase>) {
    let app = Arc::clone(app);
    let mut accepting = phase.subscribe();
    let connections = phase.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => connection(&app, stream, connections.clone()),
                    Err(error) => {
                        // Out of descriptors, for one: try again shortly rather than spin.
                        tracing::warn!("could not accept a connection: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                () = reached(&mut accepting, Phase::Draining) => return,
            }
        }
    });
}

/// Resolves once `phase` reached `wanted`.
async fn reached(phase: &mut watch::Receiver<Phase>, wanted: Phase) {
    // The sender lives as long as the daemon; without it, there is nothing left to serve.
    let _ = phase.wait_for(|phase| *phase >= wanted).await;
}

fn connection(app: &Arc<App>, stream: TcpStream, phase: watch::Receiver<Phase>) {
    let served = Arc::clone(app);
    let service = service_fn(move |request| {
        let app = Arc::clone(&served);
        async move { Ok::<_, Infallible>(app.dispatch(request).await) }
    });
    let (mut drain, mut closed) = (phase.clone(), phase);
    tokio::spawn(panic::request(async move {
        tokio::select! {
            // A failed connection is the client's: a hang-up, a bad request or a timeout.
            _ = serve_connection(stream, service, reached(&mut drain, Phase::Draining)) => {}
            () = reached(&mut closed, Phase::Closed) => {}
        }
    }));
}
