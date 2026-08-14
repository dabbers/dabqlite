//! Test helper: a real second process that tries to open a dabqlite
//! directory. Used by the locking suite to prove the single-writer lock
//! holds across genuine process boundaries, not just within one process.
//!
//! `lockprobe <dir>`       — try to open; print `acquired` or `refused`.
//! `lockprobe <dir> hold`  — open, print `acquired`, then hold the lock
//!                           until killed (for kill-the-holder tests).

fn main() {
    #[cfg(unix)]
    {
        use std::io::Write;

        let mut args = std::env::args().skip(1);
        let dir = args.next().expect("usage: lockprobe <dir> [hold]");
        let mode = args.next().unwrap_or_default();
        match dabqlite_host::PosixStorage::open_dir(std::path::Path::new(&dir)) {
            Ok(_storage) => {
                println!("acquired");
                std::io::stdout().flush().expect("flush");
                if mode == "hold" {
                    // Hold the lock until the parent kills us.
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                }
            }
            Err(e) => {
                println!("refused: {e}");
                std::process::exit(2);
            }
        }
    }
}
