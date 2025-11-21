use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::Result;
use core_types::DocKey;
use fst::automaton::{Automaton, Str};
use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use memmap2::Mmap;

/// A memory-mapped FST index for fast prefix lookups.
///
/// Keys are encoded as `normalized_name + \0 + doc_key_be_bytes` to handle duplicates.
/// The value associated with the FST key is unused (always 0) because the DocKey
/// is embedded in the key itself to allow multiple files with the same name.
pub struct FstIndex {
    map: Map<Mmap>,
}

impl FstIndex {
    /// Open an FST index from a path.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: We assume the file is immutable and safe to map.
        let mmap = unsafe { Mmap::map(&file)? };
        let map = Map::new(mmap)?;
        Ok(Self { map })
    }

    /// Search for keys starting with the given prefix.
    ///
    /// `prefix` should be normalized (lowercased) if the index was built with normalized names.
    pub fn search<'a>(&'a self, prefix: &str) -> impl Iterator<Item = DocKey> + 'a {
        // Create an automaton that matches any key starting with `prefix`.
        // We convert to String to own the bytes, ensuring 'prefix lifetime doesn't leak into the iterator.
        let matcher = Str::new(prefix.to_string()).starts_with();
        let stream = self.map.search(matcher).into_stream();

        StreamIter {
            stream,
            _marker: std::marker::PhantomData,
        }
        .filter_map(move |(k, _)| {
            // Key format: name_bytes + \0 + 8 bytes DocKey (BE).
            // Minimal length is 1 + 8 = 9.
            if k.len() < 9 {
                return None;
            }

            let (rest, dk_bytes) = k.split_at(k.len() - 8);
            // The byte before DocKey must be \0
            if rest.last() != Some(&0) {
                return None;
            }

            // Parse u64
            let val = u64::from_be_bytes(dk_bytes.try_into().ok()?);
            Some(DocKey(val))
        })
    }
}

/// Builder for FST index.
pub struct FstBuilder {
    writer: MapBuilder<BufWriter<File>>,
}

impl FstBuilder {
    /// Create a new builder writing to the specified path.
    pub fn new(path: &Path) -> Result<Self> {
        let file = File::create(path)?;
        let writer = MapBuilder::new(BufWriter::new(file))?;
        Ok(Self { writer })
    }

    /// Insert a batch of entries.
    ///
    /// `entries` is a list of `(normalized_name, doc_key)`.
    /// This function sorts them internally to satisfy FST insertion requirements.
    pub fn insert_batch(&mut self, entries: Vec<(String, DocKey)>) -> Result<()> {
        // Transform to encoded keys: name + \0 + doc_key(BE)
        let mut keys: Vec<Vec<u8>> = entries
            .into_iter()
            .map(|(name, dk)| {
                let mut k = name.into_bytes();
                k.push(0);
                k.extend_from_slice(&dk.0.to_be_bytes());
                k
            })
            .collect();

        keys.sort();
        keys.dedup(); // Dedup exact matches just in case

        for k in keys {
            self.writer.insert(&k, 0)?;
        }
        Ok(())
    }

    /// Finish writing the index.
    pub fn finish(self) -> Result<()> {
        self.writer.finish()?;
        Ok(())
    }
}

struct StreamIter<'a, S> {
    stream: S,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a, S> Iterator for StreamIter<'a, S>
where
    S: Streamer<'a, Item = (&'a [u8], u64)>,
{
    type Item = (Vec<u8>, u64);

    fn next(&mut self) -> Option<Self::Item> {
        self.stream.next().map(move |(k, v)| (k.to_vec(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_fst_round_trip() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("test.fst");

        let mut builder = FstBuilder::new(&path)?;
        let entries = vec![
            ("foo".to_string(), DocKey(1)),
            ("foobar".to_string(), DocKey(2)),
            ("foo".to_string(), DocKey(3)), // Duplicate name
            ("baz".to_string(), DocKey(4)),
        ];
        builder.insert_batch(entries)?;
        builder.finish()?;

        let index = FstIndex::open(&path)?;

        // Exact match "foo" -> should return 1 and 3
        let mut hits: Vec<u64> = index.search("foo").map(|k| k.0).collect();
        hits.sort();
        // search("foo") is prefix search. It matches "foo\0..." (1, 3) and "foobar\0..." (2).
        // Wait, "foobar" encoded is "foobar\0..."
        // "foo" prefix matches "foobar" string.
        // So hits should include 2?
        // "foo" bytes match prefix of "foobar".
        // Yes.
        assert_eq!(hits, vec![1, 2, 3]);

        // Prefix "foob" -> 2
        let hits: Vec<u64> = index.search("foob").map(|k| k.0).collect();
        assert_eq!(hits, vec![2]);

        // Prefix "ba" -> 4
        let hits: Vec<u64> = index.search("ba").map(|k| k.0).collect();
        assert_eq!(hits, vec![4]);

        // No match
        let hits: Vec<u64> = index.search("z").map(|k| k.0).collect();
        assert!(hits.is_empty());

        Ok(())
    }
}