//! Vendored stub of `new-asm` for MacNCL.
//!
//! The upstream crate generated Win64-ABI x86 assembly text to back
//! the Lisp-facing `defasm` form. On Apple Silicon that output is
//! meaningless, and the real generator is not part of this fork. This
//! module preserves the exact type surface used by `ncl-compiler` and
//! `ncl-llvm` so they compile unchanged; the actual `defasm` codegen
//! path is short-circuited with a Lisp error in
//! `ncl_llvm::jit_compile_asm_proc` on non-x86 targets.
//!
//! A real AAPCS64 (`x0..x7`) backend is deferred work — see
//! docs/PORTING_DESIGN.md §3.1 and Phase 3.

/// Type of a `defasm` parameter (and the word it occupies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmType {
    /// A tagged Lisp word, passed as an integer register.
    Word,
    /// A `double-float`, passed as a float register.
    Float,
    /// 128-bit float aggregate (reserved; treated as float here).
    FQuad,
    /// 256-bit float aggregate (reserved; treated as float here).
    FOct,
}

/// Return type of a `defasm` procedure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmRetType {
    /// No meaningful return; the shim yields `NIL`.
    Void,
    /// A tagged Lisp word in the integer return register.
    Word,
    /// A `double-float` in the float return register.
    Float,
    /// 128-bit float aggregate (reserved; treated as float here).
    FQuad,
    /// 256-bit float aggregate (reserved; treated as float here).
    FOct,
}

/// One named parameter of a `defasm` procedure.
#[derive(Clone, Debug)]
pub struct AsmParam {
    /// Lower-cased parameter name, as referenced by `#name` in the body.
    pub name: String,
    /// The parameter's machine type.
    pub ty: AsmType,
}

/// A complete `defasm` procedure: a native name, its parameters, a
/// return type, and the raw assembly body the user wrote.
#[derive(Clone, Debug)]
pub struct AsmProc {
    /// Mangled, asm-legal label (lower-cased, `-` → `_`).
    pub name: String,
    /// Positional parameters.
    pub params: Vec<AsmParam>,
    /// Return type.
    pub return_type: AsmRetType,
    /// Raw assembly body as written in the `defasm` form.
    pub body: String,
}

/// Stub for the upstream module-level inline-asm string builder.
///
/// The real implementation performed `#param` substitution and wrapped
/// the body in a `.globl name` / Intel-syntax module. In MacNCL the
/// codegen path that would consume this is gated off on non-x86, so
/// this is only reachable on an x86 build; there we return the raw body
/// verbatim so it remains inspectable. It is intentionally *not* a
/// faithful Win64 generator.
pub fn build_module_asm_string(proc: &AsmProc) -> String {
    proc.body.clone()
}
