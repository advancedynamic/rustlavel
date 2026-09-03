use rustlavel::prelude::*;

/// One entry in a navigation menu.
///
/// A tree, held as a flat table with `parent_id`. Rendering walks it once in
/// memory rather than querying per level: a menu is small, and a query per
/// node is how a sidebar becomes the slowest thing on the page.
#[derive(Model, Default, Debug, Clone)]
#[model(table = "menu_items")]
pub struct MenuItem {
    #[model(primary_key, generated)]
    pub id: i64,
    pub location: String,
    pub parent_id: Option<i64>,
    pub label: String,
    pub route: Option<String>,
    pub url: Option<String>,
    pub icon: Option<String>,
    pub permission: Option<String>,
    pub sort_order: i64,
    pub is_active: bool,
    pub target: Option<String>,
}

impl MenuItem {
    /// One menu, in the order it is drawn.
    pub fn in_location(location: &str) -> QueryBuilder {
        MenuItem::query()
            .filter("location", location)
            .order_by("sort_order", rustlavel::db::Direction::Asc)
            .order_by("id", rustlavel::db::Direction::Asc)
    }

    /// Where this item points. A route name wins over a raw URL when both are
    /// set, because a route survives a path changing and a URL does not.
    pub fn href(&self) -> String {
        if let Some(url) = self.url.as_deref().filter(|u| !u.is_empty()) {
            return url.to_string();
        }
        self.route.clone().unwrap_or_else(|| "#".into())
    }

    pub fn is_external(&self) -> bool {
        self.url.as_deref().is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
    }
}

/// A menu, arranged as a tree.
///
/// Built from the flat rows in one pass. An item whose parent is missing — the
/// parent was deleted, or belongs to another location — is promoted to the top
/// rather than dropped: a menu that silently loses a branch is a menu nobody
/// can debug.
pub struct Node {
    pub item: MenuItem,
    pub children: Vec<Node>,
    pub depth: usize,
}

pub fn tree(items: Vec<MenuItem>) -> Vec<Node> {
    let ids: std::collections::BTreeSet<i64> = items.iter().map(|item| item.id).collect();
    let mut by_parent: std::collections::BTreeMap<Option<i64>, Vec<MenuItem>> = Default::default();

    for item in items {
        let parent = item.parent_id.filter(|id| ids.contains(id));
        by_parent.entry(parent).or_default().push(item);
    }

    build(&mut by_parent, None, 0)
}

fn build(
    by_parent: &mut std::collections::BTreeMap<Option<i64>, Vec<MenuItem>>,
    parent: Option<i64>,
    depth: usize,
) -> Vec<Node> {
    // A cycle — an item made its own ancestor by a bad edit — would recurse
    // forever. Depth is capped rather than trusted, and the cap is well past
    // any menu a person would build.
    if depth > 8 {
        return Vec::new();
    }
    let Some(children) = by_parent.remove(&parent) else {
        return Vec::new();
    };
    children
        .into_iter()
        .map(|item| {
            let id = item.id;
            Node { item, children: build(by_parent, Some(id), depth + 1), depth }
        })
        .collect()
}

/// The tree flattened back out, parents before their children, with the depth
/// each one sits at. What the management page draws.
pub fn flatten(nodes: &[Node]) -> Vec<&Node> {
    let mut out = Vec::new();
    for node in nodes {
        out.push(node);
        out.extend(flatten(&node.children));
    }
    out
}

/// How deep the deepest branch goes, counting the top level as one.
pub fn depth_of(nodes: &[Node]) -> usize {
    nodes.iter().map(|n| 1 + depth_of(&n.children)).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, parent: Option<i64>, label: &str) -> MenuItem {
        MenuItem {
            id,
            location: "sidebar".into(),
            parent_id: parent,
            label: label.into(),
            is_active: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_flat_list_becomes_a_tree_in_order() {
        let nodes = tree(vec![
            item(1, None, "Dashboard"),
            item(2, None, "Projects"),
            item(3, Some(2), "All Projects"),
            item(4, Some(3), "Archived"),
        ]);

        assert_eq!(nodes.len(), 2);
        assert_eq!(depth_of(&nodes), 3);

        let flat: Vec<&str> = flatten(&nodes).iter().map(|n| n.item.label.as_str()).collect();
        assert_eq!(flat, vec!["Dashboard", "Projects", "All Projects", "Archived"]);
        assert_eq!(flatten(&nodes)[3].depth, 2);
    }

    /// The parent was deleted, or belongs to another menu. The child must
    /// still appear: a row that exists and is drawn nowhere is unfindable.
    #[test]
    fn an_orphan_is_promoted_rather_than_dropped() {
        let nodes = tree(vec![item(1, None, "Dashboard"), item(9, Some(404), "Stray")]);

        let labels: Vec<&str> = nodes.iter().map(|n| n.item.label.as_str()).collect();
        assert_eq!(labels, vec!["Dashboard", "Stray"]);
    }

    /// Two items each claiming the other as parent. Neither is reachable from
    /// the top, so neither is drawn — but the function has to return.
    #[test]
    fn a_cycle_does_not_recurse_forever() {
        let nodes = tree(vec![item(1, Some(2), "A"), item(2, Some(1), "B")]);

        assert!(nodes.is_empty(), "a cycle has no top level to draw from");
    }

    #[test]
    fn a_url_wins_over_a_route_and_an_external_link_is_recognised() {
        let mut menu = item(1, None, "Docs");
        menu.route = Some("/dashboard".into());
        assert_eq!(menu.href(), "/dashboard");
        assert!(!menu.is_external());

        menu.url = Some("https://example.com/docs".into());
        assert_eq!(menu.href(), "https://example.com/docs");
        assert!(menu.is_external());
    }
}
