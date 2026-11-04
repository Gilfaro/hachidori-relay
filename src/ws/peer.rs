// SPDX-License-Identifier: GPL-3.0-or-later
//! What the relay holds for each connection: a way to send it text, and a way to close it.
//!
//! Sending never waits. A frame goes onto the peer's own ordered queue, and that
//! peer's writer task drains the queue. Frames to one peer keep the order in which
//! the relay sent them, and a peer that stops reading holds up nobody but itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use tokio::sync::{mpsc, watch};

use crate::ws::frame;

/// One frame waiting for the writer task.
#[derive(Debug)]
pub(crate) struct Outgoing {
    pub(crate) opcode: u8,
    pub(crate) payload: Bytes,
}

/// A connection the relay can send to: a WebSocket, or the in-process API session.
#[derive(Clone, Debug)]
pub(crate) enum Peer {
    Socket(SocketPeer),
    Api(mpsc::UnboundedSender<ApiEvent>),
}

/// What the relay delivers to the API session.
#[derive(Debug)]
pub(crate) enum ApiEvent {
    Text(Bytes),
    Closed,
}

impl Peer {
    /// Queues one text message.
    pub(crate) fn send(&self, text: Bytes) {
        match self {
            Self::Socket(socket) => socket.send(frame::TEXT, text),
            Self::Api(session) => {
                let _ = session.send(ApiEvent::Text(text));
            }
        }
    }

    /// Asks the connection to close. Its reader then tells the relay that it closed.
    pub(crate) fn close(&self) {
        match self {
            Self::Socket(socket) => socket.close(),
            Self::Api(session) => {
                let _ = session.send(ApiEvent::Closed);
            }
        }
    }
}

/// The relay's handle on one WebSocket connection.
#[derive(Clone, Debug)]
pub(crate) struct SocketPeer(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    queue: mpsc::UnboundedSender<Outgoing>,
    queued: AtomicUsize,
    max_queued: usize,
    closed: watch::Sender<bool>,
}

impl SocketPeer {
    /// A new peer and the receiving end of its queue, for the writer task.
    pub(crate) fn new(max_queued: usize) -> (Self, mpsc::UnboundedReceiver<Outgoing>) {
        let (queue, receiver) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        (Self(Arc::new(Shared { queue, queued: AtomicUsize::new(0), max_queued, closed })), receiver)
    }

    /// Queues one frame. A peer that would go over its queue limit is closed instead.
    pub(crate) fn send(&self, opcode: u8, payload: Bytes) {
        let shared = &self.0;
        if self.is_closed() {
            return;
        }
        let length = payload.len();
        let queued = shared.queued.fetch_add(length, Ordering::AcqRel).saturating_add(length);
        if queued > shared.max_queued {
            shared.queued.fetch_sub(length, Ordering::AcqRel);
            tracing::warn!(queued, limit = shared.max_queued, "a peer stopped reading and went over its queue limit; dropping it");
            self.close();
            return;
        }
        if shared.queue.send(Outgoing { opcode, payload }).is_err() {
            shared.queued.fetch_sub(length, Ordering::AcqRel);
        }
    }

    /// Called by the writer task once a frame has been handed to the socket in full.
    pub(crate) fn written(&self, length: usize) {
        self.0.queued.fetch_sub(length, Ordering::AcqRel);
    }

    pub(crate) fn close(&self) {
        self.0.closed.send_replace(true);
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.0.closed.borrow()
    }

    /// Resolves (via `wait_for`) once the peer has been asked to close.
    pub(crate) fn closed(&self) -> watch::Receiver<bool> {
        self.0.closed.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_their_order_and_the_queue_limit_closes_the_peer() {
        let (peer, mut queue) = SocketPeer::new(10);
        peer.send(frame::TEXT, Bytes::from_static(b"12345"));
        peer.send(frame::TEXT, Bytes::from_static(b"678"));
        assert!(!peer.is_closed());
        peer.send(frame::TEXT, Bytes::from_static(b"9abc"));
        assert!(peer.is_closed(), "15 bytes would be over the 10-byte limit");
        let first = queue.try_recv().map(|item| item.payload);
        let second = queue.try_recv().map(|item| item.payload);
        assert_eq!(first.ok(), Some(Bytes::from_static(b"12345")));
        assert_eq!(second.ok(), Some(Bytes::from_static(b"678")));
        assert!(queue.try_recv().is_err(), "nothing is queued after the limit");
    }

    #[test]
    fn written_bytes_make_room_again() {
        let (peer, _queue) = SocketPeer::new(10);
        peer.send(frame::TEXT, Bytes::from_static(b"1234567890"));
        peer.written(10);
        peer.send(frame::TEXT, Bytes::from_static(b"1234567890"));
        assert!(!peer.is_closed());
    }
}
