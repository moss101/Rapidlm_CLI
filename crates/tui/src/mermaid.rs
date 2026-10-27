//! Mermaid rendering: a fenced ```mermaid block becomes an ASCII diagram.
//!
//! The subset is the one diagrams in agent answers actually use —
//! `flowchart`/`graph` with `A --> B` edges, `A --- B` plain links, and
//! `[label]`/`(label)`/`{label}` node shapes. What comes out is the
//! boxes-and-arrows a text grid owes: each node a box on its own row
//! (`┌─…─┐ │ label │ └─…─┘`), edges between consecutive rows as `│` with
//! the arrowhead, and side-by-side edges (`A --> B`) on one row joined by
//! `──▶`. Anything the parser does not understand is preserved verbatim
//! below the diagram — a diagram that silently dropped half the model's
//! content would be worse than no diagram.

/// Most nodes/edges one diagram renders.
pub const MAX_NODES: usize = 32;

/// One parsed node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    pub id: String,
    pub label: String,
}

/// One parsed edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// `-->` (arrow) or `---` (plain).
    pub arrow: bool,
}

/// A parsed diagram, or why it would not parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Diagram {
    Flow {
        nodes: Vec<Node>,
        edges: Vec<Edge>,
    },
    /// Not a diagram this renders: the caller shows the block verbatim.
    Unsupported(String),
}

/// Parse the body of a ```mermaid fence (the fence lines themselves
/// stripped by the caller).
pub fn parse(body: &str) -> Diagram {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut declared_flow = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if !declared_flow {
            let first = line.split_whitespace().next().unwrap_or_default();
            if matches!(first, "flowchart" | "graph") {
                declared_flow = true;
                continue;
            }
            return Diagram::Unsupported(body.to_owned());
        }
        // An edge line: `A --> B`, `A[labeled] --> B(labeled)`, `A --- B`.
        if let Some((left, rest, arrow)) = split_edge(line) {
            let from = intern(&mut nodes, &left);
            let to = intern(&mut nodes, &rest);
            edges.push(Edge { from, to, arrow });
            continue;
        }
        // Arrow forms this does not render (`-.->`, `==>`, `<-->`) are
        // not bare node declarations: the diagram shows verbatim rather
        // than half-parsed.
        if ["-.->", "==>", "<-->", "-->", "---"]
            .iter()
            .any(|form| line.contains(form))
        {
            return Diagram::Unsupported(body.to_owned());
        }
        // A bare node declaration: `A[label]` or `A`.
        intern(&mut nodes, line);
        if nodes.len() > MAX_NODES {
            return Diagram::Unsupported(body.to_owned());
        }
    }
    if !declared_flow || nodes.is_empty() {
        return Diagram::Unsupported(body.to_owned());
    }
    Diagram::Flow { nodes, edges }
}

/// Split `A ...arrow... B` into (left node text, right node text, arrow?)
/// for the arrow forms this renders.
fn split_edge(line: &str) -> Option<(String, String, bool)> {
    for (token, arrow) in [("-->", true), ("---", false)] {
        if let Some((left, right)) = line.split_once(token) {
            let left = left.trim();
            let right = right.trim();
            if left.is_empty() || right.is_empty() {
                return None;
            }
            // A longer arrow (`---->`, `-.->`) is not this token's shape.
            if left.ends_with('-') || right.starts_with('-') {
                return None;
            }
            return Some((left.to_owned(), right.to_owned(), arrow));
        }
    }
    None
}

/// Register `text` (a node id or `id[label]` shape) and return its id.
fn intern(nodes: &mut Vec<Node>, text: &str) -> String {
    // A shaped node: `id[label]`, `id(label)`, `id{label}`, `id([stadium])`
    // — the id is what edges name, the label what the box shows. An
    // unshaped node is its own label.
    let (id, label) = match text.find(['[', '(', '{']) {
        Some(at) => {
            let node_id = text[..at].trim();
            let mut inner = &text[at..];
            // Strip matched wrapper pairs while they wrap the whole
            // remainder — `([Stadium])` unwraps twice; a bracket inside
            // the label (`a[b[c]d]`) never matches the tail and stays.
            loop {
                let mut stripped = false;
                for (open, close) in [('[', ']'), ('(', ')'), ('{', '}')] {
                    if inner.starts_with(open) && inner.ends_with(close) {
                        inner = &inner[1..inner.len() - 1];
                        stripped = true;
                    }
                }
                if !stripped {
                    break;
                }
            }
            (node_id.to_owned(), inner.trim().to_owned())
        }
        None => (text.to_owned(), text.to_owned()),
    };
    let label = label.trim();
    let label = if label.is_empty() {
        id.clone()
    } else {
        label.to_owned()
    };
    if let Some(existing) = nodes.iter().find(|node| node.id == id) {
        return existing.id.clone();
    }
    nodes.push(Node {
        id: id.clone(),
        label,
    });
    id
}

/// Render a parsed flow: nodes in declaration order, each a box; an edge
/// between consecutive nodes as a vertical arrow; a side-by-side edge on
/// one row as `A ──▶ B`.
pub fn render(diagram: &Diagram) -> String {
    let Diagram::Flow { nodes, edges } = diagram else {
        return String::new();
    };
    let mut out = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if index > 0 {
            // The edge(s) from the previous node to this one, if any.
            let previous = &nodes[index - 1].id;
            let mut linked = false;
            for edge in edges {
                if &edge.from == previous && edge.to == node.id {
                    linked = true;
                }
            }
            if linked {
                out.push_str("  │");
            } else {
                out.push_str("  ·");
            }
            out.push('\n');
        }
        out.push_str(&box_of(&node.label));
        // Side-by-side edges out of this node render on the box's row.
        let sibling: Vec<&Edge> = edges.iter().filter(|edge| edge.from == node.id).collect();
        if let Some(edge) = sibling.first() {
            let to = nodes
                .iter()
                .find(|candidate| candidate.id == edge.to)
                .map(|node| node.label.clone())
                .unwrap_or_else(|| edge.to.clone());
            out.push_str(if edge.arrow {
                " ──▶ "
            } else {
                " ─── "
            });
            out.push_str(&to);
        }
        out.push('\n');
    }
    out
}

/// A one-row box around `label`, wide enough for it.
fn box_of(label: &str) -> String {
    let width = label.chars().count().max(1);
    format!("┌{:─<w$}┐\n│{label}│\n└{:─<w$}┘", "", "", w = width)
}

/// The whole pipeline for one fenced block: parse and render, or the
/// original body when this does not render it (the caller shows that
/// verbatim — never a silently partial diagram).
pub fn render_block(body: &str) -> String {
    match parse(body) {
        Diagram::Flow { .. } => {
            let rendered = render(&parse(body));
            if rendered.is_empty() {
                body.to_owned()
            } else {
                rendered
            }
        }
        Diagram::Unsupported(_) => body.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flowchart_of_boxes_and_arrows_renders() {
        let body = "flowchart LR\n  start([Start]) --> check{Valid?}\n  check --> done[Done]\n";
        let rendered = render_block(body);
        assert!(rendered.contains("┌─────┐"), "a Start box: {rendered}");
        assert!(rendered.contains("│Start│"), "{rendered}");
        assert!(rendered.contains("│Valid?│"), "{rendered}");
        assert!(
            rendered.contains("──▶ Done"),
            "the side edge names its target: {rendered}"
        );
        assert!(
            rendered.contains("  │"),
            "consecutive nodes are linked vertically: {rendered}"
        );
    }

    #[test]
    fn plain_links_and_unlinked_nodes_render_with_honest_separators() {
        let body = "graph TD\n  a --- b\n  c[Standalone]\n";
        let rendered = render_block(body);
        assert!(
            rendered.contains(" ─── "),
            "a plain link has no arrowhead: {rendered}"
        );
        assert!(
            rendered.contains("  ·"),
            "an unlinked node is a soft break: {rendered}"
        );
        assert!(rendered.contains("│Standalone│"), "{rendered}");
    }

    #[test]
    fn anything_else_comes_back_verbatim() {
        for body in [
            "sequenceDiagram\n  A->>B: hi\n",
            "not a diagram at all\n",
            "",
            "flowchart LR\n",
        ] {
            assert_eq!(render_block(body), body, "{body:?}");
        }
        // Dashed and over-long arrows are not this renderer's shapes:
        // verbatim, never half-parsed.
        assert_eq!(
            render_block("flowchart LR\n  a -.-> b\n"),
            "flowchart LR\n  a -.-> b\n"
        );
    }

    #[test]
    fn edges_between_labeled_shapes_find_their_nodes() {
        let body = "flowchart TD\n  first[One] --> second(Two)\n  second --> third{Three}\n";
        let rendered = render_block(body);
        for label in ["One", "Two", "Three"] {
            assert!(rendered.contains(label), "{rendered}");
        }
        assert!(rendered.matches("──▶").count() >= 1, "{rendered}");
    }
}
