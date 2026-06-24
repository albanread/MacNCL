//! Pie chart diagram support

mod parser;
mod types;

pub use parser::parse;
pub use types::{PieConfig, PieDb};

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> PieDb {
        PieDb::new()
    }

    mod parse_tests {
        use super::*;

        #[test]
        fn should_handle_very_simple_pie() {
            let mut db = setup();
            parse_into(
                r#"pie
      "ash": 100
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_section("ash"), Some(100.0));
        }

        #[test]
        fn should_handle_simple_pie() {
            let mut db = setup();
            parse_into(
                r#"pie
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_show_data() {
            let mut db = setup();
            parse_into(
                r#"pie showData
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert!(db.get_show_data());

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_comments() {
            let mut db = setup();
            parse_into(
                r#"pie
      %% comments
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_title() {
            let mut db = setup();
            parse_into(
                r#"pie title a 60/40 pie
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_diagram_title(), Some("a 60/40 pie"));

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_acc_title() {
            let mut db = setup();
            parse_into(
                r#"pie title a neat chart
      accTitle: a neat acc title
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_diagram_title(), Some("a neat chart"));
            assert_eq!(db.get_acc_title(), Some("a neat acc title"));

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_acc_description() {
            let mut db = setup();
            parse_into(
                r#"pie title a neat chart
      accDescr: a neat description
      "ash" : 60
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_diagram_title(), Some("a neat chart"));
            assert_eq!(db.get_acc_description(), Some("a neat description"));

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_multiline_acc_description() {
            let mut db = setup();
            parse_into(
                r#"pie title a neat chart
      accDescr {
        a neat description
        on multiple lines
      }
      "ash" : 60
      "bat" : 40
    "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_diagram_title(), Some("a neat chart"));
            assert_eq!(
                db.get_acc_description(),
                Some("a neat description\non multiple lines")
            );

            assert_eq!(db.get_section("ash"), Some(60.0));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_handle_simple_pie_with_positive_decimal() {
            let mut db = setup();
            parse_into(
                r#"pie
      "ash" : 60.67
      "bat" : 40
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_section("ash"), Some(60.67));
            assert_eq!(db.get_section("bat"), Some(40.0));
        }

        #[test]
        fn should_reject_negative_decimal() {
            let mut db = setup();
            let result = parse_into(
                r#"pie
        "ash" : -60.67
        "bat" : 40.12
        "#,
                &mut db,
            );

            assert!(result.is_err());
        }

        #[test]
        fn should_handle_zero_slice_value() {
            let mut db = setup();
            parse_into(
                r#"pie title Default text position: Animal adoption
        accTitle: simple pie char demo
        accDescr: pie chart with 3 sections: dogs, cats, rats. Most are dogs.
         "dogs" : 0
        "rats" : 40.12
      "#,
                &mut db,
            )
            .unwrap();

            assert_eq!(db.get_section("dogs"), Some(0.0));
            assert_eq!(db.get_section("rats"), Some(40.12));
        }

        #[test]
        fn should_reject_negative_slice_value_with_message() {
            let mut db = setup();
            let result = parse_into(
                r#"pie title Default text position: Animal adoption
        accTitle: simple pie char demo
        accDescr: pie chart with 3 sections: dogs, cats, rats. Most are dogs.
         "dogs" : -60.67
        "rats" : 40.12
    "#,
                &mut db,
            );

            assert!(result.is_err());
            let err = result.unwrap_err();
            let err_msg = err.to_string();
            assert!(err_msg.contains("dogs"));
            assert!(err_msg.contains("-60.67"));
            assert!(err_msg.contains("Negative values are not allowed"));
        }

        #[test]
        fn should_handle_unsafe_properties() {
            let mut db = setup();
            parse_into(
                r#"pie title Unsafe props test
        "__proto__" : 386
        "constructor" : 85
        "prototype" : 15"#,
                &mut db,
            )
            .unwrap();

            // Verify sections contain the unsafe property names
            let sections = db.get_sections();
            let labels: Vec<&str> = sections.iter().map(|(l, _)| l.as_str()).collect();
            assert!(labels.contains(&"__proto__"));
            assert!(labels.contains(&"constructor"));
            assert!(labels.contains(&"prototype"));
        }
    }

    mod config_tests {
        use super::*;

        #[test]
        fn get_config_returns_default() {
            let db = setup();
            let config = db.get_config();
            assert_eq!(config.text_position, 0.5);
        }
    }

    // Helper function to parse into an existing db
    fn parse_into(input: &str, db: &mut PieDb) -> crate::error::Result<()> {
        parser::parse_into(input, db)
    }
}
