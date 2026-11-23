//! The loopback listener a provider sends the browser back to after sign-in (the callback
//! server of `sign-in.ts`). It listens on 127.0.0.1 only and hands each request's method and
//! target to its handler. Closing it stops accepting at once: draining lets the requests in
//! flight finish, closing drops them too.

use std::convert::Infallible;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use nsv_core::pool::BoxFuture;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// How long a browser gets to send a request's head, as Node's `headersTimeout`.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// The wait after a failed accept, such as when the process is out of descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// What the handler answers: a fixed page.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: &'static [(&'static str, &'static str)],
    pub(crate) body: &'static str,
}

/// Answers a request's method and target.
pub(crate) type Handler = Arc<dyn Fn(Method, String) -> BoxFuture<'static, Reply> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Open,
    Drain,
    Close,
}

/// A listening callback server. Dropping it closes it.
pub(crate) struct Callback {
    id: u64,
    port: u16,
    stop: Arc<watch::Sender<Stop>>,
}

impl Callback {
    /// Listens on 127.0.0.1:`port`; port 0 picks a free one.
    ///
    /// # Errors
    /// The port is taken or cannot be bound.
    pub(crate) async fn listen(id: u64, port: u16, handler: Handler) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(watch::Sender::new(Stop::Open));
        tokio::spawn(accept(listener, Arc::clone(&stop), handler));
        Ok(Self { id, port, stop })
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }

    /// Stops accepting; requests in flight finish and idle connections close.
    pub(crate) fn drain(self) {
        self.stop.send_replace(Stop::Drain);
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        self.stop.send_if_modified(|stop| {
            let open = *stop == Stop::Open;
            if open {
                *stop = Stop::Close;
            }
            open
        });
    }
}

async fn accept(listener: TcpListener, stop: Arc<watch::Sender<Stop>>, handler: Handler) {
    let mut stopped = stop.subscribe();
    loop {
        let accepted = tokio::select! {
            _ = stopped.wait_for(|stop| *stop != Stop::Open) => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(serve(stream, Arc::clone(&stop), Arc::clone(&handler)));
            }
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

async fn serve(stream: TcpStream, stop: Arc<watch::Sender<Stop>>, handler: Handler) {
    let mut stopped = stop.subscribe();
    let service = service_fn(move |request: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        async move {
            let reply = handler(request.method().clone(), request.uri().to_string()).await;
            let mut response = Response::new(Full::new(Bytes::from_static(reply.body.as_bytes())));
            *response.status_mut() =
                StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_REQUEST);
            for (name, value) in reply.headers {
                response
                    .headers_mut()
                    .insert(*name, hyper::header::HeaderValue::from_static(value));
            }
            Ok::<_, Infallible>(response)
        }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    // A stop sent between the accept and this subscription is already seen.
    let mut current = *stopped.borrow_and_update();
    loop {
        match current {
            Stop::Close => return,
            Stop::Drain => connection.as_mut().graceful_shutdown(),
            Stop::Open => {}
        }
        tokio::select! {
            // A failed connection is the browser's; nothing waits for its outcome.
            _ = connection.as_mut() => return,
            changed = stopped.changed() => {
                current = if changed.is_ok() { *stopped.borrow_and_update() } else { Stop::Close };
            }
        }
    }
}
