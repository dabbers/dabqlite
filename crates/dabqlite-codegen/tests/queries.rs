//! Declared-query compiler tests. The "query planner" in this design runs
//! at build time (docs/DESIGN.md §4.3): shapes are validated here, plans
//! are fixed in the generated code, and nothing unvalidated can reach the
//! engine. So this is where planning gets stress-tested: every accepted
//! shape pinned, every rejection path exercised with a specific message.

use dabqlite_codegen::{parse_queries, parse_schema, query_space_hash, QueryKind, QueryShape};

fn schema() -> dabqlite_codegen::Schema {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schema/records.sql"
    ))
    .expect("schema");
    parse_schema(&sql).expect("records schema parses")
}

fn queries_sql() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schema/queries.sql"
    ))
    .expect("queries")
}

#[test]
fn declared_queries_parse_to_the_expected_operation_space() {
    let queries = parse_queries(&queries_sql(), &schema()).expect("queries parse");
    assert_eq!(queries.len(), 2);
    let get = queries
        .iter()
        .find(|q| q.name == "get_record")
        .expect("get");
    assert_eq!(get.kind, QueryKind::One);
    assert_eq!(get.shape, QueryShape::SelectByPk);
    let ins = queries
        .iter()
        .find(|q| q.name == "insert_record")
        .expect("insert");
    assert_eq!(ins.kind, QueryKind::Exec);
    assert_eq!(ins.shape, QueryShape::InsertRow);

    // Pin the generated constants against the live core: the compiled
    // client surface and this file must agree or the build is lying.
    assert_eq!(
        dabqlite_core::generated::queries::OPERATIONS,
        &["get_record", "insert_record"]
    );
    assert_eq!(
        query_space_hash(&queries),
        dabqlite_core::generated::queries::QUERY_SPACE_HASH,
        "schema/queries.sql drifted from the generated operation space"
    );
}

#[test]
fn query_space_hash_is_sensitive_and_stable() {
    let s = schema();
    let base = query_space_hash(&parse_queries(&queries_sql(), &s).unwrap());

    // Removing an operation changes the hash.
    let only_get = "-- name: get_record :one\nSELECT id, value FROM records WHERE id = $1;\n";
    assert_ne!(
        base,
        query_space_hash(&parse_queries(only_get, &s).unwrap())
    );

    // Renaming an operation changes the hash.
    let renamed = queries_sql().replace("get_record", "fetch_record");
    assert_ne!(
        base,
        query_space_hash(&parse_queries(&renamed, &s).unwrap())
    );

    // Formatting, comments, and declaration order do not.
    let reordered = "\
        -- a comment\n\
        -- name: insert_record :exec\n\
        INSERT   INTO records (id,value)\n\
        VALUES ($1, $2);\n\n\
        -- name: get_record :one\n\
        SELECT id,value FROM records WHERE id=$1;\n";
    assert_eq!(
        base,
        query_space_hash(&parse_queries(reordered, &s).unwrap())
    );
}

#[test]
fn every_rejection_path_is_loud_and_specific() {
    let s = schema();
    let cases: &[(&str, &str)] = &[
        // Statement without annotation.
        (
            "SELECT id, value FROM records WHERE id = $1;",
            "without a '-- name:'",
        ),
        // Bad kind.
        (
            "-- name: q :many\nSELECT id, value FROM records WHERE id = $1;",
            "unknown result kind",
        ),
        // Missing kind entirely.
        (
            "-- name: q\nSELECT id, value FROM records WHERE id = $1;",
            "expected '-- name:",
        ),
        // Duplicate names.
        (
            "-- name: q :one\nSELECT id, value FROM records WHERE id = $1;\n\
             -- name: q :one\nSELECT id, value FROM records WHERE id = $1;",
            "duplicate query name",
        ),
        // Unterminated statement.
        (
            "-- name: q :one\nSELECT id, value FROM records WHERE id = $1",
            "no terminating ';'",
        ),
        // Wrong table.
        (
            "-- name: q :one\nSELECT id, value FROM users WHERE id = $1;",
            "must be exactly",
        ),
        // Wrong column set (projection narrowing not in v1).
        (
            "-- name: q :one\nSELECT id FROM records WHERE id = $1;",
            "must be exactly",
        ),
        // Wrong column order.
        (
            "-- name: q :one\nSELECT value, id FROM records WHERE id = $1;",
            "must be exactly",
        ),
        // Non-PK predicate: would need a scan; no scans exist in v1.
        (
            "-- name: q :one\nSELECT id, value FROM records WHERE value = $1;",
            "must be exactly",
        ),
        // Wrong param index.
        (
            "-- name: q :one\nSELECT id, value FROM records WHERE id = $2;",
            "must be exactly",
        ),
        // SELECT must be :one.
        (
            "-- name: q :exec\nSELECT id, value FROM records WHERE id = $1;",
            "must be ':one'",
        ),
        // INSERT must be :exec.
        (
            "-- name: q :one\nINSERT INTO records (id, value) VALUES ($1, $2);",
            "must be ':exec'",
        ),
        // INSERT with swapped params.
        (
            "-- name: q :exec\nINSERT INTO records (id, value) VALUES ($2, $1);",
            "must be exactly",
        ),
        // INSERT with missing column.
        (
            "-- name: q :exec\nINSERT INTO records (id) VALUES ($1);",
            "must be exactly",
        ),
        // Unsupported verbs: no DELETE/UPDATE paths exist in the engine yet,
        // so the compiler must refuse rather than pretend.
        (
            "-- name: q :exec\nDELETE FROM records WHERE id = $1;",
            "unsupported statement",
        ),
        (
            "-- name: q :exec\nUPDATE records SET value = $2 WHERE id = $1;",
            "unsupported statement",
        ),
        // Empty operation space.
        ("-- just comments\n", "may not be empty"),
    ];
    for (sql, needle) in cases {
        match parse_queries(sql, &s) {
            Err(e) => assert!(
                e.msg.contains(needle),
                "error for {sql:?} should mention {needle:?}, got: {e}"
            ),
            Ok(q) => panic!("{sql:?} should not parse, got {q:?}"),
        }
    }
}

#[test]
fn golden_generated_queries_file_is_current() {
    let s = schema();
    let queries = parse_queries(&queries_sql(), &s).unwrap();
    let emitted = dabqlite_codegen::emit_queries_rust(&s, &queries, "schema/queries.sql");
    let checked_in = include_str!("../../dabqlite-core/src/generated/queries.rs");
    assert_eq!(
        emitted, checked_in,
        "generated queries.rs is stale; regenerate with `cargo run -p \
         dabqlite-codegen -- schema/records.sql \
         crates/dabqlite-core/src/generated/records.rs schema/queries.sql \
         crates/dabqlite-core/src/generated/queries.rs`"
    );
}
