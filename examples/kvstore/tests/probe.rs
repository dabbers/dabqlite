use dabqlite::{Db, Error, Value};

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("kv-probe-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn probe_capacity_persistence() {
    let dir = scratch("cap");
    {
        let mut db = Db::open_with(&dir, 100).unwrap();
        for i in 0..50u64 {
            db.insert(i, Value::from_text("x").unwrap()).unwrap();
        }
        eprintln!("created with 100: stats={:?}", db.stats());
    }
    {
        let db = Db::open(&dir).unwrap();
        eprintln!("reopened with default: stats={:?}", db.stats());
    }
    {
        let r = Db::open_with(&dir, 10);
        eprintln!("reopen with 10 (below data): {:?}", r.map(|d| d.stats()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn probe_full_then_delete() {
    let mut db = Db::in_memory_with(8).unwrap();
    for i in 0..8u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    eprintln!("full stats={:?}", db.stats());
    eprintln!("insert at full: {:?}", db.insert(9, Value::from_text("x").unwrap()));
    eprintln!("delete at full: {:?}", db.delete(0));
    eprintln!("update at full: {:?}", db.update(1, Value::from_text("y").unwrap()));
    eprintln!("after: stats={:?}", db.stats());
    eprintln!("all still readable: {}", db.all().unwrap().len());
    let c = db.compact_to_memory();
    eprintln!("compact_to_memory: {:?}", c.map(|mut d| (d.stats(), d.all().unwrap().len())));
}

#[test]
fn probe_find_semantics() {
    let mut db = Db::in_memory().unwrap();
    db.insert(1, Value::from_text("hello world!!").unwrap()).unwrap();
    db.insert(2, Value::from_bytes(&[0u8; 16]).unwrap()).unwrap();
    db.insert(3, Value::from_bytes(b"ab\0cd").unwrap()).unwrap();
    eprintln!("find 'world' -> {:?}", db.find_text("world"));
    eprintln!("find '' -> {:?}", db.find_text("").map(|v| v.len()));
    eprintln!("find zeros -> {:?}", db.find(&[0u8; 4]).map(|v| v.len()));
    eprintln!("row3 as_bytes={:?} raw={:?}", db.get(3).unwrap().unwrap().as_bytes(), db.get(3).unwrap().unwrap().raw());
    // 16-byte exact needle
    eprintln!("find full16 -> {:?}", db.find(&[b'z'; 16]).map(|v| v.len()));
}

#[test]
fn probe_id_extremes() {
    let mut db = Db::in_memory().unwrap();
    eprintln!("insert id 0: {:?}", db.insert(0, Value::from_text("zero").unwrap()));
    eprintln!("insert id u64::MAX: {:?}", db.insert(u64::MAX, Value::from_text("max").unwrap()));
    eprintln!("all: {:?}", db.all().unwrap().iter().map(|r| r.0).collect::<Vec<_>>());
    eprintln!("range(u64::MAX,u64::MAX): {:?}", db.range(u64::MAX, u64::MAX).unwrap().len());
}

#[test]
fn probe_second_open_is_refused() {
    let dir = scratch("lock");
    let db = Db::open(&dir).unwrap();
    let second = Db::open(&dir);
    eprintln!("second open: {:?}", second.err());
    drop(db);
    eprintln!("after drop, open again: {:?}", Db::open(&dir).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn probe_error_is_not_matchable_by_kind() {
    let e: Error = Error::NotFound { id: 1 };
    eprintln!("{e}");
}
