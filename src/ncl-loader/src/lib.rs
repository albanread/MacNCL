//! Source module graph, dirty tracking, generations, retirement,
//! execution-scope generation pinning, and the non-canonical artifact
//! cache. Modelled on `newcp-loader`. See MANIFESTO.md, "The loader".
//!
//! Note: function redefinition uses symbol-cell pointer swap, NOT the
//! retirement machinery. See MANIFESTO.md, "Note: function
//! redefinition and dispatch".
