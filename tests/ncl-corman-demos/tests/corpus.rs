//! Corpus run: parse every `.lisp` file under `cormanlisp\examples\`.
//!
//! Phase 1d's deliverable. The bar is: every demo file either parses
//! cleanly, or fails with an error in [`KNOWN_DEFERRED_KINDS`] —
//! categories of reader behavior we have explicitly sequenced for a
//! later phase.
//!
//! When you find a new failure category that is *not* a real bug,
//! add it to the deferred list with a comment explaining why and
//! when we expect to land it.

use std::fs;
use std::path::{Path, PathBuf};

use ncl_reader::{ReaderError, ReaderErrorKind};

const EXAMPLES_DIR: &str = r"E:\CL\cormanlisp\examples";
const SYS_DIR: &str = r"E:\CL\cormanlisp\Sys";

/// Categories of reader failure we accept for now. Each variant
/// documents the reason and the planned fix.
fn is_deferred(err: &ReaderError) -> Option<&'static str> {
    use ReaderErrorKind::*;
    match &err.kind {
        // `#.(...)` — needs eval. Lands when the JIT comes up.
        ReadEvalUnsupported => Some("read-time eval (#.) — needs the JIT"),
        // `#A`, `#S`, `#P`, `#C`, `#=`, `##` — sequenced for later.
        UnsupportedSharpDispatch { ch, .. } => match ch {
            'A' | 'a' => Some("array literals (#A) — phase tbd"),
            'S' | 's' => Some("struct literals (#S) — phase tbd"),
            'P' | 'p' => Some("pathname literals (#P) — phase tbd"),
            'C' | 'c' => Some("complex literals (#C) — needs numeric tower"),
            '=' | '#' => Some("circular structure (#N=, #N#) — phase tbd"),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug)]
struct Outcome {
    path: PathBuf,
    result: Result<usize, ReaderError>,
}

fn collect_lisp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return; };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_lisp_files(&p, out);
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lisp")) {
            out.push(p);
        }
    }
}

fn run_corpus(dir: &Path) -> Vec<Outcome> {
    let mut files = Vec::new();
    collect_lisp_files(dir, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let src = fs::read_to_string(&path).unwrap_or_default();
            let result = ncl_reader::read_all(&src).map(|forms| forms.len());
            Outcome { path, result }
        })
        .collect()
}

#[test]
fn parses_all_examples() {
    let dir = Path::new(EXAMPLES_DIR);
    if !dir.exists() {
        eprintln!("upstream not present at {} — skipping", dir.display());
        return;
    }

    let outcomes = run_corpus(dir);
    let mut passes = 0usize;
    let mut deferred: Vec<(&Path, &'static str)> = Vec::new();
    let mut real_failures: Vec<&Outcome> = Vec::new();

    for o in &outcomes {
        match &o.result {
            Ok(_) => passes += 1,
            Err(e) => {
                if let Some(reason) = is_deferred(e) {
                    deferred.push((&o.path, reason));
                } else {
                    real_failures.push(o);
                }
            }
        }
    }

    eprintln!("\n=== corpus run: {} files ===", outcomes.len());
    eprintln!("  passed:   {}", passes);
    eprintln!("  deferred: {}", deferred.len());
    eprintln!("  failed:   {}", real_failures.len());

    if !deferred.is_empty() {
        eprintln!("\n--- deferred (categorised) ---");
        for (p, why) in &deferred {
            eprintln!("  {} — {}", p.file_name().unwrap().to_string_lossy(), why);
        }
    }

    if !real_failures.is_empty() {
        eprintln!("\n--- real failures (must fix) ---");
        for o in &real_failures {
            let e = o.result.as_ref().unwrap_err();
            eprintln!(
                "  {}: {:?} at {:?}",
                o.path.file_name().unwrap().to_string_lossy(),
                e.kind,
                e.span
            );
        }
        panic!("{} unexpected reader failures (see stderr)", real_failures.len());
    }
}

/// Stretch goal: try to parse the Corman implementation sources too.
/// Informational only — failures here are reported but don't fail
/// the build. The bar is `examples/`; `Sys/` is a maturity indicator.
#[test]
fn parses_sys_directory_informational() {
    let dir = Path::new(SYS_DIR);
    if !dir.exists() {
        eprintln!("upstream Sys/ not present at {} — skipping", dir.display());
        return;
    }

    let outcomes = run_corpus(dir);
    let mut passes = 0;
    let mut deferred = 0;
    let mut failures: Vec<&Outcome> = Vec::new();

    for o in &outcomes {
        match &o.result {
            Ok(_) => passes += 1,
            Err(e) => {
                if is_deferred(e).is_some() {
                    deferred += 1;
                } else {
                    failures.push(o);
                }
            }
        }
    }

    eprintln!("\n=== Sys/ corpus (informational) ===");
    eprintln!("  total:    {}", outcomes.len());
    eprintln!("  passed:   {}", passes);
    eprintln!("  deferred: {}", deferred);
    eprintln!("  other:    {}", failures.len());
    if !failures.is_empty() {
        eprintln!("\n  first 10 failure categories:");
        let mut seen = std::collections::HashMap::<String, usize>::new();
        for o in &failures {
            let kind = format!("{:?}", o.result.as_ref().unwrap_err().kind);
            // Take the variant name only — strip everything from the
            // first space, paren, or brace.
            let key = kind
                .split(|c: char| c == '(' || c == '{' || c == ' ')
                .next()
                .unwrap_or(&kind)
                .to_string();
            *seen.entry(key).or_insert(0) += 1;
        }
        let mut entries: Vec<_> = seen.into_iter().collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in entries.iter().take(10) {
            eprintln!("    {n:>3}  {k}");
        }
    }
}
