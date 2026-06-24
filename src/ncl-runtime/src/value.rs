//! Lisp value representation.
//!
//! This is the in-memory shape of every Lisp value the reader, compiler,
//! and runtime hand around. It is deliberately small — the simplicity
//! rule (MANIFESTO.md, "Design values") says no premature variants.
//!
//! Memory model: `Arc` for shared, immutable-by-default substructure.
//! A real GC replaces these `Arc`s later; the public API stays the
//! same. Mutation (`rplaca`, `setf` on a symbol cell, etc.) lives
//! behind interior mutability where needed, not in `Value` itself.
//!
//! `nil` representation: there is one canonical nil — `Value::Nil`.
//! It is the empty list, the only false value, and (semantically) the
//! symbol `COMMON-LISP:NIL`. The reader normalises the token `NIL`
//! (when it would interpret to `COMMON-LISP:NIL`) into `Value::Nil`,
//! which makes `(eq 'nil ())` automatically true without a special
//! case in `eq`.

use std::sync::Arc;

use crate::symbol::Symbol;

/// A Lisp value.
#[derive(Clone, Debug)]
pub enum Value {
    /// The empty list, `nil`, false. Also the symbol `COMMON-LISP:NIL`.
    Nil,
    /// A cons cell. `Arc` is provisional; a real GC replaces this.
    Cons(Arc<Cons>),
    /// An interned symbol. `Arc::ptr_eq` is `eq`.
    Symbol(Arc<Symbol>),
    /// A tagged 63-bit integer in the final representation; for Phase 1
    /// we use the native `i64` directly.
    Fixnum(i64),
    /// An integer too big to fit in the fixnum range. Stored as a
    /// decimal-string representation at the reader/Value level; the
    /// lowering pass converts to a heap-allocated bignum Word via
    /// `ncl_parse_bignum`. (See `ncl-runtime::bignum`.)
    Bignum(Arc<String>),
    /// A rational literal `num/den` from the reader. Both parts
    /// stored as decimal strings so the same Value can carry
    /// arbitrary-precision numerators / denominators. Lowering
    /// converts to a heap or static-area ratio Word via
    /// `ratio::alloc_ratio_in_static`.
    Ratio(Arc<String>, Arc<String>),
    /// IEEE 754 double. Single-floats and ratios land later.
    Float(f64),
    /// A Unicode scalar. CL `character` semantics.
    Char(char),
    /// A simple string. `Arc<String>` is the simplest correct shape;
    /// we'll specialise (UTF-8 vs base-string, mutable vs simple) when
    /// the standard library forces it.
    String(Arc<String>),
    /// A simple vector. Specialised vectors land later.
    Vector(Arc<Vec<Value>>),
    /// A `#!...!#` foreign-declaration block. The reader captures the
    /// header plist and the C body verbatim; the FFI consumes them.
    /// See MANIFESTO.md and `ncl-reader` for syntax.
    FfiBlock(Arc<FfiBlock>),
}

#[derive(Debug)]
pub struct Cons {
    pub car: Value,
    pub cdr: Value,
}

#[derive(Debug)]
pub struct FfiBlock {
    /// Header plist as raw text (e.g. `(:library "ole32" :pascal "WINAPI")`).
    /// Parsed into a real plist later, when the FFI lands.
    pub header: String,
    /// Body C source between `#!` ... `!#`, verbatim, no normalisation.
    pub body: String,
}

impl Value {
    /// Construct a fresh cons cell.
    pub fn cons(car: Value, cdr: Value) -> Value {
        Value::Cons(Arc::new(Cons { car, cdr }))
    }

    /// Build a proper list from an iterator of values.
    pub fn list<I: IntoIterator<Item = Value>>(items: I) -> Value
    where
        I::IntoIter: DoubleEndedIterator,
    {
        items
            .into_iter()
            .rev()
            .fold(Value::Nil, |acc, v| Value::cons(v, acc))
    }

    /// `eq` in the CL sense: object identity for boxed values, value
    /// equality for fixnums/chars/floats. Floats compare by bit
    /// pattern so `(eq 0.0 -0.0)` is false, matching SBCL/Allegro.
    pub fn eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Nil, Value::Nil) => true,
            (Value::Cons(x), Value::Cons(y)) => Arc::ptr_eq(x, y),
            (Value::Symbol(x), Value::Symbol(y)) => Arc::ptr_eq(x, y),
            (Value::Fixnum(x), Value::Fixnum(y)) => x == y,
            (Value::Bignum(x), Value::Bignum(y)) => x == y,
            (Value::Ratio(xn, xd), Value::Ratio(yn, yd)) => xn == yn && xd == yd,
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            (Value::Char(x), Value::Char(y)) => x == y,
            (Value::String(x), Value::String(y)) => Arc::ptr_eq(x, y),
            (Value::Vector(x), Value::Vector(y)) => Arc::ptr_eq(x, y),
            (Value::FfiBlock(x), Value::FfiBlock(y)) => Arc::ptr_eq(x, y),
            _ => false,
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_is_nil() {
        assert!(Value::Nil.is_nil());
        assert!(Value::eq(&Value::Nil, &Value::Nil));
    }

    #[test]
    fn cons_eq_is_identity() {
        let a = Value::cons(Value::Fixnum(1), Value::Nil);
        let b = a.clone();
        let c = Value::cons(Value::Fixnum(1), Value::Nil);
        assert!(Value::eq(&a, &b), "clones share the Arc");
        assert!(!Value::eq(&a, &c), "fresh cons cells are not eq");
    }

    #[test]
    fn fixnum_eq_is_value() {
        assert!(Value::eq(&Value::Fixnum(42), &Value::Fixnum(42)));
        assert!(!Value::eq(&Value::Fixnum(42), &Value::Fixnum(43)));
    }

    #[test]
    fn float_eq_is_bitwise() {
        assert!(Value::eq(&Value::Float(1.0), &Value::Float(1.0)));
        assert!(!Value::eq(&Value::Float(0.0), &Value::Float(-0.0)));
        let nan = Value::Float(f64::NAN);
        assert!(Value::eq(&nan, &nan), "nan eq nan by bit pattern");
    }

    #[test]
    fn list_builds_proper_list() {
        let lst = Value::list([Value::Fixnum(1), Value::Fixnum(2), Value::Fixnum(3)]);
        let Value::Cons(c1) = &lst else { panic!("not a cons") };
        assert!(matches!(c1.car, Value::Fixnum(1)));
        let Value::Cons(c2) = &c1.cdr else { panic!("not a cons") };
        assert!(matches!(c2.car, Value::Fixnum(2)));
        let Value::Cons(c3) = &c2.cdr else { panic!("not a cons") };
        assert!(matches!(c3.car, Value::Fixnum(3)));
        assert!(c3.cdr.is_nil());
    }
}
