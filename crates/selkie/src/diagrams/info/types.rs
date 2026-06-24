//! Info diagram types

/// The info diagram database
#[derive(Debug, Clone, Default)]
pub struct InfoDb {
    /// Whether to show info
    pub show_info: bool,
}

impl InfoDb {
    /// Create a new info database
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the database
    pub fn clear(&mut self) {
        self.show_info = false;
    }
}
