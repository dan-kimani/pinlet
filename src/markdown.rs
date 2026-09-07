//! Markdown rendering for the note preview.
//!
//! The note editor shows the raw Markdown source; a separate read-only preview
//! renders it. `source_to_preview` turns task-list checkboxes into ☐/☑ glyphs,
//! and `MarkdownStyler` styles the text with GTK text tags (bold, italic,
//! headings, code, lists, block quotes), hiding the `**`/`# ` markers with an
//! `invisible` tag. The preview is never edited, so no cursor or round-trip
//! concerns arise.

use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{TextBuffer, TextTag};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const OPTIONS: Options = Options::ENABLE_TASKLISTS.union(Options::ENABLE_STRIKETHROUGH);

/// A styling decision over a byte range of the Markdown source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    kind: SpanKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    /// Hide these characters (a formatting marker).
    Hide,
    /// Apply this style to the characters.
    Style(Style),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
    H1,
    H2,
    H3,
    Quote,
    /// A bullet or number marker on a list item.
    ListMarker,
    /// Indent a list item to the given nesting depth (1-based).
    Indent(usize),
}

impl Style {
    fn heading(level: HeadingLevel) -> Style {
        match level {
            HeadingLevel::H1 => Style::H1,
            HeadingLevel::H2 => Style::H2,
            HeadingLevel::H3 => Style::H3,
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => Style::H3,
        }
    }
}

impl Span {
    fn hide(start: usize, end: usize) -> Span {
        Span { start, end, kind: SpanKind::Hide }
    }
    fn style(start: usize, end: usize, style: Style) -> Span {
        Span { start, end, kind: SpanKind::Style(style) }
    }
}

/// One open inline/block element while walking the event stream.
#[derive(Debug, Clone, Copy)]
struct Active {
    style: Style,
    /// Byte offset of the element's opening marker (or fence).
    marker: usize,
    /// Width of the inline delimiter on each side (`**` = 2, `*` = 1).
    delimiter: usize,
    /// Byte offset where the element's first text begins, if seen yet.
    content_start: Option<usize>,
    /// Byte offset where the element's last text ends, if seen yet.
    content_end: Option<usize>,
}

/// Compute the hide/style spans for `text`, in byte offsets.
///
/// Pure and free of GTK so it can be unit-tested.
fn compute_styles(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut stack: Vec<Active> = Vec::new();
    let mut list_depth: usize = 0;
    let mut item_start: Option<usize> = None;
    let mut item_depth: usize = 0;
    let mut item_indent_done: bool = false;

    for (event, range) in Parser::new_ext(text, OPTIONS).into_offset_iter() {
        let s = range.start;
        let e = range.end;
        match event {
            Event::Start(tag) => match tag {
                Tag::Emphasis => {
                    spans.push(Span::hide(s, s + 1));
                    stack.push(Active { style: Style::Italic, marker: s, delimiter: 1, content_start: None, content_end: None });
                }
                Tag::Strong => {
                    spans.push(Span::hide(s, s + 2));
                    stack.push(Active { style: Style::Bold, marker: s, delimiter: 2, content_start: None, content_end: None });
                }
                Tag::Strikethrough => {
                    spans.push(Span::hide(s, s + 2));
                    stack.push(Active { style: Style::Strike, marker: s, delimiter: 2, content_start: None, content_end: None });
                }
                Tag::Link { .. } => stack.push(Active {
                    style: Style::Link,
                    marker: s,
                    delimiter: 0,
                    content_start: None,
                    content_end: None,
                }),
                Tag::Heading { level, .. } => stack.push(Active {
                    style: Style::heading(level),
                    marker: s,
                    delimiter: 0,
                    content_start: None,
                    content_end: None,
                }),
                Tag::BlockQuote(..) => stack.push(Active {
                    style: Style::Quote,
                    marker: s,
                    delimiter: 0,
                    content_start: None,
                    content_end: None,
                }),
                Tag::CodeBlock(..) => stack.push(Active {
                    style: Style::Code,
                    marker: s,
                    delimiter: 0,
                    content_start: None,
                    content_end: None,
                }),
                Tag::List(_) => list_depth += 1,
                Tag::Item => {
                    item_start = Some(s);
                    item_depth = list_depth;
                    item_indent_done = false;
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    if let Some(active) = stack.pop() {
                        if active.delimiter > 0 {
                            spans.push(Span::hide(e - active.delimiter, e));
                        }
                    }
                }
                TagEnd::Heading(..) | TagEnd::BlockQuote(..) => {
                    if let Some(active) = stack.pop() {
                        if let Some(content) = active.content_start {
                            spans.push(Span::hide(active.marker, content));
                        }
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(active) = stack.pop() {
                        if let (Some(cs), Some(ce)) = (active.content_start, active.content_end) {
                            spans.push(Span::hide(active.marker, cs));
                            spans.push(Span::hide(ce, e));
                        }
                    }
                }
                TagEnd::Link => {
                    stack.pop();
                }
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                TagEnd::Item => item_start = None,
                _ => {}
            },
            Event::Text(_) => {
                if let Some(item) = item_start {
                    if !item_indent_done {
                        emit_item_marker(&mut spans, item, s, e, item_depth);
                        item_indent_done = true;
                    }
                }
                for active in &mut stack {
                    if active.content_start.is_none() {
                        active.content_start = Some(s);
                    }
                    active.content_end = Some(e);
                }
                for active in &stack {
                    spans.push(Span::style(s, e, active.style));
                }
            }
            Event::Code(_) => {
                // Inline code span: the range includes the backticks.
                let n = text[s..].chars().take_while(|&c| c == '`').count();
                let cs = s + n;
                let ce = e - n;
                if let Some(item) = item_start {
                    if !item_indent_done {
                        emit_item_marker(&mut spans, item, cs, ce, item_depth);
                        item_indent_done = true;
                    }
                }
                for active in &mut stack {
                    if active.content_start.is_none() {
                        active.content_start = Some(cs);
                    }
                    active.content_end = Some(ce);
                }
                spans.push(Span::hide(s, cs));
                spans.push(Span::hide(ce, e));
                spans.push(Span::style(cs, ce, Style::Code));
                for active in &stack {
                    spans.push(Span::style(cs, ce, active.style));
                }
            }
            _ => {}
        }
    }

    spans
}

/// Style the bullet/number and indent a list item, called once per item on
/// its first content.
fn emit_item_marker(
    spans: &mut Vec<Span>,
    item_start: usize,
    content_start: usize,
    content_end: usize,
    depth: usize,
) {
    if depth >= 2 {
        // `left-margin` is paragraph-level, so tagging any range inside the
        // item indents the whole paragraph.
        spans.push(Span::style(content_start, content_end, Style::Indent(depth)));
    }
    if content_start > item_start {
        spans.push(Span::style(item_start, content_start, Style::ListMarker));
    }
}

/// The GTK text tags, created once per buffer and reused on every restyle.
#[derive(Clone)]
struct MarkdownTags {
    bold: TextTag,
    italic: TextTag,
    strike: TextTag,
    code: TextTag,
    link: TextTag,
    h1: TextTag,
    h2: TextTag,
    h3: TextTag,
    quote: TextTag,
    marker: TextTag,
    list_marker: TextTag,
    /// `left-margin` tags, one per extra nesting level (index 0 = depth 2).
    list_indent: Vec<TextTag>,
}

impl MarkdownTags {
    fn new(table: &gtk4::TextTagTable) -> Self {
        let tags = MarkdownTags {
            bold: TextTag::builder().name("md-bold").weight(700).build(),
            italic: TextTag::builder().name("md-italic").style(pango::Style::Italic).build(),
            strike: TextTag::builder().name("md-strike").strikethrough(true).build(),
            code: TextTag::builder().name("md-code").family("monospace").build(),
            link: TextTag::builder()
                .name("md-link")
                .underline(pango::Underline::Single)
                .build(),
            h1: TextTag::builder()
                .name("md-h1")
                .weight(700)
                .scale(1.5)
                .build(),
            h2: TextTag::builder()
                .name("md-h2")
                .weight(700)
                .scale(1.3)
                .build(),
            h3: TextTag::builder()
                .name("md-h3")
                .weight(700)
                .scale(1.15)
                .build(),
            quote: TextTag::builder()
                .name("md-quote")
                .left_margin(24)
                .style(pango::Style::Italic)
                .build(),
            marker: TextTag::builder().name("md-marker").invisible(true).build(),
            list_marker: TextTag::builder()
                .name("md-list-marker")
                .foreground_rgba(&gtk4::gdk::RGBA::new(0.5, 0.5, 0.5, 0.6))
                .build(),
            list_indent: (0..6)
                .map(|i| {
                    TextTag::builder()
                        .name(&format!("md-indent-{i}"))
                        .left_margin(24 * (i as i32 + 1))
                        .build()
                })
                .collect(),
        };
        for tag in tags.all() {
            table.add(tag);
        }
        tags
    }

    fn all(&self) -> Vec<&TextTag> {
        let mut tags = vec![
            &self.bold,
            &self.italic,
            &self.strike,
            &self.code,
            &self.link,
            &self.h1,
            &self.h2,
            &self.h3,
            &self.quote,
            &self.marker,
            &self.list_marker,
        ];
        tags.extend(self.list_indent.iter());
        tags
    }

    fn for_style(&self, style: Style) -> &TextTag {
        match style {
            Style::Bold => &self.bold,
            Style::Italic => &self.italic,
            Style::Strike => &self.strike,
            Style::Code => &self.code,
            Style::Link => &self.link,
            Style::H1 => &self.h1,
            Style::H2 => &self.h2,
            Style::H3 => &self.h3,
            Style::Quote => &self.quote,
            Style::ListMarker => &self.list_marker,
            Style::Indent(depth) => {
                let index = depth.saturating_sub(2).min(self.list_indent.len() - 1);
                &self.list_indent[index]
            }
        }
    }
}

/// Styles a note preview buffer on demand.
pub struct MarkdownStyler {
    tags: MarkdownTags,
}

impl MarkdownStyler {
    /// Create the styler for `buffer`, registering its tags once.
    pub fn new(buffer: &TextBuffer) -> Self {
        let tags = MarkdownTags::new(&buffer.tag_table());
        Self { tags }
    }

    /// Re-render the buffer's Markdown source: clear prior styling, then
    /// re-parse and re-apply tags. Only touches tags, never the text, so it is
    /// safe to call on a read-only preview buffer.
    pub fn restyle(&self, buffer: &TextBuffer) {
        let start = buffer.start_iter();
        let end = buffer.end_iter();
        let text = buffer.text(&start, &end, true).to_string();

        for tag in self.tags.all() {
            buffer.remove_tag(tag, &start, &end);
        }
        if !text.trim().is_empty() {
            let char_of = char_offsets(&text);
            for span in compute_styles(&text) {
                let tag = match span.kind {
                    SpanKind::Hide => &self.tags.marker,
                    SpanKind::Style(style) => self.tags.for_style(style),
                };
                if span.start >= span.end {
                    continue;
                }
                let tag_start = buffer.iter_at_offset(char_of[span.start] as i32);
                let tag_end = buffer.iter_at_offset(char_of[span.end] as i32);
                buffer.apply_tag(tag, &tag_start, &tag_end);
            }
        }
    }
}

/// Convert canonical Markdown source into preview text: task-list checkboxes
/// (`- [ ]` / `- [x]`) become ☐/☑ glyphs, with the list bullet dropped. The
/// preview is read-only, so a plain text transform is safe (no round-trip).
pub fn source_to_preview(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    let mut item_start: Option<usize> = None;
    for (event, range) in Parser::new_ext(source, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Item) => item_start = Some(range.start),
            Event::TaskListMarker(checked) => {
                if let Some(item) = item_start {
                    out.push_str(&source[last..item]);
                    out.push(if checked { '☑' } else { '☐' });
                    last = range.end;
                }
            }
            Event::End(TagEnd::Item) => item_start = None,
            _ => {}
        }
    }
    out.push_str(&source[last..]);
    out
}

/// Prefix array mapping byte offsets to character (codepoint) offsets.
/// `result[b]` is the number of characters before byte `b`.
fn char_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize; text.len() + 1];
    let mut chars = 0usize;
    for (byte, _) in text.char_indices() {
        offsets[byte] = chars;
        chars += 1;
    }
    offsets[text.len()] = chars;
    // Fill the interior bytes of multi-byte characters so any byte offset
    // maps to the number of characters before it.
    for i in 1..=text.len() {
        if offsets[i] == 0 {
            offsets[i] = offsets[i - 1];
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hide(start: usize, end: usize) -> Span {
        Span::hide(start, end)
    }
    fn styled(start: usize, end: usize, style: Style) -> Span {
        Span::style(start, end, style)
    }

    #[test]
    fn bold_and_italic_hide_delimiters() {
        let text = "Some **bold** and *italic*.";
        let spans = compute_styles(text);

        // `**` (5..7) and `**` (11..13) hidden; `bold` (7..11) styled Bold.
        assert!(spans.contains(&hide(5, 7)));
        assert!(spans.contains(&hide(11, 13)));
        assert!(spans.contains(&styled(7, 11, Style::Bold)));
        // `*italic*`: `*` (18..19) and `*` (25..26) hidden; text 19..25 italic.
        assert!(spans.contains(&hide(18, 19)));
        assert!(spans.contains(&hide(25, 26)));
        assert!(spans.contains(&styled(19, 25, Style::Italic)));
    }

    #[test]
    fn heading_hides_hash() {
        let text = "# Big heading\n";
        let spans = compute_styles(text);
        assert!(spans.contains(&hide(0, 2)));
        assert!(spans.contains(&styled(2, 13, Style::H1)));
    }

    #[test]
    fn source_to_preview_renders_checkboxes() {
        // Task-list checkboxes become ☐/☑, dropping the `- ` bullet.
        assert_eq!(
            source_to_preview("- [ ] open\n- [x] done\n"),
            "☐ open\n☑ done\n"
        );
        // Brackets that aren't a task list are left untouched.
        assert_eq!(source_to_preview("note [x] text\n"), "note [x] text\n");
    }

    #[test]
    fn inline_code_hides_backticks() {
        let text = "`code`";
        let spans = compute_styles(text);
        assert!(spans.contains(&hide(0, 1)));
        assert!(spans.contains(&hide(5, 6)));
        assert!(spans.contains(&styled(1, 5, Style::Code)));
    }

    #[test]
    fn char_offsets_count_codepoints() {
        // "aé" is 3 bytes, 2 chars.
        let offsets = char_offsets("aé");
        assert_eq!(offsets, vec![0, 1, 1, 2]);
        assert_eq!(offsets[3], 2);
    }

    /// Map each span to `(kind, source substring)` for readable assertions.
    fn annotated<'a>(text: &'a str, spans: &'a [Span]) -> Vec<(SpanKind, &'a str)> {
        spans
            .iter()
            .map(|span| (span.kind, &text[span.start..span.end]))
            .collect()
    }

    #[test]
    fn fenced_code_block_hides_fences() {
        let text = "```rust\nfn main() {}\n```\n";
        let spans = compute_styles(text);
        let a = annotated(text, &spans);
        // Opening fence hidden.
        assert!(a.contains(&(SpanKind::Hide, "```rust\n")));
        // Code body styled monospace.
        assert!(a.contains(&(SpanKind::Style(Style::Code), "fn main() {}\n")));
        // Closing fence hidden (may or may not include the trailing newline).
        assert!(a
            .iter()
            .any(|(kind, sub)| *kind == SpanKind::Hide && sub.starts_with("```") && !sub.contains("rust")));
    }

    #[test]
    fn blockquote_hides_marker_and_styles_text() {
        let text = "> quoted text\n";
        let spans = compute_styles(text);
        let a = annotated(text, &spans);
        assert!(a.contains(&(SpanKind::Hide, "> ")));
        assert!(a.contains(&(SpanKind::Style(Style::Quote), "quoted text")));
    }

    #[test]
    fn heading_contains_bold() {
        let text = "# Big **bold**\n";
        let spans = compute_styles(text);
        let a = annotated(text, &spans);
        assert!(a.contains(&(SpanKind::Hide, "# ")));
        assert!(a.contains(&(SpanKind::Style(Style::H1), "Big ")));
        assert!(a.contains(&(SpanKind::Style(Style::Bold), "bold")));
        assert!(a.contains(&(SpanKind::Style(Style::H1), "bold")));
    }

    #[test]
    fn incomplete_heading_keeps_hashes_visible() {
        // While typing a heading, the hashes must not hide prematurely.
        for text in ["#", "##", "## "] {
            let spans = compute_styles(text);
            assert!(
                !spans.iter().any(|span| span.kind == SpanKind::Hide),
                "hashes hid prematurely for {text:?}: {spans:?}"
            );
        }
        // Once content appears, the marker hides and the text is styled.
        let spans = compute_styles("## H");
        let a = annotated("## H", &spans);
        assert!(a.contains(&(SpanKind::Hide, "## ")));
        assert!(a.contains(&(SpanKind::Style(Style::H2), "H")));
    }

    #[test]
    fn list_marker_styles_bullet() {
        let text = "- one\n- two\n";
        let spans = compute_styles(text);
        let a = annotated(text, &spans);
        assert!(a.contains(&(SpanKind::Style(Style::ListMarker), "- ")));
        assert!(!spans
            .iter()
            .any(|span| matches!(span.kind, SpanKind::Style(Style::Indent(_)))));
    }

    #[test]
    fn nested_list_indents() {
        let text = "- parent\n  - child\n";
        let spans = compute_styles(text);
        let a = annotated(text, &spans);
        assert!(a.contains(&(SpanKind::Style(Style::Indent(2)), "child")));
    }
}
