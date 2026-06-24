//! Common utilities shared across diagram types

mod sanitize;

pub use sanitize::{count_occurrence, parse_generic_types, remove_script, sanitize_text};

/// Common database fields shared across diagram types
#[derive(Debug, Clone, Default)]
pub struct CommonDb {
    /// Accessibility title
    pub acc_title: Option<String>,
    /// Accessibility description
    pub acc_description: Option<String>,
    /// Diagram title
    pub diagram_title: Option<String>,
}

impl CommonDb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.acc_title = None;
        self.acc_description = None;
        self.diagram_title = None;
    }

    pub fn set_acc_title(&mut self, title: impl Into<String>) {
        self.acc_title = Some(title.into());
    }

    pub fn get_acc_title(&self) -> Option<&str> {
        self.acc_title.as_deref()
    }

    pub fn set_acc_description(&mut self, desc: impl Into<String>) {
        self.acc_description = Some(desc.into());
    }

    pub fn get_acc_description(&self) -> Option<&str> {
        self.acc_description.as_deref()
    }

    pub fn set_diagram_title(&mut self, title: impl Into<String>) {
        self.diagram_title = Some(title.into());
    }

    pub fn get_diagram_title(&self) -> Option<&str> {
        self.diagram_title.as_deref()
    }
}
