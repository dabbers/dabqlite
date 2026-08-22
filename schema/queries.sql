-- Declared queries (docs/DESIGN.md §4.3): real SQL in, typed functions out,
-- sqlc-style. This file IS the operation space — the production core links
-- no parser and no planner, so nothing outside this list can ever be asked
-- of the engine. That finiteness is what keeps exhaustive simulation
-- affordable, and the generated OPERATIONS manifest makes it checkable.
--
-- v1 shapes, enforced loudly by the generator:
--   * SELECT of all columns, in schema order, by primary-key equality (:one)
--   * INSERT of all columns, in schema order (:exec)

-- name: get_record :one
SELECT id, value FROM records WHERE id = $1;

-- name: insert_record :exec
INSERT INTO records (id, value) VALUES ($1, $2);

-- name: list_records :many
SELECT id, value FROM records WHERE id >= $1 AND id <= $2;

-- name: find_records :find
-- Substring search over value bytes: trigram-accelerated, verification-
-- exact, pages in insertion order (docs/DESIGN.md 4.6).
SELECT id, value FROM records WHERE value LIKE $1;
