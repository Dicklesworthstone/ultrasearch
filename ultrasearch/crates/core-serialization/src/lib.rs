//! Common serialization helpers shared across the workspace.

use anyhow::{Context, Result, anyhow};
use core_types::{DocKey, FileId, VolumeId};
use rkyv::{
    AlignedVec, Archive, CheckBytes, Deserialize as RDeserialize, Serialize as RSerialize,
    ser::{Serializer, serializers::AllocSerializer},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Minimal wire-safe representation of a document key.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Archive,
    RSerialize,
    RDeserialize,
)]
#[archive(check_bytes)]
pub struct DocKeyWire {
    pub volume: VolumeId,
    pub file: FileId,
}

impl From<DocKey> for DocKeyWire {
    fn from(value: DocKey) -> Self {
        let (volume, file) = value.into_parts();
        Self { volume, file }
    }
}

impl From<DocKeyWire> for DocKey {
    fn from(value: DocKeyWire) -> Self {
        DocKey::from_parts(value.volume, value.file)
    }
}

/// Serialize a value to bincode.
pub fn to_bincode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serialize(value).context("bincode serialize")
}

/// Deserialize a value from bincode.
pub fn from_bincode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    bincode::deserialize(bytes).context("bincode deserialize")
}

/// Deserialize with a defensive maximum payload length (in bytes).
///
/// This helps avoid allocating or parsing untrusted oversized buffers.
pub fn from_bincode_with_limit<T: DeserializeOwned>(bytes: &[u8], max_len: usize) -> Result<T> {
    if bytes.len() > max_len {
        anyhow::bail!("bincode payload too large: {} > {}", bytes.len(), max_len);
    }
    from_bincode(bytes)
}

/// Serialize a type that implements `rkyv::Serialize` into an aligned byte buffer.
pub fn to_rkyv_bytes<T>(value: &T) -> Result<AlignedVec>
where
    T: RSerialize<AllocSerializer<1024>>,
{
    let mut serializer = AllocSerializer::<1024>::default();
    serializer
        .serialize_value(value)
        .context("rkyv serialize")?;
    Ok(serializer.into_serializer().into_inner())
}

/// Validate and deserialize an rkyv buffer into an owned value.
pub fn from_rkyv_bytes<T>(bytes: &[u8]) -> Result<T>
where
    T: Archive,
    for<'a> T::Archived: CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
        + RDeserialize<T, rkyv::Infallible>,
{
    let archived =
        rkyv::check_archived_root::<T>(bytes).map_err(|e| anyhow!("rkyv check failed: {e:?}"))?;
    archived
        .deserialize(&mut rkyv::Infallible)
        .map_err(|_| anyhow!("rkyv deserialize failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_key_wire_round_trip() {
        let dk = DocKey::from_parts(5, 0x1234);
        let wire: DocKeyWire = dk.into();
        let back: DocKey = wire.into();
        assert_eq!(back, dk);
    }

    #[test]
    fn bincode_helpers_work() {
        let dk = DocKeyWire {
            volume: 2,
            file: 99,
        };
        let bytes = to_bincode(&dk).unwrap();
        let round: DocKeyWire = from_bincode(&bytes).unwrap();
        assert_eq!(round, dk);
    }

    #[test]
    fn bincode_with_limit_errors_on_oversize() {
        let dk = DocKeyWire {
            volume: 2,
            file: 99,
        };
        let bytes = to_bincode(&dk).unwrap();
        assert!(from_bincode_with_limit::<DocKeyWire>(&bytes, bytes.len() - 1).is_err());
        assert!(from_bincode_with_limit::<DocKeyWire>(&bytes, bytes.len()).is_ok());
    }

    #[derive(Archive, RSerialize, RDeserialize, Debug, PartialEq, Eq)]
    #[archive(check_bytes)]
    struct Small {
        key: DocKeyWire,
        flag: bool,
    }

    #[test]
    fn rkyv_helpers_work() {
        let s = Small {
            key: DocKeyWire { volume: 3, file: 7 },
            flag: true,
        };
        let bytes = to_rkyv_bytes(&s).unwrap();
        let round: Small = from_rkyv_bytes(&bytes).unwrap();
        assert_eq!(round, s);
    }

    #[test]
    fn rkyv_helpers_fail_with_invalid_input() {
        // Provide too-short bytes to trigger validation failure.
        let bytes = [0_u8, 1, 2, 3];
        let err = from_rkyv_bytes::<Small>(&bytes);
        assert!(err.is_err());
    }
}
