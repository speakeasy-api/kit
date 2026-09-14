//! Bounded, neutral display of compose source.

const MAX_SOURCE: usize = 64 * 1024;

pub(crate) fn bounded_source(source: String) -> String {
    if source.len() <= MAX_SOURCE {
        return source;
    }
    let mut end = MAX_SOURCE - 32;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… source truncated", &source[..end])
}

#[cfg(test)]
#[allow(clippy::disallowed_macros)]
mod tests {
    use super::*;

    #[test]
    fn bounded_source_preserves_small_input_and_utf8_at_the_limit() {
        for source in [String::new(), "return 🦀".into(), "x".repeat(MAX_SOURCE)] {
            assert_eq!(bounded_source(source.clone()), source);
        }
        let source = "🦀".repeat(MAX_SOURCE);
        let bounded = bounded_source(source.clone());
        assert!(bounded.len() <= MAX_SOURCE);
        let suffix = "\n… source truncated";
        assert!(bounded.ends_with(suffix));
        assert!(source.starts_with(&bounded[..bounded.len() - suffix.len()]));
    }
}
