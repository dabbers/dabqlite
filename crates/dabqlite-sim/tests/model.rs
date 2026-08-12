//! Model-based testing (docs/DESIGN.md §7.2 technique 3): the oracle is a
//! dumb, obviously-correct map. Generate op sequences from a seed, run both
//! implementations, diff after every step.
//!
//! `BTreeMap` rather than `HashMap` so the oracle itself is deterministic
//! (§7.1: never iterate a HashMap).

use std::collections::BTreeMap;

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{Driven, SimDisk, SimHost};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

const SEEDS: u64 = 16;
const OPS: usize = 300;
const CAPS: Capacities = Capacities { rows: 64 };
/// Small id space so duplicates and hits actually happen.
const ID_SPACE: u64 = 96;

#[test]
fn engine_matches_oracle_on_random_op_sequences() {
    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
        let mut host = SimHost::new(CAPS, SimDisk::new(), None);
        assert!(matches!(
            host.open(),
            Driven::Done(Output::OpenDone { result: Ok(0) })
        ));

        for step in 0..OPS {
            let ctx = format!("seed={seed} step={step}");
            let id = rng.gen_range(0..ID_SPACE);
            if rng.gen_bool(0.6) {
                let mut value = [0u8; VALUE_LEN];
                rng.fill_bytes(&mut value);
                let out = host.run(ClientOp::Insert { id, value });
                let expected = if oracle.contains_key(&id) {
                    Err(DbError::DuplicateId { id })
                } else if oracle.len() as u64 == CAPS.rows {
                    Err(DbError::Full {
                        entity: "records",
                        capacity: CAPS.rows,
                    })
                } else {
                    oracle.insert(id, value);
                    Ok(())
                };
                assert_eq!(
                    out,
                    Driven::Done(Output::InsertDone {
                        id,
                        result: expected
                    }),
                    "[{ctx}] insert diverged from oracle"
                );
            } else {
                assert_eq!(
                    host.get(id),
                    oracle.get(&id).copied(),
                    "[{ctx}] get({id}) diverged from oracle"
                );
            }
            // Full-state diff after every step (§7.2: diff after every step).
            let (used, cap) = host.engine.usage();
            assert_eq!(used, oracle.len() as u64, "[{ctx}] row count diverged");
            assert_eq!(cap, CAPS.rows);
        }

        // End of run: restart the process and diff the full recovered state.
        let disk = std::mem::take(&mut host.disk);
        let mut reopened = SimHost::new(CAPS, disk, None);
        match reopened.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                assert_eq!(n, oracle.len() as u64, "seed={seed} reopen row count")
            }
            other => panic!("seed={seed} reopen failed: {other:?}"),
        }
        for id in 0..ID_SPACE {
            assert_eq!(
                reopened.get(id),
                oracle.get(&id).copied(),
                "seed={seed} reopen: get({id}) diverged from oracle"
            );
        }
    }
}
