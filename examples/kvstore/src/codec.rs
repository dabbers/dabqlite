//! How a variable-length key/value record is laid out over dabqlite's
//! fixed shape of `(id: u64, value: [u8; 16])`.
//!
//! dabqlite stores 16-byte values keyed by a u64. A key/value store needs
//! arbitrary-length keys and values, so this module invents the missing
//! layer:
//!
//! * The u64 id is split into `(record, chunk)`: the top 56 bits pick a
//!   *record*, the low 8 bits pick one of its 256 rows.
//! * Chunk 0 of a record is its **header** — the only row that decides
//!   whether the record exists. It is written last, so a crash before it
//!   lands leaves invisible garbage rather than a half-record.
//! * The remaining 254 usable chunks are split into two **banks**. An
//!   update writes the new payload into the bank that is *not* live and
//!   then flips one bit in the header, so the old value survives a crash
//!   at any point. This is hand-rolled double buffering; dabqlite commits
//!   one row at a time and has no multi-row transaction.
//! * The payload is `key ++ value`, with both lengths stored in the
//!   header, so values may contain NUL bytes. (`Value::as_bytes()` cannot
//!   be used for this: it stops at the first zero byte.)

use dabqlite::{Value, VALUE_LEN};

use crate::KvError;

/// Low bits of a row id that select a chunk within a record.
pub const CHUNK_BITS: u32 = 8;
pub const CHUNKS_PER_RECORD: u64 = 1 << CHUNK_BITS;
pub const CHUNK_MASK: u64 = CHUNKS_PER_RECORD - 1;
/// The 56 bits left over for the record number.
pub const RECORD_MASK: u64 = u64::MAX >> CHUNK_BITS;

/// Chunks in one payload bank: 1..=127 for bank A, 128..=254 for bank B.
pub const BANK_CHUNKS: u64 = 127;
pub const BANK_A_START: u64 = 1;
pub const BANK_B_START: u64 = 128;

/// Bytes a record can hold, key and value together.
pub const MAX_PAYLOAD: usize = (BANK_CHUNKS as usize) * VALUE_LEN; // 2032
/// Keys are capped well below that so a key always leaves room for a value.
pub const MAX_KEY: usize = 256;
/// How far a lookup probes past a colliding record before giving up.
pub const MAX_PROBE: u64 = 256;

/// The two banks must tile the record without overlapping.
const _: () = assert!(BANK_A_START + BANK_CHUNKS == BANK_B_START);
const _: () = assert!(BANK_B_START + BANK_CHUNKS <= CHUNKS_PER_RECORD);

const MAGIC: u8 = 0xD5;
const F_LIVE: u8 = 0b0000_0001;
const F_BANK_B: u8 = 0b0000_0010;

/// Row id of chunk `chunk` of record `rec`.
pub fn row_id(rec: u64, chunk: u64) -> u64 {
    debug_assert!(chunk < CHUNKS_PER_RECORD);
    (rec << CHUNK_BITS) | chunk
}

/// Row id of a record's header.
pub fn header_id(rec: u64) -> u64 {
    row_id(rec, 0)
}

/// Which record a row id belongs to.
pub fn record_of(id: u64) -> u64 {
    id >> CHUNK_BITS
}

/// Which chunk within its record a row id is.
pub fn chunk_of(id: u64) -> u64 {
    id & CHUNK_MASK
}

/// Rows needed to hold `n` payload bytes.
pub fn chunks_for(n: usize) -> u64 {
    n.div_ceil(VALUE_LEN) as u64
}

/// FNV-1a, so a key lands on a record without a hashing dependency.
pub fn hash_key(key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The record a key hashes to before probing.
pub fn home_record(key: &[u8]) -> u64 {
    hash_key(key) & RECORD_MASK
}

/// The `i`th record in a key's probe sequence.
pub fn probe(home: u64, i: u64) -> u64 {
    home.wrapping_add(i) & RECORD_MASK
}

/// A record's header row, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub key_len: usize,
    pub val_len: usize,
    /// Unix seconds at which the record expires; 0 means never.
    pub expires_at: u64,
}

impl Header {
    pub fn tombstone() -> Header {
        Header {
            flags: 0,
            key_len: 0,
            val_len: 0,
            expires_at: 0,
        }
    }

    pub fn is_live(&self) -> bool {
        self.flags & F_LIVE != 0
    }

    pub fn bank_start(&self) -> u64 {
        if self.flags & F_BANK_B != 0 {
            BANK_B_START
        } else {
            BANK_A_START
        }
    }

    /// The bank an update should write into: the one that is not live.
    pub fn spare_bank_start(&self) -> u64 {
        if self.is_live() && self.flags & F_BANK_B == 0 {
            BANK_B_START
        } else {
            BANK_A_START
        }
    }

    pub fn payload_len(&self) -> usize {
        self.key_len + self.val_len
    }

    pub fn expired(&self, now: u64) -> bool {
        self.expires_at != 0 && self.expires_at <= now
    }

    pub fn live_at(&self, now: u64) -> bool {
        self.is_live() && !self.expired(now)
    }

    pub fn new(key_len: usize, val_len: usize, expires_at: u64, bank_start: u64) -> Header {
        let mut flags = F_LIVE;
        if bank_start == BANK_B_START {
            flags |= F_BANK_B;
        }
        Header {
            flags,
            key_len,
            val_len,
            expires_at,
        }
    }

    pub fn encode(&self) -> Value {
        let mut b = [0u8; VALUE_LEN];
        b[0] = MAGIC;
        b[1] = self.flags;
        b[2..4].copy_from_slice(&(self.key_len as u16).to_le_bytes());
        b[4..8].copy_from_slice(&(self.val_len as u32).to_le_bytes());
        b[8..16].copy_from_slice(&self.expires_at.to_le_bytes());
        Value::from(b)
    }

    pub fn decode(v: &Value) -> Result<Header, KvError> {
        // NOTE: `raw()`, not `as_bytes()` — the latter stops at the first
        // zero byte, which for a header means "almost always".
        let b = v.raw();
        if b[0] != MAGIC {
            return Err(KvError::Layout(
                "row 0 of a record is not a kvstore header".into(),
            ));
        }
        let key_len = u16::from_le_bytes([b[2], b[3]]) as usize;
        let val_len = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let expires_at = u64::from_le_bytes(b[8..16].try_into().expect("8 bytes"));
        let h = Header {
            flags: b[1],
            key_len,
            val_len,
            expires_at,
        };
        if h.is_live() && h.payload_len() > MAX_PAYLOAD {
            return Err(KvError::Layout(format!(
                "header declares {} payload bytes; a record holds {MAX_PAYLOAD}",
                h.payload_len()
            )));
        }
        Ok(h)
    }
}

/// Split a payload into 16-byte rows, zero padding the last one.
pub fn split(payload: &[u8]) -> Vec<Value> {
    payload
        .chunks(VALUE_LEN)
        .map(|c| Value::from_bytes(c).expect("chunk is at most VALUE_LEN"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_split_and_rejoin() {
        for &(rec, chunk) in &[(0, 0), (1, 5), (RECORD_MASK, 255), (12345, 128)] {
            let id = row_id(rec, chunk);
            assert_eq!(record_of(id), rec);
            assert_eq!(chunk_of(id), chunk);
        }
    }

    #[test]
    fn header_round_trips_and_alternates_banks() {
        let h = Header::new(7, 300, 1_700_000_000, BANK_A_START);
        let back = Header::decode(&h.encode()).unwrap();
        assert_eq!(back, h);
        assert_eq!(back.bank_start(), BANK_A_START);
        assert_eq!(back.spare_bank_start(), BANK_B_START);
        let h2 = Header::new(7, 300, 0, back.spare_bank_start());
        assert_eq!(h2.bank_start(), BANK_B_START);
        assert_eq!(h2.spare_bank_start(), BANK_A_START);
    }

    #[test]
    fn a_non_header_row_is_rejected_rather_than_misread() {
        let junk = Value::from_text("just some text").unwrap();
        assert!(Header::decode(&junk).is_err());
    }

    #[test]
    fn tombstones_are_not_live() {
        let t = Header::tombstone();
        assert!(!t.is_live());
        assert!(!t.live_at(0));
        assert_eq!(Header::decode(&t.encode()).unwrap(), t);
    }

    #[test]
    fn expiry_is_honoured() {
        let h = Header::new(1, 1, 100, BANK_A_START);
        assert!(h.live_at(99));
        assert!(!h.live_at(100));
        assert!(!h.live_at(101));
        let never = Header::new(1, 1, 0, BANK_A_START);
        assert!(never.live_at(u64::MAX));
    }

    #[test]
    fn split_pads_the_tail_without_losing_interior_zeros() {
        let payload = b"ab\0cd".to_vec();
        let chunks = split(&payload);
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].raw()[..5], &payload[..]);
        // `as_bytes()` used to stop at the first zero ANYWHERE, returning
        // b"ab" and silently losing the tail. It now trims only trailing
        // padding, so interior zeros survive. (This crate still uses
        // `raw()` plus an explicit length, because a payload whose own
        // last byte is zero remains indistinguishable from padding in a
        // fixed-width slot.)
        assert_eq!(chunks[0].as_bytes(), b"ab\0cd");
    }
}
