//! Pretty-printer for `Word` values.
//!
//! Used by the driver's `--eval` output and by tests that want a
//! human-readable rendering of a value. Walks heap pointers, so
//! must be called from the Lisp thread (or after stop-the-world).
//! Cycles in cons-graphs would loop forever in this v1 printer —
//! `*print-circle*` and friends land later.

use crate::word::{Tag, Word};
use std::sync::Arc;

/// Format `w` as a CL-style printed value (readable / `prin1`-style):
/// strings get wrapped in `"..."`, characters as `#\X`. This is the
/// form used by the REPL and by the `~S` directive of `format`.
pub fn format_word(w: Word) -> String {
    let mut out = String::new();
    write_word(&mut out, w, /*readable*/ true);
    out
}

/// Format `w` in aesthetic style (`princ`-style): strings without
/// quotes, characters as the bare codepoint. Used by `~A` and by
/// any printing path that wants display formatting rather than
/// reader-roundtrippable formatting.
pub fn format_word_aesthetic(w: Word) -> String {
    let mut out = String::new();
    write_word(&mut out, w, /*readable*/ false);
    out
}

fn write_word(out: &mut String, w: Word, readable: bool) {
    if w.is_nil() {
        out.push_str("nil");
        return;
    }
    if w.is_t() {
        out.push('T');
        return;
    }
    if w.is_unbound() {
        out.push_str("<unbound>");
        return;
    }
    match w.tag() {
        Tag::Fixnum => out.push_str(&w.as_fixnum().unwrap().to_string()),
        Tag::Cons => write_list(out, w, readable),
        Tag::Immediate => {
            if let Some(c) = w.as_char() {
                if readable {
                    out.push_str("#\\");
                    // Use the standard name where one exists, so a
                    // space prints as `#\Space` (not `#\ `) and a
                    // newline as `#\Newline` (not a literal LF).
                    // The reader recognises all of these names
                    // case-insensitively — see
                    // ncl-reader/src/lexer.rs::resolve_char_name —
                    // so the printed form is reader-roundtrippable.
                    if let Some(name) = standard_char_name(c) {
                        out.push_str(name);
                    } else {
                        out.push(c);
                    }
                } else {
                    out.push(c);
                }
            } else {
                out.push_str(&format!("<imm {:#x}>", w.raw()));
            }
        }
        Tag::Forward => out.push_str("<forward>"),
        Tag::Symbol => match crate::sym_names::lookup(w.raw()) {
            Some(name) => out.push_str(&name),
            None => out.push_str("<symbol>"),
        },
        Tag::Vector => write_vector(out, w, readable),
        Tag::Function => out.push_str("<function>"),
        Tag::String => write_string(out, w, readable),
    }
}

/// Print a Vector-tagged Word as `#(elem1 elem2 ...)`. Each
/// element is recursively formatted with the same `readable`
/// flag. Skips the heap header (cell 0) and walks the
/// `length_cells` payload cells.
///
/// Special case: CLOS instances are 4-cell vectors whose first
/// cell is the symbol `%CLOS-INSTANCE`. Their second cell is the
/// metaclass, which is itself a CLOS instance with a circular
/// metaclass back-link — recursing into it would stack-overflow.
/// We render them as `#<NAME>` where NAME is the class's class-
/// name slot, if reachable in two hops without crossing back into
/// CLOS-instance territory.
fn write_vector(out: &mut String, w: Word, readable: bool) {
    let p = match w.as_ptr::<u64>(Tag::Vector) {
        Some(p) => p,
        None => {
            out.push_str("#<bad-vector>");
            return;
        }
    };
    let header = crate::heap::HeapHeader::from_raw(unsafe { *p });
    let n = header.length_cells();

    // Bignum check: HeapType::Bignum in the header.
    if header.ty() == crate::heap::HeapType::Bignum {
        out.push_str(&crate::bignum::bignum_to_decimal(w));
        return;
    }
    // Float check: HeapType::Float in the header.
    if header.ty() == crate::heap::HeapType::Float {
        out.push_str(&crate::float::float_to_string(crate::float::float_value(w)));
        return;
    }
    // Ratio check: HeapType::Ratio.
    if header.ty() == crate::heap::HeapType::Ratio {
        out.push_str(&crate::ratio::ratio_to_string(w));
        return;
    }
    // Complex check: HeapType::Complex.
    if header.ty() == crate::heap::HeapType::Complex {
        out.push_str(&crate::complex::complex_to_string(w));
        return;
    }
    // CLOS-instance check: 4 cells, slot 0 = symbol named "%CLOS-INSTANCE".
    if n == 4 {
        let marker = Word::from_raw(unsafe { *p.add(1) });
        if marker.tag() == Tag::Symbol {
            if let Some(name) = crate::sym_names::lookup(marker.raw()) {
                if &*name == "%CLOS-INSTANCE" {
                    write_clos_instance(out, p);
                    return;
                }
            }
        }
    }

    out.push_str("#(");
    for i in 0..n {
        if i > 0 {
            out.push(' ');
        }
        let cell = unsafe { *p.add(1 + i as usize) };
        write_word(out, Word::from_raw(cell), readable);
    }
    out.push(')');
}

/// Render a CLOS instance as `#<CLASSNAME>` without recursing into
/// its metaclass (which would cycle). Given `p` is the
/// vector-tagged-pointer payload (cell 0 = header, cell 1 = marker,
/// cell 2 = class, cell 3 = slot-storage, cell 4 = signature).
///
/// The class itself is a CLOS instance whose slot[10] is its name
/// (per `*standard-class-slot-names*`). We pull that out directly.
/// If anything looks off we fall back to a bare `#<CLOS-INSTANCE>`.
fn write_clos_instance(out: &mut String, p: *const u64) {
    // cell layout in `p`: [header, marker, class, slots, signature]
    let class = Word::from_raw(unsafe { *p.add(2) });
    let name = clos_class_name(class);
    out.push_str("#<");
    match name {
        Some(s) => out.push_str(&*s),
        None => out.push_str("CLOS-INSTANCE"),
    }
    out.push('>');
}

/// Pull the class-name out of a class (itself a CLOS instance).
/// Returns None if the class doesn't have the expected shape — we
/// silently fall back to a generic label rather than crash. The
/// shape assumed here is the post-bootstrap one defined in
/// `the-defclass-standard-class`: slot vector cell 10 holds the
/// class's name. We walk: class-ptr → slot-storage (vector at
/// cell 3) → vector cell 11 (skip header).
fn clos_class_name(class: Word) -> Option<Arc<str>> {
    let p = class.as_ptr::<u64>(Tag::Vector)?;
    let header = crate::heap::HeapHeader::from_raw(unsafe { *p });
    if header.length_cells() != 4 {
        return None;
    }
    let slots = Word::from_raw(unsafe { *p.add(3) });
    let sp = slots.as_ptr::<u64>(Tag::Vector)?;
    let sheader = crate::heap::HeapHeader::from_raw(unsafe { *sp });
    if sheader.length_cells() < 11 {
        return None;
    }
    let name = Word::from_raw(unsafe { *sp.add(1 + 10) });
    if name.tag() != Tag::Symbol {
        return None;
    }
    crate::sym_names::lookup(name.raw())
}

/// Print a string. Readable form wraps in `"..."` with `\` and `"`
/// escaped; aesthetic form emits the raw characters.
fn write_string(out: &mut String, w: Word, readable: bool) {
    if readable {
        out.push('"');
        for c in crate::gc_string::chars_of(w) {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        for c in crate::gc_string::chars_of(w) {
            out.push(c);
        }
    }
}

/// Print a cons cell as a CL list: `(a b c)` if proper, `(a b . c)`
/// if dotted at the end.
fn write_list(out: &mut String, head: Word, readable: bool) {
    out.push('(');
    let mut cur = head;
    loop {
        let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
        let car = Word::from_raw(unsafe { *p });
        let cdr = Word::from_raw(unsafe { *p.add(1) });
        write_word(out, car, readable);
        if cdr.is_nil() {
            out.push(')');
            return;
        }
        if !cdr.is_cons() {
            out.push_str(" . ");
            write_word(out, cdr, readable);
            out.push(')');
            return;
        }
        out.push(' ');
        cur = cdr;
    }
}

/// Map a character to its standard reader-accepted name, or None
/// if the character doesn't have one. The names here are the
/// canonical CL spellings — the form a reader-roundtrip should
/// produce — paralleling the case-insensitive lookup in
/// `ncl-reader::lexer::resolve_char_name`. The alternative reader
/// spellings (`SP`, `NL`, `LF`, `CR`, `FF`, `BS`, `DEL`, `NUL`,
/// `ESC`) are accepted on input but never emitted.
fn standard_char_name(c: char) -> Option<&'static str> {
    match c {
        ' '    => Some("Space"),
        '\n'   => Some("Newline"),
        '\t'   => Some("Tab"),
        '\r'   => Some("Return"),
        '\x0c' => Some("Page"),
        '\x08' => Some("Backspace"),
        '\x7f' => Some("Rubout"),
        '\x00' => Some("Null"),
        '\x1b' => Some("Escape"),
        _      => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{GcConfig, GcCoordinator};

    fn small_config() -> GcConfig {
        GcConfig {
            young_bytes: 16 * 1024,
            old_bytes: 16 * 1024,
            static_bytes: 8 * 1024,
            tlab_cells: 64,
        }
    }

    #[test]
    fn primitives_print() {
        assert_eq!(format_word(Word::NIL), "nil");
        assert_eq!(format_word(Word::T), "T");
        assert_eq!(format_word(Word::UNBOUND), "<unbound>");
        assert_eq!(format_word(Word::fixnum(0)), "0");
        assert_eq!(format_word(Word::fixnum(42)), "42");
        assert_eq!(format_word(Word::fixnum(-7)), "-7");
        assert_eq!(format_word(Word::char('a')), "#\\a");
    }

    #[test]
    fn standard_char_names_print_with_their_canonical_form() {
        // The nine named characters that round-trip with the reader.
        assert_eq!(format_word(Word::char(' ')),     "#\\Space");
        assert_eq!(format_word(Word::char('\n')),    "#\\Newline");
        assert_eq!(format_word(Word::char('\t')),    "#\\Tab");
        assert_eq!(format_word(Word::char('\r')),    "#\\Return");
        assert_eq!(format_word(Word::char('\x0c')),  "#\\Page");
        assert_eq!(format_word(Word::char('\x08')),  "#\\Backspace");
        assert_eq!(format_word(Word::char('\x7f')),  "#\\Rubout");
        assert_eq!(format_word(Word::char('\x00')),  "#\\Null");
        assert_eq!(format_word(Word::char('\x1b')),  "#\\Escape");
    }

    #[test]
    fn ordinary_characters_keep_raw_form() {
        // Anything outside the named-table prints raw — letters,
        // digits, punctuation, and arbitrary Unicode.
        assert_eq!(format_word(Word::char('A')),     "#\\A");
        assert_eq!(format_word(Word::char('5')),     "#\\5");
        assert_eq!(format_word(Word::char('!')),     "#\\!");
        // Cyrillic letter Г (U+0413) — alphabetic, not a control.
        assert_eq!(format_word(Word::char('\u{0413}')), "#\\\u{0413}");
    }

    #[test]
    fn aesthetic_form_emits_raw_character_regardless_of_name() {
        // princ-style (~A): the raw glyph, no `#\` prefix and no
        // named substitution. A space is just a space.
        assert_eq!(format_word_aesthetic(Word::char(' ')),   " ");
        assert_eq!(format_word_aesthetic(Word::char('\n')),  "\n");
        assert_eq!(format_word_aesthetic(Word::char('A')),   "A");
    }

    #[test]
    fn dotted_cons_prints_with_dot() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let c = m.alloc_cons(Word::fixnum(1), Word::fixnum(2));
        assert_eq!(format_word(c), "(1 . 2)");
    }

    #[test]
    fn proper_list_prints_without_dots() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let c3 = m.alloc_cons(Word::fixnum(3), Word::NIL);
        let c2 = m.alloc_cons(Word::fixnum(2), c3);
        let c1 = m.alloc_cons(Word::fixnum(1), c2);
        assert_eq!(format_word(c1), "(1 2 3)");
    }

    #[test]
    fn list_with_dotted_tail() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // (1 2 . 3)
        let c2 = m.alloc_cons(Word::fixnum(2), Word::fixnum(3));
        let c1 = m.alloc_cons(Word::fixnum(1), c2);
        assert_eq!(format_word(c1), "(1 2 . 3)");
    }

    #[test]
    fn nested_lists_print_recursively() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // ((1 2) 3)
        let inner_tail = m.alloc_cons(Word::fixnum(2), Word::NIL);
        let inner = m.alloc_cons(Word::fixnum(1), inner_tail);
        let outer_tail = m.alloc_cons(Word::fixnum(3), Word::NIL);
        let outer = m.alloc_cons(inner, outer_tail);
        assert_eq!(format_word(outer), "((1 2) 3)");
    }

    #[test]
    fn singleton_list() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let c = m.alloc_cons(Word::fixnum(42), Word::NIL);
        assert_eq!(format_word(c), "(42)");
    }
}
