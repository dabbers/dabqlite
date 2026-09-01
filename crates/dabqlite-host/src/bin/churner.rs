//! Test helper: a real writer process that commits a known workload, for
//! pause-the-writer tests (SIGSTOP mid-work, resume, verify).
//!
//! `churner <dir> <n>` — open the directory, print `ready`, wait for a
//! line on stdin (the go signal), insert rows 0..n with value bytes
//! `(i * 37) % 251`, print `done <n>`, then wait for one more stdin line
//! before exiting.
//!
//! The handshakes exist so the parent test can arrange its signals around
//! a writer that is DEFINITELY mid-workload (not still linking) and that
//! DEFINITELY still holds the single-writer lock for every freeze cycle,
//! however fast the disk finished the workload.

fn main() {
    #[cfg(unix)]
    {
        use std::io::{BufRead, Write};

        use dabqlite_core::{Capacities, Output, VALUE_LEN};
        use dabqlite_host::{Host, PosixStorage};

        let mut args = std::env::args().skip(1);
        let usage = "usage: churner <dir> <n>";
        let dir = args.next().expect(usage);
        let n: u64 = args.next().expect(usage).parse().expect("n");

        let storage = PosixStorage::open_dir(std::path::Path::new(&dir)).expect("open dir");
        let mut host = Host::new(Capacities { rows: n }, storage);
        match host.open().expect("probe") {
            Output::OpenDone { result: Ok(0) } => {}
            other => panic!("open: {other:?}"),
        }

        println!("ready");
        std::io::stdout().flush().expect("flush");
        let mut stdin = std::io::stdin().lock();
        let mut line = String::new();
        stdin.read_line(&mut line).expect("go");

        for i in 0..n {
            let value = [(i as u8).wrapping_mul(37) % 251; VALUE_LEN];
            match host.insert(i, value) {
                Output::InsertDone { result: Ok(()), .. } => {}
                other => panic!("insert {i}: {other:?}"),
            }
        }
        println!("done {n}");
        std::io::stdout().flush().expect("flush");
        // Keep the storage (and its flock) alive until dismissed.
        line.clear();
        stdin.read_line(&mut line).expect("exit signal");
        drop(host);
    }
}
