//! Git graph build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::git::{Commit, CommitType, DiagramOrientation, GitGraphDb};

use crate::mermaid::ir::*;
use crate::theme;

const FLOW_START: f32 = 132.0;
const FLOW_STEP: f32 = 82.0;
const FLOW_PAD: f32 = 52.0;
const LANE_START: f32 = 46.0;
const LANE_GAP: f32 = 72.0;
const SIDE_PAD: f32 = 26.0;
const BRANCH_FONT: f32 = 12.0;
const COMMIT_FONT: f32 = 10.5;
const TAG_FONT: f32 = 10.5;
const COMMIT_R: f32 = 9.0;

const BRANCH_COLORS: [u32; 8] = [
    0x4FC1FF, 0xC586C0, 0x6A9955, 0xDCDCAA, 0xCE9178, 0xB5CEA8, 0x569CD6, 0xD7BA7D,
];

pub fn build(db: &GitGraphDb) -> GitGraph {
    let branches = db.get_branches_as_obj_array();
    let commits = sorted_commits(db);
    if branches.is_empty() || commits.is_empty() {
        return GitGraph {
            width: 400.0,
            height: 160.0,
            branches: Vec::new(),
            edges: Vec::new(),
            commits: Vec::new(),
        };
    }

    let dir = db.get_direction();
    let branch_index: HashMap<String, usize> = branches
        .iter()
        .enumerate()
        .map(|(idx, branch)| (branch.name.clone(), idx))
        .collect();

    let max_seq = commits.iter().map(|commit| commit.seq).max().unwrap_or(0) as f32;
    let max_lane = branches.len().saturating_sub(1) as f32;
    let (width, height) = match dir {
        DiagramOrientation::LeftToRight => (
            FLOW_START + max_seq * FLOW_STEP + FLOW_PAD,
            LANE_START + max_lane * LANE_GAP + SIDE_PAD + 44.0,
        ),
        DiagramOrientation::TopToBottom | DiagramOrientation::BottomToTop => (
            LANE_START + max_lane * LANE_GAP + SIDE_PAD + 150.0,
            FLOW_START + max_seq * FLOW_STEP + FLOW_PAD,
        ),
    };

    let mut positions = HashMap::new();
    for commit in &commits {
        let lane = *branch_index.get(&commit.branch).unwrap_or(&0) as f32;
        let flow = FLOW_START + commit.seq as f32 * FLOW_STEP;
        let lane_pos = LANE_START + lane * LANE_GAP;
        let (x, y) = match dir {
            DiagramOrientation::LeftToRight => (flow, lane_pos),
            DiagramOrientation::TopToBottom => (lane_pos, flow),
            DiagramOrientation::BottomToTop => (lane_pos, height - flow),
        };
        positions.insert(commit.id.clone(), (x, y));
    }

    let mut out_branches = Vec::new();
    for (idx, branch) in branches.iter().enumerate() {
        let color = branch_color(idx);
        let lane_pos = LANE_START + idx as f32 * LANE_GAP;
        let line = match dir {
            DiagramOrientation::LeftToRight => {
                vec![(FLOW_START - 28.0, lane_pos), (width - SIDE_PAD, lane_pos)]
            }
            DiagramOrientation::TopToBottom => {
                vec![(lane_pos, FLOW_START - 28.0), (lane_pos, height - SIDE_PAD)]
            }
            DiagramOrientation::BottomToTop => {
                vec![(lane_pos, height - FLOW_START + 28.0), (lane_pos, SIDE_PAD)]
            }
        };
        let (label_x, label_y) = match dir {
            DiagramOrientation::LeftToRight => (SIDE_PAD, lane_pos - 11.0),
            DiagramOrientation::TopToBottom => (lane_pos - 26.0, SIDE_PAD),
            DiagramOrientation::BottomToTop => (lane_pos - 26.0, height - SIDE_PAD - 18.0),
        };
        out_branches.push(GitBranch {
            name: branch.name.clone(),
            color,
            line,
            label_x,
            label_y,
        });
    }

    let mut edges = Vec::new();
    let commit_lookup = db.get_commits();
    for commit in &commits {
        let Some(&to) = positions.get(&commit.id) else {
            continue;
        };
        for (parent_idx, parent_id) in commit.parents.iter().enumerate() {
            let Some(parent) = commit_lookup.get(parent_id) else {
                continue;
            };
            let Some(&from) = positions.get(parent_id) else {
                continue;
            };
            let color = if parent_idx > 0 {
                branch_color(*branch_index.get(&parent.branch).unwrap_or(&0))
            } else {
                branch_color(*branch_index.get(&commit.branch).unwrap_or(&0))
            };
            edges.push(GitEdge {
                points: route_edge(from, to, dir),
                color,
                line_style: if commit.commit_type == CommitType::CherryPick {
                    LineStyle::Dash
                } else {
                    LineStyle::Solid
                },
            });
        }
    }

    let mut out_commits = Vec::new();
    for commit in commits {
        let Some(&(x, y)) = positions.get(&commit.id) else {
            continue;
        };
        let branch_idx = *branch_index.get(&commit.branch).unwrap_or(&0);
        out_commits.push(GitCommit {
            x,
            y,
            label: commit_label(commit),
            tags: commit.tags.clone(),
            kind: convert_kind(commit.commit_type),
            color: branch_color(branch_idx),
        });
    }

    GitGraph {
        width: width.max(1.0),
        height: height.max(1.0),
        branches: out_branches,
        edges,
        commits: out_commits,
    }
}

fn sorted_commits(db: &GitGraphDb) -> Vec<&Commit> {
    let mut commits: Vec<&Commit> = db.get_commits().values().collect();
    commits.sort_by_key(|commit| commit.seq);
    commits
}

fn branch_color(index: usize) -> u32 {
    BRANCH_COLORS[index % BRANCH_COLORS.len()]
}

fn commit_label(commit: &Commit) -> String {
    if commit.custom_id {
        commit.id.clone()
    } else if !commit.message.is_empty() {
        commit.message.clone()
    } else {
        String::new()
    }
}

fn convert_kind(kind: CommitType) -> GitCommitKind {
    match kind {
        CommitType::Reverse => GitCommitKind::Reverse,
        CommitType::Highlight => GitCommitKind::Highlight,
        CommitType::Merge => GitCommitKind::Merge,
        CommitType::CherryPick => GitCommitKind::CherryPick,
        CommitType::Normal => GitCommitKind::Normal,
    }
}

fn route_edge(from: (f32, f32), to: (f32, f32), dir: DiagramOrientation) -> Vec<(f32, f32)> {
    if (from.0 - to.0).abs() < 1.0 || (from.1 - to.1).abs() < 1.0 {
        return vec![from, to];
    }

    match dir {
        DiagramOrientation::LeftToRight => {
            let mid = (from.0 + to.0) / 2.0;
            vec![from, (mid, from.1), (mid, to.1), to]
        }
        DiagramOrientation::TopToBottom | DiagramOrientation::BottomToTop => {
            let mid = (from.1 + to.1) / 2.0;
            vec![from, (from.0, mid), (to.0, mid), to]
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &GitGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;

    for branch in &graph.branches {
        draw_branch(target, branch, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    for edge in &graph.edges {
        draw_edge(target, factory, edge, scale, &tx, &ty, &mut brush)?;
    }
    for commit in &graph.commits {
        draw_commit(
            target, factory, commit, scale, &tx, &ty, &mut brush, &mut fmt,
        )?;
    }
    Ok(())
}

unsafe fn draw_branch(
    target: &ID2D1RenderTarget,
    branch: &GitBranch,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if branch.line.len() >= 2 {
        let br = brush(branch.color)?;
        target.DrawLine(
            Vector2 {
                X: tx(branch.line[0].0),
                Y: ty(branch.line[0].1),
            },
            Vector2 {
                X: tx(branch.line[1].0),
                Y: ty(branch.line[1].1),
            },
            &br,
            (1.4 * scale).max(0.8),
            None::<&ID2D1StrokeStyle>,
        );
    }

    let label_w = (branch.name.chars().count() as f32 * BRANCH_FONT * 0.58 + 16.0) * scale;
    let label_h = 22.0 * scale;
    let x = tx(branch.label_x);
    let y = ty(branch.label_y);
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + label_w,
        bottom: y + label_h,
    };
    let bg = brush(theme::SIDEBAR_BG)?;
    let stroke = brush(branch.color)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        1.0,
        None::<&ID2D1StrokeStyle>,
    );
    draw_text(
        target,
        &branch.name,
        rect,
        BRANCH_FONT * scale,
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        branch.color,
        brush,
        fmt,
    )
}

unsafe fn draw_edge(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    edge: &GitEdge,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    if edge.points.len() < 2 {
        return Ok(());
    }
    let br = brush(edge.color)?;
    let style = match edge.line_style {
        LineStyle::Solid => None,
        LineStyle::Dash | LineStyle::Dot => {
            Some(crate::mermaid::render::sequence_dash_style(factory))
        }
    };
    let line_w = (2.0 * scale).max(0.9);
    let pts: Vec<(f32, f32)> = edge.points.iter().map(|(x, y)| (tx(*x), ty(*y))).collect();
    for segment in pts.windows(2) {
        target.DrawLine(
            Vector2 {
                X: segment[0].0,
                Y: segment[0].1,
            },
            Vector2 {
                X: segment[1].0,
                Y: segment[1].1,
            },
            &br,
            line_w,
            style,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_commit(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    commit: &GitCommit,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(commit.x);
    let y = ty(commit.y);
    let r = (COMMIT_R * scale).max(5.0);
    let fill = brush(match commit.kind {
        GitCommitKind::Reverse => 0x7F1D1D,
        GitCommitKind::Highlight => 0x3A3320,
        GitCommitKind::CherryPick => 0x5F3B76,
        _ => theme::BG,
    })?;
    let stroke = brush(commit.color)?;

    match commit.kind {
        GitCommitKind::Highlight => {
            let rect = D2D_RECT_F {
                left: x - r,
                top: y - r,
                right: x + r,
                bottom: y + r,
            };
            target.FillRectangle(std::ptr::addr_of!(rect), &fill);
            target.DrawRectangle(
                std::ptr::addr_of!(rect),
                &stroke,
                2.0,
                None::<&ID2D1StrokeStyle>,
            );
        }
        _ => {
            let ellipse = D2D1_ELLIPSE {
                point: Vector2 { X: x, Y: y },
                radiusX: r,
                radiusY: r,
            };
            target.FillEllipse(std::ptr::addr_of!(ellipse), &fill);
            target.DrawEllipse(
                std::ptr::addr_of!(ellipse),
                &stroke,
                2.0,
                None::<&ID2D1StrokeStyle>,
            );
            if matches!(commit.kind, GitCommitKind::Merge) {
                let inner = D2D1_ELLIPSE {
                    point: Vector2 { X: x, Y: y },
                    radiusX: r * 0.58,
                    radiusY: r * 0.58,
                };
                target.DrawEllipse(
                    std::ptr::addr_of!(inner),
                    &stroke,
                    1.25,
                    None::<&ID2D1StrokeStyle>,
                );
            }
        }
    }

    if matches!(
        commit.kind,
        GitCommitKind::Reverse | GitCommitKind::CherryPick
    ) {
        let mark = brush(theme::TEXT_BRIGHT)?;
        let d = r * 0.52;
        target.DrawLine(
            Vector2 { X: x - d, Y: y - d },
            Vector2 { X: x + d, Y: y + d },
            &mark,
            1.5,
            None::<&ID2D1StrokeStyle>,
        );
        target.DrawLine(
            Vector2 { X: x - d, Y: y + d },
            Vector2 { X: x + d, Y: y - d },
            &mark,
            1.5,
            None::<&ID2D1StrokeStyle>,
        );
    }

    if !commit.label.is_empty() {
        draw_label(
            target,
            &commit.label,
            x,
            y + 21.0 * scale,
            COMMIT_FONT * scale,
            brush,
            fmt,
        )?;
    }
    for (idx, tag) in commit.tags.iter().enumerate() {
        draw_tag(
            target,
            factory,
            tag,
            x,
            y - (24.0 + idx as f32 * 20.0) * scale,
            TAG_FONT * scale,
            commit.color,
            brush,
            fmt,
        )?;
    }
    Ok(())
}

unsafe fn draw_label(
    target: &ID2D1RenderTarget,
    text: &str,
    cx: f32,
    cy: f32,
    size: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let w = (text.chars().count() as f32 * size * 0.58).max(22.0) + 8.0;
    let h = size + 7.0;
    let rect = D2D_RECT_F {
        left: cx - w / 2.0,
        top: cy - h / 2.0,
        right: cx + w / 2.0,
        bottom: cy + h / 2.0,
    };
    let bg = brush(theme::SIDEBAR_BG)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);
    draw_text(
        target,
        text,
        rect,
        size,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        theme::TEXT,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_tag(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    text: &str,
    cx: f32,
    cy: f32,
    size: f32,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let w = (text.chars().count() as f32 * size * 0.58).max(22.0) + 16.0;
    let h = size + 8.0;
    let notch = h * 0.35;
    let points = [
        (cx - w / 2.0 + notch, cy - h / 2.0),
        (cx + w / 2.0, cy - h / 2.0),
        (cx + w / 2.0, cy + h / 2.0),
        (cx - w / 2.0 + notch, cy + h / 2.0),
        (cx - w / 2.0, cy),
    ];
    let geo = crate::mermaid::render::build_polygon_pub(factory, &points)?;
    let bg = brush(0x2D2A20)?;
    let stroke = brush(color)?;
    target.FillGeometry(&geo, &bg, None);
    target.DrawGeometry(&geo, &stroke, 1.0, None::<&ID2D1StrokeStyle>);

    let rect = D2D_RECT_F {
        left: cx - w / 2.0 + notch,
        top: cy - h / 2.0,
        right: cx + w / 2.0 - 3.0,
        bottom: cy + h / 2.0,
    };
    draw_text(
        target,
        text,
        rect,
        size,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        theme::TEXT,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    text: &str,
    rect: D2D_RECT_F,
    size: f32,
    bold: bool,
    align: DWRITE_TEXT_ALIGNMENT,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let f = fmt(theme::BODY_FONT, size, bold, false)?;
    let _ = f.SetTextAlignment(align);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
    let br = brush(color)?;
    let buf: Vec<u16> = text.encode_utf16().collect();
    target.DrawText(
        &buf,
        &f,
        std::ptr::addr_of!(rect),
        &br,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_branch_and_merge_graph() {
        let source = r#"gitGraph
    commit id:"base"
    branch feature
    checkout feature
    commit id:"work"
    checkout main
    commit id:"main"
    merge feature id:"merge" tag:"v1"
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Git(db) = diagram else {
            panic!("expected git graph");
        };
        let graph = build(&db);
        assert_eq!(graph.branches.len(), 2);
        assert_eq!(graph.commits.len(), 4);
        assert!(graph.edges.len() >= 3);
        assert!(graph.commits.iter().any(|c| c.kind == GitCommitKind::Merge));
    }
}
