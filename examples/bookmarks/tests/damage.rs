//! What one bad byte costs, and where the rescue path is missing.

use bookmarks::{Store, StoreError};
use dabqlite::Error as DbErr;

const MDN: &str = "https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API";
const T0: u64 = 1_700_000_000;

fn tags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[cfg(unix)]
#[test]
fn one_damaged_slot_costs_the_bookmark_that_owns_it_and_not_the_others() {
    // The good news, and it is genuinely good: a value spans many slots
    // now, and damaging one of them is DETECTED rather than served as a
    // silently short value. Strict open refuses; salvage quarantines the
    // whole multi-slot value and keeps serving everything else.
    //
    // The blast radius is worth stating: one bad 32-byte slot costs the
    // entire bookmark, not one row of it.
    let dir = std::env::temp_dir().join(format!("bookmarks-damage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut s: Store<dabqlite::PosixStorage> = Store::open_with(&dir, 1024).unwrap();
        s.add(MDN, "IndexedDB API", &tags(&["browser"]), T0)
            .unwrap();
        s.add("https://short.test", "S", &[], T0 + 1).unwrap();
    }
    let rows = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rows-")
        })
        .max_by_key(|p| p.metadata().unwrap().len())
        .expect("rows file");
    let mut bytes = std::fs::read(&rows).unwrap();
    // Slot 2 is a continuation of bookmark 1's value (slot 0 is its head).
    bytes[2 * 32 + 20] ^= 0xFF;
    std::fs::write(&rows, &bytes).unwrap();

    // Strict open refuses the WHOLE database rather than serving a
    // truncated value. Correct, and total: there is no partial open.
    let strict = Store::<dabqlite::PosixStorage>::open(&dir);
    assert!(
        matches!(strict, Err(StoreError::Db(DbErr::Corrupt { .. }))),
        "{:?}",
        strict.map(|_| ())
    );

    // Salvage is where the containment claim is honoured.
    let rescued = dabqlite::SalvageDb::salvage_with(&dir, 1024).unwrap();
    assert!(rescued.is_degraded());
    assert!(
        matches!(rescued.get(1), Err(DbErr::Degraded { .. })),
        "the damaged bookmark is refused, not guessed at"
    );
    assert!(
        rescued.get(2).unwrap().is_some(),
        "the undamaged bookmark survives"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_damaged_blob_is_salvageable_and_rebuildable() {
    // This used to be a RESILIENCY HOLE landing squarely on this crate's
    // headline use case. `Db::salvage` existed only for a DIRECTORY, so a
    // database living as one opaque blob — a browser, an IndexedDB row, a
    // downloaded file: the story `snapshot`/`load` is sold on — had no
    // rescue path at all. One flipped byte and `load` refused the whole
    // thing, with an error advising "reopen in salvage mode": advice that
    // could not be taken, because there was no directory and the rows
    // file's name is not part of the API.
    let mut s = Store::in_memory_with(1024).unwrap();
    for i in 1..=5u64 {
        s.add(
            &format!("https://example.com/{i}/{}", "a".repeat(120)),
            "Title",
            &tags(&["tag"]),
            T0 + i,
        )
        .unwrap();
    }
    let good = s.to_blob().unwrap();
    assert_eq!(Store::load(&good).unwrap().count().unwrap(), 5);

    let sb_len = u64::from_le_bytes(good[8..16].try_into().unwrap()) as usize;
    let rows_at = 24 + sb_len;
    // Each of these lands inside a DIFFERENT bookmark's value.
    for slot in [1usize, 5, 20] {
        let mut bad = good.clone();
        bad[rows_at + slot * 32 + 4] ^= 0xFF;

        // A strict load still refuses the whole database: detection over
        // availability, unchanged.
        let e = Store::load(&bad).unwrap_err();
        assert!(
            matches!(e, StoreError::Db(DbErr::Corrupt { .. })),
            "slot {slot}: {e:?}"
        );
        assert!(e.to_string().contains("salvage mode"));

        // And the advice can now be taken, from bytes, with no
        // filesystem in sight.
        let mut rescued = Store::salvage(&bad).unwrap();
        assert!(rescued.is_degraded(), "slot {slot}");
        let survivors = rescued.list().unwrap();
        assert_eq!(
            survivors.len(),
            4,
            "slot {slot}: one bookmark lost, four readable"
        );
        assert!(rescued.add("https://x.test", "T", &[], T0).is_err());

        // Rebuild, and the result is a healthy store that can be written
        // to and snapshotted straight back out.
        let mut rebuilt = rescued.rebuild_from_salvage().unwrap();
        assert!(!rebuilt.is_degraded());
        assert_eq!(rebuilt.list().unwrap(), survivors);
        rebuilt.add("https://new.test", "New", &[], T0).unwrap();
        let round = rebuilt.to_blob().unwrap();
        assert_eq!(Store::load(&round).unwrap().count().unwrap(), 5);
    }
}
