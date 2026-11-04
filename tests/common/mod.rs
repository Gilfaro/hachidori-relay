// SPDX-License-Identifier: GPL-3.0-or-later
//! A minimal raw WebSocket client for the integration tests. It speaks only what
//! the relay needs, and it can stop reading on purpose to play a stalled peer.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, unreachable_pub, missing_docs)]

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub const EXTENSION_ORIGIN: &str = "chrome-extension://hachidorirelaytestextensionid";
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Ws {
    pub status: u16,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

/// Connects and sends an upgrade request whose head carries `padding` extra header bytes.
pub async fn connect_padded(port: u16, path: &str, padding: usize) -> Ws {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    let (read, mut writer) = stream.into_split();
    let pad = if padding == 0 { String::new() } else { format!("X-Pad: {}\r\n", "p".repeat(padding)) };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: {EXTENSION_ORIGIN}\r\n{pad}\r\n"
    );
    writer.write_all(request.as_bytes()).await.expect("send request");
    let mut reader = BufReader::with_capacity(1 << 16, read);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(TIMEOUT, reader.read_until(b'\n', &mut head)).await.expect("head in time").expect("read head");
        assert!(read > 0, "the relay closed before answering");
    }
    let status = String::from_utf8_lossy(&head).split(' ').nth(1).and_then(|code| code.parse().ok()).expect("status");
    Ws { status, reader, writer }
}

pub async fn connect(port: u16, path: &str) -> Ws {
    connect_padded(port, path, 0).await
}

impl Ws {
    /// Sends one masked frame. The mask is all zeros, which leaves the payload as it is
    /// and is legal, so large frames cost no masking time in the test.
    pub async fn frame(&mut self, opcode: u8, fin: bool, payload: &[u8]) {
        let mut header = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            short @ 0..=125 => header.push(0x80 | u8::try_from(short).unwrap()),
            medium @ 126..=0xFFFF => {
                header.push(0x80 | 0x7E);
                header.extend_from_slice(&u16::try_from(medium).unwrap().to_be_bytes());
            }
            long => {
                header.push(0x80 | 0x7F);
                header.extend_from_slice(&(long as u64).to_be_bytes());
            }
        }
        header.extend_from_slice(&[0, 0, 0, 0]);
        self.writer.write_all(&header).await.expect("send header");
        self.writer.write_all(payload).await.expect("send payload");
    }

    pub async fn text(&mut self, text: &str) {
        self.frame(0x1, true, text.as_bytes()).await;
    }

    pub async fn json(&mut self, value: &Value) {
        self.text(&value.to_string()).await;
    }

    /// The next frame as (opcode, payload), or `None` once the relay has closed the connection.
    pub async fn next(&mut self) -> Option<(u8, Vec<u8>)> {
        tokio::time::timeout(TIMEOUT, self.read_frame()).await.expect("a frame or a close in time")
    }

    async fn read_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut head = [0_u8; 2];
        self.reader.read_exact(&mut head).await.ok()?;
        let length = match head[1] & 0x7F {
            126 => u64::from(self.reader.read_u16().await.ok()?),
            127 => self.reader.read_u64().await.ok()?,
            short => u64::from(short),
        };
        let mut payload = vec![0; usize::try_from(length).unwrap()];
        self.reader.read_exact(&mut payload).await.ok()?;
        Some((head[0] & 0x0F, payload))
    }

    /// The next text frame whose JSON `kind` is `kind`, skipping pings and other kinds.
    pub async fn until_kind(&mut self, kind: &str) -> Value {
        loop {
            let (opcode, payload) = self.next().await.unwrap_or_else(|| panic!("closed while waiting for {kind}"));
            if opcode != 0x1 {
                continue;
            }
            let value: Value = serde_json::from_slice(&payload).expect("JSON frame");
            if value["kind"] == kind {
                return value;
            }
        }
    }

    /// Waits until the relay ends the connection (a close frame or EOF), ignoring everything else.
    pub async fn until_closed(&mut self) {
        while let Some((opcode, _)) = self.next().await {
            if opcode == 0x8 {
                return;
            }
        }
    }
}
