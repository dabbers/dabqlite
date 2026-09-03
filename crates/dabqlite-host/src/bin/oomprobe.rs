//! Test helper: a real process that opens a dabqlite directory and
//! constructs a host with the given capacity, so an oversized capacity
//! declaration under a small address-space limit dies by GENUINE
//! allocator exhaustion — not a simulated one.
//!
//! `oomprobe <dir> <rows>` — open the directory, construct a host with
//! `rows` capacity, open the database, print `opened <n>`, exit 0.
//!
//! The address-space ceiling is imposed from OUTSIDE this process: the
//! oom suite runs it under `sh -c 'ulimit -v <kib> && exec oomprobe ...'`
//! (the shell's `RLIMIT_AS`), so the probe itself needs no FFI and no
//! unsafe code. With a huge `rows` under a small limit, `Engine::new`'s
//! single init-time allocation (docs/DESIGN.md §4.2) fails and Rust's
//! allocator error handler aborts the process — BEFORE the engine has
//! issued one I/O request. The oom suite asserts the death, that the
//! database directory is byte-untouched, and that the file lock died
//! with the process.

fn main() {
    #[cfg(unix)]
    {
        use dabqlite_core::{Capacities, Output};
        use dabqlite_host::{Host, PosixStorage};

        let mut args = std::env::args().skip(1);
        let usage = "usage: oomprobe <dir> <rows>";
        let dir = args.next().expect(usage);
        let rows: u64 = args.next().expect(usage).parse().expect("rows");

        // Real embedder order: lock + handles first, then the one big
        // allocation. An OOM here must leave every file byte alone.
        let storage = PosixStorage::open_dir(std::path::Path::new(&dir)).expect("open dir");
        let mut host = Host::new(Capacities { rows }, storage);
        match host.open().expect("probe") {
            Output::OpenDone { result: Ok(n) } => println!("opened {n}"),
            other => panic!("open: {other:?}"),
        }
    }
}
