//! One HTTP/1 exchange over a stream both clients share: the handshake, the request, and a
//! body read frame by frame while the connection is driven beside it.

use std::future::Future;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1::{self, Connection};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::NetError;

pub(crate) struct Wire<I: AsyncRead + AsyncWrite + Unpin + Send + 'static> {
    connection: Connection<TokioIo<I>, Full<Bytes>>,
    closed: bool,
}

impl<I: AsyncRead + AsyncWrite + Unpin + Send + 'static> Wire<I> {
    /// Sends `request` on a new connection over `io` and answers the response head.
    pub(crate) async fn send(
        io: I,
        request: Request<Full<Bytes>>,
    ) -> Result<(Self, Response<Incoming>), NetError> {
        let (mut sender, connection) = http1::handshake(TokioIo::new(io)).await?;
        let mut wire = Self {
            connection,
            closed: false,
        };
        let response = wire
            .drive(async { sender.send_request(request).await.map_err(NetError::from) })
            .await?;
        Ok((wire, response))
    }

    /// Runs `work` while the connection reads and writes for it.
    pub(crate) async fn drive<T>(
        &mut self,
        work: impl Future<Output = Result<T, NetError>>,
    ) -> Result<T, NetError> {
        let mut work = std::pin::pin!(work);
        loop {
            tokio::select! {
                biased;
                result = &mut work => return result,
                // A completed connection future must not be polled again; the body it fed
                // is already buffered for `work`.
                closed = &mut self.connection, if !self.closed => {
                    closed?;
                    self.closed = true;
                }
            }
        }
    }

    /// Hands each data frame of `body` to `sink` until the body ends.
    pub(crate) async fn read(
        &mut self,
        body: &mut Incoming,
        mut sink: impl AsyncFnMut(Bytes) -> Result<(), NetError>,
    ) -> Result<(), NetError> {
        while let Some(frame) = self.drive(async { Ok(body.frame().await) }).await? {
            if let Ok(data) = frame?.into_data() {
                sink(data).await?;
            }
        }
        Ok(())
    }
}

/// Collects a body of at most `limit` bytes; a declared length above it fails before reading.
pub(crate) async fn collect<I: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    wire: &mut Wire<I>,
    response: Response<Incoming>,
    limit: u64,
) -> Result<crate::Response, NetError> {
    let (head, mut body) = response.into_parts();
    check_declared(&head.headers, limit)?;
    let mut bytes = Vec::new();
    wire.read(&mut body, async |data: Bytes| {
        if too_large(bytes.len(), data.len(), limit) {
            return Err(NetError::TooLarge { limit });
        }
        bytes.extend_from_slice(&data);
        Ok(())
    })
    .await?;
    Ok(crate::Response {
        status: head.status,
        headers: head.headers,
        body: bytes,
    })
}

pub(crate) fn check_declared(headers: &hyper::HeaderMap, limit: u64) -> Result<(), NetError> {
    let declared = headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    match declared {
        Some(length) if length > limit => Err(NetError::TooLarge { limit }),
        _ => Ok(()),
    }
}

pub(crate) fn too_large(have: usize, more: usize, limit: u64) -> bool {
    u64::try_from(have.saturating_add(more)).map_or(true, |total| total > limit)
}
