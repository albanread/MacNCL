//! S-expression reader.
//!
//! Phase 1b (lexer): tokenizer module.
//! Phase 1c (parser): readtable, numbers, parser modules.
//!
//! Top-level entry points: [`read_one`] for a single form,
//! [`read_all`] for every top-level form in a source string.

pub mod lexer;
pub mod numbers;
pub mod parser;
pub mod readtable;
pub mod token;

pub use lexer::{LexError, LexErrorKind, Lexer, LexerIter};
pub use parser::{Parser, ReaderError, ReaderErrorKind};
pub use readtable::{Readtable, ReadtableCase};
pub use token::{AtomText, Span, SpannedToken, Token};

use ncl_runtime::Value;

/// Read every top-level form from `src` using a default-readtable
/// parser whose current package is `COMMON-LISP-USER`.
pub fn read_all(src: &str) -> Result<Vec<Value>, ReaderError> {
    Parser::new(src).read_all()
}

/// Read exactly one form from `src`. Errors on EOF.
pub fn read_one(src: &str) -> Result<Value, ReaderError> {
    Parser::new(src).read_form()
}
