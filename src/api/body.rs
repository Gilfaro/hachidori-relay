// SPDX-License-Identifier: GPL-3.0-or-later
//! The response body: a whole JSON answer, or a dictionary streamed chunk by chunk.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use tokio::sync::mpsc;

/// A response body.
#[derive(Debug)]
pub(crate) enum Body {
    /// A body that is already complete. `None` once it has been sent.
    Full(Option<Bytes>),
    /// Chunks as the host sends them. An `Err` ends the response early, and the
    /// client sees it cut short.
    Stream(mpsc::Receiver<io::Result<Bytes>>),
}

impl Body {
    pub(crate) fn full(bytes: impl Into<Bytes>) -> Self {
        Self::Full(Some(bytes.into()))
    }

    pub(crate) const fn empty() -> Self {
        Self::Full(None)
    }
}

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match self.get_mut() {
            Self::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Self::Stream(chunks) => chunks.poll_recv(context).map(|chunk| chunk.map(|chunk| chunk.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Self::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |bytes| bytes.len() as u64)),
            Self::Stream(_) => SizeHint::default(),
        }
    }
}
