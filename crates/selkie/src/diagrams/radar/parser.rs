//! Radar diagram parser
//!
//! Parses radar diagrams using pest grammar.

use pest::Parser;
use pest_derive::Parser;

use super::types::{RadarAxis, RadarDb, RadarEntry};

#[derive(Parser)]
#[grammar = "diagrams/radar/radar.pest"]
struct RadarParser;

/// Parse a radar diagram string into a database
pub fn parse(input: &str) -> Result<RadarDb, Box<dyn std::error::Error>> {
    let pairs = RadarParser::parse(Rule::diagram, input)?;
    let mut db = RadarDb::new();

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
    db: &mut RadarDb,
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
    db: &mut RadarDb,
) -> Result<(), Box<dyn std::error::Error>> {
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::title_stmt => process_title(inner, db),
            Rule::acc_title_stmt => process_acc_title(inner, db),
            Rule::acc_descr_stmt => process_acc_descr(inner, db),
            Rule::acc_descr_multiline_stmt => process_acc_descr_multiline(inner, db),
            Rule::axis_stmt => process_axis_stmt(inner, db),
            Rule::curve_stmt => process_curve_stmt(inner, db)?,
            Rule::option_stmt => process_option_stmt(inner, db),
            Rule::comment_stmt => {} // Skip comments
            _ => {}
        }
    }
    Ok(())
}

fn process_title(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::title_text {
            db.set_title(inner.as_str().trim());
        }
    }
}

fn process_acc_title(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_title_text {
            db.set_acc_title(inner.as_str().trim());
        }
    }
}

fn process_acc_descr(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_descr_text {
            db.set_acc_description(inner.as_str().trim());
        }
    }
}

fn process_acc_descr_multiline(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::acc_descr_multiline_text {
            db.set_acc_description(inner.as_str().trim());
        }
    }
}

fn process_axis_stmt(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::axis {
            let axis = process_axis(inner);
            db.add_axis(axis);
        }
    }
}

fn process_axis(pair: pest::iterators::Pair<Rule>) -> RadarAxis {
    let mut name = String::new();
    let mut label: Option<String> = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::axis_name => {
                name = inner.as_str().to_string();
            }
            Rule::axis_label => {
                for label_inner in inner.into_inner() {
                    if label_inner.as_rule() == Rule::axis_label_text {
                        let text = label_inner.as_str();
                        // Remove surrounding quotes if present
                        label = Some(if text.starts_with('"') && text.ends_with('"') {
                            text[1..text.len() - 1].to_string()
                        } else {
                            text.to_string()
                        });
                    }
                }
            }
            _ => {}
        }
    }

    match label {
        Some(l) => RadarAxis::with_label(&name, &l),
        None => RadarAxis::new(&name),
    }
}

fn process_curve_stmt(
    pair: pest::iterators::Pair<Rule>,
    db: &mut RadarDb,
) -> Result<(), Box<dyn std::error::Error>> {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::curve {
            process_curve(inner, db)?;
        }
    }
    Ok(())
}

fn process_curve(
    pair: pest::iterators::Pair<Rule>,
    db: &mut RadarDb,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut name = String::new();
    let mut label: Option<String> = None;
    let mut simple_entries: Vec<f64> = Vec::new();
    let mut detailed_entries: Vec<RadarEntry> = Vec::new();
    let mut is_detailed = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::curve_name => {
                name = inner.as_str().to_string();
            }
            Rule::curve_label => {
                for label_inner in inner.into_inner() {
                    if label_inner.as_rule() == Rule::curve_label_text {
                        let text = label_inner.as_str();
                        // Remove surrounding quotes if present
                        label = Some(if text.starts_with('"') && text.ends_with('"') {
                            text[1..text.len() - 1].to_string()
                        } else {
                            text.to_string()
                        });
                    }
                }
            }
            Rule::entries => {
                for entry_inner in inner.into_inner() {
                    match entry_inner.as_rule() {
                        Rule::number_entry => {
                            for val in entry_inner.into_inner() {
                                if val.as_rule() == Rule::entry_value {
                                    simple_entries.push(val.as_str().parse()?);
                                }
                            }
                        }
                        Rule::detailed_entry => {
                            is_detailed = true;
                            let mut axis: Option<String> = None;
                            let mut value: f64 = 0.0;
                            for detail_inner in entry_inner.into_inner() {
                                match detail_inner.as_rule() {
                                    Rule::entry_axis => {
                                        axis = Some(detail_inner.as_str().to_string());
                                    }
                                    Rule::entry_value => {
                                        value = detail_inner.as_str().parse()?;
                                    }
                                    _ => {}
                                }
                            }
                            detailed_entries.push(RadarEntry { axis, value });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if is_detailed {
        db.add_curve_with_axis_refs(&name, label.as_deref(), detailed_entries)?;
    } else {
        db.add_curve(&name, label.as_deref(), simple_entries);
    }

    Ok(())
}

fn process_option_stmt(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::option {
            process_option(inner, db);
        }
    }
}

fn process_option(pair: pest::iterators::Pair<Rule>, db: &mut RadarDb) {
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::show_legend_opt => {
                for val in inner.into_inner() {
                    if val.as_rule() == Rule::boolean_value {
                        db.set_option("showLegend", val.as_str());
                    }
                }
            }
            Rule::ticks_opt => {
                for val in inner.into_inner() {
                    if val.as_rule() == Rule::number_value {
                        db.set_option("ticks", val.as_str());
                    }
                }
            }
            Rule::max_opt => {
                for val in inner.into_inner() {
                    if val.as_rule() == Rule::number_value {
                        db.set_option("max", val.as_str());
                    }
                }
            }
            Rule::min_opt => {
                for val in inner.into_inner() {
                    if val.as_rule() == Rule::number_value {
                        db.set_option("min", val.as_str());
                    }
                }
            }
            Rule::graticule_opt => {
                for val in inner.into_inner() {
                    if val.as_rule() == Rule::graticule_value {
                        db.set_option("graticule", val.as_str());
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagrams::radar::types::Graticule;

    #[test]
    fn test_simple_radar() {
        let input = r#"radar-beta
    axis A,B,C
    curve mycurve{1,2,3}"#;
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_radar_with_colon() {
        let input = r#"radar-beta:
    axis A,B,C
    curve mycurve{1,2,3}"#;
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_diagram_with_title_and_accessibility() {
        let input = r#"radar-beta
    title Radar diagram
    accTitle: Radar accTitle
    accDescr: Radar accDescription
    axis A["Axis A"], B["Axis B"] ,C["Axis C"]
    curve mycurve["My Curve"]{1,2,3}
    "#;
        let result = parse(input).unwrap();
        assert_eq!(result.get_title(), "Radar diagram");
        assert_eq!(result.get_acc_title(), "Radar accTitle");
        assert_eq!(result.get_acc_description(), "Radar accDescription");

        let axes = result.get_axes();
        assert_eq!(axes.len(), 3);
        assert_eq!(axes[0].name, "A");
        assert_eq!(axes[0].label, "Axis A");
        assert_eq!(axes[1].name, "B");
        assert_eq!(axes[1].label, "Axis B");
        assert_eq!(axes[2].name, "C");
        assert_eq!(axes[2].label, "Axis C");

        let curves = result.get_curves();
        assert_eq!(curves.len(), 1);
        assert_eq!(curves[0].name, "mycurve");
        assert_eq!(curves[0].label, "My Curve");
        assert_eq!(curves[0].entries, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_radar_with_options() {
        let input = r#"radar-beta
    ticks 10
    showLegend false
    graticule polygon
    min 1
    max 10
    "#;
        let result = parse(input).unwrap();
        let options = result.get_options();
        assert!(!options.show_legend);
        assert_eq!(options.ticks, 10);
        assert_eq!(options.min, 1.0);
        assert_eq!(options.max, Some(10.0));
        assert_eq!(options.graticule, Graticule::Polygon);
    }

    #[test]
    fn test_curve_with_detailed_entries() {
        let input = r#"radar-beta
    axis A,B,C
    curve mycurve{ C: 3, A: 1, B: 2 }"#;
        let result = parse(input).unwrap();
        let curves = result.get_curves();
        // Values should be ordered by axis order: A, B, C
        assert_eq!(curves[0].entries, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_radar_with_comments() {
        let input = r#"radar-beta
    %% This is a comment
    axis A,B,C
    %% This is another comment
    curve mycurve{1,2,3}
    "#;
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_multiple_curves() {
        let input = r#"radar-beta
    axis A,B,C
    curve curve1{1,2,3}
    curve curve2{4,5,6}"#;
        let result = parse(input).unwrap();
        let curves = result.get_curves();
        assert_eq!(curves.len(), 2);
        assert_eq!(curves[0].name, "curve1");
        assert_eq!(curves[1].name, "curve2");
    }

    #[test]
    fn test_axis_without_labels() {
        let input = r#"radar-beta
    axis Speed, Power, Agility"#;
        let result = parse(input).unwrap();
        let axes = result.get_axes();
        assert_eq!(axes.len(), 3);
        assert_eq!(axes[0].name, "Speed");
        assert_eq!(axes[0].label, "Speed"); // Label defaults to name
    }

    #[test]
    fn test_default_options() {
        let input = r#"radar-beta
    axis A,B,C
    curve c{1,2,3}"#;
        let result = parse(input).unwrap();
        let options = result.get_options();
        assert!(options.show_legend);
        assert_eq!(options.ticks, 5);
        assert_eq!(options.min, 0.0);
        assert_eq!(options.max, None);
        assert_eq!(options.graticule, Graticule::Circle);
    }

    #[test]
    fn test_decimal_values() {
        let input = r#"radar-beta
    axis A,B,C
    curve c{1.5,2.7,3.9}"#;
        let result = parse(input).unwrap();
        let curves = result.get_curves();
        assert_eq!(curves[0].entries, vec![1.5, 2.7, 3.9]);
    }

    #[test]
    fn test_leading_comment() {
        let input = r#"%% Comment before keyword
radar-beta
    axis A,B,C
    curve c{1,2,3}"#;
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_multiline_accessibility() {
        let input = r#"radar-beta
    accDescr {
        This is a multiline
        accessibility description
    }
    axis A,B,C
    curve c{1,2,3}"#;
        let result = parse(input).unwrap();
        assert!(result.get_acc_description().contains("multiline"));
    }

    // Cypress test diagrams
    #[test]
    fn test_cypress_radar_1() {
        let input = r#"radar-beta
                title Best Radar Ever
                axis A, B, C
                curve c1{1, 2, 3}"#;
        let result = parse(input);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
    }

    #[test]
    fn test_cypress_radar_2() {
        let input = r#"radar-beta
                title Best Radar Ever
                axis A, B, C
                curve c1{1, 2, 3}
                curve c2{2, 3, 1}"#;
        let result = parse(input);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
    }

    #[test]
    fn test_cypress_radar_3_complex() {
        let input = r#"radar-beta
                title My favorite ninjas
                axis Agility, Speed, Strength
                axis Stam["Stamina"] , Intel["Intelligence"]

                curve Ninja1["Naruto Uzumaki"]{
                    Agility 2, Speed 2,
                    Strength 3, Stam 5,
                    Intel 0
                }
                curve Ninja2["Sasuke"]{2, 3, 4, 1, 5}
                curve Ninja3 {3, 2, 1, 5, 4}

                showLegend true
                ticks 3
                max 8
                min 0
                graticule polygon"#;
        let result = parse(input);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
    }

    #[test]
    fn test_cypress_radar_4() {
        let input = r#"radar-beta
                title Best Radar Ever
                axis A,B,C
                curve mycurve{1,2,3}"#;
        let result = parse(input);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
    }

    #[test]
    fn test_cypress_radar_5() {
        let input = r#"radar-beta
                axis A,B,C
                curve mycurve{1,2,3}"#;
        let result = parse(input);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
    }
}
