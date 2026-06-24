//! Rope-based text buffer for the iGui ledit editor.
//!
//! Replacement storage for ledit's current `Vec<String>` (one
//! `String` per line). That representation is O(n) per insert past
//! the first few KB and quadratic when files have many lines. This
//! rope is a weight-balanced binary tree (AVL invariant) over
//! chunked UTF-32 leaves; insert/delete/line-lookup are all
//! O(log n) for files up to 250K+ lines.
//!
//! Ported from the sister winscheme repo's
//! `src/editor/ed_buffer.zig` (Zig). The port is faithful in shape
//! but tightens the AVL join algorithm — see `join_nodes` below
//! for the bug the Zig source still has under repeated split+join
//! cycles, and the fix here.
//!
//! Design
//! ------
//!   * Leaves hold up to `LEAF_MAX` (2048) code points each.
//!   * Internal (branch) nodes cache: left-subtree length, total
//!     length, total newline count, and subtree height.
//!   * Joins of two small leaves merge into one leaf when both
//!     fit under `LEAF_MAX`.
//!   * Operations are non-mutating at the node level — split/join
//!     produce fresh sub-trees and the old ones drop naturally
//!     (no manual allocator/free, no `destroy`, no leak risk).
//!   * No `unsafe` anywhere in this file (see `forbid` below).
//!
//! All indices are 0-based code point offsets unless noted.
//!
//! Once ledit switches its line store to `RopeBuffer`, the giant
//! `Vec<String>` shuffling around `insert_char` / `delete_forward`
//! / `join_with_previous_line` goes away and pasting a 100k-line
//! file becomes instant.

#![allow(dead_code)] // some accessors are for ledit's upcoming wiring
#![forbid(unsafe_code)]

// ─── Configuration ──────────────────────────────────────────────────────

/// Maximum code points per leaf node.
pub const LEAF_MAX: usize = 2048;

/// Minimum code points per leaf before merging with a sibling.
/// (Currently unused — join always merges when the combined size
/// fits in `LEAF_MAX`. Kept for future rebalance heuristics.)
pub const LEAF_MIN: usize = 512;

/// The newline code point. Matches `'\n'`.
pub const NEWLINE: u32 = 0x0A;

// ─── Node ───────────────────────────────────────────────────────────────

/// A node in the rope tree.
#[derive(Debug)]
pub enum Node {
    /// Leaf — stores a contiguous slice of code points and a cached
    /// newline count.
    Leaf {
        /// Code points in this leaf. Length is up to `LEAF_MAX`.
        buf: Vec<u32>,
        /// Number of `NEWLINE` values in `buf`. Recomputed on
        /// construction.
        newline_count: usize,
    },
    /// Internal node joining two subtrees.
    Branch {
        /// Left subtree.
        left: Box<Node>,
        /// Right subtree.
        right: Box<Node>,
        /// Total code points in the left subtree (the "weight" in a
        /// weight-balanced rope).
        left_len: usize,
        /// Total code points across left + right.
        total_len: usize,
        /// Total newlines across left + right.
        total_newlines: usize,
        /// `1 + max(left.height, right.height)`.
        height: u32,
    },
}

impl Node {
    /// Create a leaf carrying `buf`. The newline count is computed.
    fn leaf(buf: Vec<u32>) -> Box<Node> {
        let newline_count = count_newlines(&buf);
        Box::new(Node::Leaf {
            buf,
            newline_count,
        })
    }

    /// Create an empty leaf.
    fn empty_leaf() -> Box<Node> {
        Self::leaf(Vec::new())
    }

    /// Create a branch joining `left` and `right`. Cached metrics
    /// are computed from the children.
    fn branch(left: Box<Node>, right: Box<Node>) -> Box<Node> {
        let l_len = left.text_len();
        let r_len = right.text_len();
        let l_nl = left.newline_count();
        let r_nl = right.newline_count();
        let l_h = left.height();
        let r_h = right.height();
        Box::new(Node::Branch {
            left,
            right,
            left_len: l_len,
            total_len: l_len + r_len,
            total_newlines: l_nl + r_nl,
            height: 1 + l_h.max(r_h),
        })
    }

    /// Total code points in this subtree.
    pub fn text_len(&self) -> usize {
        match self {
            Node::Leaf { buf, .. } => buf.len(),
            Node::Branch { total_len, .. } => *total_len,
        }
    }

    /// Total newline count in this subtree.
    pub fn newline_count(&self) -> usize {
        match self {
            Node::Leaf { newline_count, .. } => *newline_count,
            Node::Branch { total_newlines, .. } => *total_newlines,
        }
    }

    /// Total line count (newlines + 1; the last line may not end
    /// with `\n`).
    pub fn line_count(&self) -> usize {
        self.newline_count() + 1
    }

    /// Subtree height (0 for a leaf).
    pub fn height(&self) -> u32 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Branch { height, .. } => *height,
        }
    }

    /// AVL balance factor: `left.height - right.height`.
    fn balance_factor(&self) -> i32 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Branch { left, right, .. } => {
                left.height() as i32 - right.height() as i32
            }
        }
    }
}

fn count_newlines(buf: &[u32]) -> usize {
    buf.iter().filter(|&&cp| cp == NEWLINE).count()
}

// ─── RopeBuffer (public API) ────────────────────────────────────────────

/// A rope-based text buffer storing UTF-32 code points.
#[derive(Debug)]
pub struct RopeBuffer {
    root: Box<Node>,
}

impl Default for RopeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl RopeBuffer {
    /// Create an empty rope.
    pub fn new() -> Self {
        RopeBuffer {
            root: Node::empty_leaf(),
        }
    }

    /// Create a rope from a slice of code points.
    pub fn from_slice(text: &[u32]) -> Self {
        RopeBuffer {
            root: build_rope_from_slice(text),
        }
    }

    /// Create a rope by decoding a UTF-8 byte string. Line endings
    /// are normalised: `\r\n` and lone `\r` both become `\n`. A
    /// leading UTF-8 BOM is skipped. Invalid UTF-8 sequences are
    /// replaced with U+FFFD.
    pub fn from_utf8(utf8: &[u8]) -> Self {
        Self::from_slice(&utf8_to_codepoints(utf8))
    }

    /// Total number of code points in the buffer.
    pub fn len(&self) -> usize {
        self.root.text_len()
    }

    /// True iff the buffer holds zero code points.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total number of lines (newline_count + 1).
    pub fn line_count(&self) -> usize {
        self.root.line_count()
    }

    /// Total number of newline characters.
    pub fn newline_count(&self) -> usize {
        self.root.newline_count()
    }

    /// Subtree height (0 for an empty rope or a single leaf).
    /// Surfaces the AVL invariant for tests; not part of the
    /// editing API.
    pub fn height(&self) -> u32 {
        self.root.height()
    }

    /// Get the code point at `pos`. `None` if out of bounds.
    pub fn char_at(&self, pos: usize) -> Option<u32> {
        if pos >= self.len() {
            None
        } else {
            Some(char_at_node(&self.root, pos))
        }
    }

    /// Insert code points at `pos`.
    pub fn insert(&mut self, pos: usize, text: &[u32]) {
        if text.is_empty() {
            return;
        }
        let insertion = build_rope_from_slice(text);
        let root = std::mem::replace(&mut self.root, Node::empty_leaf());
        self.root = insert_node(root, pos, insertion);
    }

    /// Insert a single code point at `pos`.
    pub fn insert_char(&mut self, pos: usize, cp: u32) {
        self.insert(pos, &[cp]);
    }

    /// Insert a UTF-8 byte string at `pos`. The input is decoded and
    /// normalised the same way `from_utf8` does.
    pub fn insert_utf8(&mut self, pos: usize, utf8: &[u8]) {
        let cps = utf8_to_codepoints(utf8);
        self.insert(pos, &cps);
    }

    /// Delete `count` code points starting at `pos`. If the range
    /// extends past the end of the buffer, only the in-bounds
    /// portion is removed.
    pub fn delete(&mut self, pos: usize, count: usize) {
        if count == 0 || pos >= self.len() {
            return;
        }
        let actual = count.min(self.len() - pos);
        let root = std::mem::replace(&mut self.root, Node::empty_leaf());
        self.root = delete_range(root, pos, actual);
    }

    /// Replace `count` code points starting at `pos` with `text`.
    pub fn replace(&mut self, pos: usize, count: usize, text: &[u32]) {
        self.delete(pos, count);
        self.insert(pos, text);
    }

    /// Code point offset where `line_idx` starts (0-based). `None`
    /// if the line index is past the end.
    pub fn line_start(&self, line_idx: usize) -> Option<usize> {
        if line_idx == 0 {
            return Some(0);
        }
        if line_idx >= self.line_count() {
            return None;
        }
        nth_newline_pos(&self.root, line_idx - 1)
    }

    /// `(start, end)` for `line_idx` (0-based). `end` does NOT
    /// include the trailing newline (if any). `None` if the line
    /// index is past the end.
    pub fn line_range(&self, line_idx: usize) -> Option<(usize, usize)> {
        let total = self.line_count();
        if line_idx >= total {
            return None;
        }
        let start = if line_idx == 0 {
            0
        } else {
            nth_newline_pos(&self.root, line_idx - 1)?
        };
        let end = if line_idx + 1 < total {
            nth_newline_pos_raw(&self.root, line_idx).unwrap_or_else(|| self.len())
        } else {
            self.len()
        };
        Some((start, end))
    }

    /// Extract a range of code points into a freshly allocated
    /// `Vec`. Out-of-range bounds are clamped to the buffer length.
    pub fn slice(&self, start: usize, end: usize) -> Vec<u32> {
        let total = self.len();
        let s = start.min(total);
        let e = end.min(total);
        if s >= e {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(e - s);
        collect_range(&self.root, s, e, &mut out);
        out
    }

    /// Extract a single line as a freshly allocated code-point
    /// vector. The trailing newline is not included.
    pub fn get_line(&self, line_idx: usize) -> Vec<u32> {
        match self.line_range(line_idx) {
            Some((s, e)) => self.slice(s, e),
            None => Vec::new(),
        }
    }

    /// Encode the entire buffer as UTF-8.
    pub fn to_utf8(&self) -> String {
        let cps = self.slice(0, self.len());
        codepoints_to_utf8(&cps)
    }

    /// Find the (line, column) for an offset. Out-of-range offsets
    /// are clamped to the end.
    pub fn offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        let pos = offset.min(self.len());
        let nl = count_newlines_before(&self.root, pos);
        let line_start_off = if nl == 0 {
            0
        } else {
            nth_newline_pos(&self.root, nl - 1).unwrap_or(0)
        };
        (nl, pos - line_start_off)
    }

    /// Find the offset for a (line, column). Out-of-range columns
    /// are clamped to the line length. An out-of-range line returns
    /// the buffer length.
    pub fn line_col_to_offset(&self, line: usize, col: usize) -> usize {
        let len = self.len();
        let start = self.line_start(line).unwrap_or(len);
        match self.line_range(line) {
            Some((s, e)) => start + col.min(e - s),
            None => start,
        }
    }

    /// Collect the entire buffer as a code-point vector.
    pub fn to_slice(&self) -> Vec<u32> {
        self.slice(0, self.len())
    }

    /// Iterate over every code point in order. The iterator borrows
    /// the rope and lives as long as the borrow.
    pub fn chars(&self) -> Chars<'_> {
        Chars {
            rope: self,
            pos: 0,
        }
    }

    /// Iterate over every line as a freshly allocated `Vec<u32>`.
    /// Each item excludes the trailing newline.
    pub fn lines(&self) -> Lines<'_> {
        Lines {
            rope: self,
            line: 0,
        }
    }
}

// ─── Iterators ──────────────────────────────────────────────────────────

/// Iterator over code points. Returned by [`RopeBuffer::chars`].
pub struct Chars<'a> {
    rope: &'a RopeBuffer,
    pos: usize,
}

impl<'a> Iterator for Chars<'a> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        let cp = self.rope.char_at(self.pos)?;
        self.pos += 1;
        Some(cp)
    }
}

/// Iterator over lines. Returned by [`RopeBuffer::lines`].
pub struct Lines<'a> {
    rope: &'a RopeBuffer,
    line: usize,
}

impl<'a> Iterator for Lines<'a> {
    type Item = Vec<u32>;
    fn next(&mut self) -> Option<Vec<u32>> {
        if self.line >= self.rope.line_count() {
            return None;
        }
        let v = self.rope.get_line(self.line);
        self.line += 1;
        Some(v)
    }
}

// ─── Internal: build rope from slice (bottom-up balanced) ───────────────

fn build_rope_from_slice(text: &[u32]) -> Box<Node> {
    if text.is_empty() {
        return Node::empty_leaf();
    }
    let chunk_count = text.len().div_ceil(LEAF_MAX);
    if chunk_count == 1 {
        return Node::leaf(text.to_vec());
    }

    // Allocate one leaf per chunk, then merge pairwise until a
    // single root remains.
    let mut nodes: Vec<Box<Node>> = (0..chunk_count)
        .map(|i| {
            let start = i * LEAF_MAX;
            let end = (start + LEAF_MAX).min(text.len());
            Node::leaf(text[start..end].to_vec())
        })
        .collect();

    while nodes.len() > 1 {
        let mut next: Vec<Box<Node>> = Vec::with_capacity(nodes.len().div_ceil(2));
        let mut iter = nodes.into_iter();
        loop {
            match (iter.next(), iter.next()) {
                (Some(l), Some(r)) => next.push(Node::branch(l, r)),
                (Some(l), None) => next.push(l),
                _ => break,
            }
        }
        nodes = next;
    }
    nodes.pop().expect("at least one node after merge")
}

// ─── Internal: char lookup + range collect ──────────────────────────────

fn char_at_node(node: &Node, pos: usize) -> u32 {
    match node {
        Node::Leaf { buf, .. } => buf[pos],
        Node::Branch {
            left,
            right,
            left_len,
            ..
        } => {
            if pos < *left_len {
                char_at_node(left, pos)
            } else {
                char_at_node(right, pos - *left_len)
            }
        }
    }
}

fn collect_range(node: &Node, start: usize, end: usize, out: &mut Vec<u32>) {
    if start >= end {
        return;
    }
    match node {
        Node::Leaf { buf, .. } => {
            let s = start.min(buf.len());
            let e = end.min(buf.len());
            out.extend_from_slice(&buf[s..e]);
        }
        Node::Branch {
            left,
            right,
            left_len,
            ..
        } => {
            if start < *left_len {
                collect_range(left, start, end.min(*left_len), out);
            }
            if end > *left_len {
                let r_start = start.saturating_sub(*left_len);
                let r_end = end - left_len;
                collect_range(right, r_start, r_end, out);
            }
        }
    }
}

// ─── Internal: split / join / insert / delete ───────────────────────────

/// Split `node` into `(left, right)` such that `left` is the first
/// `pos` code points and `right` is the remainder. Consumes `node`.
fn split_node(node: Box<Node>, pos: usize) -> (Box<Node>, Box<Node>) {
    let node_len = node.text_len();
    if pos == 0 {
        return (Node::empty_leaf(), node);
    }
    if pos >= node_len {
        return (node, Node::empty_leaf());
    }
    match *node {
        Node::Leaf { mut buf, .. } => {
            // split_off keeps [0..pos) in `buf` and returns [pos..).
            let right_vec = buf.split_off(pos);
            (Node::leaf(buf), Node::leaf(right_vec))
        }
        Node::Branch {
            left,
            right,
            left_len,
            ..
        } => {
            if pos == left_len {
                (left, right)
            } else if pos < left_len {
                let (ll, lr) = split_node(left, pos);
                let new_right = join_nodes(lr, right);
                (ll, new_right)
            } else {
                let (rl, rr) = split_node(right, pos - left_len);
                let new_left = join_nodes(left, rl);
                (new_left, rr)
            }
        }
    }
}

/// Join two subtrees into a balanced AVL tree. Empty subtrees
/// collapse; pairs of small leaves merge when the combined length
/// fits in `LEAF_MAX`. For unequal-height inputs we use the
/// standard AVL join algorithm: recurse into the taller side until
/// heights are compatible, then rebalance on the way back up.
///
/// The Zig source's `joinNodes` did only a single top-level
/// rotation, which is enough for the insert-one-node case but
/// leaves the tree progressively unbalanced under repeated splits
/// and joins. The recursive variant here keeps height proportional
/// to log(n) under any sequence of edits.
fn join_nodes(left: Box<Node>, right: Box<Node>) -> Box<Node> {
    if left.text_len() == 0 {
        return right;
    }
    if right.text_len() == 0 {
        return left;
    }
    // Merge two small leaves into one.
    if matches!(*left, Node::Leaf { .. }) && matches!(*right, Node::Leaf { .. }) {
        let (Node::Leaf { buf: lbuf, .. }, Node::Leaf { buf: rbuf, .. }) = (&*left, &*right)
        else {
            unreachable!()
        };
        if lbuf.len() + rbuf.len() <= LEAF_MAX {
            let mut merged = Vec::with_capacity(lbuf.len() + rbuf.len());
            merged.extend_from_slice(lbuf);
            merged.extend_from_slice(rbuf);
            return Node::leaf(merged);
        }
    }

    let lh = left.height();
    let rh = right.height();
    if lh.abs_diff(rh) <= 1 {
        // Heights compatible — direct join, no rotation needed.
        return Node::branch(left, right);
    }

    if lh > rh {
        // Left is taller. Split off its right subtree, join with the
        // smaller right side, and rebalance the resulting branch.
        let (ll, lr) = take_children(left);
        let new_right = join_nodes(lr, right);
        balance(Node::branch(ll, new_right))
    } else {
        // Symmetric: right is taller.
        let (rl, rr) = take_children(right);
        let new_left = join_nodes(left, rl);
        balance(Node::branch(new_left, rr))
    }
}

fn insert_node(root: Box<Node>, pos: usize, insertion: Box<Node>) -> Box<Node> {
    let (l, r) = split_node(root, pos);
    let left = join_nodes(l, insertion);
    join_nodes(left, r)
}

fn delete_range(root: Box<Node>, pos: usize, count: usize) -> Box<Node> {
    let (before, rest) = split_node(root, pos);
    let (_deleted, after) = split_node(rest, count);
    // _deleted drops here, freeing the removed subtree.
    join_nodes(before, after)
}

// ─── Internal: AVL balancing ────────────────────────────────────────────

fn balance(node: Box<Node>) -> Box<Node> {
    let bf = node.balance_factor();
    if bf > 1 {
        // Left-heavy.
        let (left, right) = take_children(node);
        if left.balance_factor() < 0 {
            let new_left = rotate_left(left);
            rotate_right(Node::branch(new_left, right))
        } else {
            rotate_right(Node::branch(left, right))
        }
    } else if bf < -1 {
        // Right-heavy.
        let (left, right) = take_children(node);
        if right.balance_factor() > 0 {
            let new_right = rotate_right(right);
            rotate_left(Node::branch(left, new_right))
        } else {
            rotate_left(Node::branch(left, right))
        }
    } else {
        node
    }
}

/// Disassemble a Branch into its `(left, right)` children. Panics
/// on a Leaf — only called from contexts that have already verified
/// the node is a Branch.
fn take_children(node: Box<Node>) -> (Box<Node>, Box<Node>) {
    match *node {
        Node::Branch { left, right, .. } => (left, right),
        Node::Leaf { .. } => panic!("take_children called on a leaf"),
    }
}

fn rotate_right(node: Box<Node>) -> Box<Node> {
    // node is L-heavy; rotate so left becomes the new root.
    let Node::Branch {
        left, right: node_r, ..
    } = *node
    else {
        return node_back_from(*node);
    };
    let Node::Branch {
        left: ll,
        right: lr,
        ..
    } = *left
    else {
        // Left isn't a branch — can't rotate; rebuild.
        return Node::branch(left, node_r);
    };
    let new_right = Node::branch(lr, node_r);
    Node::branch(ll, new_right)
}

fn rotate_left(node: Box<Node>) -> Box<Node> {
    let Node::Branch {
        left: node_l, right, ..
    } = *node
    else {
        return node_back_from(*node);
    };
    let Node::Branch {
        left: rl,
        right: rr,
        ..
    } = *right
    else {
        return Node::branch(node_l, right);
    };
    let new_left = Node::branch(node_l, rl);
    Node::branch(new_left, rr)
}

/// Reconstitute a `Box<Node>` from an owned `Node`. Used in the
/// "rotate refused" fallthroughs in `rotate_left`/`right` to
/// satisfy the type system; can't actually fire in practice
/// because we only call those on Branches.
fn node_back_from(n: Node) -> Box<Node> {
    Box::new(n)
}

// ─── Internal: newline navigation ───────────────────────────────────────

/// How many newlines appear at offsets `< pos`.
fn count_newlines_before(node: &Node, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    match node {
        Node::Leaf { buf, .. } => {
            let scan_to = pos.min(buf.len());
            count_newlines(&buf[..scan_to])
        }
        Node::Branch {
            left,
            right,
            left_len,
            ..
        } => {
            if pos <= *left_len {
                count_newlines_before(left, pos)
            } else {
                left.newline_count() + count_newlines_before(right, pos - left_len)
            }
        }
    }
}

/// Offset just AFTER the `n`th newline (0-based). i.e. the start
/// of line `n+1`. `None` if there aren't that many newlines.
fn nth_newline_pos(node: &Node, n: usize) -> Option<usize> {
    nth_newline_pos_raw(node, n).map(|p| p + 1)
}

/// Offset OF the `n`th newline character itself (0-based).
fn nth_newline_pos_raw(node: &Node, n: usize) -> Option<usize> {
    match node {
        Node::Leaf { buf, .. } => {
            let mut count = 0usize;
            for (i, &cp) in buf.iter().enumerate() {
                if cp == NEWLINE {
                    if count == n {
                        return Some(i);
                    }
                    count += 1;
                }
            }
            None
        }
        Node::Branch {
            left,
            right,
            left_len,
            ..
        } => {
            let left_nl = left.newline_count();
            if n < left_nl {
                nth_newline_pos_raw(left, n)
            } else {
                let right_pos = nth_newline_pos_raw(right, n - left_nl)?;
                Some(*left_len + right_pos)
            }
        }
    }
}

// ─── UTF-8 ↔ UTF-32 ─────────────────────────────────────────────────────

/// Decode a UTF-8 byte string into UTF-32 code points.
///
/// Behaviour matches the Zig source:
///   * `\r\n` → `\n` and lone `\r` → `\n`.
///   * Leading BOM (EF BB BF) skipped.
///   * Invalid sequences emit U+FFFD and advance by one byte.
pub fn utf8_to_codepoints(utf8: &[u8]) -> Vec<u32> {
    let mut result: Vec<u32> = Vec::with_capacity(utf8.len());
    let mut i = 0usize;

    // BOM at start.
    if utf8.len() >= 3 && utf8[0] == 0xEF && utf8[1] == 0xBB && utf8[2] == 0xBF {
        i = 3;
    }

    while i < utf8.len() {
        let byte = utf8[i];

        // CR / CRLF normalisation.
        if byte == b'\r' {
            result.push(NEWLINE);
            if i + 1 < utf8.len() && utf8[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if byte < 0x80 {
            result.push(byte as u32);
            i += 1;
        } else if byte & 0xE0 == 0xC0 {
            // 2-byte sequence.
            if i + 1 < utf8.len() && utf8[i + 1] & 0xC0 == 0x80 {
                let cp = ((byte & 0x1F) as u32) << 6 | (utf8[i + 1] & 0x3F) as u32;
                result.push(cp);
                i += 2;
            } else {
                result.push(0xFFFD);
                i += 1;
            }
        } else if byte & 0xF0 == 0xE0 {
            // 3-byte sequence.
            if i + 2 < utf8.len()
                && utf8[i + 1] & 0xC0 == 0x80
                && utf8[i + 2] & 0xC0 == 0x80
            {
                let cp = ((byte & 0x0F) as u32) << 12
                    | ((utf8[i + 1] & 0x3F) as u32) << 6
                    | (utf8[i + 2] & 0x3F) as u32;
                result.push(cp);
                i += 3;
            } else {
                result.push(0xFFFD);
                i += 1;
            }
        } else if byte & 0xF8 == 0xF0 {
            // 4-byte sequence.
            if i + 3 < utf8.len()
                && utf8[i + 1] & 0xC0 == 0x80
                && utf8[i + 2] & 0xC0 == 0x80
                && utf8[i + 3] & 0xC0 == 0x80
            {
                let cp = ((byte & 0x07) as u32) << 18
                    | ((utf8[i + 1] & 0x3F) as u32) << 12
                    | ((utf8[i + 2] & 0x3F) as u32) << 6
                    | (utf8[i + 3] & 0x3F) as u32;
                result.push(cp);
                i += 4;
            } else {
                result.push(0xFFFD);
                i += 1;
            }
        } else {
            result.push(0xFFFD);
            i += 1;
        }
    }
    result
}

/// Encode UTF-32 code points to a UTF-8 string. Surrogate halves
/// and out-of-range code points are replaced with U+FFFD.
pub fn codepoints_to_utf8(cps: &[u32]) -> String {
    // Worst case 4 bytes per code point + a few for U+FFFD escapes.
    let mut out = String::with_capacity(cps.len() * 2);
    for &cp in cps {
        match char::from_u32(cp) {
            Some(c) => out.push(c),
            None => out.push('\u{FFFD}'),
        }
    }
    out
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let buf = RopeBuffer::new();
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.char_at(0), None);
    }

    #[test]
    fn init_from_utf8() {
        let buf = RopeBuffer::from_utf8(b"Hello\nWorld");
        assert_eq!(buf.len(), 11);
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.char_at(0), Some('H' as u32));
        assert_eq!(buf.char_at(5), Some('\n' as u32));
        assert_eq!(buf.char_at(6), Some('W' as u32));
    }

    #[test]
    fn insert_and_delete() {
        let mut buf = RopeBuffer::from_utf8(b"Hello");
        assert_eq!(buf.len(), 5);

        buf.insert_utf8(5, b" World");
        assert_eq!(buf.len(), 11);
        assert_eq!(buf.to_utf8(), "Hello World");

        buf.delete(5, 6);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.to_utf8(), "Hello");
    }

    #[test]
    fn line_navigation() {
        let buf = RopeBuffer::from_utf8(b"Line 0\nLine 1\nLine 2");
        assert_eq!(buf.line_count(), 3);

        assert_eq!(buf.line_start(0), Some(0));
        assert_eq!(buf.line_start(1), Some(7));
        assert_eq!(buf.line_start(2), Some(14));
        assert_eq!(buf.line_start(3), None);

        let line0 = buf.get_line(0);
        assert_eq!(codepoints_to_utf8(&line0), "Line 0");
        let line2 = buf.get_line(2);
        assert_eq!(codepoints_to_utf8(&line2), "Line 2");
    }

    #[test]
    fn offset_to_line_col_works() {
        let buf = RopeBuffer::from_utf8(b"AB\nCD\nEF");
        // A=0, B=1, \n=2, C=3, D=4, \n=5, E=6, F=7
        assert_eq!(buf.offset_to_line_col(0), (0, 0));
        assert_eq!(buf.offset_to_line_col(3), (1, 0));
        assert_eq!(buf.offset_to_line_col(7), (2, 1));
    }

    #[test]
    fn line_col_to_offset_works() {
        let buf = RopeBuffer::from_utf8(b"AB\nCD\nEF");
        assert_eq!(buf.line_col_to_offset(0, 0), 0);
        assert_eq!(buf.line_col_to_offset(0, 1), 1);
        assert_eq!(buf.line_col_to_offset(1, 0), 3);
        assert_eq!(buf.line_col_to_offset(2, 0), 6);
        assert_eq!(buf.line_col_to_offset(2, 1), 7);
    }

    #[test]
    fn utf8_round_trip() {
        let original = "Hello, 世界! 🌍";
        let buf = RopeBuffer::from_utf8(original.as_bytes());
        assert_eq!(buf.to_utf8(), original);
    }

    #[test]
    fn crlf_normalisation() {
        let buf = RopeBuffer::from_utf8(b"A\r\nB\rC\nD");
        assert_eq!(buf.line_count(), 4);
        assert_eq!(buf.to_utf8(), "A\nB\nC\nD");
    }

    #[test]
    fn insert_at_beginning_and_end() {
        let mut buf = RopeBuffer::from_utf8(b"World");
        buf.insert_utf8(0, b"Hello ");
        let len = buf.len();
        buf.insert_utf8(len, b"!");
        assert_eq!(buf.to_utf8(), "Hello World!");
    }

    #[test]
    fn single_char_operations() {
        let mut buf = RopeBuffer::new();
        buf.insert_char(0, 'A' as u32);
        buf.insert_char(1, 'C' as u32);
        buf.insert_char(1, 'B' as u32);
        assert_eq!(buf.char_at(0), Some('A' as u32));
        assert_eq!(buf.char_at(1), Some('B' as u32));
        assert_eq!(buf.char_at(2), Some('C' as u32));
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn large_insert_balanced_tree() {
        // Build 1000 lines of ~75 chars each — enough to force many leaves.
        let line = "This is a line of BASIC code that is reasonably long for testing purposes.\n";
        let mut big = String::with_capacity(line.len() * 1000);
        for _ in 0..1000 {
            big.push_str(line);
        }
        let buf = RopeBuffer::from_utf8(big.as_bytes());
        // 1000 newlines → 1001 lines (the trailing empty line).
        assert_eq!(buf.line_count(), 1001);

        let first = buf.get_line(0);
        assert_eq!(
            codepoints_to_utf8(&first),
            "This is a line of BASIC code that is reasonably long for testing purposes."
        );

        // AVL guarantees height <= 1.44 * log2(n); ~1000 leaves → height <= ~15.
        // The Zig test asserted <= 20; we use the same generous bound.
        assert!(
            buf.height() <= 20,
            "tree height {} should be modest for 1000 lines",
            buf.height()
        );
    }

    #[test]
    fn delete_entire_content() {
        let mut buf = RopeBuffer::from_utf8(b"Hello World");
        let len = buf.len();
        buf.delete(0, len);
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.line_count(), 1);
    }

    #[test]
    fn replace_works() {
        let mut buf = RopeBuffer::from_utf8(b"Hello World");
        let replacement: Vec<u32> = "Zig".chars().map(|c| c as u32).collect();
        buf.replace(6, 5, &replacement);
        assert_eq!(buf.to_utf8(), "Hello Zig");
    }

    #[test]
    fn empty_line_range() {
        let buf = RopeBuffer::from_utf8(b"\n\n\n");
        assert_eq!(buf.line_count(), 4);
        assert_eq!(buf.line_range(0), Some((0, 0)));
        assert_eq!(buf.line_range(1), Some((1, 1)));
    }

    #[test]
    fn slice_extraction() {
        let buf = RopeBuffer::from_utf8(b"ABCDEFGHIJ");
        let sub = buf.slice(3, 7);
        assert_eq!(codepoints_to_utf8(&sub), "DEFG");
    }

    #[test]
    fn bom_is_skipped() {
        let with_bom: &[u8] = b"\xEF\xBB\xBFHello";
        let buf = RopeBuffer::from_utf8(with_bom);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.to_utf8(), "Hello");
    }

    // ── Rust-port additions ─────────────────────────────────────────

    #[test]
    fn chars_iterator() {
        let buf = RopeBuffer::from_utf8(b"AB\nCD");
        let collected: Vec<u32> = buf.chars().collect();
        assert_eq!(collected, vec!['A' as u32, 'B' as u32, '\n' as u32, 'C' as u32, 'D' as u32]);
    }

    #[test]
    fn lines_iterator() {
        let buf = RopeBuffer::from_utf8(b"alpha\nbeta\ngamma");
        let lines: Vec<String> = buf.lines().map(|l| codepoints_to_utf8(&l)).collect();
        assert_eq!(lines, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn many_small_inserts_keep_balance() {
        // Stress test: 5000 single-char inserts at various positions.
        // The AVL invariant should keep height bounded.
        let mut buf = RopeBuffer::new();
        for i in 0..5000 {
            let pos = if buf.is_empty() { 0 } else { i % (buf.len() + 1) };
            buf.insert_char(pos, ('a' as u32) + ((i as u32) % 26));
        }
        assert_eq!(buf.len(), 5000);
        // log2(5000) ≈ 12; 1.44 × 12 ≈ 18. Allow a generous margin.
        assert!(
            buf.height() <= 25,
            "tree height {} should be modest after 5000 inserts",
            buf.height()
        );
    }

    #[test]
    fn split_at_exact_branch_boundary() {
        // Build something that forces a branch, then operate at the
        // weight boundary.
        let text: Vec<u32> = (0..(LEAF_MAX as u32 * 3)).map(|i| 'a' as u32 + (i % 26)).collect();
        let mut buf = RopeBuffer::from_slice(&text);
        let half = buf.len() / 2;
        let cp_at = buf.char_at(half).unwrap();
        buf.delete(half, 1);
        assert_eq!(buf.len(), text.len() - 1);
        // After deletion, the new code point at `half` should be
        // what came AFTER the removed one in the original.
        assert_eq!(buf.char_at(half).unwrap(), text[half + 1]);
        // And the removed one shouldn't be at half.
        assert_ne!(buf.char_at(half).unwrap(), cp_at);
    }
}
