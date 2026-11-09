//! Copying the transcript as formatted text: beside the plain text, an HTML
//! form of the Markdown the selection covers, so a paste into Slack, Notes or
//! a mail keeps headings, links, emphasis, lists and code.

use std::ops::Range;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use rune::TextSelection;

/// The prefix of the keys `block_keys` gives a part's blocks.
const BLOCK: &str = "blk";

/// Keys for the top-level blocks of `src`, so a selection's paths name the
/// block they're in; give them to `Markdown::block_keys`.
pub fn block_keys(src: &str) -> Vec<(usize, String)> {
    blocks(src)
        .into_iter()
        .enumerate()
        .map(|(i, at)| (at.start, format!("{BLOCK}{i}")))
        .collect()
}

/// The selection as HTML, with `source` giving a transcript part's Markdown
/// by its key; `None` when it covers no whole Markdown block, so copying
/// stays plain.
pub fn html<'a>(sel: &TextSelection, source: impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    let mut runs: Vec<Run> = Vec::new();
    for t in sel.texts().iter().filter(|t| t.is_selected()) {
        let segments: Vec<&str> = t.path().split('/').collect();
        let found = segments.windows(2).find_map(|w| {
            let block = w[1].strip_prefix(BLOCK)?.parse::<usize>().ok()?;
            Some((w[0], source(w[0])?, block))
        });
        match (found, runs.last_mut()) {
            (Some((part, _, block)), Some(Run::Blocks(b))) if b.part == part => {
                b.last = block;
                b.whole &= t.is_whole();
                b.drawn.push('\n');
                b.drawn.push_str(t.text());
            }
            (Some((part, src, block)), _) => runs.push(Run::Blocks(Blocks {
                part,
                src,
                first: block,
                last: block,
                whole: t.is_whole(),
                drawn: t.text().to_owned(),
            })),
            (None, _) => runs.push(Run::Plain(t.text().to_owned())),
        }
    }
    if !runs
        .iter()
        .any(|r| matches!(r, Run::Blocks(b) if b.is_markdown()))
    {
        return None;
    }
    let mut out = String::new();
    for run in &runs {
        match run {
            Run::Blocks(b) if b.is_markdown() => {
                let spans = blocks(b.src);
                let start = spans.get(b.first).map_or(0, |s| s.start);
                let end = spans.get(b.last).map_or(b.src.len(), |s| s.end);
                let md = b.src[start..end].trim_end();
                html::push_html(&mut out, Parser::new_ext(md, options()));
            }
            Run::Blocks(Blocks { drawn: text, .. }) | Run::Plain(text) => paragraph(&mut out, text),
        }
    }
    Some(out)
}

/// A stretch of the selection: blocks of one Markdown part, or other text.
enum Run<'a> {
    Blocks(Blocks<'a>),
    Plain(String),
}

struct Blocks<'a> {
    part: &'a str,
    src: &'a str,
    first: usize,
    last: usize,
    /// Every selected text in it is selected whole.
    whole: bool,
    /// The selected texts as drawn.
    drawn: String,
}

impl Blocks<'_> {
    /// Copies as Markdown: whole blocks, or more than one. Part of one block
    /// copies as drawn, as Rune's `copy_source` does.
    fn is_markdown(&self) -> bool {
        self.first != self.last || self.whole
    }
}

/// `text` as an HTML paragraph, its lines kept and its markup escaped.
fn paragraph(out: &mut String, text: &str) {
    let mut events = vec![Event::Start(Tag::Paragraph)];
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            events.push(Event::HardBreak);
        }
        events.push(Event::Text(CowStr::from(line)));
    }
    events.push(Event::End(TagEnd::Paragraph));
    html::push_html(out, events.into_iter());
}

/// Rune's Markdown dialect.
fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

/// Each top-level block's source, from the start of its first line to the
/// next block, as Rune's Markdown splits them.
fn blocks(src: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut depth = 0usize;
    for (ev, range) in Parser::new_ext(src, options()).into_offset_iter() {
        let starts = match &ev {
            Event::Start(tag) => {
                depth += 1;
                depth == 1 && is_block(tag)
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                false
            }
            Event::Rule => depth == 0,
            _ => false,
        };
        if starts {
            let at = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
            if let Some(prev) = out.last_mut() {
                prev.end = at;
            }
            out.push(at..src.len());
        }
    }
    out
}

fn is_block(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Table(_)
            | Tag::HtmlBlock
    )
}

#[cfg(test)]
mod tests {
    use rune::{Cx, El, Env, Runtime, Tx, col};
    use rune_kit::{Kit, Markdown};

    use super::{block_keys, html};

    const SRC: &str =
        "# Title\n\nHello **world**, see [docs](https://x.dev/a).\n\n- alpha\n- `beta`\n";

    fn view(_: &(), _: &mut Cx<()>) -> El {
        let k = Kit::neutral(true);
        col().w_full().child(
            Markdown::new(SRC)
                .block_keys(block_keys(SRC))
                .selectable(true)
                .build(&k)
                .key("m1#0"),
        )
    }

    /// The HTML that copying copies after `select`.
    fn copied(select: impl FnOnce(&mut Tx)) -> Option<String> {
        let mut rt = Runtime::new((), view, (480.0, 800.0), Env::new(true, 2.0));
        rt.frame(0.0);
        rt.update(|_, tx| select(tx));
        rt.frame(1.0 / 60.0);
        let mut out = None;
        rt.update(|_, tx| {
            let sel = tx.text_selection().expect("a selection");
            out = html(sel, |key| (key == "m1#0").then_some(SRC));
        });
        out
    }

    #[test]
    fn copies_whole_blocks_as_html() {
        let all = copied(|tx| tx.select_all_text("m1#0")).expect("html");
        assert_eq!(
            all,
            "<h1>Title</h1>\n<p>Hello <strong>world</strong>, see <a href=\"https://x.dev/a\">docs</a>.</p>\n<ul>\n<li>alpha</li>\n<li><code>beta</code></li>\n</ul>\n"
        );
        assert_eq!(
            copied(|tx| tx.select_text("m1#0/blk1", 6..11)),
            None,
            "part of a paragraph stays plain"
        );
    }
}
