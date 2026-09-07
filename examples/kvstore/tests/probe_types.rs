use dabqlite::{Db, Value};

#[test]
fn closure_inference_workaround() {
    let mut db = Db::in_memory().unwrap();
    // A closure can take the unnameable type via inference...
    let mut put = |db: &mut _, id: u64, s: &str| {
        Db::put(db, id, Value::from_text(s).unwrap()).unwrap()
    };
    put(&mut db, 1, "a");
    // ...but the closure is monomorphic and cannot be stored next to the db.
    assert_eq!(db.get(1).unwrap().unwrap().text(), "a");
}
