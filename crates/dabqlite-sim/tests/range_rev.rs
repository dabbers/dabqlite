//! Descending range scans against the same free oracle the ascending ones
//! use (docs/DESIGN.md §4.6): a `BTreeMap` of the live rows, read
//! backwards.
//!
//! The point of the operation is COST, not just order. "The twenty
//! newest" through an ascending scan means walking every row below them
//! first — every sample application built on this library wrote that
//! query and every one of them materialised the whole database to answer
//! it. So the tests here check the answer against the oracle and then
//! check that reaching it did not read the database.

use std::collections::BTreeMap;

use dabqlite_core::{BatchOp, Capacities, DbError, Input, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 256 };

fn val(i: u64) -> [u8; VALUE_LEN] {
    let mut v = [0u8; VALUE_LEN];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

fn fresh() -> SimHost {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host
}

/// The oracle: the live map, read backwards, bounded.
fn oracle_rev(model: &BTreeMap<u64, Vec<u8>>, lo: u64, hi: u64) -> Vec<(u64, Vec<u8>)> {
    if lo > hi {
        return Vec::new();
    }
    model
        .range(lo..=hi)
        .rev()
        .map(|(&k, v)| (k, v.clone()))
        .collect()
}

#[test]
fn descending_pages_match_the_oracle_over_random_traffic() {
    for seed in 0..8u64 {
        let mut rng = crash_rng(0x5245_5645, seed); // "REVE"
        let mut host = fresh();
        let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        use rand::Rng;

        for step in 0..90u64 {
            let roll = rng.gen_range(0..100u32);
            let existing = (!model.is_empty()).then(|| {
                *model
                    .keys()
                    .nth(rng.gen_range(0..model.len()))
                    .expect("nonempty")
            });
            match (existing, roll) {
                (Some(id), r) if r < 15 => {
                    host.run(ClientOp::Delete { id });
                    model.remove(&id);
                }
                (Some(id), r) if r < 30 => {
                    let v = val(step + 1000);
                    host.run(ClientOp::Update { id, value: v });
                    model.insert(id, v.to_vec());
                }
                _ => {
                    let id = rng.gen_range(0..500u64);
                    if model.contains_key(&id) {
                        continue;
                    }
                    // Every third insert carries a value too long for one
                    // slot, so the page's "value did not fit" path is on
                    // the descending side too.
                    if step % 3 == 0 {
                        let mut long = vec![0u8; 3 * VALUE_LEN + 4];
                        long[..8].copy_from_slice(&id.to_le_bytes());
                        assert!(matches!(
                            host.batch(&[BatchOp::Insert { id, value: &long }]),
                            Driven::Done(Output::BatchDone { result: Ok(()), .. })
                        ));
                        model.insert(id, long);
                    } else {
                        let v = val(id);
                        host.run(ClientOp::Insert { id, value: v });
                        model.insert(id, v.to_vec());
                    }
                }
            }

            // Whole database, both directions, and bounded windows.
            assert_eq!(
                host.range_all_rev(0, u64::MAX),
                oracle_rev(&model, 0, u64::MAX),
                "seed={seed} step={step}: full descending scan"
            );
            let a: u64 = rng.gen_range(0..600);
            let b: u64 = rng.gen_range(0..600);
            let (lo, hi) = (a.min(b), a.max(b));
            assert_eq!(
                host.range_all_rev(lo, hi),
                oracle_rev(&model, lo, hi),
                "seed={seed} step={step}: descending [{lo},{hi}]"
            );
            // Ascending, reversed, must be the same rows.
            let mut ascending = host.range_all_bytes(lo, hi);
            ascending.reverse();
            assert_eq!(
                host.range_all_rev(lo, hi),
                ascending,
                "seed={seed} step={step}: the two directions disagree"
            );
        }
    }
}

/// The cost claim: a descending page reads a page's worth of rows, not
/// the database. Reads perform no I/O at all, so the honest measure is
/// that the answer arrives in ONE page for a bounded ask.
#[test]
fn the_highest_ids_cost_one_page_not_a_scan() {
    let caps = Capacities { rows: 4096 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for i in 0..2000u64 {
        host.run(ClientOp::Insert {
            id: i,
            value: val(i),
        });
    }
    let io = host.io_count;
    let page = match host.run_input(Input::RangeRev {
        lo: 0,
        hi: u64::MAX,
    }) {
        Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(page.count as usize, dabqlite_core::RANGE_PAGE);
    let ids: Vec<u64> = page.items[..page.count as usize]
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, (1992..2000u64).rev().collect::<Vec<_>>());
    assert!(page.next.is_some());
    assert_eq!(host.io_count, io, "a scan must not touch the disk");
}

/// Inverted bounds are empty, not an error — the same answer the
/// ascending scan gives, from the other end.
#[test]
fn inverted_and_empty_bounds_behave() {
    let mut host = fresh();
    for i in 0..5u64 {
        host.run(ClientOp::Insert {
            id: i * 10,
            value: val(i),
        });
    }
    assert!(host.range_all_rev(40, 10).is_empty());
    assert!(host.range_all_rev(41, 49).is_empty());
    assert_eq!(host.range_all_rev(40, 40).len(), 1);
    // An empty database has nothing in either direction.
    let mut empty = fresh();
    assert!(empty.range_all_rev(0, u64::MAX).is_empty());
}

/// Descending scans answer over the recovered prefix like everything
/// else: rebuilt index, same oracle, at every crash boundary.
#[test]
fn descending_scans_survive_every_crash_boundary() {
    for seed in 0..4u64 {
        for boundary in 0..5u64 {
            let mut host = fresh();
            let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            for i in 0..6u64 {
                let v = val(i);
                host.run(ClientOp::Insert {
                    id: i * 3,
                    value: v,
                });
                model.insert(i * 3, v.to_vec());
            }
            host.crash_after = Some(host.io_count + boundary);
            assert!(matches!(
                host.run(ClientOp::Insert {
                    id: 999,
                    value: val(999)
                }),
                Driven::Crashed
            ));
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(0x4445_5343, seed * 10 + boundary));

            let mut host = SimHost::new(CAPS, disk, None);
            let n = match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("seed={seed} b={boundary}: {other:?}"),
            };
            if n == 7 {
                model.insert(999, val(999).to_vec());
            }
            assert_eq!(
                host.range_all_rev(0, u64::MAX),
                oracle_rev(&model, 0, u64::MAX),
                "seed={seed} b={boundary}"
            );
        }
    }
}

/// The lifecycle, like every other read: refused before open, and the
/// original error forever after a fail-stop.
#[test]
fn descending_scans_respect_the_engine_lifecycle() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    assert!(matches!(
        host.run_input(Input::RangeRev { lo: 0, hi: 9 }),
        Driven::Done(Output::RangeDone {
            result: Err(DbError::NotOpen)
        })
    ));
    host.open();
    host.fail_after = Some(host.io_count);
    host.run(ClientOp::Insert {
        id: 1,
        value: val(1),
    });
    assert!(matches!(
        host.run_input(Input::RangeRev { lo: 0, hi: 9 }),
        Driven::Done(Output::RangeDone {
            result: Err(DbError::IoFailed { .. })
        })
    ));
}
