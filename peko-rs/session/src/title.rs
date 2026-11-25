//! Session titles: the readable name beside a session's slug.
//!
//! A slug addresses a session (`sess:/user-bob/direct-links`); a title
//! says what it is about ("Direct device connections"). Whoever creates
//! a session may give one; when they do not, the session is named after
//! the first line of the task it was created for, so every session an
//! agent starts can be told apart in a listing and found by name.

/// Most characters a title keeps.
pub const MAX_TITLE_LEN: usize = 80;

/// Most characters of a task's first line used as a title.
const MAX_TASK_TITLE_LEN: usize = 60;

/// A title as stored: one line, single spaces, at most
/// [`MAX_TITLE_LEN`] characters. `None` when nothing is left.
#[must_use]
pub fn clean_title(raw: &str) -> Option<String> {
    let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| shorten(&line, MAX_TITLE_LEN))
}

/// A title for a session nobody named: the first line of its task that
/// has words in it, without list or heading marks, cut at a word.
#[must_use]
pub fn title_from_task(task: &str) -> Option<String> {
    let line = task
        .lines()
        .map(|line| {
            line.trim_start_matches(['#', '-', '*', '>', ' ', '\t'])
                .trim()
        })
        .find(|line| line.chars().any(char::is_alphanumeric))?;
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(shorten(&line, MAX_TASK_TITLE_LEN))
}

/// `line` cut to `max` characters, at a word where one is near, with an
/// ellipsis marking the cut.
fn shorten(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let kept: String = line.chars().take(max - 1).collect();
    let cut = match kept.rfind(' ') {
        Some(at) if at >= kept.len() / 2 => &kept[..at],
        _ => kept.as_str(),
    };
    format!("{}…", cut.trim_end_matches([',', ';', ':', '.', ' ']))
}

/// Whether `query` (already lowercased) names this session: it appears
/// in the title or in the session's own slug. An ancestor's slug does
/// not count, or a query for one session would name its whole subtree.
#[must_use]
pub fn names_session(query_lower: &str, title: Option<&str>, slug: Option<&str>) -> bool {
    !query_lower.is_empty()
        && [title, slug]
            .into_iter()
            .flatten()
            .any(|name| name.to_lowercase().contains(query_lower))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_one_line_of_single_spaces() {
        assert_eq!(
            clean_title("  Direct  device\n connections ").as_deref(),
            Some("Direct device connections")
        );
        assert_eq!(clean_title(" \n\t"), None);
    }

    #[test]
    fn a_long_title_is_cut_at_a_word() {
        let long = "word ".repeat(40);
        let title = clean_title(&long).unwrap();
        assert!(title.chars().count() <= MAX_TITLE_LEN, "{title}");
        assert!(title.ends_with("word…"), "{title}");
    }

    #[test]
    fn a_task_names_its_session_by_its_first_line_with_words() {
        assert_eq!(
            title_from_task("\n---\n## Fix the login redirect\n\nSteps:\n- one").as_deref(),
            Some("Fix the login redirect")
        );
        assert_eq!(title_from_task("- [ ] \n***\n"), None);
        assert_eq!(title_from_task(""), None);
    }

    #[test]
    fn a_long_first_line_is_cut_short() {
        let task = "Investigate why the nightly export job fails on the second shard, then report";
        let title = title_from_task(task).unwrap();
        assert_eq!(
            title,
            "Investigate why the nightly export job fails on the second…"
        );
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let title = clean_title(&"日本語".repeat(40)).unwrap();
        assert_eq!(title.chars().count(), MAX_TITLE_LEN);
    }

    #[test]
    fn a_query_names_a_session_by_title_or_slug() {
        assert!(names_session(
            "device",
            Some("Direct Device connections"),
            Some("a")
        ));
        assert!(names_session("direct-links", None, Some("Direct-Links")));
        assert!(!names_session("device", None, Some("a")));
        assert!(!names_session("device", None, None));
        assert!(!names_session("", Some("x"), Some("a")));
    }
}
