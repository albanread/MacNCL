//! Number parsing.
//!
//! `try_parse_number` is called on every unescaped atom before symbol
//! resolution. If it returns `Some`, the atom IS a number — that
//! decision is independent of readtable case (numbers are
//! case-insensitive by spec).
//!
//! Supported shapes:
//!   * fixnums (i64) — `42`, `-7`
//!   * bignums — integer-shaped literals that overflow i64; carried
//!     as a decimal-string `Value::Bignum` and converted to a
//!     heap-allocated bignum by the lowering pass.
//!   * ratios — `[sign]digits/digits`; carried as a string pair
//!     `Value::Ratio` and reduced by the lowering pass.
//!   * floats (f64) — `3.14`, `1e3`, `1.5d-2`, `1.5s2`.

use ncl_runtime::Value;

/// Try to read `text` as a number. Returns `None` if it's not a
/// number — caller should fall through to symbol resolution.
///
/// Integer-looking inputs that overflow i64 become `Value::Bignum`
/// carrying the literal as a decimal string; the lowering pass
/// converts it to a heap-allocated bignum at compile time.
pub fn try_parse_number(text: &str) -> Option<Value> {
    if let Some(n) = parse_integer(text, 10) {
        return Some(Value::Fixnum(n));
    }
    // Ratio recognition: `[sign]digits/digits`. Tried before
    // float so `3/4` doesn't accidentally hit the float parser
    // (it wouldn't, but order documents intent).
    if let Some(rat) = parse_ratio(text) {
        return Some(rat);
    }
    // Bignum-literal recognition: an integer-shaped text that
    // overflowed parse_integer is still a number.
    if looks_like_integer(text) {
        // Strip trailing `.` if present (the integer-with-dot CL quirk).
        let normalised = if text.ends_with('.') {
            &text[..text.len() - 1]
        } else {
            text
        };
        return Some(Value::Bignum(std::sync::Arc::new(normalised.to_string())));
    }
    if let Some(f) = parse_float(text) {
        return Some(Value::Float(f));
    }
    None
}

/// Recognise a CL ratio literal `[sign]digits/digits`. Both halves
/// must be integer-shaped; the denominator can't carry a sign (CL
/// folds the sign onto the numerator). num-rational simplifies the
/// (num, den) pair on the way through the lowering pass.
fn parse_ratio(text: &str) -> Option<Value> {
    let slash = text.find('/')?;
    let num_part = &text[..slash];
    let den_part = &text[slash + 1..];
    if !looks_like_integer(num_part) || !looks_like_integer(den_part) {
        return None;
    }
    if den_part.starts_with('+') || den_part.starts_with('-') {
        return None;
    }
    if num_part.ends_with('.') || den_part.ends_with('.') {
        return None;
    }
    Some(Value::Ratio(
        std::sync::Arc::new(num_part.to_string()),
        std::sync::Arc::new(den_part.to_string()),
    ))
}

/// True iff TEXT has the shape of a decimal integer literal:
/// optional sign + at least one digit + optional trailing `.`.
fn looks_like_integer(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() { return false; }
    let mut i = 0;
    if bytes[0] == b'+' || bytes[0] == b'-' { i += 1; }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
    if i == digits_start { return false; }
    // Optional trailing `.`
    if i == bytes.len() - 1 && bytes[i] == b'.' { i += 1; }
    i == bytes.len()
}

/// Parse a ratio literal under a non-decimal radix: same shape as
/// [`parse_ratio`] (`[sign]digits/digits`), but each digit is in
/// `radix` (2..=36). Used by the reader's `#b` / `#o` / `#x` / `#nr`
/// dispatch path so `(read "#o-101/75")` produces a Value::Ratio
/// rather than failing.
///
/// Both halves are converted to decimal strings on the way out so
/// downstream consumers (the lowerer's `Value::Ratio → heap-ratio`
/// path) see the same shape as a decimal-literal ratio. The sign,
/// per CL, is folded onto the numerator — `#o-1/2` becomes the
/// ratio with numerator `-1` and denominator `2`.
pub fn parse_ratio_radix(text: &str, radix: u32) -> Option<Value> {
    let slash = text.find('/')?;
    let num_part = &text[..slash];
    let den_part = &text[slash + 1..];
    if num_part.is_empty() || den_part.is_empty() {
        return None;
    }
    // CL: sign is only allowed on the numerator.
    if den_part.starts_with('+') || den_part.starts_with('-') {
        return None;
    }
    let num = parse_integer(num_part, radix)?;
    let den = parse_integer(den_part, radix)?;
    if den == 0 {
        return None;
    }
    Some(Value::Ratio(
        std::sync::Arc::new(num.to_string()),
        std::sync::Arc::new(den.to_string()),
    ))
}

/// Parse an integer in the given radix. Allows optional leading `+`
/// or `-`. For radix 10 only, allows an optional trailing `.` (CL
/// quirk: `123.` is the integer 123, not a float).
pub fn parse_integer(text: &str, radix: u32) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() { return None; }
    let (sign, rest) = match bytes[0] {
        b'+' => (1i64, &text[1..]),
        b'-' => (-1i64, &text[1..]),
        _ => (1i64, text),
    };
    let body = if radix == 10 && rest.ends_with('.') {
        &rest[..rest.len() - 1]
    } else {
        rest
    };
    if body.is_empty() { return None; }
    let mut acc: i64 = 0;
    for c in body.chars() {
        let d = c.to_digit(radix)? as i64;
        acc = acc.checked_mul(radix as i64)?.checked_add(d)?;
    }
    Some(sign.checked_mul(acc)?)
}

/// Parse a float per CL syntax. Must contain either `.` or an
/// exponent marker (`eEdDsSfFlL`) — otherwise it's an integer (or
/// not a number). The exponent marker is normalised to `e` for
/// Rust's `parse::<f64>`.
pub fn parse_float(text: &str) -> Option<f64> {
    let mut iter = text.chars().peekable();
    let mut out = String::with_capacity(text.len());
    let mut has_dot_or_exp = false;

    if let Some(&c) = iter.peek() {
        if c == '+' || c == '-' { out.push(c); iter.next(); }
    }

    let mut digits_before = 0;
    while let Some(&c) = iter.peek() {
        if c.is_ascii_digit() { out.push(c); iter.next(); digits_before += 1; }
        else { break; }
    }

    let mut digits_after = 0;
    if iter.peek() == Some(&'.') {
        out.push('.');
        iter.next();
        has_dot_or_exp = true;
        while let Some(&c) = iter.peek() {
            if c.is_ascii_digit() { out.push(c); iter.next(); digits_after += 1; }
            else { break; }
        }
    }

    if digits_before == 0 && digits_after == 0 {
        return None;
    }

    if let Some(&c) = iter.peek() {
        if matches!(c, 'e' | 'E' | 'd' | 'D' | 's' | 'S' | 'f' | 'F' | 'l' | 'L') {
            // Must have at least one digit before exponent marker.
            if digits_before == 0 && digits_after == 0 { return None; }
            out.push('e');
            iter.next();
            has_dot_or_exp = true;
            if let Some(&sc) = iter.peek() {
                if sc == '+' || sc == '-' { out.push(sc); iter.next(); }
            }
            let mut exp_digits = 0;
            while let Some(&c) = iter.peek() {
                if c.is_ascii_digit() { out.push(c); iter.next(); exp_digits += 1; }
                else { break; }
            }
            if exp_digits == 0 { return None; }
        }
    }

    if !has_dot_or_exp { return None; }
    if iter.next().is_some() { return None; }

    out.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fxn(v: &Value) -> i64 { match v { Value::Fixnum(n) => *n, _ => panic!("not fixnum: {v:?}") } }
    fn flt(v: &Value) -> f64 { match v { Value::Float(f) => *f, _ => panic!("not float: {v:?}") } }

    #[test]
    fn integers() {
        assert_eq!(fxn(&try_parse_number("42").unwrap()), 42);
        assert_eq!(fxn(&try_parse_number("-7").unwrap()), -7);
        assert_eq!(fxn(&try_parse_number("+123").unwrap()), 123);
        assert_eq!(fxn(&try_parse_number("0").unwrap()), 0);
        // CL trailing dot on integer
        assert_eq!(fxn(&try_parse_number("42.").unwrap()), 42);
    }

    #[test]
    fn floats() {
        assert_eq!(flt(&try_parse_number("1.0").unwrap()), 1.0);
        assert_eq!(flt(&try_parse_number(".5").unwrap()), 0.5);
        assert_eq!(flt(&try_parse_number("1e10").unwrap()), 1e10);
        assert_eq!(flt(&try_parse_number("1.5e-3").unwrap()), 1.5e-3);
        assert_eq!(flt(&try_parse_number("-3.14").unwrap()), -3.14);
        // CL exponent markers — d/s/f/l all parse as float; we
        // ignore precision distinctions for now.
        assert_eq!(flt(&try_parse_number("1.0d0").unwrap()), 1.0);
        assert_eq!(flt(&try_parse_number("1.5s2").unwrap()), 150.0);
    }

    #[test]
    fn not_numbers() {
        assert!(try_parse_number("foo").is_none());
        assert!(try_parse_number("").is_none());
        assert!(try_parse_number("3foo").is_none());
        assert!(try_parse_number("1e").is_none());
        assert!(try_parse_number(".").is_none());
        // Sign on the denominator is not a ratio (CL folds sign
        // onto the numerator) — fall through to symbol.
        assert!(try_parse_number("3/-4").is_none());
        // Float-shaped numerator or denominator likewise.
        assert!(try_parse_number("3.0/4").is_none());
    }

    #[test]
    fn ratios_are_recognised() {
        // `Value::Ratio(num, den)` carries both halves as their
        // source decimal strings; the lowering pass reduces.
        match try_parse_number("3/4") {
            Some(Value::Ratio(n, d)) => {
                assert_eq!(&*n, "3");
                assert_eq!(&*d, "4");
            }
            other => panic!("3/4 should be Value::Ratio, got {other:?}"),
        }
        // Negative numerator.
        match try_parse_number("-7/8") {
            Some(Value::Ratio(n, d)) => {
                assert_eq!(&*n, "-7");
                assert_eq!(&*d, "8");
            }
            other => panic!("-7/8 should be Value::Ratio, got {other:?}"),
        }
        // Bignum-sized numerator survives the unreduced ratio path.
        match try_parse_number("100000000000000000000/3") {
            Some(Value::Ratio(n, d)) => {
                assert_eq!(&*n, "100000000000000000000");
                assert_eq!(&*d, "3");
            }
            other => panic!("big ratio should be Value::Ratio, got {other:?}"),
        }
    }

    #[test]
    fn radix() {
        assert_eq!(parse_integer("FF", 16), Some(255));
        assert_eq!(parse_integer("ff", 16), Some(255));
        assert_eq!(parse_integer("777", 8), Some(0o777));
        assert_eq!(parse_integer("1010", 2), Some(10));
        assert_eq!(parse_integer("ZZ", 36), Some(35 * 36 + 35));
        assert_eq!(parse_integer("-FF", 16), Some(-255));
        assert_eq!(parse_integer("FG", 16), None);
    }

    fn rat(v: &Value) -> (&str, &str) {
        match v {
            Value::Ratio(n, d) => (&**n, &**d),
            _ => panic!("not ratio: {v:?}"),
        }
    }

    #[test]
    fn ratio_under_radix() {
        // `#o-101/75` (corman ANSI chapter-2 RATIO-FORMAT block):
        // octal -101 = -65, octal 75 = 61 → -65/61.
        assert_eq!(rat(&parse_ratio_radix("-101/75", 8).unwrap()),
                   ("-65", "61"));
        // `#3r120/21`: ternary 120 = 15, ternary 21 = 7 → 15/7.
        assert_eq!(rat(&parse_ratio_radix("120/21", 3).unwrap()),
                   ("15", "7"));
        // `#Xbc/ad`: hex bc = 188, hex ad = 173 → 188/173.
        assert_eq!(rat(&parse_ratio_radix("bc/ad", 16).unwrap()),
                   ("188", "173"));
        // Positive sign on numerator survives.
        assert_eq!(rat(&parse_ratio_radix("+10/11", 8).unwrap()),
                   ("8", "9"));
        // Sign on denominator: rejected (sign-on-numerator is the CL
        // convention; reader must fall through to the symbol path).
        assert!(parse_ratio_radix("10/-11", 8).is_none());
        // Zero denominator: reject.
        assert!(parse_ratio_radix("10/0", 8).is_none());
        // Missing half: reject.
        assert!(parse_ratio_radix("/3", 8).is_none());
        assert!(parse_ratio_radix("3/", 8).is_none());
        // Out-of-radix digit: reject.
        assert!(parse_ratio_radix("9/2", 8).is_none());
    }
}
