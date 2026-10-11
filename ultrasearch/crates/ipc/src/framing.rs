//! Length framing and a versioned message envelope for pipe/stream transport.
use anyhow::{Result, bail, ensure};

pub const MAX_FRAME: usize = 256 * 1024;

/// Version 1 sent unversioned bincode with a lossy, packed 64-bit DocKey.
/// Version 2 requires lossless volume + full FRN identities at both peers.
pub const PROTOCOL_VERSION: u16 = 2;

// The leading zero u64 is deliberately an invalid UUID byte-string length for
// legacy bincode requests, so an old service cannot mistake this envelope for
// a request or allocate based on a large ASCII-derived length.
const MESSAGE_MAGIC: &[u8; 12] = b"\0\0\0\0\0\0\0\0USIP";
const MESSAGE_HEADER_LEN: usize = MESSAGE_MAGIC.len() + 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageKind {
    Ping = 0,
    Status = 1,
    Search = 2,
    ReloadConfig = 3,
    Rescan = 4,
}

impl TryFrom<u8> for MessageKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Ping,
            1 => Self::Status,
            2 => Self::Search,
            3 => Self::ReloadConfig,
            4 => Self::Rescan,
            _ => bail!("unknown UltraSearch IPC message kind {value}"),
        })
    }
}

/// Wrap bincode without changing its individual message models. The request
/// kind is echoed on responses; identically shaped control requests remain
/// distinct, and neither peer parses incompatible identity layouts.
pub fn encode_message(kind: MessageKind, payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        payload.len() <= MAX_FRAME - MESSAGE_HEADER_LEN,
        "IPC payload exceeds the versioned frame limit"
    );
    let mut message = Vec::with_capacity(MESSAGE_HEADER_LEN + payload.len());
    message.extend_from_slice(MESSAGE_MAGIC);
    message.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    message.push(kind as u8);
    message.extend_from_slice(payload);
    Ok(message)
}

/// Serialize a search page after all response labels have been filled in.
/// Keep the longest ranked prefix that fits the actual versioned wire frame;
/// the complete match count and existing truncation state remain authoritative.
pub fn serialize_search_response(response: &mut crate::SearchResponse) -> Result<Vec<u8>> {
    let mut size = bincode::serialized_size(response)?;
    let limit = (MAX_FRAME - MESSAGE_HEADER_LEN) as u64;
    let had_hits = !response.hits.is_empty();
    while size > limit {
        let Some(hit) = response.hits.last() else {
            bail!("search response labels exceed the IPC frame limit");
        };
        let hit_size = bincode::serialized_size(hit)?;
        ensure!(size >= hit_size, "invalid serialized search page size");
        // bincode's top-level functions use fixed-width integer encoding, so
        // the vector length prefix has the same size after removing one hit.
        size -= hit_size;
        response.hits.pop();
        response.truncated = true;
    }
    ensure!(
        !had_hits || !response.hits.is_empty(),
        "the first ranked search hit cannot fit in an IPC frame"
    );
    let payload = bincode::serialize(response)?;
    ensure!(
        payload.len() <= MAX_FRAME - MESSAGE_HEADER_LEN,
        "serialized search response exceeds the IPC frame limit"
    );
    Ok(payload)
}

/// Validate compatibility before handing any bytes to a bincode deserializer.
pub fn decode_message(message: &[u8]) -> Result<(MessageKind, &[u8])> {
    ensure!(
        message.len() <= MAX_FRAME,
        "IPC message exceeds frame limit"
    );
    ensure!(
        message.len() >= MESSAGE_HEADER_LEN && message.starts_with(MESSAGE_MAGIC),
        "incompatible UltraSearch IPC protocol: expected version {PROTOCOL_VERSION}; update UI and service together"
    );
    let version = u16::from_le_bytes([
        message[MESSAGE_MAGIC.len()],
        message[MESSAGE_MAGIC.len() + 1],
    ]);
    ensure!(
        version == PROTOCOL_VERSION,
        "incompatible UltraSearch IPC protocol version {version}; expected {PROTOCOL_VERSION}; update UI and service together"
    );
    let kind = MessageKind::try_from(message[MESSAGE_HEADER_LEN - 1])?;
    Ok((kind, &message[MESSAGE_HEADER_LEN..]))
}

/// Encode a payload with a little-endian u32 length prefix.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_FRAME {
        bail!("frame too large: {} bytes", payload.len());
    }
    if payload.len() > u32::MAX as usize {
        bail!("frame exceeds u32 length: {} bytes", payload.len());
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
    fn rejects_maximum_u32_length_without_allocating_its_body() {
        assert!(decode_frame(&u32::MAX.to_le_bytes()).is_err());
    }

    #[test]
    fn zero_length_payload_roundtrips() {
        let payload: &[u8] = &[];
        let framed = encode_frame(payload).unwrap();
        let (out, rem) = decode_frame(&framed).unwrap();
        assert!(out.is_empty());
        assert!(rem.is_empty());
    }

    #[test]
    fn max_frame_boundary_is_allowed() {
        let payload = vec![0u8; MAX_FRAME];
        let framed = encode_frame(&payload).unwrap();
        let (out, rem) = decode_frame(&framed).unwrap();
        assert_eq!(out.len(), MAX_FRAME);
        assert!(rem.is_empty());
    }

    #[test]
    fn decode_rejects_header_over_max_frame() {
        // crafted header claims a payload bigger than MAX_FRAME
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME as u32) + 1).to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn detects_incomplete() {
        let res = decode_frame(&[0, 0, 0]);
        assert!(res.is_err());
    }

    #[test]
    fn envelope_rejects_legacy_and_other_versions_before_deserialization() {
        let request = crate::StatusRequest {
            id: uuid::Uuid::new_v4(),
        };
        let legacy = bincode::serialize(&request).unwrap();
        assert!(
            decode_message(&legacy)
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        for kind in [
            MessageKind::Ping,
            MessageKind::Status,
            MessageKind::Search,
            MessageKind::ReloadConfig,
            MessageKind::Rescan,
        ] {
            let encoded = encode_message(kind, &legacy).unwrap();
            let (decoded_kind, payload) = decode_message(&encoded).unwrap();
            assert_eq!(decoded_kind, kind);
            assert_eq!(payload, legacy);
            // An old service sees an invalid UUID length, not a valid control
            // request or an unbounded allocation derived from the magic.
            assert!(bincode::deserialize::<crate::StatusRequest>(&encoded).is_err());
            for version in [1u16, PROTOCOL_VERSION + 1] {
                let mut incompatible = encoded.clone();
                incompatible[MESSAGE_MAGIC.len()..MESSAGE_MAGIC.len() + 2]
                    .copy_from_slice(&version.to_le_bytes());
                assert!(
                    decode_message(&incompatible)
                        .unwrap_err()
                        .to_string()
                        .contains("incompatible")
                );
            }
        }
    }

    #[test]
    fn envelope_bounds_and_message_kind_are_checked() {
        let payload = vec![0; MAX_FRAME - MESSAGE_HEADER_LEN];
        let mut encoded = encode_message(MessageKind::Search, &payload).unwrap();
        assert_eq!(encoded.len(), MAX_FRAME);
        assert_eq!(decode_message(&encoded).unwrap().1, payload);
        assert!(encode_message(MessageKind::Search, &vec![0; payload.len() + 1]).is_err());
        encoded[MESSAGE_HEADER_LEN - 1] = u8::MAX;
        assert!(decode_message(&encoded).is_err());
        for length in 0..MESSAGE_HEADER_LEN {
            assert!(decode_message(&encoded[..length]).is_err());
        }
    }

    #[test]
    fn framed_search_response_preserves_full_frn_identity() {
        let key = core_types::DocKey::from_parts(42, 0xfedc_0000_0000_0042);
        let response = crate::SearchResponse {
            id: uuid::Uuid::new_v4(),
            hits: vec![crate::SearchHit {
                key,
                score: 1.0,
                name: Some("report.txt".into()),
                path: None,
                ext: None,
                size: Some(10),
                modified: Some(20),
                snippet: None,
            }],
            total: 1,
            truncated: false,
            took_ms: 1,
            served_by: Some("service".into()),
        };
        let payload = bincode::serialize(&response).unwrap();
        let frame = encode_frame(&encode_message(MessageKind::Search, &payload).unwrap()).unwrap();
        let (message, remainder) = decode_frame(&frame).unwrap();
        assert!(remainder.is_empty());
        let (kind, payload) = decode_message(&message).unwrap();
        assert_eq!(kind, MessageKind::Search);
        let decoded: crate::SearchResponse = bincode::deserialize(payload).unwrap();
        assert_eq!(decoded.id, response.id);
        assert_eq!(decoded.hits[0].key, key);
        assert_eq!(decoded.hits[0].size, Some(10));
    }

    fn search_page(hits: Vec<crate::SearchHit>, total: u64) -> crate::SearchResponse {
        crate::SearchResponse {
            id: uuid::Uuid::new_v4(),
            hits,
            total,
            truncated: false,
            took_ms: 42,
            served_by: Some("final-named-pipe-host-label".into()),
        }
    }

    fn search_hit(identity: u64, path: String) -> crate::SearchHit {
        crate::SearchHit {
            key: core_types::DocKey::from_parts(42, 0xfedc_0000_0000_0000 | identity),
            score: 1.0,
            name: Some(format!("report-{identity:02}.txt")),
            path: Some(path),
            ext: Some("txt".into()),
            size: Some(100),
            modified: Some(200),
            snippet: None,
        }
    }

    fn wire_roundtrip(response: &mut crate::SearchResponse) -> Result<crate::SearchResponse> {
        let payload = serialize_search_response(response)?;
        let frame = encode_frame(&encode_message(MessageKind::Search, &payload)?)?;
        assert!(frame.len() <= MAX_FRAME + 4);
        let (message, remainder) = decode_frame(&frame)?;
        assert!(remainder.is_empty());
        let (kind, payload) = decode_message(&message)?;
        assert_eq!(kind, MessageKind::Search);
        Ok(bincode::deserialize(payload)?)
    }

    #[test]
    fn long_path_search_pages_fit_the_wire_without_losing_order_or_total() -> Result<()> {
        // Each path fits the native UTF-16 path bound, with ordinary-sized
        // components, but one default-sized page exceeds the pipe frame.
        let prefix = format!(r"C:\{}", format!("{}\\", "目".repeat(80)).repeat(120));
        let original: Vec<_> = (0..24)
            .map(|id| search_hit(id, format!("{prefix}report-{id:02}.txt")))
            .collect();
        assert!(
            original
                .iter()
                .all(|hit| hit.path.as_ref().unwrap().encode_utf16().count() < 32_767)
        );
        let full = search_page(original.clone(), original.len() as u64);
        assert!(encode_message(MessageKind::Search, &bincode::serialize(&full)?).is_err());

        let mut received = Vec::new();
        while received.len() < original.len() {
            let offset = received.len();
            let mut page = search_page(original[offset..].to_vec(), original.len() as u64);
            let decoded = wire_roundtrip(&mut page)?;
            assert_eq!(decoded.id, page.id);
            assert_eq!(decoded.total, original.len() as u64);
            assert_eq!(decoded.took_ms, 42);
            assert_eq!(decoded.served_by, page.served_by);
            assert!(!decoded.hits.is_empty());
            assert_eq!(
                decoded.truncated,
                decoded.hits.len() < original.len() - offset
            );
            for (got, expected) in decoded.hits.iter().zip(&original[offset..]) {
                assert_eq!(got.key, expected.key);
                assert_eq!(got.path, expected.path);
                assert_eq!(got.name, expected.name);
                received.push(got.key);
            }
        }
        assert_eq!(
            received,
            original.iter().map(|hit| hit.key).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn search_page_size_includes_final_labels_and_the_versioned_envelope() -> Result<()> {
        let mut exact = search_page(vec![search_hit(1, r"C:\boundary.txt".into())], 1);
        let available = MAX_FRAME - MESSAGE_HEADER_LEN - bincode::serialized_size(&exact)? as usize;
        exact.hits[0]
            .path
            .as_mut()
            .unwrap()
            .push_str(&"x".repeat(available));
        let payload = serialize_search_response(&mut exact)?;
        assert_eq!(payload.len() + MESSAGE_HEADER_LEN, MAX_FRAME);
        assert!(!wire_roundtrip(&mut exact)?.truncated);

        let mut one_byte_over = exact.clone();
        one_byte_over.hits[0].path.as_mut().unwrap().push('x');
        assert!(serialize_search_response(&mut one_byte_over).is_err());

        let mut prefix = search_page(
            vec![search_hit(0, r"C:\small.txt".into()), exact.hits[0].clone()],
            2,
        );
        let decoded = wire_roundtrip(&mut prefix)?;
        assert_eq!(decoded.hits.len(), 1);
        assert_eq!(
            decoded.hits[0].key,
            core_types::DocKey::from_parts(42, 0xfedc_0000_0000_0000)
        );
        assert_eq!(decoded.total, 2);
        assert!(decoded.truncated);
        Ok(())
    }

    #[test]
    fn unencodable_search_rows_fail_and_existing_empty_truncation_is_preserved() -> Result<()> {
        let mut bad_key = search_page(vec![search_hit(1, r"C:\identity.txt".into())], 1);
        bad_key.hits[0].key = core_types::DocKey(u128::MAX);
        assert!(serialize_search_response(&mut bad_key).is_err());

        let mut oversized_label = search_page(Vec::new(), 99);
        oversized_label.served_by = Some("h".repeat(MAX_FRAME));
        assert!(serialize_search_response(&mut oversized_label).is_err());
        // The server's unavailable fallback drops unencodable rows/labels and
        // remains a valid, explicitly unavailable protocol response.
        oversized_label.hits.clear();
        oversized_label.served_by = Some("service-unavailable".into());
        oversized_label.truncated = true;
        let decoded = wire_roundtrip(&mut oversized_label)?;
        assert!(decoded.hits.is_empty());
        assert!(decoded.truncated);
        assert_eq!(decoded.total, 99);
        assert_eq!(decoded.served_by.as_deref(), Some("service-unavailable"));

        let mut past_window = search_page(Vec::new(), 123);
        past_window.truncated = true;
        let decoded = wire_roundtrip(&mut past_window)?;
        assert!(decoded.hits.is_empty());
        assert!(decoded.truncated);
        assert_eq!(decoded.total, 123);
        Ok(())
    }
}
