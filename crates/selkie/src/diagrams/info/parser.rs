//! Info diagram parser

use regex::Regex;
use std::sync::LazyLock;

use super::types::InfoDb;
use crate::error::{MermaidError, Result};

static INFO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^info(\s+showInfo)?\s*$").unwrap());

/// Parse an info diagram
pub fn parse(input: &str) -> Result<InfoDb> {
    let mut db = InfoDb::new();
    parse_into(input, &mut db)?;
    Ok(db)
}

/// Parse into an existing database
pub fn parse_into(input: &str, db: &mut InfoDb) -> Result<()> {
    db.clear();

    let trimmed = input.trim();

    if let Some(caps) = INFO_RE.captures(trimmed) {
        db.show_info = caps.get(1).is_some();
        Ok(())
    } else {
        // Find the unexpected character
        let prefix = "info";
        if trimmed.to_lowercase().starts_with(prefix) {
            let rest = &trimmed[prefix.len()..].trim_start();
            if !rest.is_empty() {
                let first_char = rest.chars().next().unwrap();
                return Err(MermaidError::ParseError(format!(
                    "Parsing failed: unexpected character: ->{}<- at offset: {}, skipped {} characters.",
                    first_char,
                    prefix.len(),
                    rest.len()
                )));
            }
        }
        Err(MermaidError::ParseError("Invalid info diagram".to_string()))
    }
}
