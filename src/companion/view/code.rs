//! How code reads in the transcript: code blocks in an editor's syntax
//! colours (One Dark and One Light), and inline code as coloured monospace
//! text without a chip. The phone app (`relay/assets/app.css`) uses the same
//! colours.

use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{Event, Options, Parser};
use rune::Color;
use rune::color::hex;
use rune_kit::Kit;
use rune_kit::markdown::{Highlight, MarkdownStyle, Syntax};

/// Token colours, dark then light. Inline code takes the keyword colour.
const PALETTE: [[(Syntax, &str); 7]; 2] = [
    [
        (Syntax::Keyword, "#c678dd"),
        (Syntax::String, "#98c379"),
        (Syntax::Number, "#d19a66"),
        (Syntax::Comment, "#7f848e"),
        (Syntax::Type, "#e5c07b"),
        (Syntax::Function, "#61afef"),
        (Syntax::Property, "#e06c75"),
    ],
    [
        (Syntax::Keyword, "#a626a4"),
        (Syntax::String, "#50a14f"),
        (Syntax::Number, "#986801"),
        (Syntax::Comment, "#a0a1a7"),
        (Syntax::Type, "#c18401"),
        (Syntax::Function, "#4078f2"),
        (Syntax::Property, "#e45649"),
    ],
];

/// The palette as colours, parsed once.
static COLORS: LazyLock<[Vec<(Syntax, Color)>; 2]> = LazyLock::new(|| {
    PALETTE.map(|tones| {
        tones
            .iter()
            .map(|&(kind, c)| (kind, hex(c).unwrap_or_else(|| panic!("bad colour {c}"))))
            .collect()
    })
});

/// The transcript's Markdown look: the kit's, with these code colours and no
/// chip behind inline code.
pub fn markdown_style(k: &Kit) -> MarkdownStyle {
    let style = MarkdownStyle::from_kit(k)
        .inline_code_bg(Color::TRANSPARENT)
        .inline_code_pad(1.0);
    palette(k)
        .iter()
        .fold(style, |style, &(kind, c)| style.syntax(kind, c))
}

/// Colour the inline code of Markdown `src`, as [`rune_kit::Markdown::highlights`] takes it.
pub fn inline_code_marks(k: &Kit, src: &str) -> Vec<(Range<usize>, Highlight)> {
    let color = palette(k)[0].1;
    inline_code(src)
        .into_iter()
        .map(|r| (r, Highlight::new().bg(Color::TRANSPARENT).fg(color)))
        .collect()
}

fn palette(k: &Kit) -> &'static [(Syntax, Color)] {
    &COLORS[usize::from(!k.dark)]
}

/// The source byte ranges of `src`'s inline code spans, backticks included.
fn inline_code(src: &str) -> Vec<Range<usize>> {
    if !src.contains('`') {
        return Vec::new();
    }
    // Rune's Markdown parses with these, so both see the same spans.
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    Parser::new_ext(src, options)
        .into_offset_iter()
        .filter_map(|(event, range)| matches!(event, Event::Code(_)).then_some(range))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_inline_code_only() {
        let cases: &[(&str, &[&str])] = &[
            ("no code here", &[]),
            ("run `cargo test` then `make`", &["`cargo test`", "`make`"]),
            ("a ``tick ` inside`` b", &["``tick ` inside``"]),
            ("```rust\nlet x = `a`;\n```\n", &[]),
            (
                "- item `x`\n\n| a | `b` |\n|---|---|\n| `c` | d |",
                &["`x`", "`b`", "`c`"],
            ),
            ("an `unclosed tick", &[]),
        ];
        for (src, want) in cases {
            let got: Vec<&str> = inline_code(src).into_iter().map(|r| &src[r]).collect();
            assert_eq!(&got, want, "source {src:?}");
        }
    }
}
