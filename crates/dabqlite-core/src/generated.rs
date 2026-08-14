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

pub mod records_v1 {
    //! Generated from `schema/records_v1.sql` — the LEGACY schema, kept
    //! compiled-in because the migration path (docs/DESIGN.md §4.8) runs
    //! inside the NEW binary: it must read every schema it promises to
    //! migrate from. Never wired into the engine; only `migration` uses it.
    include!("generated/records_v1.rs");
}

pub mod queries {
    //! Generated from `schema/queries.sql`: the complete, finite operation
    //! space (docs/DESIGN.md §4.3). There is deliberately no runtime query
    //! planner — plans are fixed here, at build time, which is what keeps
    //! the operation space enumerable and exhaustive simulation affordable.
    include!("generated/queries.rs");
}
