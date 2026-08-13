//! CLI: `dabqlite-codegen <schema.sql> [out.rs]` — parse, generate, write
//! (or print to stdout). Exit code 1 with a line-numbered message on any
//! schema error.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(schema_path) = args.next() else {
        eprintln!("usage: dabqlite-codegen <schema.sql> [out.rs]");
        return ExitCode::FAILURE;
    };
    let out_path = args.next();

    let sql = match std::fs::read_to_string(&schema_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {schema_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let schema = match dabqlite_codegen::parse_schema(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {schema_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = dabqlite_codegen::emit_rust(&schema, &schema_path);
    eprintln!(
        "schema {}: {} columns, row_size={}, schema_hash=0x{:016X}",
        schema.table,
        schema.columns.len(),
        schema.layout().row_size,
        schema.schema_hash()
    );
    match out_path {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &code) {
                eprintln!("error: cannot write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{code}"),
    }
    ExitCode::SUCCESS
}
