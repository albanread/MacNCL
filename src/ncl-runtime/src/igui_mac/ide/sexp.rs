//! S-expression analysis over a code-point slice.
//!
//! Pure functions the editor uses for Lisp-aware navigation, paren
//! matching, and paredit (slurp/barf/wrap/splice/raise). All positions
//! are code-point offsets into `cps`. String, `;` comment, and `#\c`
//! char-literal contents are skipped so brackets inside them don't count.

#[inline]
fn is_open(c: char) -> bool {
    matches!(c, '(' | '[')
}
#[inline]
fn is_close(c: char) -> bool {
    matches!(c, ')' | ']')
}
#[inline]
fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"' | ';' | '\'' | '`' | ',')
}

/// Advance past whitespace and `;` line comments. Returns the next
/// significant index (or `cps.len()`).
pub fn skip_ws_fwd(cps: &[char], mut i: usize) -> usize {
    while i < cps.len() {
        let c = cps[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == ';' {
            while i < cps.len() && cps[i] != '\n' {
                i += 1;
            }
        } else {
            break;
        }
    }
    i
}

/// Move back over whitespace (comments are not handled backward).
fn skip_ws_bwd(cps: &[char], mut i: usize) -> usize {
    while i > 0 && cps[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Index just past a string literal whose opening quote is at `i`.
fn end_of_string(cps: &[char], i: usize) -> usize {
    let mut j = i + 1;
    while j < cps.len() {
        match cps[j] {
            '\\' => j += 2,
            '"' => return j + 1,
            _ => j += 1,
        }
    }
    cps.len()
}

/// Given the index of an opening bracket, return the index of its
/// matching close, or `None` if unbalanced.
pub fn matching_close(cps: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < cps.len() {
        match cps[i] {
            '"' => {
                i = end_of_string(cps, i);
                continue;
            }
            ';' => {
                while i < cps.len() && cps[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '#' if i + 1 < cps.len() && cps[i + 1] == '\\' => {
                i += 3;
                continue;
            }
            c if is_open(c) => depth += 1,
            c if is_close(c) => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Given the index of a closing bracket, return the index of its
/// matching open.
pub fn matching_open(cps: &[char], close: usize) -> Option<usize> {
    // Re-scan from the start tracking a stack; cheap for editor-sized text.
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < cps.len() {
        match cps[i] {
            '"' => {
                i = end_of_string(cps, i);
                continue;
            }
            ';' => {
                while i < cps.len() && cps[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '#' if i + 1 < cps.len() && cps[i + 1] == '\\' => {
                i += 3;
                continue;
            }
            c if is_open(c) => stack.push(i),
            c if is_close(c) => {
                let o = stack.pop();
                if i == close {
                    return o;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The innermost bracket pair `(open, close)` strictly containing `pos`.
pub fn enclosing(cps: &[char], pos: usize) -> Option<(usize, usize)> {
    let mut stack: Vec<usize> = Vec::new();
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < cps.len() {
        match cps[i] {
            '"' => {
                i = end_of_string(cps, i);
                continue;
            }
            ';' => {
                while i < cps.len() && cps[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '#' if i + 1 < cps.len() && cps[i + 1] == '\\' => {
                i += 3;
                continue;
            }
            c if is_open(c) => stack.push(i),
            c if is_close(c) => {
                if let Some(o) = stack.pop() {
                    // Strictly contains pos if o < pos <= i.
                    if o < pos && pos <= i {
                        match best {
                            Some((bo, _)) if bo >= o => {}
                            _ => best = Some((o, i)),
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    best
}

/// Skip leading reader prefixes (`'` `` ` `` `,` `,@` `#'`) at `i`,
/// returning the index of the prefixed datum.
fn skip_prefix(cps: &[char], mut i: usize) -> usize {
    loop {
        if i >= cps.len() {
            return i;
        }
        match cps[i] {
            '\'' | '`' => i += 1,
            ',' => {
                i += 1;
                if i < cps.len() && cps[i] == '@' {
                    i += 1;
                }
            }
            '#' if i + 1 < cps.len() && cps[i + 1] == '\'' => i += 2,
            _ => return i,
        }
    }
}

/// The range `(start, end)` of the s-expression at or after `pos`, or
/// `None` if there is no datum forward (e.g. only a closing bracket).
pub fn sexp_after(cps: &[char], pos: usize) -> Option<(usize, usize)> {
    let start = skip_ws_fwd(cps, pos);
    if start >= cps.len() || is_close(cps[start]) {
        return None;
    }
    let datum = skip_prefix(cps, start);
    if datum >= cps.len() {
        return None;
    }
    let end = if is_open(cps[datum]) {
        matching_close(cps, datum)? + 1
    } else if cps[datum] == '"' {
        end_of_string(cps, datum)
    } else {
        let mut k = datum;
        while k < cps.len() && !is_delim(cps[k]) {
            k += 1;
        }
        k.max(datum + 1)
    };
    Some((start, end))
}

/// End offset after the sexp following `pos` (cursor motion: forward).
pub fn forward_sexp(cps: &[char], pos: usize) -> Option<usize> {
    sexp_after(cps, pos).map(|(_, e)| e)
}

/// Start offset of the sexp ending at/just before `pos` (cursor motion:
/// backward). Whitespace before `pos` is skipped first.
pub fn backward_sexp(cps: &[char], pos: usize) -> Option<usize> {
    let end = skip_ws_bwd(cps, pos);
    if end == 0 {
        return None;
    }
    let last = end - 1;
    if is_close(cps[last]) {
        let open = matching_open(cps, last)?;
        // Include any reader prefixes immediately before the open.
        Some(prefix_start(cps, open))
    } else if is_open(cps[last]) {
        None
    } else {
        let mut k = last;
        while k > 0 && !is_delim(cps[k - 1]) {
            k -= 1;
        }
        Some(prefix_start(cps, k))
    }
}

/// Walk back over reader prefixes preceding `start`.
fn prefix_start(cps: &[char], mut start: usize) -> usize {
    while start > 0 {
        let p = cps[start - 1];
        if matches!(p, '\'' | '`' | ',' | '@') {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cv(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn matching_brackets() {
        let c = cv("(a (b) c)");
        assert_eq!(matching_close(&c, 0), Some(8));
        assert_eq!(matching_close(&c, 3), Some(5));
        assert_eq!(matching_open(&c, 8), Some(0));
        assert_eq!(matching_open(&c, 5), Some(3));
    }

    #[test]
    fn brackets_in_strings_ignored() {
        let c = cv("(f \")(\" x)");
        assert_eq!(matching_close(&c, 0), Some(9));
    }

    #[test]
    fn enclosing_is_innermost() {
        let c = cv("(a (b c) d)");
        // pos inside (b c)
        assert_eq!(enclosing(&c, 5), Some((3, 7)));
        // pos in the outer list, outside inner
        assert_eq!(enclosing(&c, 9), Some((0, 10)));
    }

    #[test]
    fn forward_over_list_and_atom() {
        let c = cv("foo (bar baz) qux");
        assert_eq!(forward_sexp(&c, 0), Some(3)); // foo
        assert_eq!(forward_sexp(&c, 3), Some(13)); // (bar baz) → exclusive end 13
        assert_eq!(forward_sexp(&c, 13), Some(17)); // qux
    }

    #[test]
    fn backward_over_list_and_atom() {
        let c = cv("foo (bar baz) qux");
        assert_eq!(backward_sexp(&c, 17), Some(14)); // qux
        assert_eq!(backward_sexp(&c, 13), Some(4)); // (bar baz)
        assert_eq!(backward_sexp(&c, 3), Some(0)); // foo
    }

    #[test]
    fn sexp_after_skips_prefix_and_ws() {
        let c = cv("  'foo");
        assert_eq!(sexp_after(&c, 0), Some((2, 6)));
    }
}
