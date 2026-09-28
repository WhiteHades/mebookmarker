//! simhash-64 over bookmark text, for near-duplicate detection.
//!
//! a bookmark's 64-bit fingerprint is the sign of each bit's column sum over
//! its tokens, so two texts that share content land at a small hamming
//! distance from each other. 4 bands of 16 bits turn that comparison into a
//! lookup: texts within distance 3 must agree on one whole band, so indexing
//! exact band values finds the candidates in one pass. cost is 8 bytes per
//! bookmark.

use blake3;
use mbm_core::error::{Error, Result};
use rusqlite::Connection;

/// how many bands the fingerprint is split into.
pub const BANDS: u8 = 4;

/// bits per band. 64 / 4 = 16.
const BAND_BITS: u32 = 16;

/// compute the fingerprint of a token stream.
#[must_use]
pub fn fingerprint(tokens: impl IntoIterator<Item = impl AsRef<[u8]>>) -> u64 {
    let mut column_sums = [0i16; 64];

    for token in tokens {
        let mut hash = blake3::Hasher::new();
        hash.update(token.as_ref());
        let digest = hash.finalize();
        let bytes = digest.as_bytes();
        for (bit, sum) in column_sums.iter_mut().enumerate() {
            let set = i16::from((bytes[bit / 8] >> (bit % 8)) & 1 == 1);
            *sum += set * 2 - 1;
        }
    }

    let mut fingerprint = 0u64;
    for (bit, &sum) in column_sums.iter().enumerate() {
        if sum > 0 {
            fingerprint |= 1u64 << bit;
        }
    }
    fingerprint
}

/// split a fingerprint into its band keys, one per band.
#[must_use]
pub fn bands(fingerprint: u64) -> [u32; BANDS as usize] {
    let mut out = [0u32; BANDS as usize];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = ((fingerprint >> (i as u32 * BAND_BITS)) & 0xFFFF) as u32;
    }
    out
}

/// number of differing bits between two fingerprints.
#[must_use]
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// whether two fingerprints are near-duplicates at the given distance.
///
/// 3 is the usual pick: over 64 bits it catches roughly 93% of true
/// near-duplicate pairs at a 1e-4 false-positive rate, which is what makes a
/// dedup pass cheap enough to run on every import.
#[must_use]
pub fn is_near_duplicate(a: u64, b: u64, max_distance: u32) -> bool {
    hamming(a, b) <= max_distance
}

#[derive(Debug, Default)]
pub struct BandIndex {
    buckets: ahash::AHashMap<(u8, u32), u64>,
    by_id: ahash::AHashMap<u64, u64>,
}

impl BandIndex {
    /// an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// how many bookmarks are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// the fingerprint stored for a bookmark id.
    #[must_use]
    pub fn fingerprint(&self, id: u64) -> Option<u64> {
        self.by_id.get(&id).copied()
    }

    /// add a fingerprint for a bookmark id.
    pub fn insert(&mut self, id: u64, fingerprint: u64) {
        self.by_id.insert(id, fingerprint);
        for (band, key) in bands(fingerprint).into_iter().enumerate() {
            self.buckets.entry((band as u8, key)).or_insert(id);
        }
    }

    /// candidate ids sharing at least one band.
    ///
    /// sharing a band only narrows the search from every bookmark to a few
    /// hundred, so the caller still filters with [`is_near_duplicate`].
    #[must_use]
    pub fn candidates(&self, fingerprint: u64) -> Vec<u64> {
        let mut seen = ahash::AHashSet::default();
        let mut out = Vec::with_capacity(BANDS as usize);
        for (band, key) in bands(fingerprint).into_iter().enumerate() {
            if let Some(&id) = self.buckets.get(&(band as u8, key))
                && seen.insert(id)
            {
                out.push(id);
            }
        }
        out
    }

    /// ids within `max_distance` of `fingerprint`, deduplicated.
    #[must_use]
    pub fn find_near(&self, fingerprint: u64, max_distance: u32) -> Vec<u64> {
        self.candidates(fingerprint)
            .into_iter()
            .filter(|&id| {
                self.by_id
                    .get(&id)
                    .is_some_and(|&other| is_near_duplicate(fingerprint, other, max_distance))
            })
            .collect()
    }

    /// read every fingerprint in the store and build the index.
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut index = Self::new();
        let mut stmt = conn
            .prepare("SELECT id, fingerprint FROM bookmark WHERE fingerprint IS NOT NULL")
            .map_err(|e| Error::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))
            .map_err(|e| Error::Store(e.to_string()))?;
        for row in rows {
            let (id, fingerprint) = row.map_err(|e| Error::Store(e.to_string()))?;
            index.insert(id, fingerprint);
        }
        Ok(index)
    }
}
