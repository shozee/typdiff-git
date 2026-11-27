use similar::{Algorithm, ChangeTag, DiffOp, TextDiff};
use typst_syntax::SyntaxKind;

use super::{Block, DiffResult, DiffSpan, SpanTag, TypstLabel};

/// Diff two sequences of blocks, returning a list of diff results.
///
/// Uses Patience diff at the block level and word-level diff for modified blocks.
pub fn diff(old_blocks: &[Block], new_blocks: &[Block]) -> Vec<DiffResult> {
    let old_sigs: Vec<String> = old_blocks.iter().map(|b| b.signature()).collect();
    let new_sigs: Vec<String> = new_blocks.iter().map(|b| b.signature()).collect();

    let old_refs: Vec<&str> = old_sigs.iter().map(|s| s.as_str()).collect();
    let new_refs: Vec<&str> = new_sigs.iter().map(|s| s.as_str()).collect();

    let text_diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_slices(&old_refs, &new_refs);

    let mut results = Vec::new();

    for op in text_diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                for b in &old_blocks[old_index..(old_index + len)] {
                    results.push(DiffResult::Unchanged(b.clone()));
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for b in &old_blocks[old_index..(old_index + old_len)] {
                    results.push(DiffResult::Deleted(b.clone()));
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for b in &new_blocks[new_index..(new_index + new_len)] {
                    results.push(DiffResult::Added(b.clone()));
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                process_replace(
                    &old_blocks[old_index..(old_index + old_len)],
                    &new_blocks[new_index..(new_index + new_len)],
                    &mut results,
                );
            }
        }
    }

    results
}

/// Compute character-level similarity ratio between two strings (0.0–1.0).
pub(crate) fn text_similarity(a: &str, b: &str) -> f32 {
    TextDiff::from_chars(a, b).ratio()
}

/// Process a Replace operation using similarity-based greedy matching.
///
/// Instead of pairing blocks by position, we compute similarity scores for all
/// compatible pairs and greedily match the most similar ones first. This produces
/// better word-level diffs when blocks are inserted or removed within a Replace region.
fn process_replace(old_range: &[Block], new_range: &[Block], results: &mut Vec<DiffResult>) {
    // 1. Build candidate pairs with similarity scores.
    let mut candidates: Vec<(usize, usize, f32)> = Vec::new();
    for (oi, old_b) in old_range.iter().enumerate() {
        if old_b.is_atomic() {
            continue;
        }
        let old_text = match old_b.diff_text() {
            Some(t) => t,
            None => continue,
        };
        for (ni, new_b) in new_range.iter().enumerate() {
            if new_b.is_atomic() || old_b.kind() != new_b.kind() {
                continue;
            }
            let new_text = match new_b.diff_text() {
                Some(t) => t,
                None => continue,
            };
            let ratio = text_similarity(&old_text, &new_text);
            if ratio >= 0.5 {
                candidates.push((oi, ni, ratio));
            }
        }
    }

    // 2. Greedy match: sort by similarity descending, assign pairs.
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    // matched_old[oi] = Some(ni), matched_new[ni] = Some(oi)
    let mut matched_old: Vec<Option<usize>> = vec![None; old_range.len()];
    let mut matched_new: Vec<Option<usize>> = vec![None; new_range.len()];

    for (oi, ni, _ratio) in &candidates {
        if matched_old[*oi].is_none() && matched_new[*ni].is_none() {
            matched_old[*oi] = Some(*ni);
            matched_new[*ni] = Some(*oi);
        }
    }

    // 3. Emit results in document order by interleaving unmatched deleted blocks
    // at their original relative positions instead of dumping them all at the beginning.
    let mut emitted_old: Vec<bool> = vec![false; old_range.len()];
    let mut next_oi = 0;

    for (ni, new_b) in new_range.iter().enumerate() {
        if let Some(oi) = matched_new[ni] {
            // Emit any unmatched old blocks that precede this matched block in old_range.
            if oi >= next_oi {
                while next_oi < oi {
                    if matched_old[next_oi].is_none() && !emitted_old[next_oi] {
                        results.push(DiffResult::Deleted(old_range[next_oi].clone()));
                        emitted_old[next_oi] = true;
                    }
                    next_oi += 1;
                }
                next_oi = oi + 1;
            }

            let old_b = &old_range[oi];
            let old_text = old_b.diff_text().unwrap();
            let new_text = new_b.diff_text().unwrap();
            let spans = word_diff(&old_text, &new_text);
            results.push(DiffResult::Modified {
                kind: old_b.kind(),
                spans,
            });
            emitted_old[oi] = true;
        } else {
            results.push(DiffResult::Added(new_b.clone()));
        }
    }

    // Emit any remaining unmatched old blocks at the end.
    for (oi, old_b) in old_range.iter().enumerate() {
        if matched_old[oi].is_none() && !emitted_old[oi] {
            results.push(DiffResult::Deleted(old_b.clone()));
            emitted_old[oi] = true;
        }
    }
}

/// Tokenize text for diffing: ASCII alphanumeric runs form word tokens,
/// everything else (CJK characters, punctuation, whitespace) becomes an
/// individual single-character token.
///
/// Typst code expressions (`#func[...]`, `#func(...)`), references (`@label`),
/// labels (`<label>`), and inline equations (`$...$`) are treated as atomic
/// tokens so the diff never fragments valid Typst syntax.
///
/// This gives word-level granularity for Latin text while allowing
/// character-level precision for CJK text (which has no whitespace boundaries).
pub(crate) fn tokenize_mixed(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut atomic_ranges = typst_atomic_ranges(s).into_iter().peekable();

    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        let c_len = c.len_utf8();

        if let Some(&(start, end)) = atomic_ranges.peek() {
            if i == start {
                atomic_ranges.next();
                tokens.push(&s[start..end]);
                i = end;
                continue;
            }
        }

        if c == '\\' {
            // Escaped punctuation (e.g. \$, \[, \#, \\)
            let start = i;
            i += c_len;
            if i < s.len() {
                let next = s[i..].chars().next().unwrap();
                if !next.is_alphanumeric() {
                    i += next.len_utf8();
                }
            }
            tokens.push(&s[start..i]);
        } else if c == '#' {
            // Typst code expression: #identifier followed by [...] and/or (...)
            let start = i;
            i += 1; // skip '#'
            if i < s.len() && s[i..].chars().next().unwrap().is_ascii_alphabetic() {
                // Consume identifier (letters, digits, hyphens, underscores)
                while i < s.len() {
                    let next = s[i..].chars().next().unwrap();
                    if next.is_ascii_alphanumeric() || next == '-' || next == '_' {
                        i += next.len_utf8();
                    } else {
                        break;
                    }
                }
                // Consume trailing bracket [...] and paren (...) groups
                while i < s.len() {
                    let next = s[i..].chars().next().unwrap();
                    if next == '[' {
                        i = skip_balanced(s, i, '[', ']');
                    } else if next == '(' {
                        i = skip_balanced(s, i, '(', ')');
                    } else {
                        break;
                    }
                }
                tokens.push(&s[start..i]);
            } else {
                // Standalone '#'
                tokens.push(&s[start..i]);
            }
        } else if c == '@' {
            // Typst reference: @label (alphanumeric, hyphens, underscores, colons, periods)
            let start = i;
            i += 1; // skip '@'
            while i < s.len() {
                let next = s[i..].chars().next().unwrap();
                if next.is_ascii_alphanumeric() || matches!(next, '-' | '_' | ':' | '.') {
                    i += next.len_utf8();
                } else {
                    break;
                }
            }
            // Strip trailing punctuation that cannot be part of a reference identifier
            while i > start + 1 && (s.as_bytes()[i - 1] == b'.' || s.as_bytes()[i - 1] == b':') {
                i -= 1;
            }
            tokens.push(&s[start..i]);
        } else if c == '<' {
            let start = i;
            if let Some(end) = TypstLabel::end(&s[start..]) {
                i += end;
            } else {
                i += c_len;
            }
            tokens.push(&s[start..i]);
        } else if c.is_ascii_alphanumeric() {
            let start = i;
            i += c_len;
            while i < s.len() {
                let next = s[i..].chars().next().unwrap();
                if !next.is_ascii_alphanumeric() {
                    break;
                }
                i += next.len_utf8();
            }
            tokens.push(&s[start..i]);
        } else {
            tokens.push(&s[i..i + c_len]);
            i += c_len;
        }
    }

    tokens
}

/// Skip over a balanced delimiter group starting at position `i`.
/// Returns the position after the closing delimiter.
/// If unbalanced, returns the end of the string.
fn skip_balanced(s: &str, start: usize, open: char, close: char) -> usize {
    let mut i = start;
    let mut depth: i32 = 0;

    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        let c_len = c.len_utf8();
        if c == '\\' {
            // Skip escaped character
            i += c_len;
            if i < s.len() {
                let next = s[i..].chars().next().unwrap();
                i += next.len_utf8();
            }
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return i + c_len;
            }
        }
        i += c_len;
    }
    i
}

/// Find composite Typst markup expressions using Typst's parser.
///
/// Diffing must not split the source text of a Typst expression, since a
/// partial expression is not necessarily valid markup. Plain text and
/// whitespace retain word/character granularity for useful highlighting.
fn typst_atomic_ranges(s: &str) -> Vec<(usize, usize)> {
    let root = typst_syntax::parse(s);
    let mut ranges = Vec::new();
    let mut start = 0;
    let nodes: Vec<_> = root.children().cloned().collect();
    let mut index = 0;
    while index < nodes.len() {
        let node = &nodes[index];
        if node.kind() == SyntaxKind::Hash
            && index + 1 < nodes.len()
            && !matches!(
                nodes[index + 1].kind(),
                SyntaxKind::Text | SyntaxKind::Space | SyntaxKind::Linebreak | SyntaxKind::Parbreak
            )
        {
            let end = start + node.len() + nodes[index + 1].len();
            ranges.push((start, end));
            start += node.len() + nodes[index + 1].len();
            index += 2;
            continue;
        }
        if !matches!(
            node.kind(),
            SyntaxKind::Text | SyntaxKind::Space | SyntaxKind::Linebreak | SyntaxKind::Parbreak
        ) {
            ranges.push((start, start + node.len()));
        }
        start += node.len();
        index += 1;
    }
    ranges
}

/// Perform mixed-granularity diff and return coalesced, reordered spans.
///
/// Uses a custom tokenizer that splits CJK characters individually while
/// keeping ASCII words intact, then diffs at the token level.
pub(crate) fn word_diff(old_text: &str, new_text: &str) -> Vec<DiffSpan> {
    let old_tokens = tokenize_mixed(old_text);
    let new_tokens = tokenize_mixed(new_text);
    diff_token_slices(&old_tokens, &new_tokens)
}

/// Diff two pre-tokenized slice sequences and return coalesced, reordered spans.
pub(crate) fn diff_token_slices(old_tokens: &[&str], new_tokens: &[&str]) -> Vec<DiffSpan> {
    let diff = TextDiff::configure().diff_slices(old_tokens, new_tokens);

    let raw_spans: Vec<DiffSpan> = diff
        .iter_all_changes()
        .map(|change| DiffSpan {
            tag: match change.tag() {
                ChangeTag::Equal => SpanTag::Equal,
                ChangeTag::Delete => SpanTag::Deleted,
                ChangeTag::Insert => SpanTag::Inserted,
            },
            text: change.value().to_string(),
        })
        .collect();

    reorder_spans(absorb_short_equals(coalesce_spans(raw_spans)))
}

/// Merge adjacent spans that have the same tag.
fn coalesce_spans(spans: Vec<DiffSpan>) -> Vec<DiffSpan> {
    let mut result: Vec<DiffSpan> = Vec::new();
    for span in spans {
        if let Some(last) = result.last_mut() {
            if last.tag == span.tag {
                last.text.push_str(&span.text);
                continue;
            }
        }
        result.push(span);
    }
    result
}

/// Maximum byte-length of an Equal span that may be absorbed into a surrounding
/// change block by [`absorb_short_equals`].  Spans longer than this are treated
/// as genuine separators between independent edits.
///
/// 15 bytes covers common English glue tokens such as " the ", " of ", " a ",
/// " and ", " in " while leaving substantive phrases intact.
const MAX_ABSORBABLE_EQUAL_BYTES: usize = 15;

/// Absorb short Equal spans that are sandwiched between non-Equal spans.
///
/// When a brief unchanged token (e.g. " the ") sits between two edited regions,
/// it prevents [`reorder_spans`] from grouping those regions into a single
/// readable change block.  This function converts such spans into a
/// Deleted+Inserted pair carrying the same text, so that the subsequent
/// [`reorder_spans`] pass can merge everything into one block:
///
///   Before: Del "b " | Ins "B " | Equal "the " | Del "c" | Ins "C"
///   After:  Del "b " | Ins "B " | Del "the " | Ins "the " | Del "c" | Ins "C"
///   →reorder: Del "b the c" | Ins "B the C"
///
/// Only Equal spans that are both short enough (≤ [`MAX_ABSORBABLE_EQUAL_BYTES`])
/// **and** surrounded by non-Equal spans on both sides are absorbed.
/// Leading/trailing Equal spans are never touched.
fn absorb_short_equals(spans: Vec<DiffSpan>) -> Vec<DiffSpan> {
    let n = spans.len();
    let mut result = Vec::with_capacity(n + 4);

    for (i, span) in spans.iter().enumerate() {
        if span.tag == SpanTag::Equal
            && span.text.len() <= MAX_ABSORBABLE_EQUAL_BYTES
            && i > 0
            && i + 1 < n
            && spans[i - 1].tag != SpanTag::Equal
            && spans[i + 1].tag != SpanTag::Equal
        {
            // Absorb: emit as both Deleted and Inserted so reorder_spans can
            // merge this token with the adjacent change blocks.
            result.push(DiffSpan {
                tag: SpanTag::Deleted,
                text: span.text.clone(),
            });
            result.push(DiffSpan {
                tag: SpanTag::Inserted,
                text: span.text.clone(),
            });
        } else {
            result.push(span.clone());
        }
    }
    result
}

/// Within each contiguous block of changed spans (Deleted/Inserted, with no
/// Equal span between them), collect all Deleted text first and all Inserted
/// text second.  This prevents interleaved patterns such as
///
///   #diff-deleted[b ]#diff-added[B ]#diff-deleted[c]#diff-added[C]
///
/// and produces the more readable:
///
///   #diff-deleted[b c]#diff-added[B C]
///
/// Call [`absorb_short_equals`] before this function so that brief glue tokens
/// between adjacent edits do not fragment the change blocks.
fn reorder_spans(spans: Vec<DiffSpan>) -> Vec<DiffSpan> {
    let mut result: Vec<DiffSpan> = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        if spans[i].tag == SpanTag::Equal {
            result.push(spans[i].clone());
            i += 1;
        } else {
            // Collect a contiguous block of non-Equal (Deleted/Inserted) spans.
            let mut deleted = String::new();
            let mut inserted = String::new();
            while i < spans.len() && spans[i].tag != SpanTag::Equal {
                match spans[i].tag {
                    SpanTag::Deleted => deleted.push_str(&spans[i].text),
                    SpanTag::Inserted => inserted.push_str(&spans[i].text),
                    SpanTag::Equal => unreachable!(),
                }
                i += 1;
            }
            // Emit Deleted before Inserted so the output is never interleaved.
            if !deleted.is_empty() {
                result.push(DiffSpan {
                    tag: SpanTag::Deleted,
                    text: deleted,
                });
            }
            if !inserted.is_empty() {
                result.push(DiffSpan {
                    tag: SpanTag::Inserted,
                    text: inserted,
                });
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typdiff::BlockKind;

    #[test]
    fn test_identical_blocks() {
        let blocks = vec![Block::Paragraph {
            source_text: "Hello world".into(),
        }];
        let results = diff(&blocks, &blocks);
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], DiffResult::Unchanged(_)));
    }

    #[test]
    fn test_added_block() {
        let old = vec![];
        let new = vec![Block::Paragraph {
            source_text: "New text".into(),
        }];
        let results = diff(&old, &new);
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], DiffResult::Added(_)));
    }

    #[test]
    fn test_deleted_block() {
        let old = vec![Block::Paragraph {
            source_text: "Old text".into(),
        }];
        let new = vec![];
        let results = diff(&old, &new);
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], DiffResult::Deleted(_)));
    }

    #[test]
    fn test_modified_paragraph() {
        let old = vec![Block::Paragraph {
            source_text: "This is old text".into(),
        }];
        let new = vec![Block::Paragraph {
            source_text: "This is new text".into(),
        }];
        let results = diff(&old, &new);
        assert_eq!(results.len(), 1);
        if let DiffResult::Modified { kind, spans } = &results[0] {
            assert_eq!(*kind, BlockKind::Paragraph);
            assert!(spans
                .iter()
                .any(|s| s.tag == SpanTag::Deleted && s.text.contains("old")));
            assert!(spans
                .iter()
                .any(|s| s.tag == SpanTag::Inserted && s.text.contains("new")));
        } else {
            panic!("expected Modified, got {:?}", results[0]);
        }
    }

    #[test]
    fn test_atomic_blocks_not_modified() {
        let old = vec![Block::RawBlock {
            content: "```rust\nold code\n```".into(),
        }];
        let new = vec![Block::RawBlock {
            content: "```rust\nnew code\n```".into(),
        }];
        let results = diff(&old, &new);
        // Atomic blocks should stay as Delete + Insert, not Modified.
        assert!(results.iter().any(|r| matches!(r, DiffResult::Deleted(_))));
        assert!(results.iter().any(|r| matches!(r, DiffResult::Added(_))));
    }

    #[test]
    fn test_similarity_matching_with_insertion() {
        // Old: [A, B]  New: [A', X, B'] where A'/B' are minor edits, X is new.
        // Positional pairing would pair A-A', B-X (wrong). Similarity should pair A-A', B-B'.
        let old = vec![
            Block::Paragraph {
                source_text: "The first paragraph about networks.".into(),
            },
            Block::Paragraph {
                source_text: "The second paragraph about routing.".into(),
            },
        ];
        let new = vec![
            Block::Paragraph {
                source_text: "The first paragraph about network systems.".into(),
            },
            Block::Paragraph {
                source_text: "A completely new paragraph about security.".into(),
            },
            Block::Paragraph {
                source_text: "The second paragraph about routing protocols.".into(),
            },
        ];

        let mut results = Vec::new();
        process_replace(&old, &new, &mut results);

        // Should produce: Modified(A→A'), Added(X), Modified(B→B')
        // Not: Modified(A→A'), Modified(B→X), Added(B')
        let modified_count = results
            .iter()
            .filter(|r| matches!(r, DiffResult::Modified { .. }))
            .count();
        let added_count = results
            .iter()
            .filter(|r| matches!(r, DiffResult::Added(_)))
            .count();
        assert_eq!(
            modified_count, 2,
            "expected 2 Modified, got results: {results:?}"
        );
        assert_eq!(added_count, 1, "expected 1 Added, got results: {results:?}");

        // The Modified blocks should contain word-level changes, not whole-block replacements.
        if let DiffResult::Modified { spans, .. } = &results[0] {
            assert!(
                spans.iter().any(|s| s.tag == SpanTag::Equal),
                "first Modified should have Equal spans (word-level diff)"
            );
        }
    }

    #[test]
    fn test_similarity_matching_with_deletion() {
        // Old: [A, X, B]  New: [A', B'] — X was removed, A and B had minor edits.
        let old = vec![
            Block::Paragraph {
                source_text: "Introduction to the research topic.".into(),
            },
            Block::Paragraph {
                source_text: "This paragraph will be removed entirely.".into(),
            },
            Block::Paragraph {
                source_text: "Conclusion of the research topic.".into(),
            },
        ];
        let new = vec![
            Block::Paragraph {
                source_text: "Introduction to the research subject.".into(),
            },
            Block::Paragraph {
                source_text: "Conclusion of the research subject.".into(),
            },
        ];

        let mut results = Vec::new();
        process_replace(&old, &new, &mut results);

        let modified_count = results
            .iter()
            .filter(|r| matches!(r, DiffResult::Modified { .. }))
            .count();
        let deleted_count = results
            .iter()
            .filter(|r| matches!(r, DiffResult::Deleted(_)))
            .count();
        assert_eq!(
            modified_count, 2,
            "expected 2 Modified, got results: {results:?}"
        );
        assert_eq!(
            deleted_count, 1,
            "expected 1 Deleted, got results: {results:?}"
        );
    }

    #[test]
    fn test_similarity_low_ratio_no_match() {
        // Two completely different paragraphs should not be matched (ratio < 0.5).
        let old = vec![Block::Paragraph {
            source_text: "AAAA BBBB CCCC DDDD".into(),
        }];
        let new = vec![Block::Paragraph {
            source_text: "XXXX YYYY ZZZZ WWWW".into(),
        }];

        let mut results = Vec::new();
        process_replace(&old, &new, &mut results);

        // With no similarity, should fall back to Deleted + Added.
        assert!(
            results.iter().any(|r| matches!(r, DiffResult::Deleted(_))),
            "expected Deleted"
        );
        assert!(
            results.iter().any(|r| matches!(r, DiffResult::Added(_))),
            "expected Added"
        );
        assert!(
            !results
                .iter()
                .any(|r| matches!(r, DiffResult::Modified { .. })),
            "should not produce Modified for dissimilar blocks"
        );
    }

    #[test]
    fn test_similarity_different_kinds_not_paired() {
        // A Paragraph and a ListItem should not be paired even if text is similar.
        let old = vec![Block::Paragraph {
            source_text: "Some shared text here.".into(),
        }];
        let new = vec![Block::ListItem {
            body_text: "Some shared text here.".into(),
        }];

        let mut results = Vec::new();
        process_replace(&old, &new, &mut results);

        assert!(
            !results
                .iter()
                .any(|r| matches!(r, DiffResult::Modified { .. })),
            "different block kinds should not produce Modified"
        );
    }

    #[test]
    fn test_tokenize_typst_funccall_atomic() {
        let tokens = tokenize_mixed("before #footnote[https://example.com] after");
        // #footnote[...] should be a single token
        assert!(
            tokens.iter().any(|t| t.starts_with("#footnote[")),
            "expected atomic #footnote[...] token, got: {tokens:?}"
        );
    }

    #[test]
    fn test_tokenize_typst_ref_atomic() {
        let tokens = tokenize_mixed("see @my_ref_2024 for details");
        assert!(
            tokens.contains(&"@my_ref_2024"),
            "expected atomic @ref token, got: {tokens:?}"
        );
    }

    #[test]
    fn test_tokenize_typst_label_atomic() {
        let tokens = tokenize_mixed("<sample-widget_anchor>");
        assert_eq!(tokens, vec!["<sample-widget_anchor>"]);
    }

    #[test]
    fn test_tokenize_typst_markup_expressions_atomic() {
        for (source, expression) in [
            ("Use *strong text* here", "*strong text*"),
            ("Use _emphasized text_ here", "_emphasized text_"),
            ("See @sample-ref for details", "@sample-ref"),
            ("Use `raw text` here", "`raw text`"),
            ("Call #text(fill: red)[styled]", "#text(fill: red)[styled]"),
            ("Use $x + y$ in text", "$x + y$"),
        ] {
            let tokens = tokenize_mixed(source);
            assert!(
                tokens.contains(&expression),
                "composite expression should remain atomic, got: {source:?} -> {tokens:?}"
            );
        }
    }

    #[test]
    fn test_tokenize_typst_escaped_dollar_as_escape() {
        let tokens = tokenize_mixed(r"costs \$10 and \$20");
        assert!(
            tokens.contains(&r"\$"),
            "expected escaped dollar token, got: {tokens:?}"
        );
    }

    #[test]
    fn test_tokenize_cjk_char_level() {
        // CJK characters should be tokenized individually (no whitespace boundaries).
        let tokens = tokenize_mixed("吾輩は猫である");
        assert_eq!(
            tokens,
            vec!["吾", "輩", "は", "猫", "で", "あ", "る"],
            "CJK characters should be individual tokens"
        );
    }

    #[test]
    fn test_tokenize_cjk_mixed_with_ascii() {
        // Mixed CJK and ASCII: ASCII words stay grouped, CJK splits per character.
        let tokens = tokenize_mixed("HTTP通信の実装");
        assert_eq!(tokens, vec!["HTTP", "通", "信", "の", "実", "装"],);
    }

    #[test]
    fn test_inline_footnote_produces_modified() {
        // Paragraph where @ref changed to #footnote[...] should produce Modified, not Delete+Add.
        // The paragraph must be long enough for the similarity ratio to exceed 0.5.
        let old = vec![Block::Paragraph {
            source_text: "This is a fairly long paragraph that discusses various topics in some detail. The key finding was reported by Smith et al. in their landmark study @smith_2024. Further research is needed to confirm these results across different settings and conditions.".into(),
        }];
        let new = vec![Block::Paragraph {
            source_text: "This is a fairly long paragraph that discusses various topics in some detail. The key finding was reported by Smith et al. in their landmark study #footnote[https://example.com/papers/smith2024.pdf accessed: 2025/01/15]. Further research is needed to confirm these results across different settings and conditions.".into(),
        }];

        let results = diff(&old, &new);
        assert!(
            results
                .iter()
                .any(|r| matches!(r, DiffResult::Modified { .. })),
            "expected Modified for inline footnote change, got: {results:?}"
        );
    }

    #[test]
    fn test_cjk_word_diff_char_granularity() {
        // CJK text with a small change should produce character-level Modified, not whole-block Delete+Add.
        let old = vec![Block::Paragraph {
            source_text: "本手法では、入力データを前処理した後、特徴量を抽出する。抽出された特徴量に基づき分類を行う。".into(),
        }];
        let new = vec![Block::Paragraph {
            source_text: "本手法では、入力データを前処理した後、特徴ベクトルを抽出する。抽出された特徴ベクトルに基づき分類を行う。".into(),
        }];
        let results = diff(&old, &new);
        if let DiffResult::Modified { spans, .. } = &results[0] {
            assert!(
                spans.iter().any(|s| s.tag == SpanTag::Equal),
                "CJK diff should have Equal spans (character-level granularity), got: {spans:?}"
            );
            assert!(
                spans
                    .iter()
                    .any(|s| s.tag == SpanTag::Deleted && s.text.contains("量")),
                "should delete old CJK characters"
            );
            assert!(
                spans
                    .iter()
                    .any(|s| s.tag == SpanTag::Inserted && s.text.contains("ベクトル")),
                "should insert new CJK characters"
            );
        } else {
            panic!("expected Modified, got: {:?}", results[0]);
        }
    }

    #[test]
    fn test_reorder_spans_groups_consecutive_changes() {
        // "a b c" vs "a B C": both 'b' and 'c' are replaced.
        // After reordering the change block should be [Del("b c"), Ins("B C")],
        // NOT [Del("b "), Ins("B "), Del("c"), Ins("C")].
        let spans = word_diff("a b c", "a B C");
        // Within any contiguous non-Equal block, Deleted must precede Inserted.
        let mut seen_inserted = false;
        for span in &spans {
            match span.tag {
                SpanTag::Equal => {
                    seen_inserted = false;
                }
                SpanTag::Deleted => {
                    assert!(
                        !seen_inserted,
                        "Deleted span must not follow Inserted within a change block: {spans:?}"
                    );
                }
                SpanTag::Inserted => {
                    seen_inserted = true;
                }
            }
        }
        // Additionally, the Deleted span should contain both "b" and "c".
        let deleted_text: String = spans
            .iter()
            .filter(|s| s.tag == SpanTag::Deleted)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            deleted_text.contains('b') && deleted_text.contains('c'),
            "all deleted tokens should be grouped: {deleted_text:?}"
        );
    }

    #[test]
    fn test_reorder_spans_short_equal_absorbed() {
        // A short Equal span ("a") sandwiched between two change regions is
        // absorbed by absorb_short_equals, so both change regions are merged
        // into a single block.
        // "X b a c Y" vs "X B a C Y": 'b'→'B' and 'c'→'C' with short "a" in between.
        let spans = word_diff("X b a c Y", "X B a C Y");
        // With absorption, there should be exactly ONE change block between the
        // leading "X " and trailing " Y" Equal spans.
        let mut blocks = 0usize;
        let mut in_block = false;
        for span in &spans {
            if span.tag == SpanTag::Equal {
                in_block = false;
            } else if !in_block {
                blocks += 1;
                in_block = true;
            }
        }
        assert_eq!(blocks, 1, "short equal should be absorbed into one block: {spans:?}");
    }

    #[test]
    fn test_reorder_spans_long_equal_keeps_separate() {
        // A long Equal span (> MAX_ABSORBABLE_EQUAL_BYTES) must NOT be absorbed;
        // the two change regions stay independent.
        // Use a 20-character equal separator that clearly exceeds the threshold.
        let old = "changed_old one two three four five rest_old";
        let new = "changed_new one two three four five rest_new";
        let spans = word_diff(old, new);
        let mut blocks = 0usize;
        let mut in_block = false;
        for span in &spans {
            if span.tag == SpanTag::Equal {
                in_block = false;
            } else if !in_block {
                blocks += 1;
                in_block = true;
            }
        }
        assert_eq!(blocks, 2, "long equal should keep change regions separate: {spans:?}");
    }

    #[test]
    fn test_coalesce_spans() {
        let spans = vec![
            DiffSpan {
                tag: SpanTag::Equal,
                text: "a".into(),
            },
            DiffSpan {
                tag: SpanTag::Equal,
                text: "b".into(),
            },
            DiffSpan {
                tag: SpanTag::Deleted,
                text: "c".into(),
            },
        ];
        let result = coalesce_spans(spans);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text, "ab");
        assert_eq!(result[1].text, "c");
    }

    #[test]
    fn test_process_replace_interleaves_deleted_blocks() {
        let old_blocks = vec![
            Block::Paragraph {
                source_text: "First paragraph with shared words.".into(),
            },
            Block::Paragraph {
                source_text: "Second paragraph that gets modified slightly.".into(),
            },
            Block::Paragraph {
                source_text: "Third paragraph that was deleted.".into(),
            },
        ];
        let new_blocks = vec![
            Block::Paragraph {
                source_text: "First paragraph with shared words.".into(),
            },
            Block::Paragraph {
                source_text: "Second paragraph that gets modified greatly.".into(),
            },
        ];
        let results = diff(&old_blocks, &new_blocks);
        // Expect: Unchanged or Modified(First), Modified(Second), Deleted(Third)
        assert_eq!(results.len(), 3);
        assert!(matches!(
            results[0],
            DiffResult::Unchanged(_) | DiffResult::Modified { .. }
        ));
        assert!(matches!(results[1], DiffResult::Modified { .. }));
        assert!(matches!(results[2], DiffResult::Deleted(_)));
    }
}
