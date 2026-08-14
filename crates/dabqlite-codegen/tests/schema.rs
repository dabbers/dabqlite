//! Schema compiler tests: parse errors must be loud and specific, and the
//! records schema must pin the exact layout the engine (and its entire
//! fault matrix) was validated against.

use dabqlite_codegen::{parse_schema, ColType};

fn records_sql() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schema/records.sql"
    ))
    .expect("schema/records.sql")
}

#[test]
fn records_schema_matches_the_engine_exactly() {
    let schema = parse_schema(&records_sql()).expect("records.sql must parse");
    assert_eq!(schema.table, "records");
    assert_eq!(schema.columns.len(), 2);
    assert_eq!(schema.columns[0].name, "id");
    assert_eq!(schema.columns[0].ty, ColType::BigInt);
    assert!(schema.columns[0].primary_key);
    assert_eq!(schema.columns[1].name, "value");
    assert_eq!(schema.columns[1].ty, ColType::FixedBytes(16));

    let layout = schema.layout();
    assert_eq!(layout.field_offsets, vec![0, 8]);
    assert_eq!(layout.crc_offset, 24);
    assert_eq!(layout.row_size, dabqlite_core::ROW_SIZE);
    assert_eq!(
        schema.columns[1].ty.width(),
        dabqlite_core::VALUE_LEN,
        "value width diverged from the engine"
    );

    // THE pin: the engine's SCHEMA_HASH is the derived value. If the schema
    // file changes in any layout-affecting way, this fails until the core
    // constant (and any migration story) is consciously updated.
    assert_eq!(
        schema.schema_hash(),
        dabqlite_core::SCHEMA_HASH,
        "schema/records.sql no longer matches dabqlite_core::SCHEMA_HASH: \
         schema drift requires a conscious core update"
    );
}

#[test]
fn hash_is_sensitive_to_every_layout_input() {
    let base = parse_schema(&records_sql()).unwrap().schema_hash();
    let variants = [
        // renamed table
        "CREATE TABLE record2 (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL -- @fixed(16)\n);",
        // renamed column
        "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n val BYTEA NOT NULL -- @fixed(16)\n);",
        // widened field
        "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL -- @fixed(32)\n);",
        // extra column
        "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL, -- @fixed(16)\n extra BIGINT NOT NULL\n);",
    ];
    for v in variants {
        let h = parse_schema(v).expect("variant parses").schema_hash();
        assert_ne!(h, base, "hash failed to distinguish variant: {v}");
    }
    // And insensitive to formatting-only changes (comments, whitespace).
    let reformatted = "\n\n-- a comment\nCREATE TABLE records (\n    id     BIGINT   NOT NULL PRIMARY KEY,\n    value  BYTEA    NOT NULL -- @fixed(16)\n);\n";
    assert_eq!(
        parse_schema(reformatted).unwrap().schema_hash(),
        base,
        "formatting must not change the schema hash"
    );
}

#[test]
fn parse_errors_are_loud_and_specific() {
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TABLE records (\n id BIGINT PRIMARY KEY\n);",
            "NOT NULL",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL\n);",
            "@fixed",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value TEXT NOT NULL\n);",
            "unsupported type",
        ),
        (
            "CREATE TABLE records (\n value BYTEA NOT NULL, -- @fixed(16)\n id BIGINT NOT NULL PRIMARY KEY\n);",
            "first column",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL\n);",
            "exactly one PRIMARY KEY",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n id BIGINT NOT NULL\n);",
            "duplicate column",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL -- @fixed(0)\n);",
            "out of range",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n value BYTEA NOT NULL -- @sized(16)\n);",
            "unrecognized annotation",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY\n);\nDROP TABLE records;",
            "unexpected content",
        ),
    ];
    for (sql, needle) in cases {
        match parse_schema(sql) {
            Err(e) => assert!(
                e.msg.contains(needle),
                "error for {sql:?} should mention {needle:?}, got: {e}"
            ),
            Ok(s) => panic!("{sql:?} should not parse, got {s:?}"),
        }
    }
}

#[test]
fn golden_generated_file_is_current() {
    // The checked-in generated file (load-bearing inside dabqlite-core)
    // must be exactly what the generator emits today. CI also regenerates
    // and diffs; this is the local guard.
    let schema = parse_schema(&records_sql()).unwrap();
    let emitted = dabqlite_codegen::emit_rust(&schema, "schema/records.sql");
    let checked_in = include_str!("../../dabqlite-core/src/generated/records.rs");
    assert_eq!(
        emitted, checked_in,
        "generated records.rs is stale; regenerate with \
         `cargo run -p dabqlite-codegen -- schema/records.sql \
         crates/dabqlite-core/src/generated/records.rs`"
    );
}
