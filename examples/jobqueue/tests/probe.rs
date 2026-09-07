use dabqlite::{Db, Error, Value};

#[test]
fn probe_full_database_behaviour() {
    let mut db = Db::in_memory_with(4).unwrap();
    for i in 0..4u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    println!("stats after fill: {:?}", db.stats());
    println!("insert at full: {:?}", db.insert(9, Value::from_text("x").unwrap()));
    println!("update at full: {:?}", db.update(0, Value::from_text("y").unwrap()));
    println!("delete at full: {:?}", db.delete(0));
    println!("stats: {:?}", db.stats());

    // one slot free
    let mut db = Db::in_memory_with(5).unwrap();
    for i in 0..4u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    println!("--- one free slot, 4 live");
    println!("delete: {:?}", db.delete(0));
    println!("stats: {:?} len={}", db.stats(), db.len());
    println!("delete again: {:?}", db.delete(1));
    let _ = Error::NotFound { id: 0 };
}
