//! Pure, iterative branch navigation over one read-only catalog snapshot.
//! No transcript loading, persistence, or runtime activation belongs here.

use std::collections::HashMap;

use crate::session::{CatalogEntry, CatalogLineage};

/// Deep chains keep their real ancestry but cannot consume the terminal width.
pub(super) const MAX_DISPLAY_DEPTH: usize = 16;
const MAX_QUERY_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BranchRow {
    pub entry: CatalogEntry,
    /// Index in `BranchForest::rows`, always preceding this row.
    pub parent: Option<usize>,
    pub depth: usize,
    /// Invalid metadata, missing parents, and broken cycles are visible roots.
    pub warning: Option<String>,
}

impl BranchRow {
    pub fn display_depth(&self) -> usize {
        self.depth.min(MAX_DISPLAY_DEPTH)
    }

    /// Render alongside the row: indentation is capped, never silently lost.
    pub fn depth_note(&self) -> Option<String> {
        (self.depth > MAX_DISPLAY_DEPTH).then(|| {
            format!(
                "{} ancestry levels not indented",
                self.depth - MAX_DISPLAY_DEPTH
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BranchMatch {
    pub index: usize,
    /// False means this row is retained solely to explain a matching descendant.
    pub direct_match: bool,
}

#[derive(Default, Debug)]
pub(super) struct BranchForest {
    /// Preorder, with stable newest-activity then descending-ID sibling ordering.
    /// Every input row occurs exactly once, including malformed/duplicate rows.
    pub rows: Vec<BranchRow>,
    search_text: Vec<[String; 3]>,
}

impl BranchForest {
    pub fn new(mut entries: Vec<CatalogEntry>) -> Self {
        entries.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
                // Catalog IDs are unique. Tie-break malformed synthetic inputs
                // too, without making hash-map iteration part of the ordering.
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.preview.cmp(&right.preview))
                .then_with(|| left.branch_point.cmp(&right.branch_point))
                .then_with(|| left.is_subagent.cmp(&right.is_subagent))
                .then_with(|| left.lineage.cmp(&right.lineage))
        });
        let count = entries.len();
        let mut by_id = HashMap::with_capacity(count);
        for (index, entry) in entries.iter().enumerate() {
            by_id
                .entry(entry.id.as_str())
                .and_modify(|value| *value = None)
                .or_insert(Some(index));
        }
        let mut parents = vec![None; count];
        let mut warnings = vec![None; count];
        for (index, entry) in entries.iter().enumerate() {
            if by_id[entry.id.as_str()].is_none() {
                warnings[index] = Some("Duplicate session ID; ancestry is ambiguous".into());
                continue;
            }
            match &entry.lineage {
                CatalogLineage::Root => {}
                CatalogLineage::Warning(warning) => warnings[index] = Some(warning.clone()),
                CatalogLineage::Branch(metadata) => {
                    match by_id.get(metadata.parent_session_id.as_str()) {
                        Some(Some(parent)) => parents[index] = Some(*parent),
                        Some(None) => {
                            warnings[index] = Some(format!(
                                "Ambiguous parent {}; shown as an orphan root",
                                metadata.parent_session_id
                            ));
                        }
                        None => {
                            warnings[index] = Some(format!(
                                "Missing parent {}; shown as an orphan root",
                                metadata.parent_session_id
                            ));
                        }
                    }
                }
            }
        }
        // A functional parent graph admits linear cycle detection. Each node
        // is walked once; no recursive DFS or per-row ancestor walk is needed.
        let mut colors = vec![0_u8; count];
        let mut positions = vec![0; count];
        let mut path: Vec<usize> = Vec::new();
        for start in 0..count {
            if colors[start] != 0 {
                continue;
            }
            path.clear();
            let mut next = Some(start);
            while let Some(index) = next {
                if colors[index] == 2 {
                    break;
                }
                if colors[index] == 1 {
                    // Cut the lexicographically smallest cycle member's edge,
                    // independent of input order, activity, or descendant order.
                    let cut = *path[positions[index]..]
                        .iter()
                        .min_by_key(|&&member| &entries[member].id)
                        .expect("a gray node is on this nonempty path");
                    let parent = parents[cut].take().expect("cycle member has a parent");
                    warnings[cut] = Some(format!(
                        "Cycle edge to {} omitted; shown as a root",
                        entries[parent].id
                    ));
                    break;
                }
                colors[index] = 1;
                positions[index] = path.len();
                path.push(index);
                next = parents[index];
            }
            for &index in &path {
                colors[index] = 2;
            }
        }
        let mut children = vec![Vec::new(); count];
        let mut roots = Vec::new();
        for (index, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(index);
            } else {
                roots.push(index);
            }
        }
        let mut stack: Vec<_> = roots
            .into_iter()
            .rev()
            .map(|index| (index, None, 0))
            .collect();
        let mut entries: Vec<_> = entries.into_iter().map(Some).collect();
        let mut rows = Vec::with_capacity(count);
        while let Some((index, parent, depth)) = stack.pop() {
            let row_index = rows.len();
            rows.push(BranchRow {
                entry: entries[index].take().expect("each row is visited once"),
                parent,
                depth,
                warning: warnings[index].take(),
            });
            stack.extend(
                children[index]
                    .iter()
                    .rev()
                    .map(|&child| (child, Some(row_index), depth + 1)),
            );
        }
        let search_text = rows
            .iter()
            .map(|row| {
                [
                    row.entry.id.to_lowercase(),
                    row.entry
                        .title
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase(),
                    row.entry
                        .preview
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase(),
                ]
            })
            .collect();
        Self { rows, search_text }
    }

    /// Case-insensitive name/ID/preview search. Retain all matching ancestors in
    /// preorder and label them via `direct_match`; unrelated descendants stay out.
    /// Work and storage are linear even for a matching leaf in a very deep chain.
    pub fn search(&self, query: &str) -> Vec<BranchMatch> {
        let query = query.trim();
        let mut end = query.len().min(MAX_QUERY_BYTES);
        while !query.is_char_boundary(end) {
            end -= 1;
        }
        let query = query[..end].to_lowercase();
        let direct: Vec<_> = self
            .search_text
            .iter()
            .map(|fields| query.is_empty() || fields.iter().any(|field| field.contains(&query)))
            .collect();
        let mut included = direct.clone();
        for index in (0..self.rows.len()).rev() {
            if included[index]
                && let Some(parent) = self.rows[index].parent
            {
                included[parent] = true;
            }
        }
        included
            .into_iter()
            .enumerate()
            .filter(|(_, include)| *include)
            .map(|(index, _)| BranchMatch {
                index,
                direct_match: direct[index],
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        BranchBoundary, BranchCompletion, BranchProvenance, BranchRequest, BranchSelection,
    };

    fn entry(id: &str, parent: Option<&str>, updated_at: u64) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            title: None,
            preview: None,
            branch_point: None,
            is_subagent: false,
            updated_at,
            lineage: parent.map_or(CatalogLineage::Root, |parent| {
                CatalogLineage::Branch(Box::new(BranchProvenance {
                    version: 1,
                    parent_session_id: parent.into(),
                    boundary: BranchBoundary {
                        state_index: 0,
                        prefix_len: 1,
                        prefix_hash: "a".repeat(64),
                    },
                    checkout_id: format!("checkout-{id}"),
                    request: BranchRequest {
                        id: format!("request-{id}"),
                        selection: BranchSelection {
                            provider: "openrouter".into(),
                            model: "model".into(),
                            reasoning: "default".into(),
                        },
                    },
                    completion: BranchCompletion {
                        prefix_len: 1,
                        prefix_hash: "b".repeat(64),
                        prompt_hash: "c".repeat(64),
                    },
                }))
            }),
        }
    }

    fn ids(forest: &BranchForest) -> Vec<&str> {
        forest
            .rows
            .iter()
            .map(|row| row.entry.id.as_str())
            .collect()
    }

    fn assert_forest(forest: &BranchForest, count: usize) {
        assert_eq!(forest.rows.len(), count);
        for (index, row) in forest.rows.iter().enumerate() {
            if let Some(parent) = row.parent {
                assert!(parent < index);
                assert_eq!(row.depth, forest.rows[parent].depth + 1);
            } else {
                assert_eq!(row.depth, 0);
            }
        }
    }

    #[test]
    fn empty_roots_nested_and_siblings_use_stable_activity_then_id_order() {
        let empty = BranchForest::new(Vec::new());
        assert!(empty.rows.is_empty());
        assert!(empty.search("").is_empty());
        let entries = vec![
            entry("root-a", None, 10),
            entry("root-b", None, 10),
            entry("old", Some("root-a"), 1),
            entry("new-a", Some("root-a"), 30),
            entry("new-b", Some("root-a"), 30),
            entry("grandchild", Some("old"), 100),
        ];
        let expected = BranchForest::new(entries.clone());
        assert_eq!(
            ids(&expected),
            ["root-b", "root-a", "new-b", "new-a", "old", "grandchild"]
        );
        assert_forest(&expected, entries.len());
        for shift in 0..entries.len() {
            let mut shuffled = entries.clone();
            shuffled.rotate_left(shift);
            shuffled.reverse();
            assert_eq!(BranchForest::new(shuffled).rows, expected.rows);
        }
    }

    #[test]
    fn missing_parents_and_metadata_warnings_stay_visible_as_roots() {
        let mut malformed = entry("malformed", None, 50);
        malformed.lineage = CatalogLineage::Warning("incomplete prompt checkout".into());
        let forest = BranchForest::new(vec![
            malformed,
            entry("orphan", Some("outside-workspace"), 10),
            entry("nested-orphan", Some("orphan"), 20),
            entry("legacy", None, 0),
        ]);
        assert_forest(&forest, 4);
        assert_eq!(
            ids(&forest),
            ["malformed", "orphan", "nested-orphan", "legacy"]
        );
        assert!(
            forest.rows[0]
                .warning
                .as_deref()
                .unwrap()
                .contains("incomplete")
        );
        assert!(
            forest.rows[1]
                .warning
                .as_deref()
                .unwrap()
                .contains("outside-workspace")
        );
        assert_eq!(forest.rows[2].parent, Some(1));
        assert!(forest.rows[3].warning.is_none());
    }

    #[test]
    fn cycles_cut_smallest_id_edge_and_preserve_every_row_once() {
        let entries = vec![
            entry("a", Some("b"), 2),
            entry("b", Some("c"), 20),
            entry("c", Some("a"), 30),
            entry("tail", Some("b"), 99),
            entry("self", Some("self"), 10),
            entry("unrelated", None, 0),
        ];
        let expected = BranchForest::new(entries.clone());
        assert_forest(&expected, entries.len());
        let mut unique = ids(&expected);
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), entries.len());
        let cut = expected
            .rows
            .iter()
            .find(|row| row.entry.id == "a")
            .unwrap();
        assert!(cut.parent.is_none());
        assert!(cut.warning.as_deref().unwrap().contains("Cycle edge to b"));
        let self_cycle = expected
            .rows
            .iter()
            .find(|row| row.entry.id == "self")
            .unwrap();
        assert!(self_cycle.parent.is_none());
        assert!(self_cycle.warning.is_some());
        for shift in 0..entries.len() {
            let mut shuffled = entries.clone();
            shuffled.rotate_left(shift);
            shuffled.reverse();
            assert_eq!(BranchForest::new(shuffled).rows, expected.rows);
        }
        let mut changed_activity = entries;
        changed_activity[0].updated_at = 1000;
        let forest = BranchForest::new(changed_activity);
        assert!(
            forest
                .rows
                .iter()
                .find(|row| row.entry.id == "a")
                .unwrap()
                .parent
                .is_none()
        );
    }

    #[test]
    fn search_matches_name_id_preview_and_retains_only_required_ancestors() {
        let mut child = entry("child", Some("root"), 5);
        child.title = Some("Café RENAMED".into());
        let mut leaf = entry("leaf-id", Some("child"), 10);
        leaf.preview = Some("Needle in the preview".into());
        let forest = BranchForest::new(vec![
            entry("root", None, 0),
            child,
            leaf,
            entry("other", Some("root"), 8),
        ]);
        for query in ["LEAF-ID", " needle "] {
            let matches = forest.search(query);
            assert_eq!(
                matches
                    .iter()
                    .map(|found| forest.rows[found.index].entry.id.as_str())
                    .collect::<Vec<_>>(),
                ["root", "child", "leaf-id"]
            );
            assert_eq!(
                matches
                    .iter()
                    .map(|found| found.direct_match)
                    .collect::<Vec<_>>(),
                [false, false, true]
            );
        }
        assert_eq!(forest.search("cAFÉ renamed").len(), 2);
        assert_eq!(forest.search("root").len(), 1);
        assert_eq!(forest.search(" ").len(), 4);
        assert!(forest.search("absent").is_empty());
        assert!(forest.search(&"界".repeat(MAX_QUERY_BYTES)).is_empty());
    }

    #[test]
    fn deep_and_wide_forests_are_iterative_and_indentation_is_bounded() {
        const COUNT: usize = 20_000;
        let mut chain = Vec::with_capacity(COUNT);
        for index in 0..COUNT {
            let parent = index.checked_sub(1).map(|index| format!("node-{index}"));
            chain.push(entry(
                &format!("node-{index}"),
                parent.as_deref(),
                index as u64,
            ));
        }
        let forest = BranchForest::new(chain);
        assert_forest(&forest, COUNT);
        let leaf = forest.rows.last().unwrap();
        assert_eq!(leaf.depth, COUNT - 1);
        assert_eq!(leaf.display_depth(), MAX_DISPLAY_DEPTH);
        assert!(
            leaf.depth_note()
                .unwrap()
                .contains("ancestry levels not indented")
        );
        assert!(forest.rows[0].depth_note().is_none());
        let matches = forest.search(&format!("node-{}", COUNT - 1));
        assert_eq!(matches.len(), COUNT);
        assert_eq!(matches.iter().filter(|found| found.direct_match).count(), 1);
        drop(forest); // Flat ownership also makes destruction stack-safe.
        let mut wide = vec![entry("root", None, 0)];
        wide.extend(
            (1..COUNT).map(|index| entry(&format!("child-{index}"), Some("root"), index as u64)),
        );
        let forest = BranchForest::new(wide);
        assert_forest(&forest, COUNT);
        assert!(
            forest.rows[1..]
                .iter()
                .all(|row| row.parent == Some(0) && row.depth == 1)
        );
    }

    #[test]
    fn duplicate_ids_do_not_drop_rows_or_invent_an_ambiguous_parent() {
        let mut duplicate = entry("duplicate", None, 1);
        duplicate.title = Some("renamed duplicate".into());
        let entries = vec![
            entry("duplicate", None, 1),
            duplicate,
            entry("duplicate", Some("different-parent"), 1),
            entry("child", Some("duplicate"), 10),
        ];
        let forest = BranchForest::new(entries.clone());
        assert_forest(&forest, 4);
        assert!(
            forest
                .rows
                .iter()
                .all(|row| row.parent.is_none() && row.warning.is_some())
        );
        let mut reversed = entries;
        reversed.reverse();
        assert_eq!(BranchForest::new(reversed).rows, forest.rows);
    }
}
