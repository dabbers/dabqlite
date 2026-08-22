-- The dabqlite schema: Postgres DDL plus annotations (docs/DESIGN.md §4.7).
-- This file is the single source of truth for record layout: field offsets,
-- row size, the wire codec, and SCHEMA_HASH are all derived from it by
-- dabqlite-codegen. An old binary opening a file written under a different
-- schema fails at startup instead of misreading offsets (§4.8).
--
-- v1 restrictions, enforced loudly by the generator:
--   * every column NOT NULL (there is no null bitmap),
--   * the first column is the BIGINT primary key,
--   * BYTEA columns carry a @fixed(n) annotation (fixed-width slots; varlen
--     spill arrives with the blob zone integration, §4.5).

CREATE TABLE records (
    id    BIGINT NOT NULL PRIMARY KEY,
    value BYTEA  NOT NULL -- @fixed(16) @index(trigram)
);
