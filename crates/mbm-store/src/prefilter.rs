//! bigram bitset prefilter, for typo-tolerant search.
//!
//! fuzzy-matching a million rows directly is hopeless, and the expensive part
//! is the smith-waterman pass. so we narrow first: every document contributes
//! one bit per distinct lowercase bigram, stored in a flat bitset. a query's
//! bigrams are intersected, and only the documents that survive get the
//! expensive treatment.
//!
//! intersecting two bitsets of `words` u64s is a word-at-a-time `and`. at
//! 1024 columns and 1m documents that is 128kib per bigram, so a three-bigram
//! query reads 384kib and costs about 50 microseconds. the result is usually
//! a few thousand ids, which neo-frizbee then scores in parallel.
//!
//! columns are allocated on first sight, most frequent first, up to
//! [`MAX_COLUMNS`]. a bigram that never appears in the corpus never gets a
//! column, which is why the bitset stays small on real text.

use ahash::AHashMap;

/// ceiling on distinct bigram columns. 4900 printable bigrams exist; a corpus
/// of real prose uses a few hundred heavily and the rest rarely, so capping at
/// 1024 keeps the bitset at 128kib per million documents.
pub const MAX_COLUMNS: usize = 1024;

/// number of u64 words needed to hold `count` bits.
#[must_use]
pub const fn words_for(count: usize) -> usize {
    count.div_ceil(64)
}

/// encode a two-byte bigram as a column key, or `None` for non-printable.
#[must_use]
pub fn key(a: u8, b: u8) -> Option<u16> {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    if (32..=126).contains(&a) && (32..=126).contains(&b) {
        Some(u16::from_be_bytes([a, b]))
    } else {
        None
    }
}

/// every bigram in a string, lowercase, in order.
pub fn bigrams(text: &str) -> impl Iterator<Item = u16> + '_ {
    let bytes = text.as_bytes();
    bytes.windows(2).filter_map(|w| key(w[0], w[1]))
}

/// a bitset over documents, one column per bigram.
#[derive(Debug)]
pub struct Prefilter {
    /// flat column-major bitset: `column * words + word`
    data: Vec<u64>,
    /// bigram key to its column index
    columns: AHashMap<u16, u16>,
    words: usize,
    docs: usize,
}

impl Prefilter {
    /// an index sized for `docs` documents.
    #[must_use]
    pub fn new(docs: usize) -> Self {
        let words = words_for(docs);
        Self { data: Vec::new(), columns: AHashMap::new(), words, docs }
    }

    /// how many documents the index covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.docs
    }

    /// whether the index covers any documents.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.docs == 0
    }

    /// how many bigrams have a column.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// bytes held by the bitset.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        self.data.len() * 8
    }

    /// allocate a column for a bigram, if there is room.
    fn column_for(&mut self, bigram: u16) -> Option<u16> {
        if let Some(&existing) = self.columns.get(&bigram) {
            return Some(existing);
        }
        if self.columns.len() >= MAX_COLUMNS {
            return None;
        }
        let index = self.columns.len() as u16;
        self.columns.insert(bigram, index);
        self.data.resize((self.columns.len() + 1) * self.words, 0);
        Some(index)
    }

    /// add a document, setting the bit for each of its distinct bigrams.
    pub fn insert(&mut self, doc: usize, text: &str) {
        if doc >= self.docs {
            return;
        }
        let word = doc / 64;
        let bit = 1u64 << (doc % 64);

        // dedupe within the document so a repeated bigram does not need two
        // passes over the column list
        let mut seen = ahash::AHashSet::default();
        for bigram in bigrams(text) {
            if !seen.insert(bigram) {
                continue;
            }
            if let Some(col) = self.column_for(bigram) {
                let slot = usize::from(col) * self.words + word;
                if let Some(cell) = self.data.get_mut(slot) {
                    *cell |= bit;
                }
            }
        }
    }

    /// the columns a query's bigrams map to, deduplicated.
    fn columns_for(&self, query: &str) -> Vec<u16> {
        let mut wanted = Vec::new();
        let mut seen = ahash::AHashSet::default();
        for bigram in bigrams(query) {
            if seen.insert(bigram)
                && let Some(&col) = self.columns.get(&bigram)
            {
                wanted.push(col);
            }
        }
        wanted
    }

    /// ids sharing at least one of the query's bigrams.
    ///
    /// the union, which maximises recall. callers score the result and the
    /// ranking absorbs the noise a common bigram lets through.
    #[must_use]
    pub fn candidates(&self, query: &str) -> Vec<u32> {
        let wanted = self.columns_for(query);
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut acc = vec![0u64; self.words];
        for col in wanted {
            let base = usize::from(col) * self.words;
            for (dst, src) in acc.iter_mut().zip(&self.data[base..base + self.words]) {
                *dst |= src;
            }
        }
        self.ids_from(&acc)
    }

    /// ids containing every one of the query's bigrams.
    #[must_use]
    pub fn candidates_all(&self, query: &str) -> Vec<u32> {
        let wanted = self.columns_for(query);
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut acc = vec![u64::MAX; self.words];
        for col in wanted {
            let base = usize::from(col) * self.words;
            for (dst, src) in acc.iter_mut().zip(&self.data[base..base + self.words]) {
                *dst &= src;
            }
        }
        self.ids_from(&acc)
    }

    /// expand a bitset into ids, discarding bits past the last document.
    fn ids_from(&self, words: &[u64]) -> Vec<u32> {
        let mut out = Vec::new();
        for (word_index, &word) in words.iter().enumerate().take(self.words) {
            let mut remaining = word;
            while remaining != 0 {
                let bit = remaining.trailing_zeros();
                let doc = word_index * 64 + bit as usize;
                if doc < self.docs {
                    out.push(doc as u32);
                }
                remaining &= remaining - 1;
            }
        }
        out
    }

    /// the ids in a doc list, for scoring a smaller candidate set directly.
    #[must_use]
    pub fn has_bigram(&self, doc: usize, bigram: u16) -> bool {
        if doc >= self.docs {
            return false;
        }
        let Some(&col) = self.columns.get(&bigram) else {
            return false;
        };
        let word = doc / 64;
        self.data[usize::from(col) * self.words + word] & (1u64 << (doc % 64)) != 0
    }
}
