//! Readtable state.
//!
//! Phase 1c only models `readtable-case`. User-installable macro
//! characters (the part exposed by `set-macro-character` etc.) come
//! later; the standard dispatch table is hard-coded in `parser.rs`.

use crate::token::AtomText;

/// `readtable-case` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadtableCase {
    /// Default. Unescaped letters fold to upper-case.
    Upcase,
    /// Unescaped letters fold to lower-case.
    Downcase,
    /// No folding.
    Preserve,
    /// If unescaped letters are uniform-case, invert; otherwise leave
    /// alone. Escaped chars are always preserved.
    Invert,
}

#[derive(Debug, Clone)]
pub struct Readtable {
    pub case: ReadtableCase,
}

impl Default for Readtable {
    fn default() -> Self { Readtable { case: ReadtableCase::Upcase } }
}

impl Readtable {
    /// Apply readtable case to an atom, returning the case-folded
    /// character sequence. Escapes from the atom are preserved
    /// verbatim; the case rule applies to unescaped chars only.
    pub fn fold_atom(&self, atom: &AtomText) -> String {
        match self.case {
            ReadtableCase::Preserve => atom.raw.clone(),
            ReadtableCase::Upcase => fold_with(atom, |c| c.to_ascii_uppercase()),
            ReadtableCase::Downcase => fold_with(atom, |c| c.to_ascii_lowercase()),
            ReadtableCase::Invert => fold_invert(atom),
        }
    }
}

fn fold_with(atom: &AtomText, f: impl Fn(char) -> char) -> String {
    atom.raw
        .chars()
        .zip(atom.escapes.iter())
        .map(|(c, &esc)| if esc { c } else { f(c) })
        .collect()
}

fn fold_invert(atom: &AtomText) -> String {
    let mut any_upper = false;
    let mut any_lower = false;
    for (c, esc) in atom.raw.chars().zip(atom.escapes.iter()) {
        if *esc { continue; }
        if c.is_ascii_uppercase() { any_upper = true; }
        if c.is_ascii_lowercase() { any_lower = true; }
    }
    if any_upper && any_lower {
        // mixed case — preserve
        atom.raw.clone()
    } else if any_upper {
        fold_with(atom, |c| c.to_ascii_lowercase())
    } else if any_lower {
        fold_with(atom, |c| c.to_ascii_uppercase())
    } else {
        atom.raw.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(raw: &str) -> AtomText {
        AtomText { raw: raw.to_string(), escapes: vec![false; raw.chars().count()] }
    }

    fn atom_mixed(parts: &[(char, bool)]) -> AtomText {
        AtomText {
            raw: parts.iter().map(|(c, _)| *c).collect(),
            escapes: parts.iter().map(|(_, e)| *e).collect(),
        }
    }

    #[test]
    fn upcase_default() {
        let rt = Readtable::default();
        assert_eq!(rt.fold_atom(&atom("foo")), "FOO");
        assert_eq!(rt.fold_atom(&atom("Foo")), "FOO");
        assert_eq!(rt.fold_atom(&atom("FOO")), "FOO");
    }

    #[test]
    fn downcase() {
        let rt = Readtable { case: ReadtableCase::Downcase };
        assert_eq!(rt.fold_atom(&atom("FOO")), "foo");
        assert_eq!(rt.fold_atom(&atom("Foo")), "foo");
    }

    #[test]
    fn preserve() {
        let rt = Readtable { case: ReadtableCase::Preserve };
        assert_eq!(rt.fold_atom(&atom("Foo")), "Foo");
    }

    #[test]
    fn upcase_protects_escapes() {
        // |foo| has all chars escaped — should stay lower even in Upcase.
        let rt = Readtable::default();
        let a = atom_mixed(&[('f', true), ('o', true), ('o', true)]);
        assert_eq!(rt.fold_atom(&a), "foo");
    }

    #[test]
    fn invert_uniform_upper_to_lower() {
        let rt = Readtable { case: ReadtableCase::Invert };
        assert_eq!(rt.fold_atom(&atom("FOO")), "foo");
    }

    #[test]
    fn invert_uniform_lower_to_upper() {
        let rt = Readtable { case: ReadtableCase::Invert };
        assert_eq!(rt.fold_atom(&atom("foo")), "FOO");
    }

    #[test]
    fn invert_mixed_preserved() {
        let rt = Readtable { case: ReadtableCase::Invert };
        assert_eq!(rt.fold_atom(&atom("Foo")), "Foo");
    }

    #[test]
    fn invert_no_letters_unchanged() {
        let rt = Readtable { case: ReadtableCase::Invert };
        assert_eq!(rt.fold_atom(&atom("123")), "123");
        assert_eq!(rt.fold_atom(&atom("+-*")), "+-*");
    }

    #[test]
    fn invert_escape_excluded_from_uniformity_check() {
        // F|o|O — unescaped chars are F O (uniform upper), |o| is escaped.
        // Invert flips F→f, O→o; escaped o stays o. Result: "foo".
        let rt = Readtable { case: ReadtableCase::Invert };
        let a = atom_mixed(&[('F', false), ('o', true), ('O', false)]);
        assert_eq!(rt.fold_atom(&a), "foo");
    }
}
