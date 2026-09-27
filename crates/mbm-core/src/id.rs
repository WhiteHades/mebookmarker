//! 64-bit ids that sort in chronological order.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU16, Ordering};

// high 48 bits are unix millis, low 16 are a per-process counter. integer
// order is time order, so a pipeline cursor is just `where id > ?` on the
// primary key. uuidv7 would sort too but needs a bytewise compare, and a
// plain rowid carries no time at all.
pub const MBM_EPOCH_MS: u64 = 1_577_836_800_000;

const SEQUENCE_SPACE: u64 = 1 << 16;

static SEQUENCE: AtomicU16 = AtomicU16::new(0);
static SEQUENCE_READY: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn sequence() -> &'static AtomicU16 {
    SEQUENCE_READY.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u32, |d| d.subsec_nanos());
        let pid = u64::from(std::process::id());
        let mixed = u64::from(nanos)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(pid.wrapping_mul(0xBF58_476D_1CE4_E5B9))
            .wrapping_add(0x94D0_49BB_1331_11EB);
        SEQUENCE.store((mixed >> 33) as u16, Ordering::Relaxed);
    });
    &SEQUENCE
}

fn next_sequence() -> u16 {
    sequence().fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(u64);

impl Id {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn to_unix_millis(self) -> u64 {
        (self.0 >> 16).wrapping_add(MBM_EPOCH_MS)
    }

    #[must_use]
    pub const fn from_parts(unix_millis: u64, sequence: u16) -> Self {
        let ms = unix_millis.wrapping_sub(MBM_EPOCH_MS);
        Self((ms << 16) | ((sequence as u64) & (SEQUENCE_SPACE - 1)))
    }

    #[must_use]
    pub fn now() -> Self {
        Self::from_parts(now_millis(), next_sequence())
    }

    #[must_use]
    pub fn at(unix_millis: u64) -> Self {
        Self::from_parts(unix_millis, next_sequence())
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Id> for u64 {
    fn from(value: Id) -> Self {
        value.0
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(MBM_EPOCH_MS, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn round_trips_through_the_unix_timestamp() {
        let ms = 1_700_000_000_123;
        let id = Id::from_parts(ms, 7);
        assert_eq!(id.to_unix_millis(), ms);
    }

    #[test]
    fn ordering_matches_chronology() {
        let older = Id::from_parts(1_700_000_000_000, 0xFFFF);
        let newer = Id::from_parts(1_700_000_000_001, 0);
        assert!(older < newer, "a later millisecond must sort higher even with a smaller sequence");
    }

    #[test]
    fn ordering_is_stable_within_one_millisecond() {
        let base = 1_700_000_000_000;
        let mut ids: Vec<Id> = (0..1000).map(|i| Id::from_parts(base, i)).collect();
        ids.sort();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn timestamps_before_the_custom_epoch_wrap_instead_of_panicking() {
        let id = Id::from_parts(0, 1);
        assert_eq!(id, Id::from_parts(0, 1), "must be deterministic");
        assert!(id > Id::from_parts(MBM_EPOCH_MS + 1_000_000, 0), "wraps above the epoch");
    }

    #[test]
    fn zero_is_the_sentinel() {
        assert!(Id::ZERO.is_zero());
        assert!(!Id::now().is_zero());
    }

    #[test]
    fn generated_ids_are_unique_within_a_millisecond() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(Id::now()), "generated a duplicate id");
        }
    }

    #[test]
    fn ids_from_the_same_instant_still_sort_chronologically() {
        let a = Id::now();
        let b = Id::now();
        assert!(a <= b);
    }
}
