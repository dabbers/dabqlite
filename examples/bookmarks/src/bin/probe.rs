//! Hammer the single-writer lock from several threads at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bookmarks::{Store, StoreError};
use dabqlite::{Error as DbErr, PosixStorage};

fn main() {
    let dir = std::env::temp_dir().join("probe-lock-race");
    let _ = std::fs::remove_dir_all(&dir);
    // Create it once so the schema files exist.
    drop(Store::<PosixStorage>::open_with(&dir, 100_000).unwrap());

    let wrote = Arc::new(AtomicU64::new(0));
    let locked = Arc::new(AtomicU64::new(0));
    let other = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for t in 0..8 {
        let dir = dir.clone();
        let (wrote, locked, other) = (wrote.clone(), locked.clone(), other.clone());
        handles.push(std::thread::spawn(move || {
            for i in 0..40 {
                match Store::<PosixStorage>::open_with(&dir, 100_000) {
                    Ok(mut s) => {
                        s.add(
                            &format!("https://t{t}.example.com/{i}"),
                            "x",
                            &["t".to_string()],
                            1,
                        )
                        .unwrap();
                        wrote.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(StoreError::Db(DbErr::Locked { .. })) => {
                        locked.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        other.fetch_add(1, Ordering::Relaxed);
                        eprintln!("UNEXPECTED: {e:?}");
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let mut s = Store::<PosixStorage>::open_with(&dir, 100_000).unwrap();
    println!(
        "wrote={} locked={} other={} final count={}",
        wrote.load(Ordering::Relaxed),
        locked.load(Ordering::Relaxed),
        other.load(Ordering::Relaxed),
        s.count().unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
