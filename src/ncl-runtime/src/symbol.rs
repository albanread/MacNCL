//! Symbols and packages.
//!
//! A `Symbol` carries its name and (optionally) a home package. Two
//! interned symbols with the same name in the same package are the
//! same `Arc<Symbol>` — pointer equality is `eq`.
//!
//! Function and value cells live here, but only as forward
//! declarations until eval lands. The function cell is the load-bearing
//! piece for hot redefinition (MANIFESTO.md, "Note: function
//! redefinition and dispatch") — `defun` is a single atomic store into
//! `function`. We use `Mutex<Option<Value>>` provisionally; a lock-free
//! cell replaces it when it shows up in a profiler.
//!
//! Packages are owned by a global `Universe`. This file does not
//! expose mutable static state directly — see `universe.rs`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::value::Value;

pub struct Symbol {
    pub name: Arc<str>,
    /// Home package. `None` for uninterned symbols (`#:foo`).
    pub home: Option<Arc<Package>>,
    /// Function cell. `None` means unbound. The atomic store happens
    /// here. See MANIFESTO.md, "Note: function redefinition".
    pub function: Mutex<Option<Value>>,
    /// Value cell (dynamic / global value).
    pub value: Mutex<Option<Value>>,
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print just the package-qualified name. The default
        // derive(Debug) recurses through `home` into the Package's
        // symbol table, which contains every other Symbol, every
        // one of THEIR packages, and so on — a multi-megabyte
        // dump. Don't do that.
        match &self.home {
            None => write!(f, "Symbol(#:{})", self.name),
            Some(p) => write!(f, "Symbol({}::{})", p.name, self.name),
        }
    }
}

impl Symbol {
    pub fn fresh_uninterned(name: Arc<str>) -> Arc<Symbol> {
        Arc::new(Symbol {
            name,
            home: None,
            function: Mutex::new(None),
            value: Mutex::new(None),
        })
    }

    /// `:keyword` and `t` self-evaluate; we mark that by pre-binding
    /// the value cell. The reader needs this for keywords; the
    /// evaluator (later) will honour it.
    pub fn fresh_self_evaluating(
        name: Arc<str>,
        home: Arc<Package>,
        self_value: Value,
    ) -> Arc<Symbol> {
        Arc::new(Symbol {
            name,
            home: Some(home),
            function: Mutex::new(None),
            value: Mutex::new(Some(self_value)),
        })
    }
}

/// Whether `find-symbol` found the name as `:internal`, `:external`,
/// `:inherited`, or not at all (`None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Internal,
    External,
    Inherited,
}

pub struct Package {
    pub name: Arc<str>,
    pub nicknames: Vec<Arc<str>>,
    /// Internal table: name → (symbol, visibility).
    pub symbols: Mutex<HashMap<Arc<str>, (Arc<Symbol>, Visibility)>>,
    /// Packages this one `:use`s. Lookups fall through these for
    /// inherited symbols.
    pub uses: Vec<Arc<Package>>,
}

impl fmt::Debug for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Likewise — don't recurse through the symbol table.
        f.debug_struct("Package")
            .field("name", &self.name)
            .field("nicknames", &self.nicknames)
            .field("uses", &self.uses.iter().map(|p| &p.name).collect::<Vec<_>>())
            .finish()
    }
}

impl Package {
    pub fn new(name: &str, nicknames: &[&str], uses: Vec<Arc<Package>>) -> Arc<Package> {
        Arc::new(Package {
            name: Arc::from(name),
            nicknames: nicknames.iter().map(|n| Arc::from(*n)).collect(),
            symbols: Mutex::new(HashMap::new()),
            uses,
        })
    }

    /// Find a symbol by name in this package. Walks `:use` packages
    /// for inherited symbols. Returns `None` if not present anywhere.
    pub fn find(&self, name: &str) -> Option<(Arc<Symbol>, Visibility)> {
        if let Some((sym, vis)) = self.symbols.lock().unwrap().get(name) {
            return Some((Arc::clone(sym), *vis));
        }
        for used in &self.uses {
            if let Some((sym, vis)) = used.symbols.lock().unwrap().get(name) {
                if *vis == Visibility::External {
                    return Some((Arc::clone(sym), Visibility::Inherited));
                }
            }
        }
        None
    }

    /// Intern a symbol. If it already exists (locally or inherited),
    /// returns the existing one. Otherwise creates a fresh internal
    /// symbol homed in this package.
    pub fn intern(self: &Arc<Self>, name: &str) -> (Arc<Symbol>, Visibility) {
        if let Some(found) = self.find(name) {
            return found;
        }
        let name_arc: Arc<str> = Arc::from(name);
        let sym = Arc::new(Symbol {
            name: Arc::clone(&name_arc),
            home: Some(Arc::clone(self)),
            function: Mutex::new(None),
            value: Mutex::new(None),
        });
        self.symbols
            .lock()
            .unwrap()
            .insert(name_arc, (Arc::clone(&sym), Visibility::Internal));
        (sym, Visibility::Internal)
    }

    /// Intern and immediately mark as `:external`. Used during package
    /// setup to publish the standard symbol set.
    pub fn intern_external(self: &Arc<Self>, name: &str) -> Arc<Symbol> {
        let (sym, _) = self.intern(name);
        let mut table = self.symbols.lock().unwrap();
        if let Some((s, vis)) = table.get_mut(&sym.name) {
            *vis = Visibility::External;
            let _ = s;
        }
        sym
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_returns_same_symbol_twice() {
        let pkg = Package::new("TEST", &[], vec![]);
        let (a, _) = pkg.intern("FOO");
        let (b, _) = pkg.intern("FOO");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn intern_distinct_names_distinct_symbols() {
        let pkg = Package::new("TEST", &[], vec![]);
        let (a, _) = pkg.intern("FOO");
        let (b, _) = pkg.intern("BAR");
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn use_package_inherits_externals_only() {
        let cl = Package::new("CL", &[], vec![]);
        let foo = cl.intern_external("FOO");
        let bar = cl.intern("BAR").0;

        let user = Package::new("USER", &[], vec![Arc::clone(&cl)]);
        let (found_foo, vis) = user.find("FOO").expect("FOO inherited");
        assert!(Arc::ptr_eq(&found_foo, &foo));
        assert_eq!(vis, Visibility::Inherited);
        assert!(user.find("BAR").is_none(), "internal symbols not inherited");
        let _ = bar;
    }

    #[test]
    fn home_package_set_on_intern() {
        let pkg = Package::new("TEST", &[], vec![]);
        let (sym, _) = pkg.intern("FOO");
        assert!(Arc::ptr_eq(sym.home.as_ref().unwrap(), &pkg));
    }

    #[test]
    fn uninterned_has_no_home() {
        let sym = Symbol::fresh_uninterned(Arc::from("GENSYM-1"));
        assert!(sym.home.is_none());
    }
}
