//! Short, one-line forms of ids and text for logs and file names.

/// Pi and chat ids are UUIDv7: the first 8 hex digits are a coarse timestamp,
/// so keep the next group too for a usable, mostly unique prefix.
pub fn short_id(id: &str) -> &str {
    id.get(..13).unwrap_or(id)
}

/// `text` cut to at most `max` characters, with `…` marking a cut.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// `text` on one line (whitespace runs become one space), at most `max`
/// characters.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&flat, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_is_char_safe_and_marks_the_cut() {
        assert_eq!(clip("жжжж", 2), "жж…");
        assert_eq!(clip("жж", 2), "жж");
        assert_eq!(one_line("  fix   the\nlogin bug ", 7), "fix the…");
        assert_eq!(short_id("01a0d3bf-536f-79b2"), "01a0d3bf-536f");
        assert_eq!(short_id("abc"), "abc");
    }
}
