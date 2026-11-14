use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub bytes: u64,
    pub children: Vec<Node>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    fn area(self) -> f64 {
        f64::from(self.width) * f64::from(self.height)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutOptions {
    pub max_depth: usize,
    pub title_height: f32,
    pub padding: f32,
    pub min_area: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            title_height: 22.0,
            padding: 2.0,
            min_area: 36.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrimitiveKind {
    Container,
    Leaf,
    Aggregate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Primitive {
    /// Aggregate primitives deliberately have no selectable source identity.
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub label: String,
    pub bytes: u64,
    pub rect: Rect,
    pub depth: usize,
    pub kind: PrimitiveKind,
}

/// Pure, background-thread-compatible layout; parents precede children in paint order.
/// Node weights are sibling proportions; a parent's own weight need not equal its children.
pub fn layout(root: &Node, bounds: Rect, options: LayoutOptions) -> Vec<Primitive> {
    if ![
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        options.title_height,
        options.padding,
        options.min_area,
    ]
    .iter()
    .all(|v| v.is_finite())
        || bounds.width <= 0.0
        || bounds.height <= 0.0
        || options.title_height < 0.0
        || options.padding < 0.0
        || options.min_area < 0.0
    {
        return Vec::new();
    }
    let mut output = Vec::new();
    visit(root, None, bounds, 0, options, &mut output);
    output
}

fn visit(
    node: &Node,
    parent: Option<&str>,
    rect: Rect,
    depth: usize,
    options: LayoutOptions,
    output: &mut Vec<Primitive>,
) {
    let expanded = depth < options.max_depth
        && node.children.iter().any(|n| n.bytes > 0)
        && rect.width >= 80.0
        && rect.height >= options.title_height + 32.0;
    output.push(Primitive {
        id: Some(node.id.clone()),
        parent_id: parent.map(str::to_owned),
        label: node.label.clone(),
        bytes: node.bytes,
        rect,
        depth,
        kind: if expanded {
            PrimitiveKind::Container
        } else {
            PrimitiveKind::Leaf
        },
    });
    if !expanded {
        return;
    }
    let inner = Rect {
        x: rect.x + options.padding,
        y: rect.y + options.title_height + options.padding,
        width: rect.width - options.padding * 2.0,
        height: rect.height - options.title_height - options.padding * 2.0,
    };
    if inner.width <= 0.0 || inner.height <= 0.0 {
        return;
    }
    let total: f64 = node.children.iter().map(|n| n.bytes as f64).sum();
    let mut items: Vec<(Option<&Node>, u64)> = Vec::new();
    let mut small = 0u64;
    for child in &node.children {
        if child.bytes == 0 {
            continue;
        }
        if child.bytes as f64 / total * inner.area() < f64::from(options.min_area) {
            small = small.saturating_add(child.bytes);
        } else {
            items.push((Some(child), child.bytes));
        }
    }
    if small > 0 {
        items.push((None, small));
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.1));
    let weights: Vec<f64> = items.iter().map(|(_, bytes)| *bytes as f64).collect();
    let mut rectangles = squarify(&weights, inner);
    // Area alone admits unreadable strips along the final row or column.
    let mut folded = false;
    items = items
        .into_iter()
        .zip(&rectangles)
        .filter_map(|(item, rect)| {
            if item.0.is_some() && (rect.width < 22.0 || rect.height < 18.0) {
                small = small.saturating_add(item.1);
                folded = true;
                None
            } else {
                Some(item)
            }
        })
        .collect();
    if folded {
        items.retain(|(child, _)| child.is_some());
        items.push((None, small));
        items.sort_by_key(|item| std::cmp::Reverse(item.1));
        let weights: Vec<f64> = items.iter().map(|(_, bytes)| *bytes as f64).collect();
        rectangles = squarify(&weights, inner);
    }
    for ((child, bytes), child_rect) in items.into_iter().zip(rectangles) {
        if let Some(child) = child {
            visit(
                child,
                Some(&node.id),
                child_rect,
                depth + 1,
                options,
                output,
            );
        } else {
            output.push(Primitive {
                id: None,
                parent_id: Some(node.id.clone()),
                label: orbment_core::i18n::tr("memory-tree-other"),
                bytes,
                rect: child_rect,
                depth: depth + 1,
                kind: PrimitiveKind::Aggregate,
            });
        }
    }
}

fn worst(areas: &[f64], side: f64) -> f64 {
    let sum: f64 = areas.iter().sum();
    let min = areas.iter().copied().fold(f64::INFINITY, f64::min);
    let max = areas.iter().copied().fold(0.0, f64::max);
    (side * side * max / (sum * sum)).max(sum * sum / (side * side * min))
}

fn squarify(weights: &[f64], mut remaining: Rect) -> Vec<Rect> {
    let total: f64 = weights.iter().sum();
    let areas: Vec<f64> = weights
        .iter()
        .map(|w| w / total * remaining.area())
        .collect();
    let mut result = Vec::with_capacity(weights.len());
    let mut start = 0;
    while start < areas.len() {
        let side = f64::from(remaining.width.min(remaining.height));
        let mut end = start + 1;
        while end < areas.len()
            && worst(&areas[start..=end], side) <= worst(&areas[start..end], side)
        {
            end += 1;
        }
        let sum: f64 = areas[start..end].iter().sum();
        let vertical = remaining.width >= remaining.height;
        let length = if vertical {
            remaining.height
        } else {
            remaining.width
        };
        let thickness = if end == areas.len() {
            if vertical {
                remaining.width
            } else {
                remaining.height
            }
        } else {
            (sum / f64::from(length)) as f32
        };
        let mut offset = 0.0;
        for (i, area) in areas[start..end].iter().enumerate() {
            let extent = if start + i + 1 == end {
                length - offset
            } else {
                (*area / sum * f64::from(length)) as f32
            };
            result.push(if vertical {
                Rect {
                    x: remaining.x,
                    y: remaining.y + offset,
                    width: thickness,
                    height: extent,
                }
            } else {
                Rect {
                    x: remaining.x + offset,
                    y: remaining.y,
                    width: extent,
                    height: thickness,
                }
            });
            offset += extent;
        }
        if vertical {
            remaining.x += thickness;
            remaining.width -= thickness;
        } else {
            remaining.y += thickness;
            remaining.height -= thickness;
        }
        start = end;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str, bytes: u64) -> Node {
        Node {
            id: id.into(),
            label: id.into(),
            bytes,
            children: Vec::new(),
        }
    }

    #[test]
    fn tiles_preserve_area_and_do_not_overlap() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 600.0,
            height: 300.0,
        };
        let tiles = squarify(&[50.0, 25.0, 10.0, 9.0, 6.0], rect);
        assert!((tiles.iter().map(|r| r.area()).sum::<f64>() - rect.area()).abs() < 0.1);
        for (i, a) in tiles.iter().enumerate() {
            assert!(a.x >= rect.x && a.y >= rect.y);
            assert!(a.x + a.width <= rect.x + rect.width + 0.001);
            assert!(a.y + a.height <= rect.y + rect.height + 0.001);
            for b in &tiles[i + 1..] {
                let w = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
                let h = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
                assert!(w <= 0.001 || h <= 0.001);
            }
        }
    }

    #[test]
    fn nesting_insets_titles_limits_depth_and_aggregates_small_nodes() {
        let root = Node {
            children: vec![
                Node {
                    children: vec![leaf("grandchild", 999)],
                    ..leaf("large", 999)
                },
                leaf("tiny", 1),
                leaf("empty", 0),
            ],
            ..leaf("root", 1000)
        };
        let options = LayoutOptions {
            max_depth: 1,
            min_area: 100.0,
            ..Default::default()
        };
        let primitives = layout(
            &root,
            Rect {
                width: 100.0,
                height: 100.0,
                ..Default::default()
            },
            options,
        );
        assert_eq!(primitives.len(), 3);
        assert!(primitives.iter().skip(1).all(|p| p.rect.y >= 24.0));
        assert_eq!(primitives[1].kind, PrimitiveKind::Leaf);
        assert_eq!(primitives[2].kind, PrimitiveKind::Aggregate);
        assert_eq!(primitives[2].bytes, 1);
        assert!(primitives[2].id.is_none());
    }

    #[test]
    fn small_parents_render_as_solid_leaves_instead_of_empty_containers() {
        let root = Node {
            children: vec![leaf("child", 100)],
            ..leaf("root", 100)
        };
        for (width, height) in [(79.0, 200.0), (200.0, 53.0), (40.0, 30.0)] {
            let primitives = layout(
                &root,
                Rect {
                    width,
                    height,
                    ..Default::default()
                },
                LayoutOptions::default(),
            );
            assert_eq!(primitives.len(), 1);
            assert_eq!(primitives[0].kind, PrimitiveKind::Leaf);
            assert_eq!(primitives[0].bytes, 100);
            assert_eq!(primitives[0].id.as_deref(), Some("root"));
        }
        let primitives = layout(
            &root,
            Rect {
                width: 80.0,
                height: 54.0,
                ..Default::default()
            },
            LayoutOptions::default(),
        );
        assert_eq!(primitives[0].kind, PrimitiveKind::Container);
        assert_eq!(primitives.len(), 2);
    }

    #[test]
    fn thin_children_fold_into_one_aggregate_and_preserve_bytes_and_area() {
        let root = Node {
            children: vec![
                leaf("large", 9500),
                leaf("strip-a", 250),
                leaf("strip-b", 250),
                leaf("already-small", 1),
                leaf("empty", 0),
            ],
            ..leaf("root", 10001)
        };
        // The 250-byte children each exceed min_area, but have a short axis
        // below the legibility threshold in both layout orientations.
        for (width, height) in [(100.0, 324.0), (304.0, 124.0)] {
            let options = LayoutOptions {
                min_area: 100.0,
                ..Default::default()
            };
            let primitives = layout(
                &root,
                Rect {
                    width,
                    height,
                    ..Default::default()
                },
                options,
            );
            let children: Vec<_> = primitives.iter().filter(|p| p.depth == 1).collect();
            assert_eq!(children.len(), 2);
            assert_eq!(children.iter().map(|p| p.bytes).sum::<u64>(), root.bytes);
            let aggregate = children
                .iter()
                .find(|p| p.kind == PrimitiveKind::Aggregate)
                .unwrap();
            assert_eq!(aggregate.bytes, 501);
            assert!(aggregate.id.is_none());
            assert_eq!(aggregate.parent_id.as_deref(), Some("root"));
            assert!(children.iter().all(|p| {
                p.kind == PrimitiveKind::Aggregate
                    || (p.rect.width >= 22.0 && p.rect.height >= 18.0)
            }));
            let inner_area = f64::from(width - 4.0) * f64::from(height - 26.0);
            assert!(
                (children.iter().map(|p| p.rect.area()).sum::<f64>() - inner_area).abs()
                    < 0.1
            );
        }
    }
}
