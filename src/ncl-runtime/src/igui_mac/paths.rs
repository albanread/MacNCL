//! Where the app's Lisp lives: deployed bundle vs development repo.
//!
//! Deployed layout (see NCLMac.md "File locations"):
//!
//! ```text
//! MacNCL.app/Contents/Resources/Lisp/     ← shipped, read-only
//!     Library/…  (standard library)
//!     demos/…    (examples; the Examples menu lists these)
//! ~/Library/Application Support/MacNCL/    ← per-user state (recents, …)
//! ```
//!
//! Development (unbundled `target/<profile>/ncl`) falls back to the repo's
//! `Lisp/`, found by walking up from the executable. `NCL_LISP_DIR`
//! overrides everything (tests). Pure path logic — fully testable.

use std::path::{Path, PathBuf};

/// Resolve the Lisp root for the process whose executable is `exe`
/// (pass `None` for "no executable", e.g. tests of the fallbacks).
/// Order: env override → bundle Resources → beside the executable →
/// repo walk-up from the executable → repo walk-up from the cwd.
pub fn lisp_dir_for(exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NCL_LISP_DIR") {
        let p = PathBuf::from(p);
        if p.join("Library").is_dir() {
            return Some(p);
        }
    }
    if let Some(exe) = exe {
        // …/MacNCL.app/Contents/MacOS/ncl → …/MacNCL.app/Contents/Resources/Lisp
        let macos = exe.parent();
        let contents = macos.and_then(|m| m.parent());
        if let Some(contents) = contents.filter(|c| c.file_name().is_some_and(|n| n == "Contents")) {
            let bundled = contents.join("Resources").join("Lisp");
            if bundled.join("Library").is_dir() {
                return Some(bundled);
            }
        }
        let beside = exe.parent().map(|d| d.join("Lisp"));
        if let Some(d) = beside.filter(|d| d.join("Library").is_dir()) {
            return Some(d);
        }
        // Dev: <repo>/target/<profile>/ncl → <repo>/Lisp
        if let Some(found) = walk_for_lisp(exe.parent()) {
            return Some(found);
        }
    }
    walk_for_lisp(std::env::current_dir().ok().as_deref())
}

fn walk_for_lisp(start: Option<&Path>) -> Option<PathBuf> {
    let start = start?;
    for a in start.ancestors() {
        let cand = a.join("Lisp");
        if cand.join("Library").is_dir() {
            return Some(cand);
        }
    }
    None
}

/// The Lisp root of this process.
pub fn lisp_dir() -> Option<PathBuf> {
    lisp_dir_for(std::env::current_exe().ok().as_deref())
}

/// Example (demo) files under `<lisp>/demos`: (name, path), name-sorted.
/// Missing directory → empty (the menu shows a disabled placeholder).
pub fn example_files(lisp: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(lisp.join("demos")) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "lisp"))
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            Some((stem, p.to_string_lossy().into_owned()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bundle(root: &Path) -> PathBuf {
        let app = root.join("Fake.app/Contents");
        let lisp = app.join("Resources/Lisp/Library");
        std::fs::create_dir_all(&lisp).unwrap();
        std::fs::write(lisp.join("init.lisp"), "(nil)\n").unwrap();
        let exe = app.join("MacOS");
        std::fs::create_dir_all(&exe).unwrap();
        exe.join("ncl")
    }

    fn fake_repo(root: &Path) -> PathBuf {
        let lib = root.join("Lisp/Library");
        std::fs::create_dir_all(lib.join("src")).unwrap();
        std::fs::write(lib.join("init.lisp"), "(nil)\n").unwrap();
        let target = root.join("target/debug");
        std::fs::create_dir_all(&target).unwrap();
        target.join("ncl")
    }

    /// Bundle layout wins when the executable lives in an .app.
    #[test]
    fn bundle_resources_resolve_first() {
        let tmp = std::env::temp_dir().join(format!("ncl_paths_bundle_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let exe = fake_bundle(&tmp);
        // Note: the parent repo ALSO has a Lisp dir (we're inside the real
        // repo when tests run) — the bundle must still win.
        let got = lisp_dir_for(Some(&exe)).expect("resolves");
        assert!(got.ends_with("Fake.app/Contents/Resources/Lisp"), "got {got:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Unbundled dev binary walks up to the repo's Lisp/.
    #[test]
    fn dev_binary_walks_up_to_repo() {
        let tmp = std::env::temp_dir().join(format!("ncl_paths_dev_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let exe = fake_repo(&tmp);
        let got = lisp_dir_for(Some(&exe)).expect("resolves");
        assert_eq!(got, tmp.join("Lisp"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Demos listing: .lisp only, sorted, name/path pairs.
    #[test]
    fn example_files_list_sorted() {
        let tmp = std::env::temp_dir().join(format!("ncl_paths_demos_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let demos = tmp.join("demos");
        std::fs::create_dir_all(&demos).unwrap();
        std::fs::write(demos.join("zeta.lisp"), "(z)\n").unwrap();
        std::fs::write(demos.join("alpha.lisp"), "(a)\n").unwrap();
        std::fs::write(demos.join("ignore.png"), b"x").unwrap();
        let got = example_files(&tmp);
        assert_eq!(
            got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert!(got[0].1.ends_with("alpha.lisp"));
        // Missing demos dir → empty.
        assert!(example_files(&tmp.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
