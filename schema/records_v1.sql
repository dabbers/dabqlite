-- The LEGACY schema (v1): the shape this table had before `value` widened
-- from 8 to 16 bytes. Kept checked in because the migration path
-- (docs/DESIGN.md §4.8) compiles BOTH codecs into the current binary: a
-- new binary migrates old files into its own schema, so it must be able
-- to read every schema it promises to migrate from.
--
-- Field discipline (§4.8): append at the end, never reorder, never
-- remove. v1 -> v2 widened the final fixed-width field, which is an
-- append in slot terms: the old 8 bytes keep their offsets, the new tail
-- is zero-filled by the migration function.

CREATE TABLE records (
    id    BIGINT NOT NULL PRIMARY KEY,
    value BYTEA  NOT NULL -- @fixed(8)
);
