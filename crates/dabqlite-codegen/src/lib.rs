//! # dabqlite-codegen
//!
//! The schema compiler (docs/DESIGN.md §9 step 3): Postgres DDL plus
//! annotations in, record layout and a typed Rust codec out. Layout is
//! fixed at compile time (§4.2) — field offsets, row width, and the schema
//! hash all derive from the schema file, so the schema is the single
//! source of truth and a drifted binary fails at open instead of
//! misreading offsets (§4.8).
//!
//! The parser is hand-rolled for a deliberately small DDL subset. This is a
//! build-time tool: it may allocate and parse freely (unlike the production
//! core, which links no parser at all, §4.3). Parse errors carry the line
//! number and read like documentation — a schema author's first contact
//! with the project is often an error message.

use std::fmt;

/// Column types supported in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    /// 8 bytes, little-endian. The primary key must be one of these.
    BigInt,
    /// Fixed-width byte string of exactly `n` bytes (`BYTEA -- @fixed(n)`).
    FixedBytes(u32),
}

impl ColType {
    pub fn width(self) -> usize {
        match self {
            ColType::BigInt => 8,
            ColType::FixedBytes(n) => n as usize,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub table: String,
    pub columns: Vec<Column>,
}

/// Computed record layout: sequential field offsets, then the CRC, then
/// zero padding to an 8-byte multiple. Every byte of the row is covered:
/// fields and CRC by the checksum, padding by the zero check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub field_offsets: Vec<usize>,
    pub crc_offset: usize,
    pub row_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "schema line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for ParseError {}

fn err(line: usize, msg: impl Into<String>) -> ParseError {
    ParseError {
        line,
        msg: msg.into(),
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parse the supported DDL subset. See `schema/records.sql` for the shape.
pub fn parse_schema(sql: &str) -> Result<Schema, ParseError> {
    let mut table: Option<String> = None;
    let mut columns: Vec<Column> = Vec::new();
    let mut in_columns = false;
    let mut closed = false;

    for (idx, raw) in sql.lines().enumerate() {
        let lineno = idx + 1;
        let (code, comment) = match raw.split_once("--") {
            Some((c, m)) => (c.trim(), m.trim()),
            None => (raw.trim(), ""),
        };
        if code.is_empty() {
            continue;
        }
        if closed {
            return Err(err(
                lineno,
                format!("unexpected content after ');': {code:?}"),
            ));
        }

        if table.is_none() {
            // Header: CREATE TABLE <name> (
            let tokens: Vec<&str> = code.split_whitespace().collect();
            let [create, tbl, name, paren] = tokens.as_slice() else {
                return Err(err(
                    lineno,
                    format!("expected 'CREATE TABLE <name> (' on one line, found {code:?}"),
                ));
            };
            if !create.eq_ignore_ascii_case("create")
                || !tbl.eq_ignore_ascii_case("table")
                || *paren != "("
            {
                return Err(err(
                    lineno,
                    format!("expected 'CREATE TABLE <name> (', found {code:?}"),
                ));
            }
            if !is_ident(name) {
                return Err(err(
                    lineno,
                    format!("table name {name:?} must be lower_snake_case ascii"),
                ));
            }
            table = Some((*name).to_string());
            in_columns = true;
            continue;
        }

        if in_columns && (code == ")" || code == ");") {
            in_columns = false;
            closed = true;
            continue;
        }
        if !in_columns {
            return Err(err(lineno, format!("unexpected content: {code:?}")));
        }

        // Column: <name> <TYPE> [NOT NULL] [PRIMARY KEY][,]  -- [@fixed(n)]
        let decl = code.strip_suffix(',').unwrap_or(code);
        let mut tokens = decl.split_whitespace();
        let name = tokens
            .next()
            .ok_or_else(|| err(lineno, "empty column declaration"))?;
        if !is_ident(name) {
            return Err(err(
                lineno,
                format!("column name {name:?} must be lower_snake_case ascii"),
            ));
        }
        if columns.iter().any(|c| c.name == name) {
            return Err(err(lineno, format!("duplicate column {name:?}")));
        }
        let ty_token = tokens
            .next()
            .ok_or_else(|| err(lineno, format!("column {name:?} is missing a type")))?;

        // Annotation, if any.
        let fixed = parse_fixed_annotation(comment, lineno)?;

        let ty = if ty_token.eq_ignore_ascii_case("bigint") {
            if fixed.is_some() {
                return Err(err(
                    lineno,
                    format!("column {name:?}: @fixed(n) applies to BYTEA, not BIGINT"),
                ));
            }
            ColType::BigInt
        } else if ty_token.eq_ignore_ascii_case("bytea") {
            let Some(n) = fixed else {
                return Err(err(
                    lineno,
                    format!(
                        "column {name:?}: BYTEA needs a width annotation, e.g. \
                         'value BYTEA NOT NULL -- @fixed(16)' (v1 stores \
                         fixed-width slots; varlen spill lands with the blob zone)"
                    ),
                ));
            };
            if n == 0 || n > 256 {
                return Err(err(
                    lineno,
                    format!(
                        "column {name:?}: @fixed({n}) out of range; row fields are \
                         1..=256 bytes (larger values belong in the blob zone)"
                    ),
                ));
            }
            ColType::FixedBytes(n)
        } else {
            return Err(err(
                lineno,
                format!(
                    "column {name:?}: unsupported type {ty_token:?}; v1 supports \
                     BIGINT and BYTEA -- @fixed(n)"
                ),
            ));
        };

        // Modifiers.
        let mut not_null = false;
        let mut primary_key = false;
        let mods: Vec<String> = tokens.map(|t| t.to_ascii_lowercase()).collect();
        let mut i = 0;
        while i < mods.len() {
            match mods[i].as_str() {
                "not" if mods.get(i + 1).map(String::as_str) == Some("null") => {
                    not_null = true;
                    i += 2;
                }
                "primary" if mods.get(i + 1).map(String::as_str) == Some("key") => {
                    primary_key = true;
                    i += 2;
                }
                other => {
                    return Err(err(
                        lineno,
                        format!("column {name:?}: unsupported modifier {other:?}"),
                    ));
                }
            }
        }
        if !not_null {
            return Err(err(
                lineno,
                format!(
                    "column {name:?} must be declared NOT NULL: v1 has no null \
                     bitmap, so nullable columns cannot be represented"
                ),
            ));
        }

        columns.push(Column {
            name: name.to_string(),
            ty,
            primary_key,
        });
    }

    let Some(table) = table else {
        return Err(err(0, "no CREATE TABLE statement found"));
    };
    if !closed {
        return Err(err(0, "CREATE TABLE was never closed with ');'"));
    }
    if columns.is_empty() {
        return Err(err(0, format!("table {table:?} has no columns")));
    }
    let pk_count = columns.iter().filter(|c| c.primary_key).count();
    if pk_count != 1 {
        return Err(err(
            0,
            format!("table {table:?} needs exactly one PRIMARY KEY column, found {pk_count}"),
        ));
    }
    if !columns[0].primary_key {
        return Err(err(
            0,
            format!(
                "table {table:?}: the PRIMARY KEY must be the first column \
                 (the engine reads the key at offset 0)"
            ),
        ));
    }
    if columns[0].ty != ColType::BigInt {
        return Err(err(
            0,
            format!("table {table:?}: the PRIMARY KEY must be BIGINT in v1"),
        ));
    }

    Ok(Schema { table, columns })
}

fn parse_fixed_annotation(comment: &str, lineno: usize) -> Result<Option<u32>, ParseError> {
    let Some(pos) = comment.find("@fixed(") else {
        if comment.contains('@') {
            return Err(err(
                lineno,
                format!("unrecognized annotation in comment {comment:?}; v1 knows @fixed(n)"),
            ));
        }
        return Ok(None);
    };
    let rest = &comment[pos + "@fixed(".len()..];
    let Some(end) = rest.find(')') else {
        return Err(err(lineno, "@fixed( is missing its closing ')'"));
    };
    rest[..end]
        .trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|_| err(lineno, format!("@fixed({}) is not a number", &rest[..end])))
}

impl Schema {
    /// Sequential field offsets, CRC after the last field, row padded to an
    /// 8-byte multiple.
    pub fn layout(&self) -> Layout {
        let mut offsets = Vec::with_capacity(self.columns.len());
        let mut at = 0usize;
        for col in &self.columns {
            offsets.push(at);
            at += col.ty.width();
        }
        let crc_offset = at;
        let row_size = (crc_offset + 4).next_multiple_of(8);
        Layout {
            field_offsets: offsets,
            crc_offset,
            row_size,
        }
    }

    /// The schema hash stored in every superblock copy (docs/DESIGN.md
    /// §4.8): FNV-1a 64 over a canonical rendering of everything that
    /// affects layout. Two schemas hash equal iff their files are
    /// byte-compatible.
    pub fn schema_hash(&self) -> u64 {
        let mut canon = format!("dabqlite-schema-v1;table={};", self.table);
        for col in &self.columns {
            let ty = match col.ty {
                ColType::BigInt => "bigint".to_string(),
                ColType::FixedBytes(n) => format!("bytea({n})"),
            };
            canon.push_str(&format!(
                "col={}:{}:{};",
                col.name,
                ty,
                if col.primary_key { "pk" } else { "col" }
            ));
        }
        fnv1a64(canon.as_bytes())
    }
}

/// FNV-1a, 64-bit. Small, stable, dependency-free; collision resistance
/// needs are modest (the hash gates version mismatches, not adversaries).
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn camel(name: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in name.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Emit the self-contained Rust codec module for a schema. The output has
/// no imports (it carries its own CRC table) so it can be included from
/// any crate, and it reproduces the exact on-disk discipline of the
/// hand-written codec: LE fields, CRC over everything before it, zero
/// padding validated on decode — no dead bytes.
pub fn emit_rust(schema: &Schema, source_name: &str) -> String {
    let layout = schema.layout();
    let row_ty = format!("{}Row", camel(&schema.table));
    let upper = schema.table.to_ascii_uppercase();
    let mut o = String::new();

    o.push_str(&format!(
        "// @generated by dabqlite-codegen from {source_name}. DO NOT EDIT.\n\
         // Layout and hash derive from the schema; regenerate with:\n\
         //   cargo run -p dabqlite-codegen -- {source_name} <this file>\n\n"
    ));
    o.push_str(&format!(
        "pub const {upper}_TABLE: &str = \"{}\";\n",
        schema.table
    ));
    o.push_str(&format!(
        "pub const {upper}_SCHEMA_HASH: u64 = 0x{:016X};\n",
        schema.schema_hash()
    ));
    o.push_str(&format!(
        "pub const {upper}_ROW_SIZE: usize = {};\n",
        layout.row_size
    ));
    o.push_str(&format!(
        "pub const {upper}_CRC_OFFSET: usize = {};\n",
        layout.crc_offset
    ));
    for (col, off) in schema.columns.iter().zip(&layout.field_offsets) {
        o.push_str(&format!(
            "pub const {upper}_COL_{}_OFFSET: usize = {off};\n",
            col.name.to_ascii_uppercase()
        ));
    }
    o.push('\n');

    // Row struct.
    o.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n");
    o.push_str(&format!("pub struct {row_ty} {{\n"));
    for col in &schema.columns {
        let ty = match col.ty {
            ColType::BigInt => "u64".to_string(),
            ColType::FixedBytes(n) => format!("[u8; {n}]"),
        };
        o.push_str(&format!("    pub {}: {ty},\n", col.name));
    }
    o.push_str("}\n\n");

    // Private CRC (self-contained on purpose).
    o.push_str(
        "const GEN_CRC_POLY: u32 = 0xEDB8_8320;\n\
         const fn gen_crc_table() -> [u32; 256] {\n\
         \x20   let mut table = [0u32; 256];\n\
         \x20   let mut i = 0;\n\
         \x20   while i < 256 {\n\
         \x20       let mut crc = i as u32;\n\
         \x20       let mut bit = 0;\n\
         \x20       while bit < 8 {\n\
         \x20           crc = if crc & 1 != 0 { (crc >> 1) ^ GEN_CRC_POLY } else { crc >> 1 };\n\
         \x20           bit += 1;\n\
         \x20       }\n\
         \x20       table[i] = crc;\n\
         \x20       i += 1;\n\
         \x20   }\n\
         \x20   table\n\
         }\n\
         static GEN_CRC_TABLE: [u32; 256] = gen_crc_table();\n\
         fn gen_crc32(data: &[u8]) -> u32 {\n\
         \x20   let mut crc = !0u32;\n\
         \x20   for &byte in data {\n\
         \x20       crc = (crc >> 8) ^ GEN_CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize];\n\
         \x20   }\n\
         \x20   !crc\n\
         }\n\n",
    );

    // Encoder.
    o.push_str(&format!(
        "pub fn encode_{}_row(row: &{row_ty}, out: &mut [u8; {upper}_ROW_SIZE]) {{\n",
        schema.table
    ));
    for (col, off) in schema.columns.iter().zip(&layout.field_offsets) {
        match col.ty {
            ColType::BigInt => o.push_str(&format!(
                "    out[{off}..{}].copy_from_slice(&row.{}.to_le_bytes());\n",
                off + 8,
                col.name
            )),
            ColType::FixedBytes(n) => o.push_str(&format!(
                "    out[{off}..{}].copy_from_slice(&row.{});\n",
                off + n as usize,
                col.name
            )),
        }
    }
    o.push_str(&format!(
        "    let crc = gen_crc32(&out[0..{upper}_CRC_OFFSET]);\n\
         \x20   out[{upper}_CRC_OFFSET..{upper}_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());\n\
         \x20   out[{upper}_CRC_OFFSET + 4..].fill(0);\n\
         \x20   debug_assert_eq!(decode_{}_row(out), Some(*row));\n\
         }}\n\n",
        schema.table
    ));

    // Decoder.
    o.push_str(&format!(
        "pub fn decode_{}_row(bytes: &[u8]) -> Option<{row_ty}> {{\n\
         \x20   if bytes.len() < {upper}_ROW_SIZE {{\n\
         \x20       return None;\n\
         \x20   }}\n\
         \x20   let stored = u32::from_le_bytes(bytes[{upper}_CRC_OFFSET..{upper}_CRC_OFFSET + 4].try_into().ok()?);\n\
         \x20   if gen_crc32(&bytes[0..{upper}_CRC_OFFSET]) != stored {{\n\
         \x20       return None;\n\
         \x20   }}\n\
         \x20   if bytes[{upper}_CRC_OFFSET + 4..{upper}_ROW_SIZE].iter().any(|&b| b != 0) {{\n\
         \x20       return None;\n\
         \x20   }}\n",
        schema.table
    ));
    for (col, off) in schema.columns.iter().zip(&layout.field_offsets) {
        match col.ty {
            ColType::BigInt => o.push_str(&format!(
                "    let {} = u64::from_le_bytes(bytes[{off}..{}].try_into().ok()?);\n",
                col.name,
                off + 8
            )),
            ColType::FixedBytes(n) => o.push_str(&format!(
                "    let {}: [u8; {n}] = bytes[{off}..{}].try_into().ok()?;\n",
                col.name,
                off + n as usize
            )),
        }
    }
    o.push_str(&format!(
        "    Some({row_ty} {{ {} }})\n}}\n",
        schema
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    o
}

// ---- declared queries (docs/DESIGN.md §4.3) ------------------------------

/// How a query returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// `:one` — returns at most one row (SELECT by primary key).
    One,
    /// `:exec` — returns success/failure only (INSERT).
    Exec,
}

/// The validated shape of a query. v1 supports exactly the shapes the
/// engine implements; anything else is a loud parse error, because an
/// operation that parses here but has no engine path would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryShape {
    /// SELECT all columns FROM table WHERE <pk> = $1
    SelectByPk,
    /// INSERT INTO table (all columns) VALUES ($1..$n)
    InsertRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub name: String,
    pub kind: QueryKind,
    pub shape: QueryShape,
}

/// Tokenize a SQL statement: identifiers/keywords, `$n` params, and the
/// punctuation the v1 shapes need.
fn sql_tokens(stmt: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in stmt.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            cur.push(c.to_ascii_lowercase());
        } else {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            match c {
                '(' | ')' | ',' | '=' | ';' => tokens.push(c.to_string()),
                c if c.is_whitespace() => {}
                other => tokens.push(other.to_string()), // surfaced as an error later
            }
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Parse and validate the declared-queries file against a schema.
pub fn parse_queries(sql: &str, schema: &Schema) -> Result<Vec<Query>, ParseError> {
    let mut queries: Vec<Query> = Vec::new();
    let mut pending: Option<(usize, String, QueryKind, String)> = None; // (line, name, kind, stmt-so-far)

    for (idx, raw) in sql.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if let Some(comment) = line.strip_prefix("--") {
            let comment = comment.trim();
            if let Some(rest) = comment.strip_prefix("name:") {
                if let Some((l, name, ..)) = &pending {
                    return Err(err(
                        lineno,
                        format!("query {name:?} (line {l}) has no terminating ';'"),
                    ));
                }
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let [name, kind] = parts.as_slice() else {
                    return Err(err(
                        lineno,
                        format!("expected '-- name: <ident> :one|:exec', found {comment:?}"),
                    ));
                };
                if !is_ident(name) {
                    return Err(err(
                        lineno,
                        format!("query name {name:?} must be lower_snake_case ascii"),
                    ));
                }
                if queries.iter().any(|q| q.name == *name) {
                    return Err(err(lineno, format!("duplicate query name {name:?}")));
                }
                let kind = match *kind {
                    ":one" => QueryKind::One,
                    ":exec" => QueryKind::Exec,
                    other => {
                        return Err(err(
                            lineno,
                            format!("unknown result kind {other:?}; v1 knows :one and :exec"),
                        ))
                    }
                };
                pending = Some((lineno, name.to_string(), kind, String::new()));
            } else if comment.contains("name:") {
                return Err(err(
                    lineno,
                    format!("malformed name annotation: {comment:?} (write '-- name: <ident> :one|:exec')"),
                ));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some((decl_line, name, kind, stmt)) = pending.as_mut() else {
            return Err(err(
                lineno,
                format!("statement without a '-- name:' annotation: {line:?}"),
            ));
        };
        stmt.push(' ');
        stmt.push_str(line);
        if line.ends_with(';') {
            let query = validate_query(*decl_line, name, *kind, stmt, schema)?;
            queries.push(query);
            pending = None;
        }
    }
    if let Some((l, name, ..)) = pending {
        return Err(err(l, format!("query {name:?} has no terminating ';'")));
    }
    if queries.is_empty() {
        return Err(err(
            0,
            "no queries declared; the operation space may not be empty",
        ));
    }
    Ok(queries)
}

fn validate_query(
    line: usize,
    name: &str,
    kind: QueryKind,
    stmt: &str,
    schema: &Schema,
) -> Result<Query, ParseError> {
    let tokens = sql_tokens(stmt);
    let toks: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let cols: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    let pk = cols[0];
    let table = schema.table.as_str();

    // Expected token streams for the two supported shapes, built from the
    // schema so the comparison is exact and the error can say precisely
    // which token diverged.
    let mut select_expect: Vec<String> = vec!["select".into()];
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            select_expect.push(",".into());
        }
        select_expect.push((*c).into());
    }
    select_expect.extend([
        "from".into(),
        table.into(),
        "where".into(),
        pk.into(),
        "=".into(),
        "$1".into(),
        ";".into(),
    ]);

    let mut insert_expect: Vec<String> =
        vec!["insert".into(), "into".into(), table.into(), "(".into()];
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            insert_expect.push(",".into());
        }
        insert_expect.push((*c).into());
    }
    insert_expect.extend([")".into(), "values".into(), "(".into()]);
    for i in 0..cols.len() {
        if i > 0 {
            insert_expect.push(",".into());
        }
        insert_expect.push(format!("${}", i + 1));
    }
    insert_expect.extend([")".into(), ";".into()]);

    let matches = |expect: &[String]| {
        toks.len() == expect.len() && toks.iter().zip(expect).all(|(a, b)| *a == b)
    };

    let shape = if toks.first() == Some(&"select") {
        if !matches(&select_expect) {
            return Err(err(
                line,
                format!(
                    "query {name:?}: v1 SELECT must be exactly \
                     'SELECT {} FROM {} WHERE {} = $1;' (all columns, schema \
                     order, primary-key equality); got tokens {toks:?}",
                    cols.join(", "),
                    table,
                    pk
                ),
            ));
        }
        if kind != QueryKind::One {
            return Err(err(
                line,
                format!("query {name:?}: SELECT by primary key must be ':one'"),
            ));
        }
        QueryShape::SelectByPk
    } else if toks.first() == Some(&"insert") {
        if !matches(&insert_expect) {
            return Err(err(
                line,
                format!(
                    "query {name:?}: v1 INSERT must be exactly \
                     'INSERT INTO {} ({}) VALUES ({});' (all columns, schema \
                     order, sequential params); got tokens {toks:?}",
                    table,
                    cols.join(", "),
                    (1..=cols.len())
                        .map(|i| format!("${i}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if kind != QueryKind::Exec {
            return Err(err(line, format!("query {name:?}: INSERT must be ':exec'")));
        }
        QueryShape::InsertRow
    } else {
        return Err(err(
            line,
            format!(
                "query {name:?}: unsupported statement {:?}; the v1 operation \
                 space is SELECT-by-primary-key and INSERT",
                toks.first().copied().unwrap_or("<empty>")
            ),
        ));
    };

    Ok(Query {
        name: name.to_string(),
        kind,
        shape,
    })
}

/// Hash of the operation space: two binaries agree on what the database
/// can be asked to do iff these match.
pub fn query_space_hash(queries: &[Query]) -> u64 {
    let mut names: Vec<&Query> = queries.iter().collect();
    names.sort_by(|a, b| a.name.cmp(&b.name));
    let mut canon = String::from("dabqlite-queries-v1;");
    for q in names {
        let kind = match q.kind {
            QueryKind::One => "one",
            QueryKind::Exec => "exec",
        };
        let shape = match q.shape {
            QueryShape::SelectByPk => "select_by_pk",
            QueryShape::InsertRow => "insert_row",
        };
        canon.push_str(&format!("{}:{}:{};", q.name, kind, shape));
    }
    fnv1a64(canon.as_bytes())
}

/// Emit the generated operation surface. Lives inside `dabqlite-core`
/// (references `crate::engine::Input`), so the typed client functions and
/// the engine can never drift apart without failing to compile.
pub fn emit_queries_rust(schema: &Schema, queries: &[Query], source_name: &str) -> String {
    // The engine's v1 client inputs are exactly Insert{id, value} and
    // Get{id}. Widening the schema requires widening the engine first;
    // fail here, loudly, rather than emitting code that cannot compile.
    assert!(
        schema.columns.len() == 2
            && schema.columns[0].name == "id"
            && schema.columns[1].name == "value",
        "engine v1 supports exactly (id BIGINT PK, value BYTEA): extend \
         dabqlite-core's Input before widening the schema"
    );
    let mut o = String::new();
    o.push_str(&format!(
        "// @generated by dabqlite-codegen from {source_name}. DO NOT EDIT.\n\
         // The complete, finite operation space (docs/DESIGN.md §4.3):\n\
         // nothing outside this module can ever be asked of the engine.\n\n"
    ));

    let mut sorted: Vec<&Query> = queries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    o.push_str("/// Every operation the compiled client surface exposes.\n");
    o.push_str(&format!(
        "pub const OPERATIONS: &[&str] = &[{}];\n",
        sorted
            .iter()
            .map(|q| format!("\"{}\"", q.name))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    o.push_str(&format!(
        "/// Two binaries agree on the operation space iff these match.\n\
         pub const QUERY_SPACE_HASH: u64 = 0x{:016X};\n\n",
        query_space_hash(queries)
    ));

    for q in queries {
        match q.shape {
            QueryShape::InsertRow => {
                let params: Vec<String> = schema
                    .columns
                    .iter()
                    .map(|c| match c.ty {
                        ColType::BigInt => format!("{}: u64", c.name),
                        ColType::FixedBytes(n) => format!("{}: [u8; {n}]", c.name),
                    })
                    .collect();
                o.push_str(&format!(
                    "/// `-- name: {} :exec` — INSERT one `{}` row. Durably\n\
                     /// committed when the engine answers `InsertDone {{ result: Ok }}`.\n\
                     pub fn {}({}) -> crate::engine::Input<'static> {{\n\
                     \x20   crate::engine::Input::Insert {{ id, value }}\n\
                     }}\n\n",
                    q.name,
                    schema.table,
                    q.name,
                    params.join(", ")
                ));
            }
            QueryShape::SelectByPk => {
                o.push_str(&format!(
                    "/// `-- name: {} :one` — SELECT one `{}` row by primary key.\n\
                     /// Answered by `GetDone`; pure in-memory, no I/O requests.\n\
                     pub fn {}(id: u64) -> crate::engine::Input<'static> {{\n\
                     \x20   crate::engine::Input::Get {{ id }}\n\
                     }}\n\n",
                    q.name, schema.table, q.name
                ));
            }
        }
    }
    o
}
