//! Length-prefixed frames between the broker and a session host, and the parsers for what is inside them.
//!
//! A frame is a 4-byte little-endian length and that many bytes. The reader tells three cases apart:
//! - the peer closed between frames (`Closed`): normal, stop quietly;
//! - the stream ended in the middle of a frame (`Truncated`): the peer died or the pipe broke, the frame is
//!   unusable and the connection must be dropped (the stream cannot be re-aligned);
//! - the length is above the cap (`TooLarge`): the header is corrupt or hostile. Nothing is allocated for it,
//!   and again the connection is dropped.
//!
//! A slow peer is not an error: the reader keeps waiting for the rest of a frame (short reads are normal on a
//! pipe). Every parser takes a slice and answers `None` for a body that is too short or the wrong size, so no
//! malformed request can index out of bounds.
use std::io::{Read, Write};

/// How much output one host read returns.
pub const READ_CHUNK: usize = 256 << 10;
/// The largest frame a host accepts from the broker: the biggest input (64 KiB) plus the op byte, with room.
pub const MAX_REQUEST_FRAME: usize = 128 << 10;
/// The largest frame the broker accepts from a host: one read chunk plus its 16-byte header, with room.
pub const MAX_REPLY_FRAME: usize = READ_CHUNK + 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The stream ended cleanly before a frame started.
    Closed,
    /// The stream ended inside a frame (header or body).
    Truncated,
    /// The announced length is above the cap; nothing was allocated.
    TooLarge { len: usize, max: usize },
}

/// Read until `buf` is full or the stream ends or fails; returns how many bytes arrived.
fn fill<R: Read>(r: &mut R, buf: &mut [u8]) -> usize {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    got
}

pub fn read_frame<R: Read>(r: &mut R, max: usize) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];
    match fill(r, &mut header) {
        0 => return Err(FrameError::Closed),
        4 => {}
        _ => return Err(FrameError::Truncated),
    }
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut body = vec![0u8; len];
    if fill(r, &mut body) == len {
        Ok(body)
    } else {
        Err(FrameError::Truncated)
    }
}

/// Write one frame. A payload above `max` is refused (false) instead of being cut or length-wrapped.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8], max: usize) -> bool {
    let Ok(len) = u32::try_from(payload.len()) else { return false };
    payload.len() <= max && w.write_all(&len.to_le_bytes()).is_ok() && w.write_all(payload).is_ok()
}

fn le_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(<[u8; 8]>::try_from(bytes.get(..8)?).ok()?))
}

/// HELLO: an 8-byte epoch then the session nonce (any length, compared by the caller).
pub fn parse_hello(body: &[u8]) -> Option<(u64, &[u8])> {
    Some((le_u64(body)?, body.get(8..)?))
}
/// READ: exactly an 8-byte stream offset.
pub fn parse_read_request(body: &[u8]) -> Option<u64> {
    if body.len() == 8 {
        le_u64(body)
    } else {
        None
    }
}
/// RESIZE: exactly columns then rows as little-endian i16.
pub fn parse_resize(body: &[u8]) -> Option<(i16, i16)> {
    if body.len() != 4 {
        return None;
    }
    Some((i16::from_le_bytes([*body.first()?, *body.get(1)?]), i16::from_le_bytes([*body.get(2)?, *body.get(3)?])))
}
/// A READ reply: next offset, bytes skipped, then the output.
pub fn parse_read_reply(reply: &[u8]) -> Option<(u64, u64, &[u8])> {
    Some((le_u64(reply)?, le_u64(reply.get(8..)?)?, reply.get(16..)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A reader that hands out at most `step` bytes per call, like a slow pipe.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn a_frame_arriving_one_byte_at_a_time_is_waited_for_not_rejected() {
        let wire = framed(b"hello host");
        assert_eq!(read_frame(&mut Trickle { data: &wire, step: 1 }, 1024), Ok(b"hello host".to_vec()));
        assert_eq!(read_frame(&mut Trickle { data: &wire, step: 3 }, 1024), Ok(b"hello host".to_vec()));
        // Several frames in a row, then a clean close.
        let two = [framed(b"a"), framed(b""), framed(b"bc")].concat();
        let mut r = Cursor::new(two);
        assert_eq!(read_frame(&mut r, 16), Ok(b"a".to_vec()));
        assert_eq!(read_frame(&mut r, 16), Ok(Vec::new()), "an empty frame is a frame");
        assert_eq!(read_frame(&mut r, 16), Ok(b"bc".to_vec()));
        assert_eq!(read_frame(&mut r, 16), Err(FrameError::Closed));
    }

    #[test]
    fn a_stream_that_ends_inside_a_frame_is_truncated_not_closed() {
        let wire = framed(b"0123456789");
        for cut in 1..wire.len() {
            assert_eq!(read_frame(&mut Cursor::new(&wire[..cut]), 1024), Err(FrameError::Truncated), "cut at {cut}");
        }
        assert_eq!(read_frame(&mut Cursor::new(Vec::<u8>::new()), 1024), Err(FrameError::Closed));
    }

    #[test]
    fn a_corrupt_length_is_refused_before_anything_is_allocated() {
        for len in [1025u32, 1 << 20, u32::MAX, 0x8000_0000] {
            let wire = len.to_le_bytes().to_vec(); // header only: no body follows
            assert_eq!(read_frame(&mut Cursor::new(wire), 1024), Err(FrameError::TooLarge { len: usize::try_from(len).unwrap(), max: 1024 }), "{len}");
        }
        assert_eq!(read_frame(&mut Cursor::new(framed(&[7; 1024])), 1024).map(|b| b.len()), Ok(1024), "exactly the cap is fine");
    }

    #[test]
    fn writing_refuses_oversized_payloads_and_round_trips_the_rest() {
        let mut out = Vec::new();
        assert!(write_frame(&mut out, b"ok", 8));
        assert!(!write_frame(&mut out, &[0u8; 9], 8));
        assert_eq!(out, framed(b"ok"), "the refused frame wrote nothing");
        assert_eq!(read_frame(&mut Cursor::new(out), 8), Ok(b"ok".to_vec()));
    }

    #[test]
    fn parsers_accept_exactly_the_right_shapes_and_never_panic_on_others() {
        let epoch = 42u64.to_le_bytes();
        let hello = [epoch.as_slice(), b"nonce"].concat();
        assert_eq!(parse_hello(&hello), Some((42, &b"nonce"[..])));
        assert_eq!(parse_hello(&epoch), Some((42, &b""[..])), "an empty nonce parses (the caller will not match it)");
        for short in 0..8 {
            assert_eq!(parse_hello(&epoch[..short]), None, "{short}");
        }
        assert_eq!(parse_read_request(&epoch), Some(42));
        for len in [0usize, 1, 7, 9, 16] {
            assert_eq!(parse_read_request(&vec![1u8; len]), None, "{len}");
        }
        assert_eq!(parse_resize(&[120, 0, 30, 0]), Some((120, 30)));
        assert_eq!(parse_resize(&[0xff, 0xff, 0xff, 0x7f]), Some((-1, 32767)));
        for len in [0usize, 1, 3, 5] {
            assert_eq!(parse_resize(&vec![1u8; len]), None, "{len}");
        }
        let reply = [7u64.to_le_bytes().as_slice(), 3u64.to_le_bytes().as_slice(), b"out"].concat();
        assert_eq!(parse_read_reply(&reply), Some((7, 3, &b"out"[..])));
        for len in 0..16 {
            assert_eq!(parse_read_reply(&reply[..len]), None, "{len}");
        }
        assert_eq!(parse_read_reply(&reply[..16]), Some((7, 3, &b""[..])));
    }

    #[test]
    fn arbitrary_bytes_never_panic_any_parser() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..5000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = usize::try_from(seed >> 59).unwrap_or(0); // 0..32
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = (parse_hello(&bytes), parse_read_request(&bytes), parse_resize(&bytes), parse_read_reply(&bytes));
            let _ = read_frame(&mut Cursor::new(&bytes), 64);
        }
    }
}
