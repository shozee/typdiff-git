use std::collections::BTreeSet;
use std::fmt::Write;
use typst_syntax::{SyntaxKind, SyntaxNode};

use super::{Block, BlockKind, DiffResult, DiffSpan, SpanTag, TypstLabel};

const PREAMBLE: &str = r##"#let diff-added(body) = {
  set text(fill: rgb("#0000ff"))
  underline(body)
}
#let diff-deleted(body) = {
  set text(fill: rgb("#cc0000"))
  strike(body)
}
"##;

/// Render a list of diff results into a compilable Typst source string.
#[allow(dead_code)]
pub fn render(results: &[DiffResult]) -> String {
    render_with_citations(results, &BTreeSet::new())
}

/// Render a list of diff results into a compilable Typst source string,
/// preserving known bibliography citation references bare so Typst resolves them.
pub fn render_with_citations(
    results: &[DiffResult],
    citation_keys: &BTreeSet<String>,
) -> String {
    let mut out = String::new();
    out.push_str(PREAMBLE);
    out.push('\n');

    // Byte index of the closing `]` of the most recent `#diff-added[...]`
    // wrapping an equation or raw block, preserved across interleaved
    // `#diff-deleted[...]` blocks so a following label attaches to the
    // inner element rather than the `#diff-added`/`#diff-deleted` wrapper.
    let mut last_added_wrapped_close: Option<usize> = None;

    for result in results {
        match result {
            DiffResult::Unchanged(block) => {
                if let Block::Paragraph { source_text } = block {
                    if is_label_only(source_text) {
                        if let Some(close_idx) = last_added_wrapped_close {
                            let label_insert = format!(" {}", source_text.trim());
                            out.insert_str(close_idx, &label_insert);
                            last_added_wrapped_close = Some(close_idx + label_insert.len());
                            continue;
                        }
                    }
                }
                last_added_wrapped_close = None;
                render_block(block, &mut out);
                out.push('\n');
            }
            DiffResult::Added(block) => {
                if let Block::Paragraph { source_text } = block {
                    if is_label_only(source_text) {
                        if let Some(close_idx) = last_added_wrapped_close {
                            let label_insert = format!(" {}", source_text.trim());
                            out.insert_str(close_idx, &label_insert);
                            last_added_wrapped_close = Some(close_idx + label_insert.len());
                            continue;
                        }
                    }
                }
                render_added_or_deleted(block, DiffTag::Added, citation_keys, &mut out);
                if matches!(block, Block::Equation { .. } | Block::RawBlock { .. }) {
                    last_added_wrapped_close = out.len().checked_sub(1);
                } else {
                    last_added_wrapped_close = None;
                }
                out.push('\n');
            }
            DiffResult::Deleted(block) => {
                render_added_or_deleted(block, DiffTag::Deleted, citation_keys, &mut out);
                out.push('\n');
            }
            DiffResult::Modified { kind, spans } => {
                last_added_wrapped_close = None;
                render_modified(kind, spans, citation_keys, &mut out);
                out.push('\n');
            }
        }
        // Always add a blank line between blocks (paragraph break).
        // Parbreaks are filtered out before diffing, so we insert them
        // unconditionally here instead.
        out.push('\n');
    }

    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffTag {
    Added,
    Deleted,
}

impl DiffTag {
    fn func_name(self) -> &'static str {
        match self {
            DiffTag::Added => "diff-added",
            DiffTag::Deleted => "diff-deleted",
        }
    }
}

/// Render a block unchanged.
fn render_block(block: &Block, out: &mut String) {
    match block {
        Block::Paragraph { source_text } => out.push_str(source_text),
        Block::Heading { depth, body_text } => {
            write_heading_prefix(*depth, out);
            out.push_str(body_text);
        }
        Block::ListItem { body_text } => {
            out.push_str("- ");
            out.push_str(body_text);
        }
        Block::EnumItem { number, body_text } => {
            write_enum_prefix(*number, out);
            out.push_str(body_text);
        }
        Block::TermItem { term, description } => {
            out.push_str("/ ");
            out.push_str(term);
            out.push_str(": ");
            out.push_str(description);
        }
        Block::RawBlock { content } => out.push_str(content),
        Block::Equation { content, .. } => out.push_str(content),
        Block::FuncCall { content } => out.push_str(content),
        Block::Parbreak => {} // newline added by caller
    }
}

/// Returns true if the text consists only of a Typst label `<...>`.
fn is_label_only(text: &str) -> bool {
    TypstLabel::is_only(text)
}

/// Render a block wrapped in #diff-added[...] or #diff-deleted[...].
fn render_added_or_deleted(
    block: &Block,
    tag: DiffTag,
    citation_keys: &BTreeSet<String>,
    out: &mut String,
) {
    let escape_refs = tag == DiffTag::Deleted;
    let func = tag.func_name();

    match block {
        Block::Paragraph { source_text } => {
            // Labels must be output bare so they attach to the preceding element.
            // Deleted labels are suppressed to avoid duplicates with added labels.
            if is_label_only(source_text) {
                if tag == DiffTag::Added {
                    out.push_str(source_text.trim());
                }
                return;
            }
            write!(
                out,
                "#{}[{}]",
                func,
                escape_content(source_text, escape_refs, citation_keys)
            )
            .unwrap();
        }
        Block::Heading { depth, body_text } => {
            write_heading_prefix(*depth, out);
            write!(
                out,
                "#{}[{}]",
                func,
                escape_content(body_text, escape_refs, citation_keys)
            )
            .unwrap();
        }
        Block::ListItem { body_text } => {
            out.push_str("- ");
            write!(
                out,
                "#{}[{}]",
                func,
                escape_content(body_text, escape_refs, citation_keys)
            )
            .unwrap();
        }
        Block::EnumItem { number, body_text } => {
            write_enum_prefix(*number, out);
            write!(
                out,
                "#{}[{}]",
                func,
                escape_content(body_text, escape_refs, citation_keys)
            )
            .unwrap();
        }
        Block::TermItem { term, description } => {
            out.push_str("/ ");
            write!(
                out,
                "#{}[{}: {}]",
                func,
                escape_content(term, escape_refs, citation_keys),
                escape_content(description, escape_refs, citation_keys)
            )
            .unwrap();
        }
        // Raw blocks and equations are content-mode; they can be wrapped.
        Block::RawBlock { content } | Block::Equation { content, .. } => {
            write!(
                out,
                "#{}[{}]",
                func,
                escape_content(content, escape_refs, citation_keys)
            )
            .unwrap();
        }
        // FuncCall represents code-mode expressions (#import, #show, etc.).
        // Wrapping them in a content block would change their semantics,
        // so we output added code as-is and deleted code as comments.
        Block::FuncCall { content } => {
            if tag == DiffTag::Added {
                out.push_str(content);
            } else {
                for line in content.lines() {
                    write!(out, "// {line}").unwrap();
                    out.push('\n');
                }
                // Remove the trailing newline since the caller adds one.
                if out.ends_with('\n') {
                    out.pop();
                }
            }
        }
        Block::Parbreak => {} // paragraph breaks have no visual content to mark
    }
}

/// Render a modified block with per-span diff markup.
fn render_modified(
    kind: &BlockKind,
    spans: &[DiffSpan],
    citation_keys: &BTreeSet<String>,
    out: &mut String,
) {
    match kind {
        BlockKind::Heading { depth } => write_heading_prefix(*depth, out),
        BlockKind::ListItem => out.push_str("- "),
        BlockKind::EnumItem { number } => write_enum_prefix(*number, out),
        BlockKind::TermItem => out.push_str("/ "),
        BlockKind::Paragraph => {}
        BlockKind::Atomic | BlockKind::Parbreak => {}
    }
    render_spans(spans, citation_keys, out);
}

/// Render diff spans with markup for deleted/inserted text.
fn render_spans(
    spans: &[DiffSpan],
    citation_keys: &BTreeSet<String>,
    out: &mut String,
) {
    let mut prev_was_diff = false;
    let mut prev_was_deleted = false;
    let mut prev_had_citation = false;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        match span.tag {
            SpanTag::Equal => {
                // After `#diff-added[...]` or `#diff-deleted[...]`, a `(` or `[`
                // would be parsed as function arguments by Typst. Insert a
                // zero-width space to break the call syntax.
                if prev_was_diff && (span.text.starts_with('(') || span.text.starts_with('[')) {
                    out.push('\u{200B}');
                }
                out.push_str(&span.text);
                prev_was_diff = false;
                prev_was_deleted = false;
                prev_had_citation = false;
            }
            SpanTag::Deleted => {
                if is_label_only(&span.text) {
                    // Nothing is emitted, so keep `prev_was_diff` tied to the
                    // last actual output.
                    continue;
                }
                let has_cite = span_has_citation(&span.text, citation_keys);
                write!(
                    out,
                    "#diff-deleted[{}]",
                    escape_content(&span.text, true, citation_keys)
                )
                .unwrap();
                prev_was_diff = true;
                prev_was_deleted = true;
                prev_had_citation = has_cite;
            }
            SpanTag::Inserted => {
                if is_label_only(&span.text) {
                    out.push_str(&span.text);
                    prev_was_diff = false;
                    prev_was_deleted = false;
                    prev_had_citation = false;
                    continue;
                }
                let has_cite = span_has_citation(&span.text, citation_keys);
                if prev_was_deleted && (prev_had_citation || has_cite) {
                    // Prevent Typst from grouping adjacent citations across #diff-deleted and #diff-added.
                    // Without this separator, Typst's citation engine merges adjacent citations
                    // into a single citation group anchored at the first citation (#diff-deleted),
                    // rendering both the old and new citation together in red strikethrough!
                    out.push('\u{200B}');
                }
                write!(
                    out,
                    "#diff-added[{}]",
                    escape_content(&span.text, false, citation_keys)
                )
                .unwrap();
                prev_was_diff = true;
                prev_was_deleted = false;
                prev_had_citation = false;
            }
        }
    }
}

fn span_has_citation(text: &str, citation_keys: &BTreeSet<String>) -> bool {
    text.contains('@')
        || text.contains("#cite")
        || citation_keys.iter().any(|k| text.contains(k.as_str()))
}

/// Write the heading prefix (`= `, `== `, etc.).
fn write_heading_prefix(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push('=');
    }
    out.push(' ');
}

/// Write the enum item prefix (`+ ` for auto, `{n}. ` for explicit).
fn write_enum_prefix(number: Option<usize>, out: &mut String) {
    match number {
        Some(n) => write!(out, "{}. ", n).unwrap(),
        None => out.push_str("+ "),
    }
}

/// Escape content so it can safely be placed inside a Typst content block `[...]`.
///
/// Uses `typst_syntax` so Typst's mode boundaries (markup, code, math, raw,
/// strings, escapes, links, and comments) are respected:
/// - Balanced `[...]` pairs in markup are left untouched; unmatched `[` or `]`
///   in markup are escaped as `\[` or `\]`.
/// - Code-mode expressions (e.g. `#Fig(<label>)`, `#ref(<label>)`,
///   `#cite(<key>)`, string literals) are never modified with backslashes,
///   avoiding invalid `\` characters in Typst code, while markup inside nested
///   content blocks (`#footnote[...]`, `#citet[...]`, etc.) is still escaped.
/// - Raw blocks/inlines, math equations, escapes, URLs, and comments are left
///   intact.
/// - If the content ends inside a line comment without a trailing newline, a
///   newline is appended so the caller's closing `]` is not commented out.
/// - If the content ends with an odd number of backslashes, a trailing space is
///   appended to prevent the closing `]` from being interpreted as `\]`.
/// - When `escape_refs` is true, markup `@` references (other than known
///   bibliography citation keys) are escaped as `\@` and markup `<label>`
///   anchors are escaped as `\<label>`.
fn escape_content(
    s: &str,
    escape_refs: bool,
    citation_keys: &BTreeSet<String>,
) -> String {
    let root = typst_syntax::parse(s);
    let mut out = String::with_capacity(s.len());
    let mut ends_with_line_comment = false;
    escape_node(
        &root,
        true,
        escape_refs,
        citation_keys,
        &mut out,
        &mut ends_with_line_comment,
    );

    if ends_with_line_comment {
        out.push('\n');
    }

    // If the result ends with an odd number of backslashes, the closing `]`
    // added by the caller would be interpreted as `\]` (an escaped bracket).
    // Append a space to break the escape sequence.
    let trailing_backslashes = out.chars().rev().take_while(|&c| c == '\\').count();
    if trailing_backslashes % 2 != 0 {
        out.push(' ');
    }
    out
}

fn escape_node(
    node: &SyntaxNode,
    in_markup: bool,
    escape_refs: bool,
    citation_keys: &BTreeSet<String>,
    out: &mut String,
    ends_with_line_comment: &mut bool,
) {
    match node.kind() {
        SyntaxKind::Raw
        | SyntaxKind::Str
        | SyntaxKind::Link
        | SyntaxKind::Escape
        | SyntaxKind::LineComment
        | SyntaxKind::BlockComment => {
            let text = node.clone().into_text();
            if !text.is_empty() {
                *ends_with_line_comment = node.kind() == SyntaxKind::LineComment;
                out.push_str(&text);
            }
        }
        SyntaxKind::Markup => {
            let mut open_brackets: Vec<usize> = Vec::new();
            for child in node.children() {
                if child.kind() == SyntaxKind::Text {
                    for ch in child.text().chars() {
                        match ch {
                            '[' => {
                                open_brackets.push(out.len());
                                out.push('[');
                            }
                            ']' => {
                                if open_brackets.pop().is_some() {
                                    out.push(']');
                                } else {
                                    out.push_str("\\]");
                                }
                            }
                            _ => out.push(ch),
                        }
                        *ends_with_line_comment = false;
                    }
                } else {
                    escape_node(
                        child,
                        true,
                        escape_refs,
                        citation_keys,
                        out,
                        ends_with_line_comment,
                    );
                }
            }
            for pos in open_brackets.into_iter().rev() {
                out.insert(pos, '\\');
            }
        }
        SyntaxKind::ContentBlock => {
            let children: Vec<_> = node.children().collect();
            let is_closed = children
                .last()
                .is_some_and(|last| last.kind() == SyntaxKind::RightBracket);
            for (idx, child) in children.iter().enumerate() {
                if idx == 0 && child.kind() == SyntaxKind::LeftBracket && !is_closed {
                    out.push_str("\\[");
                    *ends_with_line_comment = false;
                } else {
                    escape_node(
                        child,
                        true,
                        escape_refs,
                        citation_keys,
                        out,
                        ends_with_line_comment,
                    );
                }
            }
        }
        SyntaxKind::Strong
        | SyntaxKind::Emph
        | SyntaxKind::Heading
        | SyntaxKind::ListItem
        | SyntaxKind::EnumItem
        | SyntaxKind::TermItem
        | SyntaxKind::Ref => {
            for child in node.children() {
                escape_node(
                    child,
                    true,
                    escape_refs,
                    citation_keys,
                    out,
                    ends_with_line_comment,
                );
            }
        }
        SyntaxKind::Label if in_markup && escape_refs => {
            out.push('\\');
            out.push_str(node.text());
            *ends_with_line_comment = false;
        }
        SyntaxKind::RefMarker if in_markup && escape_refs => {
            let text = node.text();
            let key = text.strip_prefix('@').unwrap_or(text.as_str());
            if citation_keys.contains(key) {
                out.push_str(text);
            } else {
                out.push('\\');
                out.push_str(text);
            }
            *ends_with_line_comment = false;
        }
        SyntaxKind::Error if in_markup && node.text().as_str() == "]" => {
            out.push_str("\\]");
            *ends_with_line_comment = false;
        }
        _ => {
            let mut has_children = false;
            for child in node.children() {
                has_children = true;
                // Only Markup/ContentBlock/markup containers stay in markup mode;
                // code expressions and equations switch out of markup mode.
                escape_node(
                    child,
                    false,
                    escape_refs,
                    citation_keys,
                    out,
                    ends_with_line_comment,
                );
            }
            if !has_children {
                let text = node.text();
                if !text.is_empty() {
                    out.push_str(text);
                    *ends_with_line_comment = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape(s: &str, escape_refs: bool) -> String {
        escape_content(s, escape_refs, &BTreeSet::new())
    }

    #[test]
    fn test_escape_content_no_special() {
        assert_eq!(escape("hello world", false), "hello world");
    }

    #[test]
    fn test_escape_content_balanced_brackets() {
        assert_eq!(escape("a [b] c", false), "a [b] c");
    }

    #[test]
    fn test_escape_content_unbalanced_bracket() {
        assert_eq!(escape("a ] b", false), "a \\] b");
    }

    #[test]
    fn test_escape_content_trailing_backslash() {
        assert_eq!(escape("text\\", false), "text\\ ");
    }

    #[test]
    fn test_escape_content_trailing_double_backslash() {
        assert_eq!(escape("text\\\\", false), "text\\\\");
    }

    #[test]
    fn test_escape_content_refs_not_escaped_by_default() {
        assert_eq!(escape("see @ref here", false), "see @ref here");
    }

    #[test]
    fn test_escape_content_refs_escaped_when_requested() {
        assert_eq!(escape("see @ref here", true), "see \\@ref here");
    }

    #[test]
    fn test_escape_content_labels_escaped_when_requested() {
        assert_eq!(escape("text <my-label>", true), "text \\<my-label>");
    }

    #[test]
    fn test_escape_content_ref_call_label_not_escaped_when_requested() {
        assert_eq!(
            escape("compare #ref(<tbl-cobalt-notes>, supplement: [])", true),
            "compare #ref(<tbl-cobalt-notes>, supplement: [])"
        );
    }

    #[test]
    fn test_escape_content_custom_func_label_args_not_escaped() {
        assert_eq!(
            escape("#Fig(<fig:EddyingCPS_uyz>)", true),
            "#Fig(<fig:EddyingCPS_uyz>)"
        );
        assert_eq!(
            escape("#Figure(<fig:QBOASH_mimjja>)", true),
            "#Figure(<fig:QBOASH_mimjja>)"
        );
        assert_eq!(
            escape("#Fig(<fig:a>, <fig:b>) and #Table(<tab:1>) and #Eq(<eq:1>)", true),
            "#Fig(<fig:a>, <fig:b>) and #Table(<tab:1>) and #Eq(<eq:1>)"
        );
        assert_eq!(
            escape("#ref(<tbl:1>, supplement: [see <inner> and @other])", true),
            "#ref(<tbl:1>, supplement: [see \\<inner> and \\@other])"
        );
        assert_eq!(
            escape("#footnote[see #Fig(<fig:kept>) and <deleted-lbl>]", true),
            "#footnote[see #Fig(<fig:kept>) and \\<deleted-lbl>]"
        );
    }

    #[test]
    fn test_escape_content_raw_math_string_escape_comment_untouched() {
        assert_eq!(
            escape("`#Eq(<eq:COBESST2_frzpt>)` and `]`", true),
            "`#Eq(<eq:COBESST2_frzpt>)` and `]`"
        );
        assert_eq!(
            escape("$ a < b <= c $ and $ [0, 1) $", true),
            "$ a < b <= c $ and $ [0, 1) $"
        );
        assert_eq!(
            escape(r#"#text("a@b]c<d>")"#, true),
            r#"#text("a@b]c<d>")"#
        );
        assert_eq!(
            escape(r"a \] b \[ c \@ d \<e>", true),
            r"a \] b \[ c \@ d \<e>"
        );
        assert_eq!(
            escape(r"a \\] b", false),
            r"a \\\] b"
        );
        assert_eq!(
            escape("a ] b [ c", false),
            r"a \] b \[ c"
        );
        assert_eq!(
            escape("a [ b [ c ] d", false),
            r"a \[ b [ c ] d"
        );
        assert_eq!(
            escape("heading // comment", false),
            "heading // comment\n"
        );
    }

    #[test]
    fn test_escape_content_citation_not_escaped() {
        let mut citations = BTreeSet::new();
        citations.insert("Smirnov2015".to_string());
        citations.insert("smirnovInvestigatingLocalAtmospheric2015".to_string());

        assert_eq!(
            escape_content("see @Smirnov2015 here", true, &citations),
            "see @Smirnov2015 here"
        );
        assert_eq!(
            escape_content("see @Smirnov2015. here", true, &citations),
            "see @Smirnov2015. here"
        );
        assert_eq!(
            escape_content("#citet[@smirnovInvestigatingLocalAtmospheric2015]", true, &citations),
            "#citet[@smirnovInvestigatingLocalAtmospheric2015]"
        );
        assert_eq!(
            escape_content("#cite(<Smirnov2015>)", true, &citations),
            "#cite(<Smirnov2015>)"
        );
    }

    #[test]
    fn test_is_label_only() {
        assert!(is_label_only("<my-label>"));
        assert!(is_label_only("  <my-label>  "));
        assert!(!is_label_only("text <label>"));
        assert!(!is_label_only("<a> <b>"));
        assert!(!is_label_only("no label"));
    }

    #[test]
    fn test_render_preamble() {
        let output = render(&[]);
        assert!(output.contains("#let diff-added"));
        assert!(output.contains("#let diff-deleted"));
    }

    #[test]
    fn test_render_unchanged() {
        let results = vec![DiffResult::Unchanged(Block::Paragraph {
            source_text: "Hello".into(),
        })];
        let output = render(&results);
        assert!(output.contains("\nHello\n"));
        let body = output.split("\n\n").last().unwrap();
        assert!(!body.contains("#diff-added["));
        assert!(!body.contains("#diff-deleted["));
    }

    #[test]
    fn test_render_added_paragraph() {
        let results = vec![DiffResult::Added(Block::Paragraph {
            source_text: "New text".into(),
        })];
        let output = render(&results);
        assert!(output.contains("#diff-added[New text]"));
    }

    #[test]
    fn test_render_deleted_paragraph_escapes_refs() {
        let results = vec![DiffResult::Deleted(Block::Paragraph {
            source_text: "see @old-ref here".into(),
        })];
        let output = render(&results);
        assert!(output.contains("#diff-deleted[see \\@old-ref here]"));
    }

    #[test]
    fn test_render_label_only_paragraph_not_wrapped() {
        let results = vec![DiffResult::Added(Block::Paragraph {
            source_text: "<my-label>".into(),
        })];
        let output = render(&results);
        assert!(output.contains("<my-label>"));
        assert!(!output.contains("#diff-added[<my-label>]"));
    }

    #[test]
    fn test_render_modified() {
        let results = vec![DiffResult::Modified {
            kind: BlockKind::Paragraph,
            spans: vec![
                DiffSpan {
                    tag: SpanTag::Equal,
                    text: "Hello ".into(),
                },
                DiffSpan {
                    tag: SpanTag::Deleted,
                    text: "world".into(),
                },
                DiffSpan {
                    tag: SpanTag::Inserted,
                    text: "there".into(),
                },
            ],
        }];
        let output = render(&results);
        assert!(output.contains("Hello #diff-deleted[world]#diff-added[there]"));
    }

    #[test]
    fn test_render_modified_label_bare() {
        let results = vec![DiffResult::Modified {
            kind: BlockKind::Paragraph,
            spans: vec![
                DiffSpan {
                    tag: SpanTag::Deleted,
                    text: "<old-label>".into(),
                },
                DiffSpan {
                    tag: SpanTag::Inserted,
                    text: "<new-label>".into(),
                },
            ],
        }];
        let output = render(&results);
        assert!(output.contains("<new-label>"));
        assert!(!output.contains("#diff-added[<new-label>]"));
        assert!(!output.contains("#diff-deleted[\\<old-label>]"));
    }

    #[test]
    fn test_render_skipped_deleted_label_keeps_diff_call_guard() {
        let results = vec![DiffResult::Modified {
            kind: BlockKind::Paragraph,
            spans: vec![
                DiffSpan {
                    tag: SpanTag::Deleted,
                    text: "old".into(),
                },
                DiffSpan {
                    tag: SpanTag::Deleted,
                    text: "<old-label>".into(),
                },
                DiffSpan {
                    tag: SpanTag::Equal,
                    text: "(next)".into(),
                },
            ],
        }];
        let output = render(&results);
        assert!(output.contains("#diff-deleted[old]\u{200B}(next)"));
    }

    #[test]
    fn test_render_modified_citation_inserts_zws_separator() {
        let mut citation_keys = BTreeSet::new();
        citation_keys.insert("smirnov2015".to_string());
        let results = vec![DiffResult::Modified {
            kind: BlockKind::Paragraph,
            spans: vec![
                DiffSpan {
                    tag: SpanTag::Deleted,
                    text: "@smirnov2015".into(),
                },
                DiffSpan {
                    tag: SpanTag::Inserted,
                    text: "#citet[@smirnov2015]".into(),
                },
            ],
        }];
        let output = render_with_citations(&results, &citation_keys);
        assert!(output.contains("#diff-deleted[@smirnov2015]\u{200B}#diff-added[#citet[@smirnov2015]]"));
    }

    #[test]
    fn test_render_equation_label_attaches_inside_added_block() {
        let results = vec![
            DiffResult::Added(Block::Equation {
                content: "$ a = b, $".into(),
                block: true,
            }),
            DiffResult::Deleted(Block::Equation {
                content: "$ a = b. $".into(),
                block: true,
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<eq:test>".into(),
            }),
        ];
        let output = render(&results);
        assert!(output.contains("#diff-added[$ a = b, $ <eq:test>]"));
        assert!(output.contains("#diff-deleted[$ a = b. $]"));
        assert!(!output.contains("#diff-deleted[$ a = b. $]\n\n<eq:test>"));
    }
}
