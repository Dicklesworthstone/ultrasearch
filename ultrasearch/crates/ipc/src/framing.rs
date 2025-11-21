//! Simple length-prefixed framing helpers for pipe/stream transport.
use anyhow::{bail, Result};

pub const MAX_FRAME: usize = 256 * 1024;

/// Encode a payload with a little-endian u32 length prefix.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_FRAME {
        bail!("frame too large: {} bytes", payload.len());
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Decode a length-prefixed frame from the provided buffer.
/// Returns (payload, remaining).
pub fn decode_frame(buf: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    if buf.len() < 4 {
        bail!("incomplete frame header");
    }
    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {} bytes", len);
    }
    if buf.len() < 4 + len {
        bail!("incomplete frame body");
    }
    let payload = buf[4..4 + len].to_vec();
    Ok((payload, &buf[4 + len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ok() {
        let payload = b"hello";
        let framed = encode_frame(payload).unwrap();
        let (out, rem) = decode_frame(&framed).unwrap();
        assert_eq!(out, payload);
        assert!(rem.is_empty());
    }

    #[test]
    fn guards_frame_size() {
        let big = vec![0u8; MAX_FRAME + 1];
        assert!(encode_frame(&big).is_err());
    }

    #[test]
    fn detects_incomplete() {
        let res = decode_frame(&[0, 0, 0]);
        assert!(res.is_err());
    }
}
