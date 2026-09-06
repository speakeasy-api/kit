//! Read-only navigation of the currently rendered history, never fork addresses.

use unicode_segmentation::UnicodeSegmentation;

use super::app::Block;

/// Process-local block identity. It survives content replacement, not activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BlockId(u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Role {
    #[default]
    All,
    User,
    Assistant,
    Thought,
    Tool,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::User => "User",
            Self::Assistant => "Assistant",
            Self::Thought => "Thought",
            Self::Tool => "Tool",
        }
    }

    pub fn cycle(self, backwards: bool) -> Self {
        let roles = [
            Self::All,
            Self::User,
            Self::Assistant,
            Self::Thought,
            Self::Tool,
        ];
        let index = roles.iter().position(|role| *role == self).unwrap_or(0);
        roles[(index + if backwards { 4 } else { 1 }) % roles.len()]
    }
}

/// Bounds navigator matching, rendering, and grapheme-aware backspace work.
pub(super) const MAX_QUERY_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct Navigator {
    pub query: String,
    pub role: Role,
    pub selected: Option<BlockId>,
}

#[derive(Default)]
pub(super) struct Navigation {
    ids: Vec<BlockId>,
    next_id: u64,
    pub dialog: Option<Navigator>,
    pub revealed: Option<BlockId>,
    pub reveal_pending: bool,
    pub anchored: bool,
    search_query: String,
    search_matches: Vec<Option<(u64, bool)>>,
}

impl Navigation {
    pub fn push(&mut self) {
        self.next_id += 1;
        self.ids.push(BlockId(self.next_id));
    }

    pub fn reset(&mut self) {
        self.ids.clear();
        self.search_matches.clear();
        self.dialog = None;
        self.revealed = None;
        self.reveal_pending = false;
        self.anchored = false;
    }

    /// Direct block setup is used by render tests; production appends via push.
    pub fn sync(&mut self, len: usize) {
        self.ids.truncate(len);
        while self.ids.len() < len {
            self.push();
        }
    }

    pub fn id(&self, index: usize) -> Option<BlockId> {
        self.ids.get(index).copied()
    }

    pub fn index(&self, id: BlockId) -> Option<usize> {
        self.ids.iter().position(|candidate| *candidate == id)
    }

    pub fn is_revealed(&self, index: usize) -> bool {
        self.revealed.is_some() && self.id(index) == self.revealed
    }

    pub fn matches(&mut self, blocks: &[Block], revisions: &[u64]) -> Vec<usize> {
        let Some(dialog) = &self.dialog else {
            return Vec::new();
        };
        let query = dialog.query.to_lowercase();
        if self.search_query != query {
            self.search_query = query;
            self.search_matches.clear();
        }
        self.search_matches.resize(blocks.len(), None);
        blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                let role = role(block)?;
                if dialog.role != Role::All && dialog.role != role {
                    return None;
                }
                if self.search_query.is_empty() {
                    return Some(index);
                }
                // Animated frames and role changes reuse the text match. Only a
                // changed query or block revision rescans potentially large output.
                if let Some((revision, matched)) = self.search_matches[index]
                    && revisions.get(index) == Some(&revision)
                {
                    return matched.then_some(index);
                }
                let matched =
                    text_parts(block).any(|text| text.to_lowercase().contains(&self.search_query));
                self.search_matches[index] =
                    revisions.get(index).map(|revision| (*revision, matched));
                matched.then_some(index)
            })
            .collect()
    }

    pub fn reconcile(&mut self, matches: &[usize]) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        if !matches
            .iter()
            .any(|index| self.ids.get(*index).copied() == dialog.selected)
        {
            dialog.selected = matches
                .first()
                .and_then(|index| self.ids.get(*index))
                .copied();
        }
    }
}

pub(super) fn role(block: &Block) -> Option<Role> {
    match block {
        Block::User(_) => Some(Role::User),
        Block::Agent(_) => Some(Role::Assistant),
        Block::Thought { .. } => Some(Role::Thought),
        Block::Tool(_) => Some(Role::Tool),
        Block::TurnDuration(_) | Block::Notice(_) | Block::Error(_) => None,
    }
}

// Only already-decoded display text participates. Never inspect image.data,
// raw ACP input, or binary media payloads for a search or preview.
fn text_parts(block: &Block) -> impl Iterator<Item = &str> {
    let (main, script, intent, output) = match block {
        Block::User(message) => (message.text.as_str(), None, None, [].as_slice()),
        Block::Agent(text) | Block::Thought { text, .. } => {
            (text.as_str(), None, None, [].as_slice())
        }
        Block::Tool(call) => (
            call.title.as_str(),
            Some(call.script.as_str()),
            call.intent.as_deref(),
            call.output.as_slice(),
        ),
        _ => ("", None, None, [].as_slice()),
    };
    std::iter::once(main)
        .chain(intent)
        .chain(script)
        .chain(output.iter().map(String::as_str))
}

// Shared across text parts, including grapheme lookahead and control filtering.
const MAX_PREVIEW_BYTES: usize = 4096;

pub(super) fn preview(block: &Block) -> String {
    let mut remaining = MAX_PREVIEW_BYTES;
    let mut count = 0;
    let mut preview = String::new();
    'parts: for text in text_parts(block) {
        // Bound source bytes BEFORE segmentation: even one grapheme (including
        // the 97th, used as lookahead) can contain arbitrarily many codepoints.
        let end = text.floor_char_boundary(text.len().min(remaining));
        remaining -= end;
        let truncated = end < text.len();
        for (index, grapheme) in text[..end].grapheme_indices(true) {
            // The final cluster may continue outside the bounded source slice.
            // Omit it rather than scan beyond the budget or display a fragment.
            if count == 96 || (truncated && index + grapheme.len() == end) {
                preview.push('…');
                break 'parts;
            }
            preview.push_str(if grapheme.chars().any(char::is_control) {
                " "
            } else {
                grapheme
            });
            count += 1;
        }
        if truncated || count == 96 {
            preview.push('…');
            break;
        }
        preview.push(' ');
        count += 1;
    }
    preview.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Navigator {
    pub fn insert(&mut self, text: &str) {
        let remaining = MAX_QUERY_BYTES.saturating_sub(self.query.len());
        if remaining == 0 {
            return;
        }
        // Limit source characters BEFORE filtering or segmenting: even a paste
        // made entirely of controls or one huge combining cluster is bounded.
        // One lookahead character lets a full ASCII prefix use the byte budget.
        let mut consumed = 0;
        let candidate: String = text
            .chars()
            .take(remaining + 1)
            .filter(|character| {
                consumed += character.len_utf8();
                !character.is_control()
            })
            .collect();
        for (index, grapheme) in candidate.grapheme_indices(true) {
            // The last cluster may continue beyond our bounded source prefix.
            // Never retain that potentially incomplete cluster.
            if consumed < text.len() && index + grapheme.len() == candidate.len() {
                break;
            }
            if self.query.len() + grapheme.len() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push_str(grapheme);
        }
    }

    pub fn backspace(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Navigation, Navigator, Role, preview};
    use crate::tui::app::Block;

    #[test]
    fn query_backspace_removes_whole_unicode_graphemes() {
        let mut dialog = Navigator::default();
        dialog.insert("École e\u{301}👩‍💻\n\u{1b}");
        assert_eq!(dialog.query, "École e\u{301}👩‍💻");
        dialog.backspace();
        assert_eq!(dialog.query, "École e\u{301}");
        dialog.backspace();
        assert_eq!(dialog.query, "École ");
        dialog.query.clear();
        dialog.backspace();
        assert!(dialog.query.is_empty());
    }

    #[test]
    fn query_large_pastes_bound_source_work_and_stored_bytes() {
        for (paste, expected) in [
            ("a".repeat(1_000_000), "a".repeat(super::MAX_QUERY_BYTES)),
            ("\n".repeat(1_000_000), String::new()),
            (
                format!("{}unvisited", "\n".repeat(super::MAX_QUERY_BYTES + 1)),
                String::new(),
            ),
            (format!("e{}", "\u{301}".repeat(1_000_000)), String::new()),
        ] {
            let mut dialog = Navigator::default();
            dialog.insert(&paste);
            assert_eq!(dialog.query, expected);
        }

        let mut dialog = Navigator {
            query: "a".repeat(super::MAX_QUERY_BYTES),
            ..Navigator::default()
        };
        dialog.insert(&"b".repeat(1_000_000));
        assert_eq!(dialog.query, "a".repeat(super::MAX_QUERY_BYTES));
        dialog.backspace();
        dialog.insert("z");
        assert!(dialog.query.ends_with('z'));
        assert_eq!(dialog.query.len(), super::MAX_QUERY_BYTES);
    }

    #[test]
    fn query_byte_cap_preserves_whole_graphemes() {
        for grapheme in ["é", "e\u{301}", "👩‍💻", "🇺🇸"] {
            let prefix = "x".repeat(super::MAX_QUERY_BYTES - grapheme.len());
            let mut dialog = Navigator {
                query: prefix.clone(),
                ..Navigator::default()
            };
            dialog.insert(&format!("{grapheme}overflow"));
            assert_eq!(dialog.query, format!("{prefix}{grapheme}"));
            dialog.backspace();
            assert_eq!(dialog.query, prefix);

            dialog.insert("x");
            let before = dialog.query.clone();
            dialog.insert(grapheme);
            assert_eq!(dialog.query, before, "must not retain a partial {grapheme}");
        }
        // A combining cluster that continues beyond the source-work budget is
        // rejected even when its bounded prefix alone would fit the byte cap.
        let mut dialog = Navigator {
            query: "x".repeat(super::MAX_QUERY_BYTES - 1),
            ..Navigator::default()
        };
        dialog.insert("e\n\u{301}");
        assert_eq!(dialog.query.len(), super::MAX_QUERY_BYTES - 1);
    }

    #[test]
    fn previews_are_compact_and_do_not_split_graphemes() {
        let block = Block::Agent(format!("{}👩‍💻end\nnext", "é".repeat(95)));
        let rendered = preview(&block);
        assert_eq!(rendered, format!("{}👩‍💻…", "é".repeat(95)));
        assert_eq!(
            preview(&Block::Agent("first\n\tsecond".into())),
            "first second"
        );
    }

    #[test]
    fn previews_omit_huge_clusters_at_the_prefix_limit_and_lookahead() {
        for prefix_len in [0, 1, 95, 96] {
            let prefix = "x".repeat(prefix_len);
            let block = Block::Agent(format!("{prefix}e{}unvisited", "\u{301}".repeat(1_000_000)));
            assert_eq!(preview(&block), format!("{prefix}…"));
        }
    }

    #[test]
    fn previews_omit_clusters_cut_at_byte_or_joiner_boundaries() {
        for (remaining, suffix) in [(3, "e\u{301}tail"), (7, "👩‍💻tail"), (5, "🇺🇸tail")]
        {
            // A complete first cluster uses most of the byte budget. The next
            // cluster is cut after a combining mark/ZWJ or inside a codepoint.
            let prefix = format!(
                "e{}",
                "\u{301}".repeat((super::MAX_PREVIEW_BYTES - remaining - 1) / 2)
            );
            assert_eq!(prefix.len() + remaining, super::MAX_PREVIEW_BYTES);
            assert_eq!(
                preview(&Block::Agent(format!("{prefix}{suffix}"))),
                format!("{prefix}…")
            );
        }
    }

    #[test]
    fn previews_preserve_complete_clusters_at_the_source_boundary() {
        let cluster = format!("e{}", "\u{301}".repeat((super::MAX_PREVIEW_BYTES - 2) / 2));
        assert_eq!(
            preview(&Block::Agent(format!("{cluster}x"))),
            format!("{cluster}x")
        );
        // With more source available, the last bounded cluster is uncertain.
        assert_eq!(
            preview(&Block::Agent(format!("{cluster}xunvisited"))),
            format!("{cluster}…")
        );
    }

    #[test]
    fn search_cache_tracks_query_and_block_revisions_not_animation() {
        let mut navigation = Navigation::default();
        let mut blocks = vec![Block::Agent("ÉCOLE".into())];
        navigation.sync(blocks.len());
        navigation.dialog = Some(Navigator {
            query: "école".into(),
            ..Navigator::default()
        });
        assert_eq!(navigation.matches(&blocks, &[1]), vec![0]);
        assert_eq!(navigation.search_matches, vec![Some((1, true))]);
        navigation.dialog.as_mut().unwrap().role = Role::User;
        assert!(navigation.matches(&blocks, &[1]).is_empty());
        navigation.dialog.as_mut().unwrap().role = Role::All;
        assert_eq!(navigation.matches(&blocks, &[1]), vec![0]);
        blocks[0] = Block::Agent("changed".into());
        assert!(navigation.matches(&blocks, &[2]).is_empty());
        assert_eq!(navigation.search_matches, vec![Some((2, false))]);
        navigation.dialog.as_mut().unwrap().query = "CHANGED".into();
        assert_eq!(navigation.matches(&blocks, &[2]), vec![0]);
        navigation.reset();
        assert!(navigation.search_matches.is_empty());
    }
}
