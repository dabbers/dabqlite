//! The record layer, as pure functions.
//!
//! Nothing here touches a `Db`. It reads a materialised map of rows and
//! returns a list of row operations to apply. That split is not a stylistic
//! choice: an open `dabqlite::Db<S>` cannot be named outside the library
//! (see `lib.rs`), so it cannot be a struct field or a function parameter.
//! Working on a snapshot of the rows is the only way to get this logic out
//! of one giant function.

use std::collections::BTreeMap;

use dabqlite::{Value, VALUE_LEN};

use crate::codec::{
    chunk_of, chunks_for, header_id, home_record, probe, record_of, row_id, split, Header,
    MAX_KEY, MAX_PAYLOAD, MAX_PROBE,
};
use crate::KvError;

/// Every row in the database, by id.
pub type Rows = BTreeMap<u64, Value>;

/// A single-row change. dabqlite commits exactly one of these atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOp {
    Put(u64, Value),
    Del(u64),
}

/// A key and everything stored with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    /// Unix seconds; 0 means no expiry.
    pub expires_at: u64,
    pub record: u64,
}

/// Where a key lives, or where it would go.
#[derive(Debug, Clone, Copy)]
pub enum Slot {
    /// The key is stored at this record (possibly expired).
    Occupied { rec: u64, header: Header },
    /// The key is absent; this record is the first reusable slot.
    Vacant { rec: u64 },
}

pub fn validate_key(key: &str) -> Result<(), KvError> {
    if key.is_empty() {
        return Err(KvError::BadKey("a key may not be empty".into()));
    }
    if key.len() > MAX_KEY {
        return Err(KvError::KeyTooLong {
            len: key.len(),
            max: MAX_KEY,
        });
    }
    Ok(())
}

/// Read one record's `key ++ value` payload out of the row map.
pub fn read_payload(rows: &Rows, rec: u64, header: &Header) -> Result<Vec<u8>, KvError> {
    let want = header.payload_len();
    let n = chunks_for(want);
    let start = header.bank_start();
    let mut out = Vec::with_capacity(n as usize * VALUE_LEN);
    for i in 0..n {
        let id = row_id(rec, start + i);
        let v = rows.get(&id).ok_or_else(|| {
            KvError::Layout(format!(
                "record {rec} is missing chunk {} of {n}; the header survived a \
                 write its payload did not",
                i + 1
            ))
        })?;
        out.extend_from_slice(&v.raw());
    }
    out.truncate(want);
    Ok(out)
}

fn read_key(rows: &Rows, rec: u64, header: &Header) -> Result<Vec<u8>, KvError> {
    let mut p = read_payload(rows, rec, header)?;
    p.truncate(header.key_len);
    Ok(p)
}

/// Find the record holding `key`, or the record a new one should use.
pub fn locate(rows: &Rows, key: &[u8]) -> Result<Slot, KvError> {
    let home = home_record(key);
    let mut first_free: Option<u64> = None;
    for i in 0..MAX_PROBE {
        let rec = probe(home, i);
        match rows.get(&header_id(rec)) {
            None => {
                return Ok(Slot::Vacant {
                    rec: first_free.unwrap_or(rec),
                })
            }
            Some(raw) => {
                let header = Header::decode(raw)?;
                if !header.is_live() {
                    first_free.get_or_insert(rec);
                    continue;
                }
                if read_key(rows, rec, &header)? == key {
                    return Ok(Slot::Occupied { rec, header });
                }
            }
        }
    }
    Err(KvError::ProbeExhausted { probes: MAX_PROBE })
}

/// Fetch a key's entry, treating an expired record as absent.
pub fn lookup(rows: &Rows, key: &str, now: u64) -> Result<Option<Entry>, KvError> {
    validate_key(key)?;
    match locate(rows, key.as_bytes())? {
        Slot::Vacant { .. } => Ok(None),
        Slot::Occupied { rec, header } => {
            if !header.live_at(now) {
                return Ok(None);
            }
            let mut payload = read_payload(rows, rec, &header)?;
            let value = payload.split_off(header.key_len);
            Ok(Some(Entry {
                key: key.to_string(),
                value,
                expires_at: header.expires_at,
                record: rec,
            }))
        }
    }
}

/// What a `set` will do.
#[derive(Debug, Clone)]
pub struct SetPlan {
    pub ops: Vec<RowOp>,
    /// True when a live, unexpired value for this key was overwritten.
    pub replaced: bool,
    pub record: u64,
}

/// Build the row writes for `key = value`.
///
/// Ordering is the whole point: payload rows first, header last. Each row
/// is committed atomically by dabqlite, so a crash anywhere before the
/// header lands leaves the previous value intact and the new payload
/// sitting in a bank nothing points at.
pub fn plan_set(
    rows: &Rows,
    key: &str,
    value: &[u8],
    expires_at: u64,
    now: u64,
) -> Result<SetPlan, KvError> {
    validate_key(key)?;
    let payload_len = key.len() + value.len();
    if payload_len > MAX_PAYLOAD {
        return Err(KvError::ValueTooLong {
            len: value.len(),
            max: MAX_PAYLOAD - key.len(),
        });
    }
    let (rec, bank, replaced) = match locate(rows, key.as_bytes())? {
        Slot::Vacant { rec } => (rec, crate::codec::BANK_A_START, false),
        Slot::Occupied { rec, header } => {
            (rec, header.spare_bank_start(), header.live_at(now))
        }
    };

    let mut payload = Vec::with_capacity(payload_len);
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);

    let mut ops = Vec::new();
    for (i, chunk) in split(&payload).into_iter().enumerate() {
        ops.push(RowOp::Put(row_id(rec, bank + i as u64), chunk));
    }
    let header = Header::new(key.len(), value.len(), expires_at, bank);
    ops.push(RowOp::Put(header_id(rec), header.encode()));
    Ok(SetPlan {
        ops,
        replaced,
        record: rec,
    })
}

/// Retire a key: one header write, nothing else.
///
/// The payload rows are deliberately left alone. In dabqlite a delete is
/// an appended tombstone that *consumes* a slot, so deleting the payload
/// would cost capacity rather than free it. The rows become unreachable
/// and are dropped by `compact`.
pub fn plan_delete(rows: &Rows, key: &str, now: u64) -> Result<Option<Vec<RowOp>>, KvError> {
    validate_key(key)?;
    match locate(rows, key.as_bytes())? {
        Slot::Occupied { rec, header } if header.live_at(now) => Ok(Some(vec![RowOp::Put(
            header_id(rec),
            Header::tombstone().encode(),
        )])),
        // An expired record is already invisible; retiring it is `purge`'s job.
        _ => Ok(None),
    }
}

/// Every live, unexpired entry, ordered by key.
pub fn scan(rows: &Rows, now: u64) -> Result<Vec<Entry>, KvError> {
    let mut out = Vec::new();
    for (&id, raw) in rows.iter() {
        if chunk_of(id) != 0 {
            continue;
        }
        let header = Header::decode(raw)?;
        if !header.live_at(now) {
            continue;
        }
        let rec = record_of(id);
        let mut payload = read_payload(rows, rec, &header)?;
        let value = payload.split_off(header.key_len);
        let key = String::from_utf8(payload)
            .map_err(|e| KvError::Layout(format!("record {rec} has a non-UTF-8 key: {e}")))?;
        out.push(Entry {
            key,
            value,
            expires_at: header.expires_at,
            record: rec,
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// Live records whose expiry has passed, and the writes that retire them.
pub fn plan_purge(rows: &Rows, now: u64) -> Result<(Vec<String>, Vec<RowOp>), KvError> {
    let mut keys = Vec::new();
    let mut ops = Vec::new();
    for (&id, raw) in rows.iter() {
        if chunk_of(id) != 0 {
            continue;
        }
        let header = Header::decode(raw)?;
        if !header.is_live() || !header.expired(now) {
            continue;
        }
        let rec = record_of(id);
        let mut payload = read_payload(rows, rec, &header)?;
        payload.truncate(header.key_len);
        keys.push(String::from_utf8_lossy(&payload).into_owned());
        ops.push(RowOp::Put(header_id(rec), Header::tombstone().encode()));
    }
    Ok((keys, ops))
}

/// Substring search over values (and optionally keys).
pub fn search(rows: &Rows, needle: &[u8], keys_too: bool, now: u64) -> Result<Vec<Entry>, KvError> {
    Ok(scan(rows, now)?
        .into_iter()
        .filter(|e| {
            contains(&e.value, needle) || (keys_too && contains(e.key.as_bytes(), needle))
        })
        .collect())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Accounting the library cannot report, because it does not know that a
/// record is more than one row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordCensus {
    pub live: u64,
    pub expired: u64,
    pub tombstones: u64,
    /// Payload rows no live header points at — dead weight until `compact`.
    pub unreachable_rows: u64,
}

pub fn census(rows: &Rows, now: u64) -> Result<RecordCensus, KvError> {
    let mut c = RecordCensus::default();
    let mut reachable = 0u64;
    let mut headers = 0u64;
    for (&id, raw) in rows.iter() {
        if chunk_of(id) != 0 {
            continue;
        }
        headers += 1;
        let header = Header::decode(raw)?;
        if !header.is_live() {
            c.tombstones += 1;
        } else if header.expired(now) {
            c.expired += 1;
            reachable += chunks_for(header.payload_len());
        } else {
            c.live += 1;
            reachable += chunks_for(header.payload_len());
        }
    }
    let payload_rows = rows.len() as u64 - headers;
    c.unreachable_rows = payload_rows.saturating_sub(reachable);
    Ok(c)
}

/// A tolerant [`scan`] for salvage: records this layout cannot read are
/// skipped and counted instead of failing the whole listing.
pub fn scan_recovered(rows: &Rows, now: u64) -> (Vec<Entry>, u64) {
    let mut out = Vec::new();
    let mut lost = 0u64;
    for (&id, raw) in rows.iter() {
        if chunk_of(id) != 0 {
            continue;
        }
        let Ok(header) = Header::decode(raw) else {
            lost += 1;
            continue;
        };
        if !header.is_live() {
            continue;
        }
        let rec = record_of(id);
        match read_payload(rows, rec, &header) {
            Ok(mut payload) => {
                let value = payload.split_off(header.key_len);
                match String::from_utf8(payload) {
                    Ok(key) => out.push(Entry {
                        key,
                        value,
                        expires_at: header.expires_at,
                        record: rec,
                    }),
                    Err(_) => lost += 1,
                }
            }
            Err(_) => lost += 1,
        }
    }
    out.retain(|e| e.expires_at == 0 || e.expires_at > now);
    out.sort_by(|a, b| a.key.cmp(&b.key));
    (out, lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(rows: &mut Rows, ops: Vec<RowOp>) {
        for op in ops {
            match op {
                RowOp::Put(id, v) => {
                    rows.insert(id, v);
                }
                RowOp::Del(id) => {
                    rows.remove(&id);
                }
            }
        }
    }

    fn set(rows: &mut Rows, k: &str, v: &str) {
        let plan = plan_set(rows, k, v.as_bytes(), 0, 0).unwrap();
        apply(rows, plan.ops);
    }

    #[test]
    fn set_then_lookup_round_trips_a_long_value() {
        let mut rows = Rows::new();
        let long = "x".repeat(1000);
        set(&mut rows, "session/42", &long);
        let got = lookup(&rows, "session/42", 0).unwrap().unwrap();
        assert_eq!(got.value, long.as_bytes());
        assert!(rows.len() > 60, "a 1000-byte value must span many rows");
    }

    #[test]
    fn values_may_contain_nul_and_newlines() {
        let mut rows = Rows::new();
        let v = b"line1\nline2\0tail".to_vec();
        let plan = plan_set(&rows, "k", &v, 0, 0).unwrap();
        apply(&mut rows, plan.ops);
        assert_eq!(lookup(&rows, "k", 0).unwrap().unwrap().value, v);
    }

    #[test]
    fn an_update_writes_the_other_bank_so_the_old_value_survives_a_torn_write() {
        let mut rows = Rows::new();
        set(&mut rows, "k", "original value that is quite long indeed");
        let before = rows.clone();
        let plan = plan_set(&rows, "k", b"replacement", 0, 0).unwrap();
        assert!(plan.replaced);

        // Apply every prefix of the plan except the final header write and
        // check the old value is still readable — that is what a crash
        // mid-update looks like.
        for cut in 0..plan.ops.len() - 1 {
            let mut torn = before.clone();
            apply(&mut torn, plan.ops[..cut].to_vec());
            assert_eq!(
                lookup(&torn, "k", 0).unwrap().unwrap().value,
                b"original value that is quite long indeed",
                "a crash after {cut} row writes lost the previous value"
            );
        }
        // The header is the commit point.
        apply(&mut rows, plan.ops);
        assert_eq!(lookup(&rows, "k", 0).unwrap().unwrap().value, b"replacement");
    }

    /// Write a record for `key` at a chosen record number, so a probe
    /// chain can be built without brute-forcing a 56-bit hash collision.
    fn plant(rows: &mut Rows, rec: u64, key: &str, value: &[u8]) {
        let mut payload = key.as_bytes().to_vec();
        payload.extend_from_slice(value);
        for (i, chunk) in crate::codec::split(&payload).into_iter().enumerate() {
            rows.insert(
                row_id(rec, crate::codec::BANK_A_START + i as u64),
                chunk,
            );
        }
        rows.insert(
            header_id(rec),
            Header::new(key.len(), value.len(), 0, crate::codec::BANK_A_START).encode(),
        );
    }

    #[test]
    fn a_taken_home_record_is_probed_past_and_tombstones_keep_the_chain() {
        let mut rows = Rows::new();
        let home = home_record(b"alpha");
        plant(&mut rows, home, "occupier", b"x");
        set(&mut rows, "alpha", "1");

        match locate(&rows, b"alpha").unwrap() {
            Slot::Occupied { rec, .. } => assert_eq!(rec, crate::codec::probe(home, 1)),
            other => panic!("expected alpha one past its home record, got {other:?}"),
        }
        assert_eq!(lookup(&rows, "alpha", 0).unwrap().unwrap().value, b"1");

        // Retiring the record that sits in front of it must not hide it:
        // the tombstone has to stay as a chain link.
        rows.insert(header_id(home), Header::tombstone().encode());
        assert_eq!(lookup(&rows, "alpha", 0).unwrap().unwrap().value, b"1");
    }

    #[test]
    fn a_tombstoned_record_is_reused_by_the_next_key_that_lands_on_it() {
        let mut rows = Rows::new();
        set(&mut rows, "k", "first");
        let rec = match locate(&rows, b"k").unwrap() {
            Slot::Occupied { rec, .. } => rec,
            other => panic!("{other:?}"),
        };
        let ops = plan_delete(&rows, "k", 0).unwrap().unwrap();
        apply(&mut rows, ops);
        assert!(lookup(&rows, "k", 0).unwrap().is_none());
        match locate(&rows, b"k").unwrap() {
            Slot::Vacant { rec: free } => assert_eq!(free, rec, "the freed slot must be reused"),
            other => panic!("{other:?}"),
        }
        set(&mut rows, "k", "second");
        assert_eq!(lookup(&rows, "k", 0).unwrap().unwrap().value, b"second");
        assert_eq!(
            rows.keys().filter(|id| chunk_of(**id) == 0).count(),
            1,
            "reuse must not leak a second header"
        );
    }

    #[test]
    fn scan_is_ordered_and_skips_expired_and_deleted() {
        let mut rows = Rows::new();
        set(&mut rows, "b", "2");
        set(&mut rows, "a", "1");
        set(&mut rows, "c", "3");
        let plan = plan_set(&rows, "d", b"4", 100, 0).unwrap();
        apply(&mut rows, plan.ops);
        let ops = plan_delete(&rows, "b", 0).unwrap().unwrap();
        apply(&mut rows, ops);

        let at_50: Vec<_> = scan(&rows, 50).unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(at_50, vec!["a", "c", "d"]);
        let at_150: Vec<_> = scan(&rows, 150).unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(at_150, vec!["a", "c"]);
    }

    #[test]
    fn search_matches_across_the_sixteen_byte_row_boundary() {
        let mut rows = Rows::new();
        // "needle" straddles rows 1 and 2 of the payload.
        set(&mut rows, "k", "0123456789abcdeneedle-tail");
        let hits = search(&rows, b"needle", false, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(search(&rows, b"absent", false, 0).unwrap().is_empty());
    }

    #[test]
    fn census_counts_rows_no_live_header_points_at() {
        let mut rows = Rows::new();
        set(&mut rows, "k", &"x".repeat(200)); // 13 payload rows
        let c = census(&rows, 0).unwrap();
        assert_eq!(c.live, 1);
        assert_eq!(c.unreachable_rows, 0);
        set(&mut rows, "k", "short"); // writes bank B, bank A is now junk
        let c = census(&rows, 0).unwrap();
        assert_eq!(c.live, 1);
        assert_eq!(c.unreachable_rows, 13);
    }

    #[test]
    fn a_value_that_does_not_fit_is_refused_rather_than_cut() {
        let rows = Rows::new();
        let huge = vec![b'x'; MAX_PAYLOAD];
        let err = plan_set(&rows, "k", &huge, 0, 0).unwrap_err();
        assert!(matches!(err, KvError::ValueTooLong { .. }), "{err:?}");
    }
}
