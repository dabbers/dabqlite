//! Schema-compiled code, produced by `dabqlite-codegen` from the schema
//! files under `schema/`. Checked in, golden-tested, and drift-checked in
//! CI; never edited by hand. This module is load-bearing: `layout` derives
//! its row codec and `SCHEMA_HASH` from here, so the schema file is the
//! single source of truth for the on-disk format (docs/DESIGN.md §4.2,
//! §4.8).

pub mod records {
    //! Generated from `schema/records.sql`.
    include!("generated/records.rs");
}
