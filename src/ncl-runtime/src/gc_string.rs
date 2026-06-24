//! GC-managed String layout — UTF-32, packed two codepoints per cell.
//!
//! See `project_strings_and_chars` in memory and the discussion in
//! the build log. Internal representation is UTF-32; UTF-8 only
//! crosses world boundaries (FFI, file I/O, the printer's output).
//!
//! Layout (`length_cells = 1 + ceil(N/2)`):
//!
//! ```text
//!   cell 0   HeapHeader (type=String, length_cells)
//!   cell 1   char_count: u64 (number of codepoints)
//!   cell 2   chars[0..2]: low 32 = chars[0], high 32 = chars[1]
//!   cell 3   chars[2..4]: low 32 = chars[2], high 32 = chars[3]
//!   ...
//! ```
//!
//! Two codepoints per cell wastes no bits. `aref` is one indexed
//! load + a shift (when the index is odd) + a mask. The trailing
//! half-cell of an odd-length string is zero-padded.

use crate::heap::{HeapHeader, HeapType};
use crate::static_area::StaticArea;
use crate::word::{Tag, Word};

pub const CHAR_COUNT_OFFSET: usize = 1;
/// Cell index where the first codepoint lives.
pub const CHARS_START_OFFSET: usize = 2;

/// Allocate a String in static, with payload copied from a Rust
/// `&str` (which is UTF-8). The string is mutable in CL semantics
/// — `setf aref` can change individual codepoints — but allocation
/// here is one-shot.
pub fn alloc_string_in_static(static_area: &StaticArea, s: &str) -> Option<Word> {
    let chars: Vec<u32> = s.chars().map(|c| c as u32).collect();
    let n = chars.len();
    let payload_cells = 1 + n.div_ceil(2);
    let header_ptr =
        static_area.try_alloc_with_header(HeapType::String, payload_cells as u32)?;
    let p = header_ptr.as_ptr() as *mut u64;
    unsafe { fill_string_payload(p, &chars) };
    Some(Word::from_ptr(p as *const u8, Tag::String))
}

/// Allocate a String on the calling thread's young heap. Used by
/// transient string-producing primitives like `format` with
/// `dest=nil`. Same payload layout as the static version.
pub fn alloc_string_in_young(
    m: &mut crate::mutator::MutatorState,
    s: &str,
) -> Word {
    let chars: Vec<u32> = s.chars().map(|c| c as u32).collect();
    let n = chars.len();
    let payload_cells = 1 + n.div_ceil(2);
    let p = m.alloc_string_buffer(payload_cells as u32);
    unsafe { fill_string_payload(p, &chars) };
    Word::from_ptr(p as *const u8, Tag::String)
}

/// Allocate a String of `n` copies of codepoint `c` on the young
/// heap — the one-shot allocator behind `make-string`. O(n): one
/// allocation plus a packed two-codepoints-per-cell fill. (The
/// previous Lisp-side `make-string` grew the result one
/// `string-append-char` at a time — a full copy of the accumulated
/// string per character, O(n²) — which silently made every
/// "allocate then fill in place" string builder quadratic too.)
pub fn alloc_string_filled(
    m: &mut crate::mutator::MutatorState,
    n: usize,
    c: char,
) -> Word {
    let payload_cells = 1 + n.div_ceil(2);
    let p = m.alloc_string_buffer(payload_cells as u32);
    let cp = c as u32 as u64;
    unsafe {
        *p.add(CHAR_COUNT_OFFSET) = n as u64;
        let packed = cp | (cp << 32);
        for cell in 0..n.div_ceil(2) {
            *p.add(CHARS_START_OFFSET + cell) = packed;
        }
        // Odd length: the trailing half-cell must be zero-padded
        // (see the layout comment at the top of this file).
        if n % 2 == 1 {
            *p.add(CHARS_START_OFFSET + n / 2) = cp;
        }
    }
    Word::from_ptr(p as *const u8, Tag::String)
}

unsafe fn fill_string_payload(p: *mut u64, chars: &[u32]) {
    let n = chars.len();
    unsafe {
        *p.add(CHAR_COUNT_OFFSET) = n as u64;
        // Zero-fill the chars region so the trailing half-cell
        // (when N is odd) starts clean.
        for c in 0..n.div_ceil(2) {
            *p.add(CHARS_START_OFFSET + c) = 0;
        }
        for (i, &cp) in chars.iter().enumerate() {
            pack_char(p, i, cp);
        }
    }
}

/// Read the codepoint count of a String-tagged Word.
pub fn char_count(s: Word) -> usize {
    let p = str_ptr(s);
    unsafe { *p.add(CHAR_COUNT_OFFSET) as usize }
}

/// Read the i-th codepoint as `u32`. Caller is responsible for
/// bounds checking.
pub fn codepoint_at(s: Word, i: usize) -> u32 {
    let p = str_ptr(s);
    unpack_char(p, i)
}

/// Read the i-th codepoint as a Rust `char`. Returns `None` if the
/// stored value happens not to be a valid Unicode scalar (which can
/// only happen if a buggy mutation has stored an invalid codepoint).
pub fn char_at(s: Word, i: usize) -> Option<char> {
    char::from_u32(codepoint_at(s, i))
}

/// Mutate the i-th codepoint. Used by `(setf (aref s i) c)`.
pub fn set_char_at(s: Word, i: usize, cp: u32) {
    let p = str_ptr(s);
    unsafe { pack_char(p, i, cp) };
}

/// Iterate over the string's codepoints as Rust `char`s.
pub fn chars_of(s: Word) -> impl Iterator<Item = char> {
    let n = char_count(s);
    (0..n).map(move |i| {
        char_at(s, i).expect("invalid codepoint in string")
    })
}

/// Compare two strings codepoint-by-codepoint. Used by `string=`.
/// Strings are equal iff they have the same codepoint count and
/// every codepoint matches at the same index.
pub fn string_eq(a: Word, b: Word) -> bool {
    let na = char_count(a);
    let nb = char_count(b);
    if na != nb {
        return false;
    }
    for i in 0..na {
        if codepoint_at(a, i) != codepoint_at(b, i) {
            return false;
        }
    }
    true
}

// -- Internals --------------------------------------------------------------

fn str_ptr(s: Word) -> *mut u64 {
    debug_assert_eq!(s.tag(), Tag::String);
    s.as_mut_ptr::<u64>(Tag::String).expect("string ptr")
}

unsafe fn pack_char(p: *mut u64, i: usize, cp: u32) {
    let cell_idx = CHARS_START_OFFSET + i / 2;
    let cell_p = unsafe { p.add(cell_idx) };
    if i % 2 == 0 {
        // Low 32 bits — clear that half, then OR in the codepoint.
        unsafe {
            let cell = *cell_p;
            *cell_p = (cell & 0xFFFF_FFFF_0000_0000) | (cp as u64);
        }
    } else {
        unsafe {
            let cell = *cell_p;
            *cell_p = (cell & 0x0000_0000_FFFF_FFFF) | ((cp as u64) << 32);
        }
    }
}

fn unpack_char(p: *const u64, i: usize) -> u32 {
    let cell_idx = CHARS_START_OFFSET + i / 2;
    unsafe {
        let cell = *p.add(cell_idx);
        if i % 2 == 0 {
            cell as u32
        } else {
            (cell >> 32) as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fresh_static() -> Arc<StaticArea> {
        Arc::new(StaticArea::new(64 * 1024))
    }

    #[test]
    fn ascii_round_trip() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "hello").unwrap();
        assert_eq!(w.tag(), Tag::String);
        assert_eq!(char_count(w), 5);
        let chars: String = chars_of(w).collect();
        assert_eq!(chars, "hello");
    }

    #[test]
    fn empty_string() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "").unwrap();
        assert_eq!(char_count(w), 0);
        assert_eq!(chars_of(w).count(), 0);
    }

    #[test]
    fn odd_length_string() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "abc").unwrap();
        assert_eq!(char_count(w), 3);
        let chars: String = chars_of(w).collect();
        assert_eq!(chars, "abc");
    }

    #[test]
    fn unicode_round_trip() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "café 🦀 日本").unwrap();
        // 4 + 1 + 1 + 1 + 2 = 9 codepoints (note: "café" with
        // composed é = 4 codepoints; with combining = 5).
        // Rust's chars() iterator returns codepoints; for the
        // composed form, this is 4 chars: c-a-f-é.
        let chars: String = chars_of(w).collect();
        assert_eq!(chars, "café 🦀 日本");
    }

    #[test]
    fn aref_get() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "hello").unwrap();
        assert_eq!(char_at(w, 0), Some('h'));
        assert_eq!(char_at(w, 1), Some('e'));
        assert_eq!(char_at(w, 2), Some('l'));
        assert_eq!(char_at(w, 3), Some('l'));
        assert_eq!(char_at(w, 4), Some('o'));
    }

    #[test]
    fn aref_set() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "hello").unwrap();
        set_char_at(w, 0, 'H' as u32);
        set_char_at(w, 4, '!' as u32);
        let chars: String = chars_of(w).collect();
        assert_eq!(chars, "Hell!");
    }

    #[test]
    fn aref_set_doesnt_disturb_neighbors() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "abcdef").unwrap();
        // Mutate only odd-position chars; even ones must stay.
        set_char_at(w, 1, 'X' as u32);
        set_char_at(w, 3, 'Y' as u32);
        set_char_at(w, 5, 'Z' as u32);
        let chars: String = chars_of(w).collect();
        assert_eq!(chars, "aXcYeZ");
    }

    #[test]
    fn unicode_aref() {
        let s = fresh_static();
        let w = alloc_string_in_static(&s, "🦀café").unwrap();
        assert_eq!(char_at(w, 0), Some('🦀'));
        assert_eq!(char_at(w, 1), Some('c'));
        assert_eq!(char_at(w, 4), Some('é'));
    }

    #[test]
    fn string_eq_basic() {
        let s = fresh_static();
        let a = alloc_string_in_static(&s, "hello").unwrap();
        let b = alloc_string_in_static(&s, "hello").unwrap();
        let c = alloc_string_in_static(&s, "world").unwrap();
        let d = alloc_string_in_static(&s, "hell").unwrap();
        assert!(string_eq(a, a)); // identity
        assert!(string_eq(a, b)); // same content, different addresses
        assert!(!string_eq(a, c));
        assert!(!string_eq(a, d)); // different lengths
    }

    #[test]
    fn string_eq_unicode() {
        let s = fresh_static();
        let a = alloc_string_in_static(&s, "café").unwrap();
        let b = alloc_string_in_static(&s, "café").unwrap();
        assert!(string_eq(a, b));
    }
}
