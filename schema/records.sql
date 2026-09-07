-- The dabqlite schema: Postgres DDL plus annotations (docs/DESIGN.md §4.7).
-- This file is the single source of truth for record layout: field offsets,
-- row size, the wire codec, and SCHEMA_HASH are all derived from it by
-- dabqlite-codegen. An old binary opening a file written under a different
-- schema fails at startup instead of misreading offsets (§4.8).
--
-- Row format: v3 (the generator's current default, so this file does not
-- pin one). v2 added the KIND byte that distinguishes a record from a
-- tombstone; v3 added the SPAN byte that says how many further rows were
-- written in the same commit, which is what lets a batch commit
-- all-or-nothing and still leave recovery able to tell an interrupted
-- batch from an acknowledged commit that storage rolled back. Both bytes
-- live inside the checksummed region. Bumping the format changes
-- SCHEMA_HASH, so a binary that predates it refuses the file at open
-- rather than misreading offsets.
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
