//! The CI workflows must be parseable, because a workflow GitHub cannot
//! parse does not run — and does not fail either.
//!
//! This is here for a specific reason. `.github/workflows/ci.yml` carried
//! the step name
//!
//! ```text
//!       - name: Clippy (wasm32: the OPFS backend)
//! ```
//!
//! for two commits. In YAML, `": "` inside an unquoted scalar starts a
//! nested mapping, so the whole document failed to load and every job in
//! it — clippy, the determinism gate, the crash sweeps, the VOPR soak —
//! silently stopped running. Nothing turned red, because nothing ran.
//! That is the worst failure mode a validation suite can have: it reports
//! the same thing whether it is passing or absent.
//!
//! A full YAML parser would be a dependency this workspace does not want,
//! so this checks the exact hazard that bit us rather than pretending to
//! be one: an unquoted scalar value that contains `": "`. It is narrow on
//! purpose, and it is honest about being narrow.

use std::path::PathBuf;

fn workflows_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows"
    ))
}

/// Strip a trailing `# comment` from a scalar, the way YAML would.
fn strip_comment(v: &str) -> &str {
    match v.find(" #") {
        Some(i) => v[..i].trim_end(),
        None => v,
    }
}

#[test]
fn every_workflow_scalar_that_needs_quoting_has_it() {
    let dir = workflows_dir();
    let mut checked = 0usize;
    let mut files = 0usize;
    for entry in std::fs::read_dir(&dir).expect("read .github/workflows") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        files += 1;
        let text = std::fs::read_to_string(&path).expect("read workflow");
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            // Only plain `key: value` lines on one line can trip on this;
            // block scalars (`run: |`) and quoted values cannot.
            let Some(colon) = trimmed.find(": ") else {
                continue;
            };
            let key = trimmed[..colon].trim_start_matches("- ");
            if key.contains(' ') || key.contains('"') || key.contains('\'') {
                continue; // not a key at all
            }
            let value = strip_comment(trimmed[colon + 2..].trim());
            if value.is_empty()
                || value.starts_with('"')
                || value.starts_with('\'')
                || value.starts_with('|')
                || value.starts_with('>')
                || value.starts_with('[')
            {
                continue;
            }
            checked += 1;
            assert!(
                !value.contains(": "),
                "{}:{}: unquoted YAML scalar contains \": \", which starts a \
                 nested mapping and makes the whole workflow unloadable — \
                 GitHub will skip the file rather than fail it. Quote it:\n  {}",
                path.display(),
                n + 1,
                line
            );
        }
    }
    assert!(files >= 1, "no workflows found under {}", dir.display());
    assert!(
        checked > 20,
        "only {checked} scalars checked; the scan is broken"
    );
}

/// The sample applications have to be RUN by something, not only compiled.
///
/// Row format v6 broke eight sample tests that no job executed. Compiling
/// them proves the API still exists; running them proves it still means
/// what the samples say it means.
#[test]
fn ci_runs_the_sample_application_tests() {
    let ci = std::fs::read_to_string(workflows_dir().join("ci.yml")).expect("ci.yml");
    assert!(
        ci.contains("cargo test --test"),
        "ci.yml no longer runs the sample application test suites"
    );
}
