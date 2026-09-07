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
    // v2 rows carry the kind discriminant between the fields and the CRC,
    // so the CRC covers it — a flip there must never be able to turn a
    // deletion back into a record. v3 adds the commit span next to it,
    // under the same checksum, so a flip cannot redraw a commit boundary
    // either. v4 adds the payload length beside those, so a flip cannot
    // lengthen a value into its own padding. v6 widens the span to two
    // bytes, which is what separates the length of a COMMIT from the
    // length of a VALUE — and consumes the row's last padding byte, so
    // every byte of a row is now covered by the checksum rather than by
    // a zero check.
    assert_eq!(schema.format, dabqlite_codegen::CURRENT_ROW_FORMAT);
    assert_eq!(layout.kind_offset, Some(24));
    assert_eq!(layout.span_offset, Some(25));
    assert_eq!(layout.span_width, 2);
    assert_eq!(layout.len_offset, Some(27));
    assert_eq!(layout.crc_offset, 28);
    assert_eq!(
        layout.crc_offset + 4,
        layout.row_size,
        "v6 rows have no padding left: the checksum reaches the end"
    );
    // The length's ceiling is the value column's width, derived rather
    // than written down twice.
    assert_eq!(layout.len_max, Some(dabqlite_core::VALUE_LEN as u8));
    // All three bytes are strictly below the CRC offset, which is what
    // "inside the checksummed region" means. Stated as assertions rather
    // than comments so that moving any of them into the padding fails here.
    assert!(layout.kind_offset.unwrap() < layout.crc_offset);
    assert!(layout.span_offset.unwrap() < layout.crc_offset);
    assert!(layout.len_offset.unwrap() < layout.crc_offset);
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

/// The hash's whole job is "equal iff byte-compatible", and the way that
/// claim dies is a layout change from an UNCHANGED declaration.
///
/// Widen the span field, move the checksum, pad the row differently — the
/// SQL says the same thing, the ruler is different, and every existing
/// file gets read with the wrong one while claiming to match. Nothing
/// fails; rows just mean something else. So the hash covers the derived
/// layout, not only the declaration, and this is that: the same schema
/// with a hand-altered layout must not hash the same.
#[test]
fn a_layout_change_moves_the_hash_even_when_the_declaration_does_not() {
    let schema = parse_schema(&records_sql()).unwrap();
    let base = schema.schema_hash();
    let layout = schema.layout();

    // Every layout field, moved by one, one at a time. Each stands for a
    // real change: a wider span, a wider len, a relocated checksum, a
    // different row size, a different payload ceiling.
    let mut mutated = 0;
    for k in 0..6 {
        let s = parse_schema(&records_sql()).unwrap();
        let mut l = s.layout();
        match k {
            0 => l.kind_offset = l.kind_offset.map(|o| o + 1),
            1 => l.span_offset = l.span_offset.map(|o| o + 1),
            2 => l.len_offset = l.len_offset.map(|o| o + 1),
            3 => l.len_max = l.len_max.map(|m| m - 1),
            4 => l.crc_offset += 1,
            _ => l.row_size += 8,
        }
        assert_ne!(
            (
                l.kind_offset,
                l.span_offset,
                l.len_offset,
                l.len_max,
                l.crc_offset,
                l.row_size
            ),
            (
                layout.kind_offset,
                layout.span_offset,
                layout.len_offset,
                layout.len_max,
                layout.crc_offset,
                layout.row_size
            ),
            "variant {k} changed nothing"
        );
        assert_ne!(
            s.hash_with_layout(&l),
            base,
            "variant {k}: a different layout hashed the same, so a file \
             written with one ruler would be read with the other"
        );
        mutated += 1;
    }
    assert_eq!(mutated, 6);
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

#[test]
fn index_annotations_shape_operations_not_layout() {
    // The load-bearing property: @index(trigram) must NOT change the
    // schema hash. Indexes are derived state — adding one must never
    // brick existing files behind the version gate or force a migration.
    let plain = "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n \
                 value BYTEA NOT NULL -- @fixed(16)\n);";
    let indexed = "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n \
                   value BYTEA NOT NULL -- @fixed(16) @index(trigram)\n);";
    let plain = parse_schema(plain).unwrap();
    let indexed = parse_schema(indexed).unwrap();
    assert_eq!(plain.schema_hash(), indexed.schema_hash());
    assert!(!plain.columns[1].trigram);
    assert!(indexed.columns[1].trigram);
}

#[test]
fn index_annotation_rejections_are_loud() {
    for (sql, msg) in [
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY -- @index(trigram)\n);",
            "applies to BYTEA",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n \
             value BYTEA NOT NULL -- @fixed(16) @index(hnsw)\n);",
            "not a v1 index method",
        ),
        (
            "CREATE TABLE records (\n id BIGINT NOT NULL PRIMARY KEY,\n \
             value BYTEA NOT NULL -- @fixed(16) @index(trigram\n);",
            "missing its closing",
        ),
    ] {
        let e = parse_schema(sql).expect_err(sql);
        assert!(e.msg.contains(msg), "{sql:?}: {e}");
    }
}
