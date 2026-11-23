//! The response body type: a buffered payload, or the core's body frame by frame.

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Bytes, Frame, Incoming, SizeHint};

use crate::flight::InFlight;

/// A gateway response body. A proxied body holds the request's in-flight count, released when
/// hyper drops the body: at its end, or when the agent hangs up. Dropping the core's body also
/// makes hyper close the upstream connection.
pub struct Body(Kind);

enum Kind {
    Full(Option<Bytes>),
    Upstream { body: Incoming, _flight: InFlight },
}

impl Body {
    /// A body of these bytes.
    #[must_use]
    pub fn full(bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        Self(Kind::Full((!bytes.is_empty()).then_some(bytes)))
    }

    /// An empty body.
    #[must_use]
    pub const fn empty() -> Self {
        Self(Kind::Full(None))
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
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.0 {
            Kind::Full(bytes) => bytes.is_none(),
            Kind::Upstream { body, .. } => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.0 {
            Kind::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            Kind::Upstream { body, .. } => body.size_hint(),
        }
    }
}
