//! Protocol type sizes.
//!
//! In its own file on purpose: `allocation.rs` counts heap allocations
//! through a global counter, so a second test running beside it in the
//! same binary makes that count meaningless.

/// The protocol's value types stay small, because every one of them is
/// returned BY VALUE on every tick — including the I/O ticks of a commit,
/// where a fat `Output` would be a memcpy per step of the write path.
///
/// `Output` is as large as its largest variant, so the interesting number
/// is not any single type but the maximum. A read window was one row wide
/// and reading 2 KiB cost 128 round trips; widening it to sixteen slots
/// cut that to eight and cost nothing, because a find page was already
/// bigger. This pins that: it stays a free change only while the window
/// does not become the largest variant.
#[test]
fn the_protocol_types_stay_small_enough_to_return_by_value() {
    use core::mem::size_of;
    use dabqlite_core::{FindPage, Input, Output, RangePage, ValueWindow};

    let window = size_of::<ValueWindow>();
    let pages = size_of::<RangePage>().max(size_of::<FindPage>());
    assert!(
        window <= pages,
        "a read window ({window} bytes) is now larger than a result page \
         ({pages}) — widening it stopped being free and every tick of the \
         write path pays for it"
    );
    // Generous ceilings, an order of magnitude below anything that would
    // matter, and low enough that a careless `Vec`-sized field fails them.
    assert!(size_of::<Output>() <= 1024, "{}", size_of::<Output>());
    assert!(size_of::<Input>() <= 128, "{}", size_of::<Input>());
    assert_eq!(
        size_of::<Output>(),
        pages,
        "the largest variant should be a result page"
    );
}
