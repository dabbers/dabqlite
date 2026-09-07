//! Argument parsing and rendering. No database access lives here.

use std::io::{Read, Write};
use std::path::PathBuf;

use crate::exec::{execute, Command, Outcome, Report};
use crate::store::Entry;
use crate::{now_secs, Config, KvError};

pub const USAGE: &str = "\
kv — a durable key/value and session store on dabqlite

USAGE:
    kv [--db DIR] [--rows N] <command> [args]

GLOBAL OPTIONS:
    --db DIR     database directory (default: $KV_DB, else ./kvdata)
    --rows N     row-slot capacity; the database remembers its own
    -h, --help   this text
    --version    version

COMMANDS:
    set <key> <value> [--ttl SECS]   store a value; `-` reads it from stdin
    get <key> [--raw]                print a value (--raw: no trailing newline)
    del <key>                        remove a key
    list [--prefix P] [--values]     list keys in order
    search <text> [--keys]           substring search over values (and keys)
    info <key>                       size, expiry and record placement
    stats                            capacity, dead weight, recovery report
    purge                            retire expired keys
    compact                          rebuild, dropping dead rows
    backup <file>                    write a snapshot blob
    restore <file>                   load a snapshot into an empty --db
    rescue <dir>                     salvage a damaged database into <dir>

EXIT CODES:
    0 success   1 error   2 no such key
";

/// Parse and run. Returns the process exit code.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match parse(args) {
        Ok(None) => {
            let _ = out.write_all(USAGE.as_bytes());
            0
        }
        Ok(Some((cfg, cmd))) => match execute(&cfg, &cmd, err) {
            Ok(outcome) => render(&outcome, out, err),
            Err(e) => {
                let _ = writeln!(err, "kv: {e}");
                1
            }
        },
        Err(e) => {
            let _ = writeln!(err, "kv: {e}");
            let _ = writeln!(err, "try `kv --help`");
            1
        }
    }
}

fn usage<T>(msg: impl Into<String>) -> Result<T, KvError> {
    Err(KvError::Usage(msg.into()))
}

/// `Ok(None)` means "printed help".
pub fn parse(args: &[String]) -> Result<Option<(Config, Command)>, KvError> {
    let mut dir: Option<PathBuf> = None;
    let mut rows: Option<u64> = None;
    let mut rest: Vec<String> = Vec::new();

    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => {
                dir =
                    Some(PathBuf::from(it.next().ok_or_else(|| {
                        KvError::Usage("--db needs a directory".into())
                    })?))
            }
            "--rows" => {
                let v = it
                    .next()
                    .ok_or_else(|| KvError::Usage("--rows needs a number".into()))?;
                rows = Some(
                    v.parse::<u64>()
                        .map_err(|_| KvError::Usage(format!("--rows: {v:?} is not a number")))?,
                );
            }
            "-h" | "--help" | "help" => return Ok(None),
            "--version" => {
                return usage(concat!("kv ", env!("CARGO_PKG_VERSION")));
            }
            _ => rest.push(a.clone()),
        }
    }

    if rest.is_empty() {
        return Ok(None);
    }

    let mut cfg = Config::new(dir.unwrap_or_else(|| {
        std::env::var_os("KV_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("kvdata"))
    }));
    if let Some(r) = rows {
        cfg = cfg.with_rows(r);
    }

    let cmd = parse_command(&rest)?;
    Ok(Some((cfg, cmd)))
}

fn flag(rest: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = rest.iter().position(|a| a == name) {
        rest.remove(i);
        true
    } else {
        false
    }
}

fn opt(rest: &mut Vec<String>, name: &str) -> Result<Option<String>, KvError> {
    match rest.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => {
            rest.remove(i);
            if i >= rest.len() {
                return usage(format!("{name} needs a value"));
            }
            Ok(Some(rest.remove(i)))
        }
    }
}

fn parse_command(rest: &[String]) -> Result<Command, KvError> {
    let mut rest = rest.to_vec();
    let verb = rest.remove(0);
    let cmd = match verb.as_str() {
        "set" => {
            let ttl = match opt(&mut rest, "--ttl")? {
                Some(v) => Some(
                    v.parse::<u64>()
                        .map_err(|_| KvError::Usage(format!("--ttl: {v:?} is not a number")))?,
                ),
                None => None,
            };
            if rest.len() != 2 {
                return usage("set takes <key> <value>; use `-` to read the value from stdin");
            }
            let key = rest[0].clone();
            let value = if rest[1] == "-" {
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .map_err(|err| KvError::Io {
                        what: "reading the value from stdin".into(),
                        err,
                    })?;
                buf
            } else {
                rest[1].clone().into_bytes()
            };
            Command::Set { key, value, ttl }
        }
        "get" => {
            let raw = flag(&mut rest, "--raw");
            if rest.len() != 1 {
                return usage("get takes exactly one key");
            }
            Command::Get {
                key: rest[0].clone(),
                raw,
            }
        }
        "del" | "rm" | "delete" => {
            if rest.len() != 1 {
                return usage("del takes exactly one key");
            }
            Command::Del {
                key: rest[0].clone(),
            }
        }
        "list" | "ls" => {
            let values = flag(&mut rest, "--values");
            let prefix = opt(&mut rest, "--prefix")?;
            if !rest.is_empty() {
                return usage(format!("list does not take {:?}", rest[0]));
            }
            Command::List { prefix, values }
        }
        "search" | "grep" => {
            let keys = flag(&mut rest, "--keys");
            if rest.len() != 1 {
                return usage("search takes exactly one substring");
            }
            Command::Search {
                needle: rest[0].clone().into_bytes(),
                keys,
            }
        }
        "info" => {
            if rest.len() != 1 {
                return usage("info takes exactly one key");
            }
            Command::Info {
                key: rest[0].clone(),
            }
        }
        "stats" => Command::Stats,
        "purge" => Command::Purge,
        "compact" => Command::Compact,
        "backup" => {
            if rest.len() != 1 {
                return usage("backup takes a file to write");
            }
            Command::Backup {
                file: PathBuf::from(&rest[0]),
            }
        }
        "restore" => {
            if rest.len() != 1 {
                return usage("restore takes a snapshot file to read");
            }
            Command::Restore {
                file: PathBuf::from(&rest[0]),
            }
        }
        "rescue" => {
            if rest.len() != 1 {
                return usage("rescue takes a destination directory");
            }
            Command::Rescue {
                dest: PathBuf::from(&rest[0]),
            }
        }
        other => return usage(format!("unknown command {other:?}")),
    };
    Ok(cmd)
}

fn render(outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match outcome {
        Outcome::Stored {
            key,
            replaced,
            rows,
        } => {
            let what = if *replaced { "replaced" } else { "stored" };
            let _ = writeln!(
                out,
                "{what} {} ({rows} row{})",
                show(key.as_bytes()),
                plural(*rows)
            );
        }
        Outcome::Value { value, raw } => {
            let _ = out.write_all(value);
            if !raw {
                let _ = out.write_all(b"\n");
            }
        }
        Outcome::Missing { key } => {
            let _ = writeln!(err, "kv: no such key: {}", show(key.as_bytes()));
            return 2;
        }
        Outcome::Deleted { key } => {
            let _ = writeln!(out, "deleted {}", show(key.as_bytes()));
        }
        Outcome::Entries { entries, values } => {
            for e in entries {
                if *values {
                    let _ = writeln!(out, "{}\t{}", show(e.key.as_bytes()), show(&e.value));
                } else {
                    let _ = writeln!(out, "{}", show(e.key.as_bytes()));
                }
            }
        }
        Outcome::Info(e) => {
            let _ = writeln!(out, "key      {}", show(e.key.as_bytes()));
            let _ = writeln!(out, "bytes    {}", e.value.len());
            let _ = writeln!(out, "row id   {}", e.id);
            match e.expires_at {
                0 => {
                    let _ = writeln!(out, "expires  never");
                }
                t => {
                    let left = t.saturating_sub(now_secs());
                    let _ = writeln!(out, "expires  in {left}s (unix {t})");
                }
            }
        }
        Outcome::Report(r) => render_report(r, out),
        Outcome::Purged(keys) => {
            let _ = writeln!(
                out,
                "purged {} expired key{}",
                keys.len(),
                plural(keys.len() as u64)
            );
            for k in keys {
                let _ = writeln!(out, "  {}", show(k.as_bytes()));
            }
        }
        Outcome::Compacted {
            before,
            after,
            keys,
        } => {
            let _ = writeln!(
                out,
                "compacted {keys} key{}: {} row slots -> {} (of {} capacity), \
                 dead {} -> {}",
                plural(*keys as u64),
                before.slots,
                after.slots,
                after.capacity,
                before.dead,
                after.dead
            );
        }
        Outcome::BackedUp { file, bytes } => {
            let _ = writeln!(out, "wrote {} ({bytes} bytes)", file.display());
        }
        Outcome::Restored { file, keys } => {
            let _ = writeln!(
                out,
                "restored {keys} key{} from {}",
                plural(*keys as u64),
                file.display()
            );
        }
        Outcome::Rescued {
            dest,
            keys,
            quarantined,
            unreadable,
        } => {
            let _ = writeln!(
                out,
                "rescued {keys} key{} into {} ({quarantined} row(s) quarantined by \
                 dabqlite, {unreadable} record(s) unreadable)",
                plural(*keys as u64),
                dest.display()
            );
        }
    }
    0
}

fn render_report(r: &Report, out: &mut dyn Write) {
    let s = r.stats;
    let c = r.census;
    let _ = writeln!(out, "database    {}", r.dir.display());
    let _ = writeln!(out, "keys        {}", c.live);
    let _ = writeln!(out, "expired     {} (run `kv purge`)", c.expired);
    let _ = writeln!(out, "tombstones  {}", c.tombstones);
    let _ = writeln!(
        out,
        "row slots   {} / {} ({:.1}% full)",
        s.slots,
        s.capacity,
        s.fill() * 100.0
    );
    let _ = writeln!(out, "live rows   {}", s.live);
    let _ = writeln!(out, "dead rows   {} (reclaim with `kv compact`)", s.dead);
    let _ = writeln!(out, "max value   {} bytes per key+value", r.max_payload);
    let _ = writeln!(
        out,
        "recovery    {} row(s), {} orphan(s), rollback_evidence={}",
        r.recovery.row_count, r.recovery.orphan_valid_rows, r.recovery.rollback_evidence
    );
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Render bytes for a terminal without letting a value forge a line.
pub fn show(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
                    c => out.push(c),
                }
            }
            out
        }
        Err(_) => bytes.iter().map(|b| format!("\\x{b:02x}")).collect(),
    }
}

/// Convenience for `Entry` lists in tests.
pub fn keys_of(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.key.clone()).collect()
}
