//! Test helper: a real process that opens a dabqlite directory under a
//! hard address-space limit, so an oversized capacity declaration dies by
//! GENUINE allocator exhaustion — not a simulated one.
//!
//! `oomprobe <dir> <rows> <rlimit_mb>` — set `RLIMIT_AS` to `rlimit_mb`
//! MiB (0 = leave unlimited), open the directory, construct a host with
//! `rows` capacity, open the database, print `opened <n>` and exit 0.
//!
//! With a huge `rows` under a small limit, `Engine::new`'s single
//! init-time allocation (docs/DESIGN.md §4.2) fails and Rust's allocator
//! error handler aborts the process — BEFORE the engine has issued one
//! I/O request. The oom suite asserts the death, that the database
//! directory is byte-untouched, and that the flock died with the process.

fn main() {
    #[cfg(unix)]
    {
        use dabqlite_core::{Capacities, Output};
        use dabqlite_host::{Host, PosixStorage};

        let mut args = std::env::args().skip(1);
        let usage = "usage: oomprobe <dir> <rows> <rlimit_mb>";
        let dir = args.next().expect(usage);
        let rows: u64 = args.next().expect(usage).parse().expect("rows");
        let rlimit_mb: u64 = args.next().expect(usage).parse().expect("rlimit_mb");

        if rlimit_mb > 0 {
            let limit = libc::rlimit {
                rlim_cur: rlimit_mb * 1024 * 1024,
                rlim_max: rlimit_mb * 1024 * 1024,
            };
            // FFI is the only way to setrlimit; confined to this syscall.
            #[allow(unsafe_code)]
            let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
            assert_eq!(rc, 0, "setrlimit failed");
        }

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
