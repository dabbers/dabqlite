//! How one key/value record is laid out inside ONE dabqlite value.
//!
//! This module used to be `codec.rs` and it used to be the largest thing
//! in the crate, because a value was exactly [`dabqlite::VALUE_LEN`] = 16
//! bytes and a record was not. It split the u64 id into `(record, chunk)`,
//! cut the payload into 16-byte chunks, kept two banks so an update could
//! be rolled back, and wrote a header row last as a commit point it had to
//! provide itself.
//!
//! A value is now any byte string up to [`dabqlite::MAX_VALUE_LEN`],
//! written across as many row slots as it needs in ONE commit, so all of
//! that is gone. A record is:
//!
//! ```text
//! 0..k   key bytes (NUL is not a legal key byte)
//! k      0x00, the key terminator
//! k+1    magic
//! k+2    flags (bit 0: live)
//! k+3..  expiry, unix seconds, u64 LE; 0 means never
//! k+11.. value bytes
//! ```
//!
//! and the id is the key's hash. Nothing about the layout is a commit
//! protocol any more; the library's commit is the commit.
//!
//! ## Why the key comes FIRST
//!
//! The header used to lead, with the key at byte 12. That put the key
//! where nothing could order by it, and `list` was a full scan and a sort
//! because the library's one ordering was by id and an id is a hash.
//!
//! The library now offers a scan in VALUE order — byte-lexicographic over
//! the stored bytes — so a record whose key is its first bytes is a
//! record the library can sort, page and prefix-scan directly. Moving the
//! key to the front is the entire cost of that, and `Db::prefix` answers
//! "everything under `session/`" off an index instead of by reading the
//! whole database.
//!
//! The terminator is what makes byte order KEY order: without it `ab`
//! would sort between `a` and `a!`, because the header byte following a
//! short key would be compared against the next key byte of a longer one.
//! `0x00` is below every legal key byte, so `a\0…` sorts before `ab\0…`
//! exactly as `a` sorts before `ab`. Keys are rejected at the door if
//! they contain one — see `store::validate_key`.

use dabqlite::{Value, MAX_VALUE_LEN};

use crate::KvError;

/// Bytes between the key's terminator and the value: magic, flags and the
/// expiry. The terminator itself is counted separately, so a record is
/// `key.len() + 1 + HEADER + value.len()` bytes.
pub const HEADER: usize = 10;

/// The byte that ends the key. Below every legal key byte, which is what
/// makes byte order key order.
pub const KEY_END: u8 = 0x00;

const MAGIC: u8 = 0xD5;
const F_LIVE: u8 = 0b0000_0001;

/// Bytes a record can hold, key and value together.
pub const MAX_PAYLOAD: usize = MAX_VALUE_LEN - HEADER - 1;
/// Keys are capped well below that so a key always leaves room for a value.
pub const MAX_KEY: usize = 256;
/// How far a lookup probes past a colliding record before giving up.
pub const MAX_PROBE: u64 = 256;

/// FNV-1a, so a key lands on an id without a hashing dependency.
pub fn hash_key(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The id a key lands on before probing.
///
/// The whole 64-bit space is available now. It used to be the top 56 bits,
/// because the low 8 had to address the 256 rows of a record.
pub fn home_id(key: &[u8]) -> u64 {
    hash_key(key)
}

/// The `i`th id in a key's probe sequence.
pub fn probe(home: u64, i: u64) -> u64 {
    home.wrapping_add(i)
}

/// A decoded record. A tombstone is one with `live == false`: it holds no
/// key and no value and exists only to keep a probe chain intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub live: bool,
    pub key: String,
    pub value: Vec<u8>,
    /// Unix seconds at which the record expires; 0 means never.
    pub expires_at: u64,
}

impl Record {
    pub fn expired(&self, now: u64) -> bool {
        self.expires_at != 0 && self.expires_at <= now
    }

    /// Live, and not yet expired — the only state `get` and `list` show.
    pub fn visible(&self, now: u64) -> bool {
        self.live && !self.expired(now)
    }
}

/// Encode `key = value`, with an expiry, as one value.
pub fn encode(key: &str, value: &[u8], expires_at: u64) -> Result<Value, KvError> {
    let payload = key.len() + value.len();
    if payload > MAX_PAYLOAD {
        return Err(KvError::ValueTooLong {
            len: value.len(),
            max: MAX_PAYLOAD - key.len(),
        });
    }
    debug_assert!(
        !key.as_bytes().contains(&KEY_END),
        "validate_key must reject a key holding the terminator"
    );
    let mut b = Vec::with_capacity(HEADER + 1 + payload);
    b.extend_from_slice(key.as_bytes());
    b.push(KEY_END);
    b.push(MAGIC);
    b.push(F_LIVE);
    b.extend_from_slice(&expires_at.to_le_bytes());
    b.extend_from_slice(value);
    // `from_vec` cannot fail here: HEADER + 1 + MAX_PAYLOAD is
    // MAX_VALUE_LEN.
    Value::from_vec(b).map_err(KvError::from)
}

/// A chain link with nothing in it.
///
/// It has an empty key, so it sorts at the very front of the value order
/// — where a scan skips it like any other record that is not visible.
pub fn tombstone() -> Value {
    let mut b = vec![0u8; HEADER + 1];
    b[0] = KEY_END;
    b[1] = MAGIC;
    Value::from_vec(b).expect("a tombstone is eleven bytes")
}

/// Decode a stored value. Fails only when the bytes are not a record this
/// crate wrote.
pub fn decode(v: &Value) -> Result<Record, KvError> {
    let b = v.as_bytes();
    // The key runs to the first terminator; everything after it is fixed
    // width, so a record is well formed exactly when both are present.
    let Some(key_len) = b.iter().position(|&c| c == KEY_END) else {
        return Err(KvError::Layout(format!(
            "a {} byte row is not a kvstore record: no key terminator",
            b.len()
        )));
    };
    let after = key_len + 1;
    if b.len() < after + HEADER || b[after] != MAGIC {
        return Err(KvError::Layout(format!(
            "a {} byte row is not a kvstore record",
            b.len()
        )));
    }
    let live = b[after + 1] & F_LIVE != 0;
    let expires_at = u64::from_le_bytes(b[after + 2..after + 10].try_into().expect("8 bytes"));
    let (key, value) = (&b[..key_len], &b[after + HEADER..]);
    let key = String::from_utf8(key.to_vec())
        .map_err(|e| KvError::Layout(format!("record has a non-UTF-8 key: {e}")))?;
    Ok(Record {
        live,
        key,
        value: value.to_vec(),
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_with_its_expiry() {
        let v = encode("session/42", b"token", 1_700_000_000).unwrap();
        let r = decode(&v).unwrap();
        assert!(r.live);
        assert_eq!(r.key, "session/42");
        assert_eq!(r.value, b"token");
        assert_eq!(r.expires_at, 1_700_000_000);
    }

    /// Used to be `split_pads_the_tail_without_losing_interior_zeros`,
    /// which recorded that a value was a fixed 16-byte slot and that the
    /// library guessed where the payload ended: a payload whose last byte
    /// was zero was indistinguishable from padding, so this crate carried
    /// an explicit length beside every chunk and read rows through
    /// `Value::raw()`. `Value` now stores its own length, so trailing
    /// zeros survive and there is nothing to pad.
    #[test]
    fn zero_bytes_survive_anywhere_in_a_value_including_the_last_one() {
        for value in [
            b"ab\0cd".to_vec(),
            b"trailing\0\0\0".to_vec(),
            vec![0u8; 40],
            Vec::new(),
        ] {
            let r = decode(&encode("k", &value, 0).unwrap()).unwrap();
            assert_eq!(r.value, value, "{value:?} did not round-trip");
        }
    }

    /// Used to be `header_round_trips_and_alternates_banks`. There are no
    /// banks: a record is one value and an update is one atomic commit, so
    /// there is no half-applied state for a spare bank to protect.
    #[test]
    fn a_whole_record_is_one_value_over_a_run_of_slots() {
        let key = "k";
        let v = encode(key, &vec![b'x'; 300], 0).unwrap();
        // key, terminator, header, value — the whole record, in that order.
        let expect = key.len() + 1 + HEADER + 300;
        assert_eq!(v.len(), expect);
        assert_eq!(
            dabqlite::Op::put(1, v).rows(),
            expect.div_ceil(dabqlite::VALUE_LEN)
        );
    }

    #[test]
    fn a_tombstone_is_not_live_and_carries_nothing() {
        let t = decode(&tombstone()).unwrap();
        assert!(!t.live);
        assert!(!t.visible(0));
        assert!(t.key.is_empty());
        assert!(t.value.is_empty());
    }

    #[test]
    fn expiry_is_honoured() {
        let at = |t| decode(&encode("k", b"v", t).unwrap()).unwrap();
        assert!(at(100).visible(99));
        assert!(!at(100).visible(100));
        assert!(!at(100).visible(101));
        assert!(at(0).visible(u64::MAX));
    }

    #[test]
    fn a_non_record_row_is_rejected_rather_than_misread() {
        let junk = Value::from_text("just some text that is long enough").unwrap();
        assert!(decode(&junk).is_err());
        assert!(decode(&Value::empty()).is_err());
    }

    #[test]
    fn a_value_that_does_not_fit_is_refused_rather_than_cut() {
        let err = encode("k", &vec![b'x'; MAX_PAYLOAD], 0).unwrap_err();
        assert!(matches!(err, KvError::ValueTooLong { .. }), "{err:?}");
        // One byte less than the ceiling, with the key counted, fits.
        assert!(encode("k", &vec![b'x'; MAX_PAYLOAD - 1], 0).is_ok());
    }
}
