//! The runtime universe: standard packages, `*features*`, and the
//! global symbol registry.
//!
//! There is one `Universe` per process. The Lisp thread owns it. Other
//! threads must not touch it directly — they go through the iGui event
//! mailbox (MANIFESTO.md, "Design values" #2). For now we use `Mutex`
//! internally because Rust's static-init story demands it; the
//! single-threaded discipline is enforced by code review.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::symbol::{Package, Symbol};
use crate::value::Value;

/// Standard package names. Look these up by exact name (case-sensitive,
/// upper-case as CL convention).
pub mod pkg {
    pub const COMMON_LISP: &str = "COMMON-LISP";
    pub const KEYWORD: &str = "KEYWORD";
    pub const COMMON_LISP_USER: &str = "COMMON-LISP-USER";
    pub const CORMANLISP: &str = "CORMANLISP";
}

pub struct Universe {
    /// Name (or nickname) → package. Lower-case-sensitive: stored
    /// upper-case, looked up upper-case.
    packages: Mutex<HashMap<Arc<str>, Arc<Package>>>,
    /// Active feature names (keyword symbol names without the leading
    /// colon, upper-case). Read by `#+`/`#-`.
    features: Mutex<HashSet<Arc<str>>>,
}

impl Universe {
    fn new() -> Universe {
        let u = Universe {
            packages: Mutex::new(HashMap::new()),
            features: Mutex::new(HashSet::new()),
        };
        u.bootstrap_packages();
        u.bootstrap_features();
        u
    }

    fn bootstrap_packages(&self) {
        // KEYWORD has no uses.
        let keyword = Package::new(pkg::KEYWORD, &[], vec![]);

        // COMMON-LISP has no uses (it IS the foundation).
        let cl = Package::new(pkg::COMMON_LISP, &["CL", "LISP"], vec![]);

        // CORMANLISP uses CL; nicknames CCL and PL per Corman.
        let corman = Package::new(
            pkg::CORMANLISP,
            &["CCL", "PL"],
            vec![Arc::clone(&cl)],
        );

        // C-TYPES (FFI helpers) and WIN32 (Windows API) and SYS
        // (Corman internals). Names and nicknames per upstream
        // E:\CL\cormanlisp\Sys\ffi.lisp and Modules\mp.lisp.
        // We pre-register them so demos that reference `ct:foo`,
        // `win:bar`, and `sys:*current-process*` resolve their
        // package qualifier even before the FFI machinery exists
        // to populate exports.
        let c_types = Package::new("C-TYPES", &["CT"], vec![Arc::clone(&cl)]);
        let win32 = Package::new("WIN32", &["WIN"], vec![Arc::clone(&cl)]);
        let sys = Package::new("SYS", &[], vec![Arc::clone(&cl)]);

        // COMMON-LISP-USER uses CL and CORMANLISP — Corman's user
        // package inherits both, which is what makes `format`, `defun`
        // and `ccl::quit` all visible in the default workspace.
        let user = Package::new(
            pkg::COMMON_LISP_USER,
            &["CL-USER", "USER"],
            vec![Arc::clone(&cl), Arc::clone(&corman)],
        );

        // Pre-intern the symbols every Lisp file references on its
        // first line. NIL and T are externals of CL.
        cl.intern_external("NIL");
        cl.intern_external("T");
        // QUOTE/FUNCTION are needed by the reader to build `'x` and
        // `#'x` forms. BACKQUOTE/UNQUOTE/UNQUOTE-SPLICING/
        // UNQUOTE-NSPLICING are non-standard but conventional names
        // for the backquote family — kept in CL so reader output is
        // package-stable.
        cl.intern_external("QUOTE");
        cl.intern_external("FUNCTION");
        cl.intern_external("BACKQUOTE");
        cl.intern_external("UNQUOTE");
        cl.intern_external("UNQUOTE-SPLICING");
        cl.intern_external("UNQUOTE-NSPLICING");

        let mut table = self.packages.lock().unwrap();
        for p in [&keyword, &cl, &corman, &c_types, &win32, &sys, &user] {
            register_package(&mut table, Arc::clone(p));
        }
    }

    fn bootstrap_features(&self) {
        // See MANIFESTO.md and the *features* policy: claim a feature
        // only if claiming it leads demo code to working code. We
        // deliberately do not claim :x86 or :32-bit (we are 64-bit
        // and architecture-portable through LLVM), and do not claim
        // :hardware-gc (we don't have one yet).
        let mut features = self.features.lock().unwrap();
        for name in [
            "COMMON-LISP",
            "ANSI-CL",
            "CORMANLISP",   // legacy demos branch on this — claim it
            "PL",           // legacy nickname — claim it
            "NEWCORMANLISP", // lets demo code detect us specifically
            "64-BIT",
            "LITTLE-ENDIAN",
            #[cfg(target_arch = "x86_64")]
            "X86-64",
            #[cfg(target_arch = "aarch64")]
            "ARM64",
            #[cfg(windows)]
            "WIN32",
            #[cfg(windows)]
            "OS-WINDOWS",
            #[cfg(target_os = "macos")]
            "OS-MACOS",
            #[cfg(target_os = "linux")]
            "OS-LINUX",
        ] {
            features.insert(Arc::from(name));
        }
    }

    pub fn find_package(&self, name_or_nickname: &str) -> Option<Arc<Package>> {
        self.packages
            .lock()
            .unwrap()
            .get(name_or_nickname)
            .cloned()
    }

    pub fn has_feature(&self, name: &str) -> bool {
        self.features.lock().unwrap().contains(name)
    }

    /// Read a `nil` value. The reader normalises the token `NIL`
    /// (interpreted in COMMON-LISP) into `Value::Nil` via this hook
    /// rather than producing `Value::Symbol(nil-sym)`, so
    /// `(eq 'nil ())` is automatically true.
    pub fn nil_value(&self) -> Value {
        Value::Nil
    }

    /// Read the symbol `T` from COMMON-LISP. Convenience for the reader.
    pub fn t_symbol(&self) -> Arc<Symbol> {
        let cl = self.find_package(pkg::COMMON_LISP).expect("CL bootstrapped");
        let (sym, _) = cl.intern("T");
        sym
    }
}

fn register_package(
    table: &mut HashMap<Arc<str>, Arc<Package>>,
    pkg: Arc<Package>,
) {
    table.insert(Arc::clone(&pkg.name), Arc::clone(&pkg));
    for nick in &pkg.nicknames {
        table.insert(Arc::clone(nick), Arc::clone(&pkg));
    }
}

static UNIVERSE: OnceLock<Universe> = OnceLock::new();

/// Get (lazy-initialising) the process-global universe.
pub fn universe() -> &'static Universe {
    UNIVERSE.get_or_init(Universe::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_packages_exist() {
        let u = universe();
        assert!(u.find_package(pkg::COMMON_LISP).is_some());
        assert!(u.find_package(pkg::KEYWORD).is_some());
        assert!(u.find_package(pkg::COMMON_LISP_USER).is_some());
        assert!(u.find_package(pkg::CORMANLISP).is_some());
    }

    #[test]
    fn cl_nicknames_resolve() {
        let u = universe();
        let cl = u.find_package("COMMON-LISP").expect("CL");
        assert!(Arc::ptr_eq(&cl, &u.find_package("CL").unwrap()));
        assert!(Arc::ptr_eq(&cl, &u.find_package("LISP").unwrap()));
    }

    #[test]
    fn corman_nicknames_resolve() {
        let u = universe();
        let corman = u.find_package("CORMANLISP").expect("CORMANLISP");
        assert!(Arc::ptr_eq(&corman, &u.find_package("CCL").unwrap()));
        assert!(Arc::ptr_eq(&corman, &u.find_package("PL").unwrap()));
    }

    #[test]
    fn cl_user_inherits_cl() {
        let u = universe();
        let user = u.find_package("CL-USER").expect("CL-USER");
        let (nil, vis) = user.find("NIL").expect("NIL inherited");
        assert_eq!(vis, crate::symbol::Visibility::Inherited);
        let cl = u.find_package("CL").unwrap();
        let (cl_nil, _) = cl.find("NIL").unwrap();
        assert!(Arc::ptr_eq(&nil, &cl_nil));
    }

    #[test]
    fn cl_user_inherits_corman() {
        // A symbol externalised in CORMANLISP should be visible from
        // CL-USER without a prefix. We pre-intern one to verify.
        let u = universe();
        let corman = u.find_package("CORMANLISP").unwrap();
        corman.intern_external("QUIT");
        let user = u.find_package("CL-USER").unwrap();
        let (q, vis) = user.find("QUIT").expect("QUIT inherited from CCL");
        assert_eq!(vis, crate::symbol::Visibility::Inherited);
        let _ = q;
    }

    #[test]
    fn features_64_bit_not_32_bit() {
        let u = universe();
        assert!(u.has_feature("64-BIT"));
        assert!(!u.has_feature("32-BIT"));
        assert!(!u.has_feature("X86")); // we are NOT plain :x86
    }

    #[test]
    fn features_corman_compatibility() {
        let u = universe();
        assert!(u.has_feature("CORMANLISP"));
        assert!(u.has_feature("PL"));
        assert!(u.has_feature("NEWCORMANLISP"));
    }

    #[test]
    fn features_arch_matches_host() {
        let u = universe();
        #[cfg(target_arch = "x86_64")]
        assert!(u.has_feature("X86-64"));
        #[cfg(target_arch = "aarch64")]
        assert!(u.has_feature("ARM64"));
    }
}
