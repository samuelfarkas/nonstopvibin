//! The response body type: a buffered payload, the core's body frame by frame, or chunks a
//! producer sends through a channel.

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Bytes, Frame, Incoming, SizeHint};
use tokio::sync::mpsc;

use crate::flight::InFlight;

/// A gateway response body. A proxied body holds the request's in-flight count, released when
/// hyper drops the body: at its end, or when the agent hangs up. Dropping the core's body also
/// makes hyper close the upstream connection. A channel body ends when its sender is dropped;
/// when the client hangs up, hyper drops the body, which closes the channel for its sender.
pub struct Body(Kind);

enum Kind {
    Full(Option<Bytes>),
    Upstream { body: Incoming, _flight: InFlight },
    Channel(mpsc::Receiver<Bytes>),
}

impl Body {
    #[must_use]
    pub fn full(bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        Self(Kind::Full((!bytes.is_empty()).then_some(bytes)))
    }

    #[must_use]
    pub const fn empty() -> Self {
        Self(Kind::Full(None))
    }

    /// The chunks `receiver` yields, in order, until its sender is dropped.
    #[must_use]
    pub const fn channel(receiver: mpsc::Receiver<Bytes>) -> Self {
        Self(Kind::Channel(receiver))
    }

    pub(crate) const fn upstream(body: Incoming, flight: InFlight) -> Self {
        Self(Kind::Upstream {
            body,
            _flight: flight,
        })
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        match &mut self.get_mut().0 {
            Kind::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Kind::Upstream { body, .. } => Pin::new(body).poll_frame(cx),
            Kind::Channel(receiver) => receiver
                .poll_recv(cx)
                .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.0 {
            Kind::Full(bytes) => bytes.is_none(),
            Kind::Upstream { body, .. } => body.is_end_stream(),
            Kind::Channel(receiver) => receiver.is_closed() && receiver.is_empty(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.0 {
            Kind::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            Kind::Upstream { body, .. } => body.size_hint(),
            Kind::Channel(_) => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;

    use hyper::body::Body as _;

    use super::*;

    async fn next(body: &mut Body) -> Option<Bytes> {
        poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx))
            .await
            .map(|frame| frame.unwrap().into_data().unwrap())
    }

    #[tokio::test]
    async fn a_channel_body_yields_each_chunk_and_ends_with_its_sender() {
        let (sender, receiver) = mpsc::channel(4);
        let mut body = Body::channel(receiver);
        sender.send(Bytes::from_static(b"one")).await.unwrap();
        sender.send(Bytes::from_static(b"two")).await.unwrap();
        assert_eq!(next(&mut body).await.as_deref(), Some(&b"one"[..]));
        assert!(!body.is_end_stream());
        drop(sender);
        assert_eq!(next(&mut body).await.as_deref(), Some(&b"two"[..]));
        assert_eq!(next(&mut body).await, None);
        assert!(body.is_end_stream());
    }
}
