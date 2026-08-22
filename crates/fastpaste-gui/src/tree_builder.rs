//! Flattens the `Vec<Item>` from `Database::load_all()` into a depth-
//! annotated flat list for Slint. DFS from root (parent_id=0), tracking
//! depth.
//!
//! `build_tree_items_with_history` additionally injects
//! the virtual "Clipboard History" folder at the top or bottom, populated
//! from `ClipboardHistory::entries()`. The folder carries
//! `HISTORY_FOLDER_ID` (-1) so the controller can recognise history items
//! and route them to the paste-by-history path instead of trying to
//! fetch them from the DB.
//!
//! Expansion state: the controller owns a `HashSet<i64>` of collapsed
//! folder ids (DB rowids plus `HISTORY_FOLDER_ID` for the virtual
//! folder) and passes it in here. Folders in the set are emitted with
//! `expanded = false` and their descendants are omitted from the flat
//! list entirely — the TreeView is a dumb flat list, so "collapsed"
//! simply means "children not in the model".

use std::collections::HashSet;

use fastpaste_app::HistoryEntry;
use fastpaste_data::{HISTORY_FOLDER_ID, HistoryPosition, Item, ItemKind};
// TreeItem is defined in the reusable `slint-tree-view` library crate,
// not in this crate's own .slint files. Both the library and this
// crate's `slint::include_modules!()` re-export the same generated type;
// we use the library's canonical one to make the dependency direction
// explicit. NO_PARENT is the sentinel for "no parent" (root-level items).
use slint_tree_view::{NO_PARENT, TreeItem};

/// Build a flat list of TreeItem from the DB's flat item list, injecting
/// the virtual Clipboard History folder populated from `history`.
///
/// `position` controls whether the folder appears at the top or bottom
/// of the top-level tree; the C++ reference's default is "bottom".
/// `history_folder_label` is the localized folder title — it lives in the
/// model (not a binding), so the caller passes `i18n.msg(
/// "clipboard-history-folder")` and rebuilds on language change.
/// `collapsed` is the set of folder ids (DB rowids and
/// `HISTORY_FOLDER_ID`) currently collapsed; their descendants are
/// left out of the returned rows.
///
/// History rows are emitted as `item-type == ItemKind::Plain` (so the
/// editor pane will open them as plain snippets — the code reuses the
/// existing editor for read-only history preview). Their `internal-id`
/// is `HISTORY_FOLDER_ID` for the folder and a synthetic negative id
/// (drawn from the row index) for each child; both are negative, which
/// `selected_item_id` already treats as "no real DB row" (the controller
/// intercepts history selections and pastes directly instead of going
/// through `Database::get`).
pub fn build_tree_items_with_history(
    items: &[Item],
    history: &[HistoryEntry],
    position: HistoryPosition,
    history_folder_label: &str,
    collapsed: &HashSet<i64>,
) -> Vec<TreeItem> {
    // Group by parent_id for quick child lookup.
    let mut children_by_parent: std::collections::HashMap<i64, Vec<&Item>> =
        std::collections::HashMap::new();
    for item in items {
        children_by_parent
            .entry(item.parent_id)
            .or_default()
            .push(item);
    }

    // Sort each group by order_index (load_all already sorts this way,
    // but be defensive — the HashMap doesn't preserve order).
    for children in children_by_parent.values_mut() {
        children.sort_by_key(|i| i.order_index);
    }

    let mut items_out = Vec::new();
    let root_children = children_by_parent.get(&0).cloned().unwrap_or_default();

    // History folder goes first when `position == Top`, last when `Bottom`.
    // The folder is always emitted (even when `history` is empty) so the
    // user can see that history capture is active — the empty folder
    // matches the C++ reference's behavior. Empty rows inside are fine.
    if position == HistoryPosition::Top {
        push_history_folder(&mut items_out, history, history_folder_label, collapsed);
    }
    // Real DB rows store parent_id = 0 to mean "root"; the TreeView
    // component reads `parent-internal-id` to emit
    // `current-parent-change-requested` and treats NO_PARENT (-1) as
    // "no parent" — so we translate 0 → NO_PARENT at the seam.
    build_subtree(
        &root_children,
        0,
        NO_PARENT,
        &children_by_parent,
        collapsed,
        &mut items_out,
    );
    if position == HistoryPosition::Bottom {
        push_history_folder(&mut items_out, history, history_folder_label, collapsed);
    }
    items_out
}

/// Append the virtual "Clipboard History" folder + its child entries
/// (children omitted when the folder is in `collapsed`).
///
/// Folder row: `internal-id = HISTORY_FOLDER_ID`, `depth = 0`,
/// `has-children` per the actual entry count. Child rows: `depth = 1`,
/// `item-type = Plain`. Each child's `internal-id` is encoded as
/// `-(i + 2)` (so child 0 → -2, child 1 → -3, …) — strictly less than
/// -1, never colliding with `HISTORY_FOLDER_ID` itself or with real DB
/// rowids (which are ≥1). The controller decodes via
/// `history_index_from_item_id` so it can map a clicked history row back
/// to an entry without storing a parallel lookup table.
fn push_history_folder(
    items: &mut Vec<TreeItem>,
    history: &[HistoryEntry],
    label: &str,
    collapsed: &HashSet<i64>,
) {
    // The history folder is a branch (has children, expanded) — but we
    // override has-children truthfully when the ring buffer is empty so
    // the branch indicator reflects reality.
    let mut folder = TreeItem::branch(HISTORY_FOLDER_ID as i32, NO_PARENT, 0, label)
        .with_icon("🕒")
        .with_item_type(ItemKind::Folder.as_i64() as i32);
    folder.has_children = !history.is_empty();
    folder.expanded = !collapsed.contains(&HISTORY_FOLDER_ID);
    items.push(folder);

    if collapsed.contains(&HISTORY_FOLDER_ID) {
        return;
    }

    for (i, entry) in history.iter().enumerate() {
        // Compact text: the captured text itself, elided by Slint's
        // `overflow: elide` in the tree. We collapse runs of newlines to
        // a single space so multi-line copies show as a single tidy row
        // label (the user-data still holds the full multi-line text for
        // the editor).
        let text: String = entry.text.chars().take(80).collect();
        let text = collapse_newlines(&text);
        items.push(
            TreeItem::leaf(
                // -(i+2) keeps it < -1 (below HISTORY_FOLDER_ID) and unique.
                -((i as i32) + 2),
                HISTORY_FOLDER_ID as i32,
                1,
                text,
                entry.text.as_str(),
            )
            .with_icon("📋")
            .with_item_type(ItemKind::Plain.as_i64() as i32),
        );
    }
}

/// Decode a Slint tree-item `internal-id` produced by `push_history_folder`
/// back into an index into the `ClipboardHistory::entries()` vector.
/// Returns `None` for the folder row itself (`HISTORY_FOLDER_ID`) or any
/// positive DB rowid.
///
/// Used by the Main Window controller to short-circuit a history-item
/// selection: instead of `db.get(id)` (which would return None), the
/// controller pastes `entries()[idx].text` directly.
pub fn history_index_from_item_id(id: i32) -> Option<usize> {
    if id <= HISTORY_FOLDER_ID as i32 {
        // -1 is the folder; -2.. are children at idx = -id - 2.
        if id < HISTORY_FOLDER_ID as i32 {
            Some((-(id) - 2) as usize)
        } else {
            None
        }
    } else {
        None
    }
}

fn build_subtree(
    items: &[&Item],
    depth: i32,
    parent_internal_id: i32,
    children_by_parent: &std::collections::HashMap<i64, Vec<&Item>>,
    collapsed: &HashSet<i64>,
    out: &mut Vec<TreeItem>,
) {
    for item in items {
        let is_folder = item.kind == ItemKind::Folder;
        let id = item.id.unwrap_or(0);
        let children = if is_folder {
            children_by_parent.get(&id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        // Qt-style: the *model* decides what icon to draw. TreeView has
        // no built-in folder/leaf fallback — we set decoration-text
        // explicitly. Empty folders (no children) still look like folders
        // because the user created them as Folder, not because of
        // `has-children`.
        let decoration: &str = if is_folder { "📁" } else { "📄" };
        // Slint `int` is i32; DB rowids are i64. Rowids are always ≥1 in
        // practice (SQLite rowid starts at 1), so truncation is
        // impossible for real data — the cast just satisfies the type
        // checker.
        let slint_id = id as i32;
        if is_folder {
            let mut branch =
                TreeItem::branch(slint_id, parent_internal_id, depth, item.title.as_str())
                    .with_icon(decoration)
                    .with_item_type(item.kind.as_i64() as i32)
                    .with_user_data(item.body_plain.as_str());
            // Truthful structural report — empty folders still claim
            // has-children = false because they have no kids right now
            // (matches what a collapse/expand would actually do).
            branch.has_children = !children.is_empty();
            branch.expanded = !collapsed.contains(&id);
            out.push(branch);
            // A collapsed folder's children are simply not part of the
            // flat model — that's all "collapse" means for a dumb
            // flat-list view.
            if collapsed.contains(&id) {
                continue;
            }
            // Recurse into the folder's children, passing our own id
            // (already i32-truncated above) as their parent.
            build_subtree(
                &children,
                depth + 1,
                slint_id,
                children_by_parent,
                collapsed,
                out,
            );
        } else {
            out.push(
                TreeItem::leaf(
                    slint_id,
                    parent_internal_id,
                    depth,
                    item.title.as_str(),
                    item.body_plain.as_str(),
                )
                .with_icon(decoration)
                .with_item_type(item.kind.as_i64() as i32),
            );
        }
    }
}

/// Collapse any run of `\r` and `\n` characters to a single space.
/// Avoids the "a\r\nb" → "a  b" (two spaces) that a naive char-by-char
/// replace produces. Used to flatten multi-line clipboard snapshots into
/// a tidy tree-row label.
fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if c == '\n' || c == '\r' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(id: i64, parent: i64, title: &str) -> Item {
        Item {
            id: Some(id),
            parent_id: parent,
            kind: ItemKind::Plain,
            title: title.into(),
            body_plain: String::new(),
            body_rtf: None,
            comment: String::new(),
            order_index: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn folder(id: i64, parent: i64, title: &str) -> Item {
        Item {
            id: Some(id),
            parent_id: parent,
            kind: ItemKind::Folder,
            title: title.into(),
            body_plain: String::new(),
            body_rtf: None,
            comment: String::new(),
            order_index: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn hist(text: &str) -> HistoryEntry {
        HistoryEntry {
            text: text.to_string(),
            timestamp: chrono::Utc::now(),
        }
    }

    /// Test shorthand: build with no history entries (the empty folder is
    /// still injected at the bottom) and nothing collapsed.
    fn no_history(items: &[Item]) -> Vec<TreeItem> {
        build_tree_items_with_history(
            items,
            &[],
            HistoryPosition::Bottom,
            "Clipboard History",
            &std::collections::HashSet::new(),
        )
    }

    #[test]
    fn empty_items_gives_empty_rows() {
        // Even with an empty DB we still emit the (empty) history folder
        // as the "history is on, nothing captured yet" affordance.
        let items = no_history(&[]);
        assert_eq!(items.len(), 1, "empty DB still emits the history folder");
        assert_eq!(items[0].internal_id, HISTORY_FOLDER_ID as i32);
        // Qt-style: no is-folder; the only structural hint is
        // has-children, which is false because the ring buffer is empty.
        assert!(!items[0].has_children);
    }

    #[test]
    fn top_level_items_at_depth_zero() {
        let items_in = vec![plain(1, 0, "a"), plain(2, 0, "b")];
        let items = no_history(&items_in);
        // 2 real items + the history folder at the bottom = 3.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].depth, 0);
        assert_eq!(items[1].depth, 0);
        assert_eq!(items[2].internal_id, HISTORY_FOLDER_ID as i32);
    }

    #[test]
    fn folder_child_at_depth_one() {
        let items_in = vec![folder(1, 0, "f"), plain(2, 1, "child")];
        let items = no_history(&items_in);
        // 2 real items + history folder.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].depth, 0);
        // Qt-style: the only structural hint is has-children.
        assert!(items[0].has_children, "folder with a child reports it");
        assert_eq!(items[1].depth, 1);
        assert!(!items[1].has_children, "leaf has no children");
        assert_eq!(items[2].internal_id, HISTORY_FOLDER_ID as i32);
    }

    #[test]
    fn nested_folders_depth_two() {
        let items_in = vec![
            folder(1, 0, "root"),
            folder(2, 1, "inner"),
            plain(3, 2, "leaf"),
        ];
        let items = no_history(&items_in);
        // 3 real items + history folder.
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].depth, 0);
        assert_eq!(items[1].depth, 1);
        assert_eq!(items[2].depth, 2);
    }

    /// Empty history → folder is appended but contains no children. The
    /// controller's refresh path always calls this; an empty folder is the
    /// desired "history is on, nothing captured yet" affordance.
    #[test]
    fn history_injected_at_bottom_when_empty() {
        let items_in = vec![plain(1, 0, "snippet")];
        let items = build_tree_items_with_history(
            &items_in,
            &[],
            HistoryPosition::Bottom,
            "Clipboard History",
            &std::collections::HashSet::new(),
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "snippet");
        assert_eq!(items[1].internal_id, HISTORY_FOLDER_ID as i32);
        // Qt-style: no is-folder; the folder's structural state is "no
        // children" when history is empty.
        assert!(!items[1].has_children);
        assert_eq!(items[1].text, "Clipboard History");
    }

    #[test]
    fn history_injected_at_top_with_entries() {
        let items_in = vec![plain(1, 0, "snippet")];
        let history = vec![hist("foo"), hist("bar")];
        let items = build_tree_items_with_history(
            &items_in,
            &history,
            HistoryPosition::Top,
            "Clipboard History",
            &std::collections::HashSet::new(),
        );
        assert_eq!(items.len(), 4, "snippet + folder + 2 history items");
        // History folder first (Top).
        assert_eq!(items[0].internal_id, HISTORY_FOLDER_ID as i32);
        assert!(items[0].has_children, "folder with entries reports it");
        assert_eq!(items[0].depth, 0);
        // Two children at depth 1, in newest-first order from
        // ClipboardHistory.
        assert_eq!(items[1].internal_id, -2);
        assert_eq!(items[1].depth, 1);
        assert_eq!(items[1].text, "foo");
        assert_eq!(items[2].internal_id, -3);
        assert_eq!(items[2].text, "bar");
        // Real snippet after.
        assert_eq!(items[3].internal_id, 1);
    }

    #[test]
    fn history_item_texts_collapse_newlines() {
        let history = vec![hist("line one\nline two\r\nthree")];
        let items = build_tree_items_with_history(
            &[],
            &history,
            HistoryPosition::Bottom,
            "Clipboard History",
            &std::collections::HashSet::new(),
        );
        assert_eq!(items[1].text, "line one line two three");
        // The user-data still has the original text.
        assert_eq!(items[1].user_data, "line one\nline two\r\nthree");
    }

    #[test]
    fn history_item_id_decoder() {
        // Folder id (-1) → None.
        assert_eq!(history_index_from_item_id(HISTORY_FOLDER_ID as i32), None,);
        // Positive DB rowid → None.
        assert_eq!(history_index_from_item_id(1), None);
        assert_eq!(history_index_from_item_id(42), None);
        // Child ids -2, -3, -4 → 0, 1, 2.
        assert_eq!(history_index_from_item_id(-2), Some(0));
        assert_eq!(history_index_from_item_id(-3), Some(1));
        assert_eq!(history_index_from_item_id(-4), Some(2));
    }

    /// Folders with children must report `has-children = true`; empty
    /// folders and leaves `false`. Pinned because the TreeView draws the
    /// branch indicator based on this field.
    #[test]
    fn folders_report_has_children_correctly() {
        let items_in = vec![
            folder(1, 0, "non-empty"), // has a child
            plain(2, 1, "kid"),
            folder(3, 0, "empty"), // no children
            plain(4, 0, "sibling"),
        ];
        let items = no_history(&items_in);
        // Walk by text for clarity (history folder is appended last).
        let by_text: std::collections::HashMap<&str, &TreeItem> =
            items.iter().map(|r| (r.text.as_str(), r)).collect();
        assert!(by_text["non-empty"].has_children, "folder with kid");
        assert!(!by_text["empty"].has_children, "empty folder");
        assert!(!by_text["sibling"].has_children, "leaf");
    }

    /// `expanded` defaults to true for every folder not in the collapsed
    /// set. Pinned so the collapse plumbing can't silently regress.
    #[test]
    fn folders_default_expanded() {
        let items_in = vec![folder(1, 0, "f"), plain(2, 1, "child")];
        let items = no_history(&items_in);
        assert!(items[0].expanded);
    }

    /// A collapsed folder stays in the model (with `expanded = false`
    /// so the view draws ▶) but its descendants are omitted — collapse
    /// is "children not in the flat list".
    #[test]
    fn collapsed_folder_hides_descendants() {
        let items_in = vec![
            folder(1, 0, "root"),
            folder(2, 1, "inner"),
            plain(3, 2, "leaf"),
            plain(4, 0, "sibling"),
        ];
        let collapsed: HashSet<i64> = [1].into_iter().collect();
        let items = build_tree_items_with_history(
            &items_in,
            &[],
            HistoryPosition::Bottom,
            "Clipboard History",
            &collapsed,
        );
        // root (collapsed) + sibling + history folder — nothing below root.
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "root");
        assert!(!items[0].expanded, "collapsed folder reports it");
        assert!(items[0].has_children, "structural truth is unchanged");
        assert_eq!(items[1].text, "sibling");
    }

    /// Collapsing an inner folder leaves the outer one expanded and its
    /// other children visible.
    #[test]
    fn collapsed_inner_folder_keeps_outer_expanded() {
        let items_in = vec![
            folder(1, 0, "root"),
            folder(2, 1, "inner"),
            plain(3, 2, "leaf"),
            plain(4, 1, "other-kid"),
        ];
        let collapsed: HashSet<i64> = [2].into_iter().collect();
        let items = build_tree_items_with_history(
            &items_in,
            &[],
            HistoryPosition::Bottom,
            "Clipboard History",
            &collapsed,
        );
        // root, inner (collapsed), other-kid, history folder.
        assert_eq!(items.len(), 4);
        assert!(items[0].expanded);
        assert!(!items[1].expanded);
        assert_eq!(items[2].text, "other-kid");
        assert_eq!(items[2].depth, 1);
    }

    /// The virtual history folder collapses like any other: entries are
    /// omitted, the folder row reports `expanded = false` while
    /// `has-children` still reflects the ring buffer.
    #[test]
    fn collapsed_history_folder_hides_entries() {
        let history = vec![hist("foo"), hist("bar")];
        let collapsed: HashSet<i64> = [HISTORY_FOLDER_ID].into_iter().collect();
        let items = build_tree_items_with_history(
            &[],
            &history,
            HistoryPosition::Bottom,
            "Clipboard History",
            &collapsed,
        );
        assert_eq!(items.len(), 1, "folder only — entries hidden");
        assert_eq!(items[0].internal_id, HISTORY_FOLDER_ID as i32);
        assert!(!items[0].expanded);
        assert!(items[0].has_children);
    }

    /// Real DB rows store `parent_id = 0` to mean "root", but the
    /// TreeView reads `parent-internal-id` and treats -1 as "no parent".
    /// The seam translation is pinned here so it doesn't regress.
    #[test]
    fn root_items_report_no_parent_sentinel() {
        let items_in = vec![plain(1, 0, "root-item")];
        let items = no_history(&items_in);
        assert_eq!(items[0].parent_internal_id, NO_PARENT);
    }
}
