// SPDX-License-Identifier: GPL-3.0-or-later
//! One accepted relay socket, from the upgrade request to the close.
//!
//! The connection task reads frames and passes whole text messages to the relay.
//! A writer task beside it drains the peer's queue. That task is the only place
//! that waits for the socket to accept bytes, and it holds no lock while waiting.

use std::io::{self, IoSlice};
use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::relay::{Role, Shared};
use crate::ws::frame::{self, ReadError};
use crate::ws::handshake::{self, Endpoint};
use crate::ws::peer::{Outgoing, Peer, SocketPeer};

const READ_BUFFER: usize = 4 * 1024;

/// Serves one accepted socket until either side closes it.
pub(crate) async fn serve(shared: Arc<Shared>, stream: TcpStream, peer: SocketAddr) {
    let limits = shared.limits().clone();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::with_capacity(READ_BUFFER, read);
    let head = match tokio::time::timeout(limits.handshake_timeout, read_head(&mut reader, limits.max_handshake_bytes)).await {
        Ok(Ok(Some(head))) => head,
        Ok(Ok(None)) => {
            tracing::warn!(%peer, limit = limits.max_handshake_bytes, "an upgrade request was over the size limit");
            let _ = write.write_all(handshake::refusal("431 Request Header Fields Too Large").as_bytes()).await;
            return;
        }
        // The peer went away, sent something that is not HTTP, or never finished its request.
        Ok(Err(_)) | Err(_) => return,
    };
    let Some(request) = handshake::parse(&head) else { return };
    let address = peer.ip();
    let endpoint = match handshake::check(&request, address, || shared.has_host()) {
        Ok(endpoint) => endpoint,
        Err(status) => {
            let _ = write.write_all(handshake::refusal(status).as_bytes()).await;
            return;
        }
    };
    let key = request.key.as_deref().unwrap_or_default();
    if write.write_all(handshake::accept(key).as_bytes()).await.is_err() {
        return;
    }

    let (socket, queue) = SocketPeer::new(limits.max_queued_bytes);
    let writer = tokio::spawn(write_loop(write, queue, socket.clone()));
    let role = match endpoint {
        Endpoint::Host => shared.connect_host(Peer::Socket(socket.clone())),
        Endpoint::Link => shared.connect_client(Peer::Socket(socket.clone()), request.origin.as_deref().unwrap_or_default(), address),
    };
    // A second host was told why it was turned away. A client that lost the host
    // between the check and here is simply closed.
    if let Some(role) = role {
        read_loop(&shared, &role, &mut reader, &socket, limits.max_message_bytes).await;
        socket.close();
        shared.closed(&role).await;
    } else {
        socket.close();
    }
    let _ = writer.await;
}

/// The request head up to the blank line, or `None` if it grows past `max` bytes first.
/// Bytes that arrive after the blank line stay in `reader` for the frame reader.
async fn read_head<R: AsyncBufRead + Unpin>(reader: &mut R, max: usize) -> io::Result<Option<Vec<u8>>> {
    const END: &[u8] = b"\r\n\r\n";
    let mut head = Vec::with_capacity(1024);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let before = head.len();
        let taken = available.len();
        head.extend_from_slice(available);
        let from = before.saturating_sub(END.len() - 1);
        if let Some(found) = head[from..].windows(END.len()).position(|window| window == END) {
            let end = from + found;
            reader.consume(end + END.len() - before);
            head.truncate(end);
            return Ok((end <= max).then_some(head));
        }
        reader.consume(taken);
        // Up to three bytes of an unfinished end marker may sit at the tail.
        if head.len() > max.saturating_add(END.len() - 1) {
            return Ok(None);
        }
    }
}

/// Feeds whole text messages to the relay until the peer closes, breaks the protocol,
/// goes over the message limit, or is closed by the relay.
async fn read_loop(shared: &Shared, role: &Role, reader: &mut BufReader<OwnedReadHalf>, socket: &SocketPeer, max_message: usize) {
    let mut closed = socket.closed();
    let mut message: Vec<u8> = Vec::new();
    loop {
        let frame = tokio::select! {
            biased;
            () = crate::until_true(&mut closed) => return,
            frame = frame::read(reader, max_message.saturating_sub(message.len())) => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(ReadError::TooLarge(length)) => {
                tracing::warn!(
                    length,
                    already = message.len(),
                    limit = max_message,
                    "a peer sent a message over the size limit; closing it"
                );
                return;
            }
            Err(ReadError::Io(_)) => return,
        };
        match frame.opcode {
            frame::CONTINUATION | frame::TEXT => {
                if message.is_empty() {
                    message = frame.payload;
                } else {
                    message.extend_from_slice(&frame.payload);
                }
                if frame.fin {
                    // Text that is not UTF-8 ends the connection, as upstream does.
                    let Ok(text) = String::from_utf8(mem::take(&mut message)) else { return };
                    shared.text(role, text).await;
                }
            }
            frame::CLOSE => return,
            frame::PING => socket.send(frame::PONG, Bytes::from(frame.payload)),
            // Binary frames, pongs and unknown opcodes are ignored.
            _ => {}
        }
    }
}

/// Drains the peer's queue into the socket, in order.
///
/// A close asked for while a frame is half written aborts the connection, because a
/// close frame cannot interrupt a data frame. Otherwise the frames the socket takes
/// right away still go out, followed by a close frame.
async fn write_loop(write: OwnedWriteHalf, mut queue: mpsc::UnboundedReceiver<Outgoing>, socket: SocketPeer) {
    let mut closed = socket.closed();
    loop {
        let next = tokio::select! {
            biased;
            () = crate::until_true(&mut closed) => break,
            next = queue.recv() => next,
        };
        let Some(item) = next else { break };
        let length = item.payload.len();
        let mut pending = Pending::new(item);
        loop {
            match pending.try_write(&write) {
                Ok(true) => break,
                Ok(false) => tokio::select! {
                    biased;
                    () = crate::until_true(&mut closed) => return,
                    ready = write.writable() => if ready.is_err() {
                        socket.close();
                        return;
                    },
                },
                Err(_) => {
                    // The reader cleans up the relay side. Wake it.
                    socket.close();
                    return;
                }
            }
        }
        socket.written(length);
    }
    while let Ok(item) = queue.try_recv() {
        if !matches!(Pending::new(item).try_write(&write), Ok(true)) {
            return;
        }
    }
    let _ = Pending::new(Outgoing { opcode: frame::CLOSE, payload: Bytes::new() }).try_write(&write);
    // Dropping the write half sends FIN.
}

/// A frame being written, which may take several tries on a backed-up socket.
struct Pending {
    header: [u8; frame::MAX_HEADER],
    header_length: usize,
    payload: Bytes,
    sent: usize,
}

impl Pending {
    fn new(item: Outgoing) -> Self {
        let (header, header_length) = frame::header(item.opcode, item.payload.len());
        Self { header, header_length, payload: item.payload, sent: 0 }
    }

    /// Writes as much as the socket takes without waiting. Returns whether the whole frame is out.
    fn try_write(&mut self, write: &OwnedWriteHalf) -> io::Result<bool> {
        let total = self.header_length + self.payload.len();
        while self.sent < total {
            let result = if self.sent < self.header_length {
                write.try_write_vectored(&[IoSlice::new(&self.header[self.sent..self.header_length]), IoSlice::new(&self.payload)])
            } else {
                write.try_write(&self.payload[self.sent - self.header_length..])
            };
            match result {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => self.sent += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn head_of(bytes: &[u8], max: usize) -> (io::Result<Option<Vec<u8>>>, Vec<u8>) {
        // A tiny buffer makes the end marker straddle reads.
        let mut reader = BufReader::with_capacity(3, bytes);
        let head = read_head(&mut reader, max).await;
        let mut rest = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut rest).await;
        (head, rest)
    }

    #[tokio::test]
    async fn head_ends_at_the_blank_line_and_keeps_what_follows() {
        let (head, rest) = head_of(b"GET /host HTTP/1.1\r\nA: b\r\n\r\n\x81\x00", 1024).await;
        assert_eq!(head.ok().flatten().as_deref(), Some(&b"GET /host HTTP/1.1\r\nA: b"[..]));
        assert_eq!(rest, b"\x81\x00");
    }

    #[tokio::test]
    async fn head_over_the_limit_is_refused() {
        let (head, _) = head_of(b"GET /host HTTP/1.1\r\nA: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 16).await;
        assert!(matches!(head, Ok(None)));
        let (exact, _) = head_of(b"0123456789\r\n\r\n", 10).await;
        assert!(matches!(exact, Ok(Some(_))));
        let (over, _) = head_of(b"0123456789X\r\n\r\n", 10).await;
        assert!(matches!(over, Ok(None)));
    }

    #[tokio::test]
    async fn a_head_without_a_blank_line_is_an_eof() {
        let (head, _) = head_of(b"GET /host HTTP/1.1\r\n", 1024).await;
        assert!(head.is_err());
    }
}
