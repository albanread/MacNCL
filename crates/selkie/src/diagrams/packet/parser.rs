//! Packet diagram parser
//!
//! Parses packet diagrams using pest grammar.

use pest::Parser;
use pest_derive::Parser;

use super::PacketDb;

#[derive(Parser)]
#[grammar = "diagrams/packet/packet.pest"]
struct PacketParser;

/// Parse a packet diagram string into a database
pub fn parse(input: &str) -> Result<PacketDb, Box<dyn std::error::Error>> {
    let pairs = PacketParser::parse(Rule::diagram, input)?;
    let mut db = PacketDb::new();

    for pair in pairs {
        if pair.as_rule() == Rule::diagram {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::document {
                    process_document(inner, &mut db)?;
                }
            }
        }
    }

    Ok(db)
}

fn process_document(
    pair: pest::iterators::Pair<Rule>,
    db: &mut PacketDb,
) -> Result<(), Box<dyn std::error::Error>> {
    for stmt in pair.into_inner() {
        if stmt.as_rule() == Rule::statement {
            process_statement(stmt, db)?;
        }
    }
    Ok(())
}

fn process_statement(
    pair: pest::iterators::Pair<Rule>,
    db: &mut PacketDb,
) -> Result<(), Box<dyn std::error::Error>> {
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::title_stmt => process_title(inner, db),
            Rule::acc_title_stmt => process_acc_title(inner, db),
            Rule::acc_descr_stmt => process_acc_descr(inner, db),
            Rule::acc_descr_multiline_stmt => process_acc_descr_multiline(inner, db),
            Rule::block_stmt => process_block(inner, db)?,
            Rule::comment_stmt => {} // Skip comments
            _ => {}
        }
    }
    Ok(())
}

fn process_title(pair: pest::iterators::Pair<Rule>, db: &mut PacketDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::title_text {
            db.set_title(inner.as_str().trim());
        }
    }
}

fn process_acc_title(pair: pest::iterators::Pair<Rule>, db: &mut PacketDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_title_text {
            db.set_acc_title(inner.as_str().trim());
        }
    }
}

fn process_acc_descr(pair: pest::iterators::Pair<Rule>, db: &mut PacketDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_descr_text {
            db.set_acc_description(inner.as_str().trim());
        }
    }
}

fn process_acc_descr_multiline(pair: pest::iterators::Pair<Rule>, db: &mut PacketDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_descr_multiline_text {
            db.set_acc_description(inner.as_str().trim());
        }
    }
}

fn process_block(
    pair: pest::iterators::Pair<Rule>,
    db: &mut PacketDb,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut label = String::new();
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut bit_count: Option<usize> = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::bit_range => {
                for range_inner in inner.into_inner() {
                    match range_inner.as_rule() {
                        Rule::start_bit => {
                            start = Some(range_inner.as_str().parse()?);
                        }
                        Rule::end_bit => {
                            end = Some(range_inner.as_str().parse()?);
                        }
                        _ => {}
                    }
                }
            }
            Rule::bit_count => {
                for count_inner in inner.into_inner() {
                    if count_inner.as_rule() == Rule::count_value {
                        bit_count = Some(count_inner.as_str().parse()?);
                    }
                }
            }
            Rule::block_label => {
                // Remove surrounding quotes
                let s = inner.as_str();
                label = s[1..s.len() - 1].to_string();
            }
            _ => {}
        }
    }

    // Determine block type and add to database
    if let Some(count) = bit_count {
        // Bit count notation: +N
        db.add_block_by_count(count, &label)?;
    } else if let Some(s) = start {
        // Range or single bit notation
        let e = end.unwrap_or(s); // If no end, single bit (end = start)
        db.add_block(s, e, &label)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_beta_keyword() {
        let input = "packet-beta";
        let result = parse(input);
        assert!(result.is_ok());
        let db = result.unwrap();
        assert!(db.get_packet().is_empty());
    }

    #[test]
    fn test_packet_keyword() {
        let input = "packet";
        let result = parse(input);
        assert!(result.is_ok());
        let db = result.unwrap();
        assert!(db.get_packet().is_empty());
    }

    #[test]
    fn test_with_title_and_accessibility() {
        let input = r#"packet
    title Packet diagram
    accTitle: Packet accTitle
    accDescr: Packet accDescription
    0-10: "test"
    "#;
        let result = parse(input).unwrap();
        assert_eq!(result.get_title(), "Packet diagram");
        assert_eq!(result.get_acc_title(), "Packet accTitle");
        assert_eq!(result.get_acc_description(), "Packet accDescription");

        let packet = result.get_packet();
        assert_eq!(packet.len(), 1);
        assert_eq!(packet[0].len(), 1);
        assert_eq!(packet[0][0].start, 0);
        assert_eq!(packet[0][0].end, 10);
        assert_eq!(packet[0][0].bits, 11);
        assert_eq!(packet[0][0].label, "test");
    }

    #[test]
    fn test_single_bit() {
        let input = r#"packet
    0-10: "test"
    11: "single"
    "#;
        let result = parse(input).unwrap();
        let packet = result.get_packet();

        assert_eq!(packet.len(), 1);
        assert_eq!(packet[0].len(), 2);

        assert_eq!(packet[0][0].start, 0);
        assert_eq!(packet[0][0].end, 10);
        assert_eq!(packet[0][0].bits, 11);
        assert_eq!(packet[0][0].label, "test");

        assert_eq!(packet[0][1].start, 11);
        assert_eq!(packet[0][1].end, 11);
        assert_eq!(packet[0][1].bits, 1);
        assert_eq!(packet[0][1].label, "single");
    }

    #[test]
    fn test_bit_count() {
        let input = r#"packet
    +8: "byte"
    +16: "word"
    "#;
        let result = parse(input).unwrap();
        let packet = result.get_packet();

        assert_eq!(packet.len(), 1);
        assert_eq!(packet[0].len(), 2);

        assert_eq!(packet[0][0].start, 0);
        assert_eq!(packet[0][0].end, 7);
        assert_eq!(packet[0][0].bits, 8);
        assert_eq!(packet[0][0].label, "byte");

        assert_eq!(packet[0][1].start, 8);
        assert_eq!(packet[0][1].end, 23);
        assert_eq!(packet[0][1].bits, 16);
        assert_eq!(packet[0][1].label, "word");
    }

    #[test]
    fn test_split_into_multiple_rows() {
        let input = r#"packet
    0-10: "test"
    11-90: "multiple"
    "#;
        let result = parse(input).unwrap();
        let packet = result.get_packet();

        // Should have 3 rows
        assert_eq!(packet.len(), 3);

        // First row: 0-10 (test) and 11-31 (multiple)
        assert_eq!(packet[0].len(), 2);
        assert_eq!(packet[0][0].start, 0);
        assert_eq!(packet[0][0].end, 10);
        assert_eq!(packet[0][0].label, "test");

        assert_eq!(packet[0][1].start, 11);
        assert_eq!(packet[0][1].end, 31);
        assert_eq!(packet[0][1].label, "multiple");

        // Second row: 32-63 (multiple)
        assert_eq!(packet[1].len(), 1);
        assert_eq!(packet[1][0].start, 32);
        assert_eq!(packet[1][0].end, 63);
        assert_eq!(packet[1][0].label, "multiple");

        // Third row: 64-90 (multiple)
        assert_eq!(packet[2].len(), 1);
        assert_eq!(packet[2][0].start, 64);
        assert_eq!(packet[2][0].end, 90);
        assert_eq!(packet[2][0].label, "multiple");
    }

    #[test]
    fn test_split_at_exact_boundary() {
        let input = r#"packet
    0-16: "test"
    17-63: "multiple"
    "#;
        let result = parse(input).unwrap();
        let packet = result.get_packet();

        assert_eq!(packet.len(), 2);

        // First row
        assert_eq!(packet[0].len(), 2);
        assert_eq!(packet[0][0].start, 0);
        assert_eq!(packet[0][0].end, 16);
        assert_eq!(packet[0][0].bits, 17);

        assert_eq!(packet[0][1].start, 17);
        assert_eq!(packet[0][1].end, 31);
        assert_eq!(packet[0][1].label, "multiple");

        // Second row
        assert_eq!(packet[1].len(), 1);
        assert_eq!(packet[1][0].start, 32);
        assert_eq!(packet[1][0].end, 63);
        assert_eq!(packet[1][0].label, "multiple");
    }

    #[test]
    fn test_not_contiguous_error() {
        let input = r#"packet
    0-16: "test"
    18-20: "error"
    "#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not contiguous"));
    }

    #[test]
    fn test_not_contiguous_with_bit_count() {
        let input = r#"packet
    +16: "test"
    18-20: "error"
    "#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not contiguous"));
    }

    #[test]
    fn test_not_contiguous_single_bit() {
        let input = r#"packet
    0-16: "test"
    18: "error"
    "#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not contiguous"));
    }

    #[test]
    fn test_invalid_range_error() {
        let input = r#"packet
    0-16: "test"
    25-20: "error"
    "#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn test_zero_bit_count_error() {
        let input = r#"packet
    +0: "test"
    "#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("zero bit field"));
    }

    #[test]
    fn test_comment() {
        let input = r#"packet
    %% This is a comment
    0-7: "byte"
    "#;
        let result = parse(input).unwrap();
        let packet = result.get_packet();
        assert_eq!(packet.len(), 1);
        assert_eq!(packet[0][0].label, "byte");
    }

    #[test]
    fn test_multiline_accessibility() {
        let input = r#"packet
    accDescr {
        Packet description
    }
    0-7: "byte"
    "#;
        let result = parse(input).unwrap();
        assert_eq!(result.get_acc_description(), "Packet description");
    }
}
