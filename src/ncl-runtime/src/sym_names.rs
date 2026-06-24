//! Process-global registry of symbol names.
//!
//! Symbol objects in the static area carry a `name` cell, but for
//! v1 we don't yet allocate strings in the heap (string allocation
//! is its own commit). So `intern` registers the name here in
//! parallel with the coordinator's intern table; the printer reads
//! from this registry to render symbol Words as their names.
//!
//! When real string-in-static lands, this module goes away — the
//! Symbol's name cell becomes the source of truth.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

static REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<str>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<u64, Arc<str>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a name for the given Symbol-tagged Word's raw bits.
pub fn register(word_raw: u64, name: Arc<str>) {
    registry().lock().unwrap().insert(word_raw, name);
}

/// Look up the name registered for this Word, if any.
pub fn lookup(word_raw: u64) -> Option<Arc<str>> {
    registry().lock().unwrap().get(&word_raw).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup_round_trip() {
        let raw = 0x1234_5678_u64;
        // Use a unique pattern to avoid collisions with other tests.
        register(raw, Arc::from("MY-SYM"));
        assert_eq!(lookup(raw).as_deref(), Some("MY-SYM"));
    }

    #[test]
    fn unknown_raw_returns_none() {
        assert!(lookup(0x9999_9999_8888_8888_u64).is_none());
    }
}
