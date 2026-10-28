//! Jumping from and to source and the rendered document.

use std::num::NonZeroUsize;
use std::ops::Range;

use tinymist_project::LspWorld;
use tinymist_std::typst::TypstDocument;
use tinymist_world::debug_loc::SourceSpanOffset;
use tinymist_world::vfs::WorkspaceResolver;
use typst::{
    World,
    introspection::PagedPosition as Position,
    layout::{Frame, FrameItem, Point, Size},
    syntax::{
        FileId, LinkedNode, Source, Span, SyntaxKind, VirtualRoot,
        ast::{self, AstNode},
    },
    visualize::Geometry,
};
use typst_shim::syntax::LinkedNodeExt;

/// The kind of frame item a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickHitKind {
    /// A glyph of a text item.
    Glyph,
    /// A rectangle shape, e.g. a fill behind text.
    Shape,
    /// An image.
    Image,
}

/// A frame item whose bounds contain a click position.
#[derive(Debug, Clone, Copy)]
pub struct ClickHit {
    /// The kind of the frame item.
    pub kind: ClickHitKind,
    /// The span (and offset in text) that layout stamped on the item.
    pub span: SourceSpanOffset,
    /// How many groups (nested frames) the item is inside of.
    pub depth: usize,
}

impl ClickHit {
    /// The file the item's span points into.
    pub fn file(&self) -> Option<FileId> {
        self.span.span.id()
    }

    /// Whether the item's span points into the project instead of a package.
    pub fn is_project(&self) -> bool {
        self.file()
            .is_some_and(|id| !WorkspaceResolver::is_package_file(id))
    }

    /// Lists the places in the source that can be offered as jump targets for
    /// this hit: the leaf span itself, followed by the calls, closures and
    /// definitions enclosing it, innermost first.
    pub fn candidates(&self, world: &LspWorld) -> Vec<JumpCandidate> {
        let span = self.span.span;
        let Some(id) = span.id() else {
            return vec![];
        };
        let Ok(source) = world.source(id) else {
            return vec![];
        };
        let Some(leaf) = source.find(span) else {
            return vec![];
        };

        let package = match id.root() {
            VirtualRoot::Package(spec) if WorkspaceResolver::is_package_file(id) => {
                Some(spec.to_string())
            }
            _ => None,
        };
        let candidate = |role, name, node: &LinkedNode| JumpCandidate {
            role,
            name,
            file: id,
            package: package.clone(),
            span: node.span(),
            range: node.range(),
        };

        let mut candidates = vec![candidate(CandidateRole::Leaf, None, &leaf)];

        let mut cursor = leaf.parent();
        while let Some(node) = cursor {
            match node.kind() {
                SyntaxKind::FuncCall => {
                    let name = node
                        .cast::<ast::FuncCall>()
                        .and_then(|call| {
                            source
                                .find(call.callee().span())
                                .map(|callee| callee.range())
                        })
                        .and_then(|range| source.text().get(range))
                        .map(|text| text.to_string());
                    candidates.push(candidate(CandidateRole::Call, name, node));
                }
                SyntaxKind::Closure => {
                    let name = node
                        .cast::<ast::Closure>()
                        .and_then(|closure| closure.name())
                        .map(|name| name.get().to_string());
                    candidates.push(candidate(CandidateRole::Closure, name, node));
                }
                SyntaxKind::LetBinding => {
                    let name =
                        node.cast::<ast::LetBinding>()
                            .and_then(|binding| match binding.kind() {
                                ast::LetBindingKind::Closure(name) => Some(name.get().to_string()),
                                ast::LetBindingKind::Normal(pattern) => pattern
                                    .bindings()
                                    .first()
                                    .map(|name| name.get().to_string()),
                            });
                    candidates.push(candidate(CandidateRole::Definition, name, node));
                }
                _ => {}
            }
            cursor = node.parent();
        }

        candidates
    }
}

/// The role of a [`JumpCandidate`] relative to the clicked item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateRole {
    /// The span stamped on the clicked item itself.
    Leaf,
    /// A function call enclosing the leaf.
    Call,
    /// An anonymous or named closure enclosing the leaf.
    Closure,
    /// A `let` binding enclosing the leaf.
    Definition,
}

/// A source location that a click can jump to.
#[derive(Debug, Clone)]
pub struct JumpCandidate {
    /// The role of the location relative to the clicked item.
    pub role: CandidateRole,
    /// The callee or binding name, if the node has one.
    pub name: Option<String>,
    /// The file containing the location.
    pub file: FileId,
    /// The package spec of the file, if it belongs to a package.
    pub package: Option<String>,
    /// The span of the syntax node.
    pub span: Span,
    /// The byte range of the syntax node in the file.
    pub range: Range<usize>,
}

/// How to pick one of the items under a click.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClickJumpPolicy {
    /// The item painted last (on top).
    #[default]
    Topmost,
    /// The topmost item whose span is in the project, skipping packages.
    Project,
}

/// Picks one of the hits ordered topmost first according to the policy.
pub fn select_click_hit(hits: &[ClickHit], policy: ClickJumpPolicy) -> Option<&ClickHit> {
    match policy {
        ClickJumpPolicy::Topmost => hits.first(),
        ClickJumpPolicy::Project => hits.iter().find(|hit| hit.is_project()),
    }
}

/// Finds a span range from a clicked physical position in a rendered paged
/// document.
pub fn jump_from_click(
    world: &LspWorld,
    frame: &Frame,
    click: Point,
) -> Option<(SourceSpanOffset, SourceSpanOffset)> {
    let hits = click_hits(world, frame, click);
    let hit = select_click_hit(&hits, ClickJumpPolicy::Topmost)?;
    Some((hit.span, hit.span))
}

/// Collects all items under a clicked physical position in a rendered paged
/// document, ordered topmost first.
pub fn click_hits(world: &LspWorld, frame: &Frame, click: Point) -> Vec<ClickHit> {
    let mut hits = vec![];
    collect_click_hits(world, frame, click, 0, &mut hits);
    hits
}

fn collect_click_hits(
    world: &LspWorld,
    frame: &Frame,
    click: Point,
    depth: usize,
    hits: &mut Vec<ClickHit>,
) {
    // Try to find a link first.
    for (pos, item) in frame.items() {
        if let FrameItem::Link(_dest, size) = item
            && is_in_rect(*pos, *size, click)
        {
            // todo: url reaction
            return;
        }
    }

    // If there's no link, search for jump targets.
    for &(mut pos, ref item) in frame.items().rev() {
        match item {
            FrameItem::Group(group) => {
                // TODO: Handle transformation.
                collect_click_hits(world, &group.frame, click - pos, depth + 1, hits);
            }

            FrameItem::Text(text) => {
                for glyph in &text.glyphs {
                    let width = glyph.x_advance.at(text.size);
                    if is_in_rect(
                        Point::new(pos.x, pos.y - text.size),
                        Size::new(width, text.size),
                        click,
                    ) {
                        let (span, span_offset) = glyph.span;
                        let mut span_offset = span_offset as usize;
                        let Some(id) = span.id() else { continue };
                        let Ok(source) = world.source(id) else {
                            continue;
                        };
                        let Some(node) = source.find(span) else {
                            continue;
                        };
                        if matches!(node.kind(), SyntaxKind::Text | SyntaxKind::MathText)
                            && (click.x - pos.x) > width / 2.0
                        {
                            span_offset += glyph.range().len();
                        }

                        hits.push(ClickHit {
                            kind: ClickHitKind::Glyph,
                            span: SourceSpanOffset {
                                span,
                                offset: span_offset,
                            },
                            depth,
                        });
                        break;
                    }

                    pos.x += width;
                }
            }

            FrameItem::Shape(shape, span) => {
                let Geometry::Rect(size) = shape.geometry else {
                    continue;
                };
                if is_in_rect(pos, size, click) {
                    hits.push(ClickHit {
                        kind: ClickHitKind::Shape,
                        span: (*span).into(),
                        depth,
                    });
                }
            }

            FrameItem::Image(_, size, span) if is_in_rect(pos, *size, click) => {
                hits.push(ClickHit {
                    kind: ClickHitKind::Image,
                    span: (*span).into(),
                    depth,
                });
            }

            _ => {}
        }
    }
}

/// Finds the output location in the document for a cursor position.
pub fn jump_from_cursor(document: &TypstDocument, source: &Source, cursor: usize) -> Vec<Position> {
    jump_from_cursor_(document, source, cursor).unwrap_or_default()
}

/// Finds the output location in the document for a cursor position.
fn jump_from_cursor_(
    document: &TypstDocument,
    source: &Source,
    cursor: usize,
) -> Option<Vec<Position>> {
    // todo: leaf_at_compat only matches the text before the cursor, but we could
    // also match a text if it is after the cursor
    // The case `leaf_at_compat` will match: `Hello|`
    // FIXME: The case `leaf_at_compat` will not match: `|Hello`
    let node = LinkedNode::new(source.root()).leaf_at_compat(cursor)?;
    // todo: When we click on a label or some math operators, we seems likely also
    // be able to jump to some place.
    if !matches!(node.kind(), SyntaxKind::Text | SyntaxKind::MathText) {
        return None;
    };

    let span = node.span();
    let offset = cursor.saturating_sub(node.offset());

    // todo: The cursor may not exact hit at the start of some AST node. For
    // example, the cursor in the text element `Hell|o` is offset by 4 from the
    // node. It seems not pretty if we ignore the offset completely.
    let _ = offset;

    match document {
        TypstDocument::Paged(paged_doc) => {
            // We checks whether there are any elements exactly matching the
            // cursor position.
            let mut positions = vec![];

            // Unluckily, we might not be able to find the exact spans, so we
            // need to find the closest one at the same time.
            let mut min_page = 0;
            let mut min_point = Point::default();
            let mut min_dis = u64::MAX;

            for (idx, page) in paged_doc.pages().iter().enumerate() {
                // In a page, we try to find a closer span than the existing found one.
                let mut p_dis = min_dis;

                if let Some(point) = find_in_frame(&page.frame, span, &mut p_dis, &mut min_point)
                    && let Some(page) = NonZeroUsize::new(idx + 1)
                {
                    positions.push(Position { page, point });
                }

                // In this page, we found a closer span and update.
                if p_dis != min_dis {
                    min_page = idx;
                    min_dis = p_dis;
                }
            }

            // If we didn't find any exact span, we add the closest one in the same page.
            if positions.is_empty() && min_dis != u64::MAX {
                positions.push(Position {
                    page: NonZeroUsize::new(min_page + 1)?,
                    point: min_point,
                });
            }

            Some(positions)
        }
        _ => None,
    }
}

/// Finds the position of a span in a frame.
fn find_in_frame(frame: &Frame, span: Span, min_dis: &mut u64, res: &mut Point) -> Option<Point> {
    for &(mut pos, ref item) in frame.items() {
        if let FrameItem::Group(group) = item {
            // TODO: Handle transformation.
            if let Some(point) = find_in_frame(&group.frame, span, min_dis, res) {
                return Some(point + pos);
            }
        }

        if let FrameItem::Text(text) = item {
            for glyph in &text.glyphs {
                if glyph.span.0 == span {
                    return Some(pos);
                }

                // We at least require that the span is in the same file.
                let is_same_file = glyph.span.0.id() == span.id();
                if is_same_file {
                    // The numbers are not offsets but a unique id on the AST tree which are
                    // nicely divided.
                    // FIXME: since typst v0.13.0, the numbers are not only the ids, but also raw
                    // ranges, See [`Span::range`].
                    let glyph_num = glyph.span.0.into_raw();
                    let span_num = span.into_raw().get();
                    let dis = glyph_num.get().abs_diff(span_num);
                    if dis < *min_dis {
                        *min_dis = dis;
                        *res = pos;
                    }
                }
                pos.x += glyph.x_advance.at(text.size);
            }
        }
    }

    None
}

/// Whether a rectangle with the given size at the given position contains the
/// click position.
fn is_in_rect(pos: Point, size: Size, click: Point) -> bool {
    pos.x <= click.x && pos.x + size.x >= click.x && pos.y <= click.y && pos.y + size.y >= click.y
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::*;
    use crate::tests::*;
    use typst::layout::Abs;

    /// Collects the origin of each text item and the corner of each rectangle
    /// of a frame, in page coordinates.
    fn click_probes(frame: &Frame, origin: Point, out: &mut Vec<(String, Point)>) {
        let inset = Point::new(Abs::pt(0.5), Abs::pt(-0.5));
        for (pos, item) in frame.items() {
            match item {
                FrameItem::Group(group) => click_probes(&group.frame, origin + *pos, out),
                FrameItem::Text(text) => {
                    out.push((format!("text {:?}", text.text), origin + *pos + inset))
                }
                FrameItem::Shape(shape, _) if matches!(shape.geometry, Geometry::Rect(_)) => {
                    let inset = Point::new(Abs::pt(1.0), Abs::pt(1.0));
                    out.push(("rect".to_string(), origin + *pos + inset))
                }
                _ => {}
            }
        }
    }

    fn describe(world: &LspWorld, candidate: &JumpCandidate) -> String {
        let place = world
            .source(candidate.file)
            .ok()
            .and_then(|source| {
                let line = source.lines().byte_to_line(candidate.range.start)?;
                let column = source.lines().byte_to_column(candidate.range.start)?;
                Some(format!("{}:{}", line + 1, column + 1))
            })
            .unwrap_or_else(|| "?".to_string());
        let path = candidate.file.vpath().get_with_slash();
        let package = candidate.package.as_deref().unwrap_or_default();
        let name = candidate.name.as_deref().unwrap_or("");
        format!(
            "{:?} {name} {package}{path}:{place} len={}",
            candidate.role,
            candidate.range.len()
        )
    }

    fn hit_in(id: FileId, kind: ClickHitKind) -> ClickHit {
        let source = Source::new(id, "hello".into());
        ClickHit {
            kind,
            span: source.root().span().into(),
            depth: 0,
        }
    }

    #[test]
    fn select_click_hit_by_policy() {
        use std::str::FromStr;
        use typst::syntax::{RootedPath, VirtualPath, package::PackageSpec};

        let spec = PackageSpec::from_str("@preview/example:0.1.0").unwrap();
        let package_file = RootedPath::new(
            VirtualRoot::Package(spec),
            VirtualPath::new("lib.typ").unwrap(),
        )
        .intern();
        let project_file = WorkspaceResolver::rootless_file(VirtualPath::new("main.typ").unwrap());

        let from_package = hit_in(package_file, ClickHitKind::Glyph);
        let from_project = hit_in(project_file, ClickHitKind::Shape);
        assert!(!from_package.is_project());
        assert!(from_project.is_project());

        let hits = [from_package, from_project];
        let topmost = select_click_hit(&hits, ClickJumpPolicy::Topmost).unwrap();
        assert_eq!(topmost.kind, ClickHitKind::Glyph);
        let project = select_click_hit(&hits, ClickJumpPolicy::Project).unwrap();
        assert_eq!(project.kind, ClickHitKind::Shape);

        let only_package = [from_package];
        assert!(select_click_hit(&only_package, ClickJumpPolicy::Project).is_none());
    }

    #[test]
    fn click_hits_snapshot() {
        snapshot_testing("jump_from_click", &|ctx, _path| {
            let document = ctx.success_doc().unwrap();
            let TypstDocument::Paged(paged) = document else {
                panic!("expected a paged document");
            };
            let world = ctx.world();

            let mut out = String::new();
            for (idx, page) in paged.pages().iter().enumerate() {
                let mut probes = vec![];
                click_probes(&page.frame, Point::zero(), &mut probes);

                for (label, click) in probes {
                    out.push_str(&format!("page {} {label}\n", idx + 1));
                    let hits = click_hits(world, &page.frame, click);
                    for (order, hit) in hits.iter().enumerate() {
                        let scope = if hit.is_project() {
                            "project"
                        } else {
                            "package"
                        };
                        out.push_str(&format!(
                            "  #{order} {:?} depth={} {scope}\n",
                            hit.kind, hit.depth
                        ));
                        for candidate in hit.candidates(world) {
                            out.push_str(&format!("      {}\n", describe(world, &candidate)));
                        }
                    }
                }
            }

            assert_snapshot!(out);
        });
    }

    #[test]
    fn test() {
        snapshot_testing("jump_from_cursor", &|ctx, path| {
            let source = ctx.source_by_path(&path).unwrap();
            let document = ctx.success_doc().unwrap();

            let cursors = find_test_range_(&source);

            let results = cursors
                .map(|cursor| {
                    let points = jump_from_cursor(document, &source, cursor);

                    if points.is_empty() {
                        return "nothing".to_string();
                    }

                    points
                        .iter()
                        .map(|pos| {
                            let page = pos.page.get();
                            let point = pos.point;
                            format!("{page},{:.3}pt,{:.3}pt", point.x.to_pt(), point.y.to_pt())
                        })
                        .join(";")
                })
                .join("\n");

            with_settings!({
                description => format!("Jump cursor on {})", make_range_annotation(&source)),
            }, {
                assert_snapshot!(results);
            })
        });
    }
}
