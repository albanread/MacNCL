//! Mindmap types

use crate::common::CommonDb;

/// Node types for mindmap nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeType {
    /// Default: plain text
    #[default]
    Default,
    /// Square brackets: [text]
    Rect,
    /// Parentheses: (text)
    RoundedRect,
    /// Double parentheses: ((text))
    Circle,
    /// Reverse parentheses: )text(
    Cloud,
    /// Double reverse: ))text((
    Bang,
    /// Double curly: {{text}}
    Hexagon,
}

/// A node in a mindmap
#[derive(Debug, Clone, Default)]
pub struct MindmapNode {
    /// Node ID (from syntax like `id[text]`)
    pub node_id: Option<String>,
    /// Display text/description
    pub descr: String,
    /// Node shape type
    pub node_type: NodeType,
    /// Child nodes
    pub children: Vec<MindmapNode>,
    /// Icon name
    pub icon: Option<String>,
    /// CSS classes
    pub class: Option<String>,
}

impl MindmapNode {
    /// Create a new mindmap node
    pub fn new(descr: impl Into<String>) -> Self {
        Self {
            descr: descr.into(),
            ..Default::default()
        }
    }

    /// Create a node with an ID
    pub fn with_id(node_id: impl Into<String>, descr: impl Into<String>) -> Self {
        Self {
            node_id: Some(node_id.into()),
            descr: descr.into(),
            ..Default::default()
        }
    }

    /// Set the node type
    pub fn with_type(mut self, node_type: NodeType) -> Self {
        self.node_type = node_type;
        self
    }

    /// Add a child node
    pub fn add_child(&mut self, child: MindmapNode) {
        self.children.push(child);
    }

    /// Set icon
    pub fn set_icon(&mut self, icon: impl Into<String>) {
        self.icon = Some(icon.into());
    }

    /// Set CSS class
    pub fn set_class(&mut self, class: impl Into<String>) {
        self.class = Some(class.into());
    }
}

/// The mindmap database
#[derive(Debug, Clone, Default)]
pub struct MindmapDb {
    /// Common diagram fields
    common: CommonDb,
    /// Root node of the mindmap
    root: Option<MindmapNode>,
}

impl MindmapDb {
    /// Create a new mindmap database
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the database
    pub fn clear(&mut self) {
        self.common.clear();
        self.root = None;
    }

    /// Set the root node
    pub fn set_root(&mut self, root: MindmapNode) {
        self.root = Some(root);
    }

    /// Get the mindmap (root node)
    pub fn get_mindmap(&self) -> Option<&MindmapNode> {
        self.root.as_ref()
    }

    /// Get mutable reference to mindmap
    pub fn get_mindmap_mut(&mut self) -> Option<&mut MindmapNode> {
        self.root.as_mut()
    }

    // Common DB delegation
    pub fn set_acc_title(&mut self, title: impl Into<String>) {
        self.common.set_acc_title(title);
    }

    pub fn get_acc_title(&self) -> Option<&str> {
        self.common.get_acc_title()
    }

    pub fn set_acc_description(&mut self, desc: impl Into<String>) {
        self.common.set_acc_description(desc);
    }

    pub fn get_acc_description(&self) -> Option<&str> {
        self.common.get_acc_description()
    }

    pub fn set_diagram_title(&mut self, title: impl Into<String>) {
        self.common.set_diagram_title(title);
    }

    pub fn get_diagram_title(&self) -> Option<&str> {
        self.common.get_diagram_title()
    }
}
