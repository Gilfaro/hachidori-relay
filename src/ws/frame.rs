// SPDX-License-Identifier: GPL-3.0-or-later
//! WebSocket frames (RFC 6455 §5): reading client frames and writing server frames.
//!
//! The relay accepts frames with or without a mask, as upstream does. It ignores
//! binary and unknown opcodes, and its only limit is the configured message size.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _};

pub(crate) const CONTINUATION: u8 = 0x0;
pub(crate) const TEXT: u8 = 0x1;
pub(crate) const CLOSE: u8 = 0x8;
pub(crate) const PING: u8 = 0x9;
pub(crate) const PONG: u8 = 0xA;

/// The first allocation for a frame's payload; it doubles from there as bytes arrive.
const INITIAL_PAYLOAD: usize = 64 * 1024;

/// The largest server frame header: 2 bytes plus a 64-bit length.
pub(crate) const MAX_HEADER: usize = 10;

/// One frame as received, already unmasked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) fin: bool,
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

/// Why no frame was read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes is over the message limit")]
    TooLarge(u64),
}

/// Reads one frame. Its payload may be at most `max_payload` bytes.
///
/// The payload buffer grows as bytes arrive rather than trusting the declared length,
/// so a peer that claims a huge frame and sends nothing costs no memory.
pub(crate) async fn read<R: AsyncRead + Unpin>(reader: &mut R, max_payload: usize) -> Result<Frame, ReadError> {
    let mut head = [0_u8; 2];
    reader.read_exact(&mut head).await?;
    let [first, second] = head;
    let length = match second & 0x7F {
        126 => u64::from(reader.read_u16().await?),
        127 => reader.read_u64().await?,
        short => u64::from(short),
    };
    let mask = if second & 0x80 == 0 {
        None
    } else {
        let mut mask = [0_u8; 4];
        reader.read_exact(&mut mask).await?;
        Some(mask)
    };
    let length_bytes = usize::try_from(length).map_err(|_| ReadError::TooLarge(length))?;
    if length_bytes > max_payload {
        return Err(ReadError::TooLarge(length));
    }
    // Grows by doubling as bytes arrive, but never past the declared length, so a
    // frame's buffer is exactly its size (read_to_end would round up to a power of two).
    let mut payload = Vec::with_capacity(length_bytes.min(INITIAL_PAYLOAD));
    while payload.len() < length_bytes {
        if payload.len() == payload.capacity() {
            payload.reserve_exact(payload.capacity().min(length_bytes - payload.len()));
        }
        let spare = payload.capacity().min(length_bytes) - payload.len();
        let read = (&mut *reader).take(spare as u64).read_buf(&mut payload).await?;
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
    }
    if let Some(mask) = mask {
        unmask(&mut payload, mask);
    }
    Ok(Frame { fin: first & 0x80 != 0, opcode: first & 0x0F, payload })
}

/// XORs the payload with the repeating 4-byte mask, eight bytes at a time.
pub(crate) fn unmask(payload: &mut [u8], mask: [u8; 4]) {
    let [a, b, c, d] = mask;
    let wide = u64::from_ne_bytes([a, b, c, d, a, b, c, d]);
    let (words, rest) = payload.as_chunks_mut::<8>();
    for word in words {
        *word = (u64::from_ne_bytes(*word) ^ wide).to_ne_bytes();
    }
    // The remainder starts at a multiple of 8, so the mask restarts at index 0.
    for (byte, key) in rest.iter_mut().zip(mask.iter().cycle()) {
        *byte ^= key;
    }
}

/// The header of a final, unmasked server frame. It returns the bytes and how many of
/// them are used.
pub(crate) fn header(opcode: u8, length: usize) -> ([u8; MAX_HEADER], usize) {
    let mut header = [0_u8; MAX_HEADER];
    header[0] = 0x80 | opcode;
    if let Ok(short) = u8::try_from(length)
        && short < 126
    {
        header[1] = short;
        (header, 2)
    } else if let Ok(medium) = u16::try_from(length) {
        header[1] = 126;
        header[2..4].copy_from_slice(&medium.to_be_bytes());
        (header, 4)
    } else {
        header[1] = 127;
        header[2..10].copy_from_slice(&(length as u64).to_be_bytes());
        (header, 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_frame(opcode: u8, fin: bool, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
        let (head, used) = header(opcode, payload.len());
        let mut bytes = head[..used].to_vec();
        if !fin {
            bytes[0] &= 0x7F;
        }
        let mut body = payload.to_vec();
        if let Some(mask) = mask {
            bytes[1] |= 0x80;
            bytes.extend_from_slice(&mask);
            unmask(&mut body, mask);
        }
        bytes.extend_from_slice(&body);
        bytes
    }

    async fn read_all(bytes: &[u8], max: usize) -> Result<Frame, ReadError> {
        let mut reader = bytes;
        read(&mut reader, max).await
    }

    #[test]
    fn header_uses_the_shortest_length_form() {
        assert_eq!(&header(TEXT, 125).0[..2], &[0x81, 125]);
        assert_eq!(header(TEXT, 126).1, 4);
        assert_eq!(&header(TEXT, 0xFFFF).0[..4], &[0x81, 126, 0xFF, 0xFF]);
        let (big, used) = header(PONG, 0x1_0000);
        assert_eq!(used, 10);
        assert_eq!(&big[..10], &[0x8A, 127, 0, 0, 0, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn unmask_matches_a_bytewise_xor_for_every_length() {
        let mask = [0x12, 0x34, 0x56, 0x78];
        for length in 0..40 {
            let original: Vec<u8> = (0..length).map(|value| u8::try_from(value * 7 % 256).unwrap_or(0)).collect();
            let mut fast = original.clone();
            unmask(&mut fast, mask);
            let slow: Vec<u8> = original.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]).collect();
            assert_eq!(fast, slow, "length {length}");
        }
    }

    #[tokio::test]
    async fn reads_masked_unmasked_and_all_length_forms() {
        for length in [0, 5, 125, 126, 300, 0xFFFF, 0x1_0005] {
            let payload: Vec<u8> = (0..length).map(|value| u8::try_from(value % 251).unwrap_or(0)).collect();
            for mask in [None, Some([1, 2, 3, 4])] {
                let frame = read_all(&client_frame(TEXT, true, &payload, mask), usize::MAX).await;
                let frame = frame.unwrap_or_else(|error| panic!("{error}"));
                assert_eq!(frame, Frame { fin: true, opcode: TEXT, payload: payload.clone() });
            }
        }
    }

    #[tokio::test]
    async fn keeps_the_fin_bit_and_opcode() {
        let frame = read_all(&client_frame(CONTINUATION, false, b"ab", Some([9, 9, 9, 9])), 10).await;
        assert!(matches!(frame, Ok(Frame { fin: false, opcode: CONTINUATION, .. })));
    }

    #[tokio::test]
    async fn refuses_frames_over_the_limit_before_reading_them() {
        let bytes = client_frame(TEXT, true, &[0; 200], None);
        assert!(matches!(read_all(&bytes, 199).await, Err(ReadError::TooLarge(200))));
        assert!(read_all(&bytes, 200).await.is_ok());
        // A header that promises 2^63 bytes allocates nothing.
        let lying = [0x81, 127, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(read_all(&lying, 1 << 20).await, Err(ReadError::TooLarge(_))));
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_eof() {
        let bytes = client_frame(TEXT, true, b"hello", None);
        assert!(matches!(read_all(&bytes[..4], 100).await, Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof));
    }
}
