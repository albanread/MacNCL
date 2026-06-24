//! Info diagram support
//!
//! The info diagram is a simple utility diagram that displays version information.

mod parser;
mod types;

pub use parser::parse;
pub use types::InfoDb;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_handle_info_definition() {
        let result = parse("info");
        assert!(result.is_ok());
    }

    #[test]
    fn should_handle_info_with_show_info() {
        let result = parse("info showInfo");
        assert!(result.is_ok());
        let db = result.unwrap();
        assert!(db.show_info);
    }

    #[test]
    fn should_reject_unsupported_grammar() {
        let result = parse("info unsupported");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unexpected"));
    }
}
