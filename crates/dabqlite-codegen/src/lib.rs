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
