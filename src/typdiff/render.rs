use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use typst_syntax::{SyntaxKind, SyntaxNode};

use super::diff::{diff_token_slices, text_similarity, tokenize_mixed, word_diff};
use super::{Block, BlockKind, DiffResult, DiffSpan, SpanTag, TypstLabel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryChangeKind {
    Modified,
    Added,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryAnchor {
    pub label: String,
    pub kind: SummaryChangeKind,
    pub block_label: &'static str,
    pub figure_label: Option<String>,
    pub context_excerpt: Option<String>,
    pub old_excerpt: Option<String>,
    pub new_excerpt: Option<String>,
}

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
    render_impl(results, citation_keys, None).0
}

/// Render a list of diff results and emit `#metadata(none) <label>` anchors
/// at each change site for the summary cover page.
pub fn render_with_citations_and_summary(
    results: &[DiffResult],
    citation_keys: &BTreeSet<String>,
    label_prefix: &str,
) -> (String, Vec<SummaryAnchor>) {
    render_impl(results, citation_keys, Some(label_prefix))
}

struct SummaryState<'a> {
    prefix: &'a str,
    anchors: Vec<SummaryAnchor>,
    pending_after_label: Vec<usize>,
    pending_from_added: bool,
}

impl<'a> SummaryState<'a> {
    fn new(prefix: &'a str) -> Self {
        Self {
            prefix,
            anchors: Vec::new(),
            pending_after_label: Vec::new(),
            pending_from_added: false,
        }
    }

    fn alloc_anchor(
        &mut self,
        kind: SummaryChangeKind,
        block_label: &'static str,
        figure_label: Option<String>,
        context_excerpt: Option<String>,
        old_excerpt: Option<String>,
        new_excerpt: Option<String>,
    ) -> (usize, String) {
        let idx = self.anchors.len();
        let label = format!("{}{}", self.prefix, idx);
        self.anchors.push(SummaryAnchor {
            label: label.clone(),
            kind,
            block_label,
            figure_label,
            context_excerpt,
            old_excerpt,
            new_excerpt,
        });
        (idx, label)
    }

    fn flush_pending(&mut self, out: &mut String) {
        self.pending_from_added = false;
        if self.pending_after_label.is_empty() {
            return;
        }
        for idx in self.pending_after_label.drain(..) {
            let label = &self.anchors[idx].label;
            let _ = writeln!(out, "#metadata(none) <{label}>");
        }
        out.push('\n');
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PairCategory {
    Equation,
    RawBlock,
    FuncCall(&'static str, String),
}

struct PendingPairSummary {
    block_label: &'static str,
    figure_label: Option<String>,
    context_excerpt: Option<String>,
    old_excerpt: Option<String>,
    new_excerpt: Option<String>,
}

struct AtomicPairings {
    /// First index of each paired (Added, Deleted) pair — emits no summary anchor.
    skip_indices: BTreeSet<usize>,
    /// Second index of each paired (Added, Deleted) pair — emits the Modified anchor.
    paired_anchors: HashMap<usize, PendingPairSummary>,
}

fn funccall_callee(content: &str) -> String {
    let trimmed = content.trim_start().strip_prefix('#').unwrap_or(content.trim_start());
    trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect()
}

fn extract_figure_or_table_label(block: Option<&DiffResult>) -> Option<String> {
    let text = match block {
        Some(DiffResult::Unchanged(Block::Paragraph { source_text }))
        | Some(DiffResult::Added(Block::Paragraph { source_text })) => source_text.trim(),
        _ => return None,
    };
    if !is_label_only(text) {
        return None;
    }
    let id = text.strip_prefix('<')?.strip_suffix('>')?.trim();
    if id.starts_with("fig:") || id.starts_with("tab:") {
        Some(id.to_string())
    } else {
        None
    }
}

fn classify_pairable(block: &Block) -> Option<(PairCategory, &str)> {
    match block {
        Block::Equation { content, .. } => Some((PairCategory::Equation, content.as_str())),
        Block::RawBlock { content } => Some((PairCategory::RawBlock, content.as_str())),
        Block::FuncCall { content } => {
            if is_boilerplate_funccall(content) || is_added_figure_wrapper(content) {
                return None;
            }
            let kind = funccall_kind(content);
            let callee = if matches!(kind, "Figure" | "Table") {
                String::new()
            } else {
                funccall_callee(content)
            };
            Some((PairCategory::FuncCall(kind, callee), content.as_str()))
        }
        _ => None,
    }
}

fn compute_atomic_pairings(results: &[DiffResult]) -> AtomicPairings {
    let mut skip_indices = BTreeSet::new();
    let mut paired_anchors = HashMap::new();

    let mut run_start = 0usize;
    while run_start < results.len() {
        if !matches!(
            results[run_start],
            DiffResult::Added(_) | DiffResult::Deleted(_)
        ) {
            run_start += 1;
            continue;
        }
        let mut run_end = run_start;
        while run_end < results.len()
            && matches!(
                results[run_end],
                DiffResult::Added(_) | DiffResult::Deleted(_)
            )
        {
            run_end += 1;
        }

        // Group pairable Added and Deleted items by compatible category within this hunk.
        type PairBuckets<'a> = (Vec<(usize, &'a str)>, Vec<(usize, &'a str)>);
        let mut by_category: HashMap<PairCategory, PairBuckets<'_>> = HashMap::new();
        for (idx, item) in results[run_start..run_end].iter().enumerate() {
            let abs_idx = run_start + idx;
            match item {
                DiffResult::Added(block) => {
                    if let Some((cat, text)) = classify_pairable(block) {
                        by_category.entry(cat).or_default().0.push((abs_idx, text));
                    }
                }
                DiffResult::Deleted(block) => {
                    if let Some((cat, text)) = classify_pairable(block) {
                        by_category.entry(cat).or_default().1.push((abs_idx, text));
                    }
                }
                _ => {}
            }
        }

        for (cat, (added, deleted)) in by_category {
            if added.is_empty() || deleted.is_empty() {
                continue;
            }
            let mut pairs: Vec<(usize, usize)> = Vec::new();
            if added.len() == 1 && deleted.len() == 1 {
                pairs.push((added[0].0, deleted[0].0));
            } else {
                let mut candidates: Vec<(usize, usize, f32)> = Vec::new();
                for (ai, &(_add_idx, add_text)) in added.iter().enumerate() {
                    for (di, &(_del_idx, del_text)) in deleted.iter().enumerate() {
                        let sim = text_similarity(add_text, del_text);
                        candidates.push((ai, di, sim));
                    }
                }
                candidates.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.cmp(&b.0))
                        .then_with(|| a.1.cmp(&b.1))
                });
                let mut used_a = vec![false; added.len()];
                let mut used_d = vec![false; deleted.len()];
                for (ai, di, _sim) in candidates {
                    if !used_a[ai] && !used_d[di] {
                        used_a[ai] = true;
                        used_d[di] = true;
                        pairs.push((added[ai].0, deleted[di].0));
                    }
                }
            }

            for (add_idx, del_idx) in pairs {
                let first_idx = add_idx.min(del_idx);
                let second_idx = add_idx.max(del_idx);
                let old_text = match &results[del_idx] {
                    DiffResult::Deleted(b) => classify_pairable(b).map(|(_, s)| s).unwrap_or(""),
                    _ => "",
                };
                let new_text = match &results[add_idx] {
                    DiffResult::Added(b) => classify_pairable(b).map(|(_, s)| s).unwrap_or(""),
                    _ => "",
                };
                let (block_label, context_excerpt, old_excerpt, new_excerpt) = match cat {
                    PairCategory::Equation => {
                        let (ctx, o, n) = summarize_equation_pair(old_text, new_text);
                        ("Equation", ctx, o, n)
                    }
                    PairCategory::RawBlock => {
                        let (ctx, o, n) = summarize_raw_pair(old_text, new_text);
                        ("Code", ctx, o, n)
                    }
                    PairCategory::FuncCall(_, _) => {
                        let (lbl, ctx, o, n) = summarize_funccall_pair(old_text, new_text);
                        (lbl, ctx, o, n)
                    }
                };
                let figure_label = extract_figure_or_table_label(results.get(add_idx + 1))
                    .or_else(|| extract_figure_or_table_label(results.get(second_idx + 1)));
                skip_indices.insert(first_idx);
                paired_anchors.insert(
                    second_idx,
                    PendingPairSummary {
                        block_label,
                        figure_label,
                        context_excerpt,
                        old_excerpt,
                        new_excerpt,
                    },
                );
            }
        }

        run_start = run_end;
    }

    AtomicPairings {
        skip_indices,
        paired_anchors,
    }
}

fn render_impl(
    results: &[DiffResult],
    citation_keys: &BTreeSet<String>,
    summary_prefix: Option<&str>,
) -> (String, Vec<SummaryAnchor>) {
    let mut out = String::new();
    out.push_str(PREAMBLE);
    out.push('\n');

    let mut summary = summary_prefix.map(SummaryState::new);
    let mut pairings = summary_prefix.map(|_| compute_atomic_pairings(results));

    // Byte index of the closing `]` of the most recent `#diff-added[...]`
    // wrapping an equation or raw block, preserved across interleaved
    // `#diff-deleted[...]` blocks so a following label attaches to the
    // inner element rather than the `#diff-added`/`#diff-deleted` wrapper.
    let mut last_added_wrapped_close: Option<usize> = None;

    let mut idx = 0usize;
    while idx < results.len() {
        let result = &results[idx];

        match result {
            DiffResult::Unchanged(block) => {
                if let Block::Paragraph { source_text } = block {
                    if is_label_only(source_text) {
                        let trimmed = source_text.trim();
                        if let Some(close_idx) = last_added_wrapped_close {
                            let label_insert = format!(" {trimmed}");
                            out.insert_str(close_idx, &label_insert);
                            last_added_wrapped_close = Some(close_idx + label_insert.len());
                            if let Some(state) = summary.as_mut() {
                                record_following_label(state, trimmed);
                            }
                            idx += 1;
                            continue;
                        }
                        if let Some(state) = summary.as_mut() {
                            if !state.pending_after_label.is_empty() {
                                render_block(block, &mut out);
                                out.push('\n');
                                record_following_label(state, trimmed);
                                state.flush_pending(&mut out);
                                idx += 1;
                                continue;
                            }
                        }
                    }
                }
                if let Some(state) = summary.as_mut() {
                    state.flush_pending(&mut out);
                }
                last_added_wrapped_close = None;
                render_block(block, &mut out);
                out.push('\n');
            }
            DiffResult::Added(block) => {
                if let Block::Paragraph { source_text } = block {
                    if is_label_only(source_text) {
                        let trimmed = source_text.trim();
                        if let Some(close_idx) = last_added_wrapped_close {
                            let label_insert = format!(" {trimmed}");
                            out.insert_str(close_idx, &label_insert);
                            last_added_wrapped_close = Some(close_idx + label_insert.len());
                            if let Some(state) = summary.as_mut() {
                                record_following_label(state, trimmed);
                            }
                            idx += 1;
                            continue;
                        }
                        if let Some(state) = summary.as_mut() {
                            if !state.pending_after_label.is_empty() {
                                render_added_or_deleted(
                                    block,
                                    DiffTag::Added,
                                    citation_keys,
                                    &mut out,
                                    None,
                                );
                                out.push('\n');
                                record_following_label(state, trimmed);
                                state.flush_pending(&mut out);
                                idx += 1;
                                continue;
                            }
                        }
                    }
                }
                if let Some(state) = summary.as_mut() {
                    state.flush_pending(&mut out);
                }
                let mut inline_anchor: Option<String> = None;
                let mut after_anchor_idx: Option<usize> = None;
                if let Some(state) = summary.as_mut() {
                    let is_skipped = pairings
                        .as_ref()
                        .is_some_and(|p| p.skip_indices.contains(&idx));
                    if !is_skipped {
                        if let Some(pair) = pairings
                            .as_mut()
                            .and_then(|p| p.paired_anchors.remove(&idx))
                        {
                            let (anchor_idx, _) = state.alloc_anchor(
                                SummaryChangeKind::Modified,
                                pair.block_label,
                                pair.figure_label,
                                pair.context_excerpt,
                                pair.old_excerpt,
                                pair.new_excerpt,
                            );
                            after_anchor_idx = Some(anchor_idx);
                        } else if let Some((block_label, excerpt, defer_after_label)) =
                            summarize_single_block(block, DiffTag::Added)
                        {
                            let (anchor_idx, label) = state.alloc_anchor(
                                SummaryChangeKind::Added,
                                block_label,
                                None,
                                None,
                                None,
                                Some(excerpt),
                            );
                            if defer_after_label {
                                after_anchor_idx = Some(anchor_idx);
                            } else {
                                inline_anchor = Some(label);
                            }
                        }
                    }
                }
                render_added_or_deleted(
                    block,
                    DiffTag::Added,
                    citation_keys,
                    &mut out,
                    inline_anchor.as_deref(),
                );
                if matches!(block, Block::Equation { .. } | Block::RawBlock { .. }) {
                    last_added_wrapped_close = out.len().checked_sub(1);
                } else {
                    last_added_wrapped_close = None;
                }
                if let (Some(state), Some(anchor_idx)) = (summary.as_mut(), after_anchor_idx) {
                    state.pending_after_label.push(anchor_idx);
                    state.pending_from_added = true;
                }
                out.push('\n');
            }
            DiffResult::Deleted(block) => {
                let is_del_label = matches!(
                    block,
                    Block::Paragraph { source_text } if is_label_only(source_text)
                );
                let keep_pending = is_del_label
                    || summary.as_ref().is_some_and(|s| s.pending_from_added);
                if !keep_pending {
                    if let Some(state) = summary.as_mut() {
                        state.flush_pending(&mut out);
                    }
                }
                let mut inline_anchor: Option<String> = None;
                let mut after_anchor_idx: Option<usize> = None;
                if let Some(state) = summary.as_mut() {
                    let is_skipped = pairings
                        .as_ref()
                        .is_some_and(|p| p.skip_indices.contains(&idx));
                    if !is_skipped {
                        if let Some(pair) = pairings
                            .as_mut()
                            .and_then(|p| p.paired_anchors.remove(&idx))
                        {
                            let (anchor_idx, _) = state.alloc_anchor(
                                SummaryChangeKind::Modified,
                                pair.block_label,
                                pair.figure_label,
                                pair.context_excerpt,
                                pair.old_excerpt,
                                pair.new_excerpt,
                            );
                            after_anchor_idx = Some(anchor_idx);
                        } else if let Some((block_label, excerpt, defer_after_label)) =
                            summarize_single_block(block, DiffTag::Deleted)
                        {
                            let (anchor_idx, label) = state.alloc_anchor(
                                SummaryChangeKind::Deleted,
                                block_label,
                                None,
                                None,
                                Some(excerpt),
                                None,
                            );
                            if defer_after_label {
                                after_anchor_idx = Some(anchor_idx);
                            } else {
                                inline_anchor = Some(label);
                            }
                        }
                    }
                }
                render_added_or_deleted(
                    block,
                    DiffTag::Deleted,
                    citation_keys,
                    &mut out,
                    inline_anchor.as_deref(),
                );
                if let (Some(state), Some(anchor_idx)) = (summary.as_mut(), after_anchor_idx) {
                    state.pending_after_label.push(anchor_idx);
                    state.pending_from_added = false;
                }
                out.push('\n');
            }
            DiffResult::Modified { kind, spans } => {
                if let Some(state) = summary.as_mut() {
                    state.flush_pending(&mut out);
                }
                last_added_wrapped_close = None;
                let mut inline_anchor: Option<String> = None;
                let mut after_anchor_idx: Option<usize> = None;
                if let Some(state) = summary.as_mut() {
                    if let Some((context_ex, old_ex, new_ex)) = summarize_modified_spans(spans) {
                        let block_label = match kind {
                            BlockKind::Heading { .. } => "Heading",
                            BlockKind::ListItem | BlockKind::EnumItem { .. } => "List",
                            BlockKind::TermItem => "Term",
                            _ => "Text",
                        };
                        let (anchor_idx, label) = state.alloc_anchor(
                            SummaryChangeKind::Modified,
                            block_label,
                            None,
                            context_ex,
                            old_ex,
                            new_ex,
                        );
                        if matches!(kind, BlockKind::Heading { .. }) {
                            after_anchor_idx = Some(anchor_idx);
                        } else {
                            inline_anchor = Some(label);
                        }
                    }
                }
                render_modified(
                    kind,
                    spans,
                    citation_keys,
                    &mut out,
                    inline_anchor.as_deref(),
                );
                if let (Some(state), Some(anchor_idx)) = (summary.as_mut(), after_anchor_idx) {
                    state.pending_after_label.push(anchor_idx);
                    state.pending_from_added = false;
                }
                out.push('\n');
            }
        }
        // Always add a blank line between blocks (paragraph break).
        // Parbreaks are filtered out before diffing, so we insert them
        // unconditionally here instead.
        out.push('\n');
        idx += 1;
    }

    if let Some(state) = summary.as_mut() {
        state.flush_pending(&mut out);
    }

    (out, summary.map(|s| s.anchors).unwrap_or_default())
}

fn record_following_label(state: &mut SummaryState<'_>, trimmed_label: &str) {
    let label_id = trimmed_label
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .map(str::trim)
        .unwrap_or("");
    if label_id.starts_with("fig:") || label_id.starts_with("tab:") {
        if let Some(&idx) = state.pending_after_label.last() {
            if state.anchors[idx].figure_label.is_none() {
                state.anchors[idx].figure_label = Some(label_id.to_string());
            }
        }
    }
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
    inline_anchor: Option<&str>,
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
            if let Some(label) = inline_anchor {
                write!(out, "#metadata(none) <{label}>").unwrap();
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
            if let Some(label) = inline_anchor {
                write!(out, "#metadata(none) <{label}>").unwrap();
            }
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
            if let Some(label) = inline_anchor {
                write!(out, "#metadata(none) <{label}>").unwrap();
            }
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
            if let Some(label) = inline_anchor {
                write!(out, "#metadata(none) <{label}>").unwrap();
            }
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
    inline_anchor: Option<&str>,
) {
    match kind {
        BlockKind::Heading { depth } => write_heading_prefix(*depth, out),
        BlockKind::ListItem => out.push_str("- "),
        BlockKind::EnumItem { number } => write_enum_prefix(*number, out),
        BlockKind::TermItem => out.push_str("/ "),
        BlockKind::Paragraph => {}
        BlockKind::Atomic | BlockKind::Parbreak => {}
    }
    if matches!(kind, BlockKind::TermItem) {
        render_spans(spans, citation_keys, out, None);
        if let Some(label) = inline_anchor {
            write!(out, "#metadata(none) <{label}>").unwrap();
        }
    } else {
        render_spans(spans, citation_keys, out, inline_anchor);
    }
}

/// Render diff spans with markup for deleted/inserted text.
fn render_spans(
    spans: &[DiffSpan],
    citation_keys: &BTreeSet<String>,
    out: &mut String,
    mut pending_inline_anchor: Option<&str>,
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
                if let Some(label) = pending_inline_anchor.take() {
                    write!(out, "#metadata(none) <{label}>").unwrap();
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
                if let Some(label) = pending_inline_anchor.take() {
                    write!(out, "#metadata(none) <{label}>").unwrap();
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

const MAX_EXCERPT_CHARS: usize = 90;

fn normalize_excerpt(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = collapsed.chars().count();
    if total <= max_chars {
        return collapsed;
    }
    let mut byte_end = collapsed
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(collapsed.len());
    if let Some(last_space) = collapsed[..byte_end].rfind(' ') {
        if last_space > byte_end / 2 {
            byte_end = last_space;
        }
    }
    format!("{}…", collapsed[..byte_end].trim_end())
}

fn trailing_context_excerpt(text: &str, max_chars: usize) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= max_chars {
        return Some(collapsed);
    }
    let start_idx = chars.len() - max_chars;
    let slice: String = chars[start_idx..].iter().collect();
    let trimmed = slice
        .find(' ')
        .filter(|&pos| pos < slice.len() / 2)
        .map(|pos| slice[pos + 1..].trim_start())
        .unwrap_or(slice.as_str());
    Some(format!("… {trimmed}"))
}

fn is_boilerplate_funccall(content: &str) -> bool {
    let trimmed = content.trim_start();
    for kw in [
        "#import",
        "#set",
        "#show",
        "#let",
        "#pagebreak",
        "#counter",
        "#bibliography",
        "#include",
    ] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            if rest.is_empty()
                || rest
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            {
                return true;
            }
        }
    }
    false
}

fn is_added_figure_wrapper(content: &str) -> bool {
    content.trim_start().starts_with("#block(") && content.contains("[Added figure]")
}

fn funccall_kind(content: &str) -> &'static str {
    let trimmed = content.trim_start();
    if trimmed.contains("table(") || trimmed.starts_with("#table") {
        "Table"
    } else if trimmed.starts_with("#figure") {
        "Figure"
    } else {
        "Block"
    }
}

fn funccall_kind_and_text(content: &str) -> (&'static str, String) {
    let trimmed = content.trim_start();
    let label = funccall_kind(trimmed);
    let (short_cap, long_cap) = extract_funccall_captions(trimmed);
    if let Some(cap) = short_cap.or(long_cap) {
        return (label, normalize_excerpt(&cap, MAX_EXCERPT_CHARS));
    }
    (label, normalize_excerpt(trimmed, MAX_EXCERPT_CHARS))
}

const ADDED_CAPTION_PREFIX: &str = "#text(fill: rgb(\"0067c0\"))[#underline[";

fn strip_added_caption_wrapper(text: &str) -> String {
    let mut cur = text.trim();
    if let Some(inner) = cur.strip_prefix(ADDED_CAPTION_PREFIX) {
        if let Some(stripped) = inner.strip_suffix("]]") {
            cur = stripped.trim();
        }
    }
    cur.to_string()
}

fn strip_added_caption_wrappers_in_source(source: &str) -> String {
    let mut out = source.to_string();
    while let Some(pos) = out.find(ADDED_CAPTION_PREFIX) {
        let inner_start = pos + ADDED_CAPTION_PREFIX.len();
        let underline_start = pos + "#text(fill: rgb(\"0067c0\"))[".len();
        let root = typst_syntax::parse(&out[underline_start..]);
        let func_len = root
            .children()
            .nth(1)
            .filter(|n| n.kind() == SyntaxKind::FuncCall)
            .map(|n| n.len());
        let Some(func_len) = func_len else {
            break;
        };
        // child 0 is `#` (1 byte), so FuncCall spans [underline_start + 1 .. underline_start + 1 + func_len).
        let underline_close = underline_start + func_len;
        if underline_close >= inner_start && out[underline_close + 1..].starts_with(']') {
            let inner = out[inner_start..underline_close].to_string();
            out.replace_range(pos..=underline_close + 1, &inner);
        } else {
            break;
        }
    }
    out
}

fn extract_node_caption_text(node: &SyntaxNode, source: &str, start: usize) -> Option<String> {
    match node.kind() {
        SyntaxKind::ContentBlock => {
            let children: Vec<_> = node.children().collect();
            let has_left = children.first().is_some_and(|c| c.kind() == SyntaxKind::LeftBracket);
            let has_right = children.last().is_some_and(|c| c.kind() == SyntaxKind::RightBracket);
            if has_left && has_right && node.len() >= 2 {
                let inner = source[start + 1..start + node.len() - 1].trim();
                let cleaned = strip_added_caption_wrapper(inner);
                (!cleaned.is_empty()).then_some(cleaned)
            } else {
                None
            }
        }
        SyntaxKind::Str => {
            let raw = &source[start..start + node.len()];
            let inner = raw.strip_prefix('"')?.strip_suffix('"')?.trim();
            (!inner.is_empty()).then(|| inner.to_string())
        }
        _ => None,
    }
}

fn extract_funccall_captions(content: &str) -> (Option<String>, Option<String>) {
    let root = typst_syntax::parse(content);
    let mut offset = 0usize;
    let mut call_info: Option<(&SyntaxNode, usize)> = None;
    for child in root.children() {
        if child.kind() == SyntaxKind::FuncCall {
            call_info = Some((child, offset));
            break;
        }
        offset += child.len();
    }
    let Some((func_node, func_offset)) = call_info else {
        return (None, None);
    };

    let mut arg_offset = func_offset;
    for child in func_node.children() {
        if child.kind() == SyntaxKind::Args {
            let mut named_offset = arg_offset;
            for arg in child.children() {
                if arg.kind() == SyntaxKind::Named {
                    let mut is_caption = false;
                    let mut val_node: Option<(&SyntaxNode, usize)> = None;
                    let mut n_off = named_offset;
                    for nc in arg.children() {
                        match nc.kind() {
                            SyntaxKind::Ident if !is_caption => {
                                is_caption = nc.text().as_str() == "caption";
                            }
                            SyntaxKind::Space
                            | SyntaxKind::Linebreak
                            | SyntaxKind::Parbreak
                            | SyntaxKind::LineComment
                            | SyntaxKind::BlockComment
                            | SyntaxKind::Colon => {}
                            _ if is_caption => {
                                val_node = Some((nc, n_off));
                            }
                            _ => {}
                        }
                        n_off += nc.len();
                    }
                    if let Some((mut val, mut val_off)) = val_node {
                        while val.kind() == SyntaxKind::Parenthesized {
                            let mut inner_off = val_off;
                            let mut next_inner = None;
                            for pc in val.children() {
                                if !matches!(
                                    pc.kind(),
                                    SyntaxKind::LeftParen
                                        | SyntaxKind::RightParen
                                        | SyntaxKind::Space
                                        | SyntaxKind::Linebreak
                                        | SyntaxKind::LineComment
                                        | SyntaxKind::BlockComment
                                ) {
                                    next_inner = Some((pc, inner_off));
                                    break;
                                }
                                inner_off += pc.len();
                            }
                            if let Some(n) = next_inner {
                                val = n.0;
                                val_off = n.1;
                            } else {
                                break;
                            }
                        }
                        if val.kind() == SyntaxKind::FuncCall {
                            let is_short_cap = val
                                .children()
                                .next()
                                .is_some_and(|c| c.kind() == SyntaxKind::Ident && c.text().as_str() == "short-cap");
                            if is_short_cap {
                                let mut caps: Vec<Option<String>> = Vec::new();
                                let mut sc_off = val_off;
                                for sc in val.children() {
                                    if sc.kind() == SyntaxKind::Args {
                                        let mut a_off = sc_off;
                                        for ac in sc.children() {
                                            if matches!(ac.kind(), SyntaxKind::ContentBlock | SyntaxKind::Str) {
                                                caps.push(extract_node_caption_text(ac, content, a_off));
                                            }
                                            a_off += ac.len();
                                        }
                                    }
                                    sc_off += sc.len();
                                }
                                let short = caps.first().cloned().flatten();
                                let long = caps.get(1).cloned().flatten();
                                return (short, long);
                            }
                        }
                        if let Some(cap) = extract_node_caption_text(val, content, val_off) {
                            return (Some(cap), None);
                        }
                    }
                }
                named_offset += arg.len();
            }
        }
        arg_offset += child.len();
    }
    (None, None)
}

fn is_figure_extension_literal(raw_str_node: &str) -> bool {
    let Some(inner) = raw_str_node.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return false;
    };
    let without_query = inner.split(['?', '#']).next().unwrap_or(inner);
    let Some((_, ext)) = without_query.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "svg" | "pdf"
    )
}

fn mask_figure_asset_paths(source: &str) -> String {
    fn collect(
        node: &SyntaxNode,
        source: &str,
        offset: usize,
        ranges: &mut Vec<(usize, usize)>,
    ) {
        if node.kind() == SyntaxKind::Str {
            let text = &source[offset..offset + node.len()];
            if is_figure_extension_literal(text) {
                ranges.push((offset, offset + node.len()));
            }
            return;
        }
        let mut child_offset = offset;
        for child in node.children() {
            collect(child, source, child_offset, ranges);
            child_offset += child.len();
        }
    }

    let cleaned = strip_added_caption_wrappers_in_source(source);
    let root = typst_syntax::parse(&cleaned);
    let mut ranges = Vec::new();
    collect(&root, &cleaned, 0, &mut ranges);
    let mut out = cleaned;
    for (start, end) in ranges.into_iter().rev() {
        out.replace_range(start..end, "\"<asset>\"");
    }
    out
}

fn is_atomic_summary_call(node: &SyntaxNode) -> bool {
    fn check(n: &SyntaxNode, is_root: bool) -> bool {
        if matches!(
            n.kind(),
            SyntaxKind::CodeBlock | SyntaxKind::Linebreak | SyntaxKind::Parbreak
        ) {
            return false;
        }
        if !is_root && n.kind() == SyntaxKind::FuncCall {
            return false;
        }
        if n.kind() == SyntaxKind::ContentBlock && n.len() > 45 {
            return false;
        }
        if n.kind() == SyntaxKind::Space && n.text().contains('\n') {
            return false;
        }
        n.children().all(|c| check(c, false))
    }

    let callee = node
        .children()
        .next()
        .filter(|c| c.kind() == SyntaxKind::Ident)
        .map(|c| c.text().as_str())
        .unwrap_or("");
    if matches!(
        callee,
        "figure" | "table" | "grid" | "block" | "box" | "align" | "pad" | "stack"
    ) {
        return false;
    }

    node.len() <= 60 && check(node, true)
}

fn tokenize_typst_for_summary(source: &str) -> Vec<&str> {
    fn walk<'a>(
        node: &SyntaxNode,
        source: &'a str,
        offset: usize,
        in_markup: bool,
        tokens: &mut Vec<&'a str>,
    ) {
        let children: Vec<_> = node.children().collect();
        if children.is_empty() {
            let slice = &source[offset..offset + node.len()];
            match node.kind() {
                SyntaxKind::Space | SyntaxKind::Linebreak | SyntaxKind::Parbreak => {
                    tokens.push(" ");
                }
                SyntaxKind::Text => tokens.extend(tokenize_mixed(slice)),
                _ if !slice.is_empty() => tokens.push(slice),
                _ => {}
            }
            return;
        }

        let mut child_offset = offset;
        let mut i = 0usize;
        while i < children.len() {
            let child = children[i];
            if in_markup && child.kind() == SyntaxKind::Hash && i + 1 < children.len() {
                let next = children[i + 1];
                let atomic_next = match next.kind() {
                    SyntaxKind::FuncCall => is_atomic_summary_call(next),
                    SyntaxKind::Ident | SyntaxKind::FieldAccess => true,
                    _ => false,
                };
                if atomic_next {
                    let end = child_offset + child.len() + next.len();
                    tokens.push(&source[child_offset..end]);
                    child_offset = end;
                    i += 2;
                    continue;
                }
            }

            let end = child_offset + child.len();
            match child.kind() {
                SyntaxKind::Space | SyntaxKind::Linebreak | SyntaxKind::Parbreak => {
                    tokens.push(" ");
                }
                SyntaxKind::Text => {
                    tokens.extend(tokenize_mixed(&source[child_offset..end]));
                }
                SyntaxKind::Raw
                | SyntaxKind::Math
                | SyntaxKind::Escape
                | SyntaxKind::Link
                | SyntaxKind::Label
                | SyntaxKind::Ref
                | SyntaxKind::RefMarker
                | SyntaxKind::Str
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment => {
                    if !source[child_offset..end].is_empty() {
                        tokens.push(&source[child_offset..end]);
                    }
                }
                SyntaxKind::FuncCall if !in_markup && is_atomic_summary_call(child) => {
                    tokens.push(&source[child_offset..end]);
                }
                _ => {
                    let child_markup = matches!(
                        child.kind(),
                        SyntaxKind::Markup
                            | SyntaxKind::ContentBlock
                            | SyntaxKind::Strong
                            | SyntaxKind::Emph
                            | SyntaxKind::Heading
                            | SyntaxKind::ListItem
                            | SyntaxKind::EnumItem
                            | SyntaxKind::TermItem
                    );
                    walk(child, source, child_offset, child_markup, tokens);
                }
            }
            child_offset = end;
            i += 1;
        }
    }

    let root = typst_syntax::parse(source);
    let mut tokens = Vec::new();
    walk(&root, source, 0, true, &mut tokens);
    tokens
}

fn summarize_code_or_markup_diff(
    old_src: &str,
    new_src: &str,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    if !old_src.contains('[')
        && !new_src.contains('[')
        && !old_src.contains('\n')
        && !new_src.contains('\n')
        && old_src.len() <= 80
        && new_src.len() <= 80
    {
        let spans = word_diff(old_src, new_src);
        if let Some(res) = summarize_modified_spans(&spans) {
            return Some(res);
        }
    }

    let old_tokens = tokenize_typst_for_summary(old_src);
    let new_tokens = tokenize_typst_for_summary(new_src);
    let spans = diff_token_slices(&old_tokens, &new_tokens);
    summarize_modified_spans(&spans)
}

fn summarize_funccall_pair(
    old_fc: &str,
    new_fc: &str,
) -> (&'static str, Option<String>, Option<String>, Option<String>) {
    let label = funccall_kind(new_fc);
    let (old_short, old_long) = extract_funccall_captions(old_fc);
    let (new_short, new_long) = extract_funccall_captions(new_fc);

    if let (Some(ol), Some(nl)) = (old_long.as_deref(), new_long.as_deref()) {
        if ol != nl {
            let spans = word_diff(ol, nl);
            if let Some((ctx, old_ex, new_ex)) = summarize_modified_spans(&spans) {
                return (label, ctx, old_ex, new_ex);
            }
        }
    }
    if let (Some(os), Some(ns)) = (old_short.as_deref(), new_short.as_deref()) {
        if os != ns {
            let spans = word_diff(os, ns);
            if let Some((ctx, old_ex, new_ex)) = summarize_modified_spans(&spans) {
                return (label, ctx, old_ex, new_ex);
            }
        }
    }

    if matches!(label, "Figure" | "Table") && new_fc.contains("/.typdiff-figures/") {
        let masked_old = mask_figure_asset_paths(old_fc);
        let masked_new = mask_figure_asset_paths(new_fc);
        let norm_old = masked_old.split_whitespace().collect::<Vec<_>>().join(" ");
        let norm_new = masked_new.split_whitespace().collect::<Vec<_>>().join(" ");
        if norm_old == norm_new {
            let cap = new_short
                .or(new_long)
                .map(|c| normalize_excerpt(&c, MAX_EXCERPT_CHARS));
            return (label, None, None, cap);
        }
        if let Some((ctx, old_ex, new_ex)) = summarize_code_or_markup_diff(&masked_old, &masked_new) {
            return (label, ctx, old_ex, new_ex);
        }
    }

    if let Some((ctx, old_ex, new_ex)) = summarize_code_or_markup_diff(old_fc, new_fc) {
        return (label, ctx, old_ex, new_ex);
    }

    let (_, new_text) = funccall_kind_and_text(new_fc);
    let (_, old_text) = funccall_kind_and_text(old_fc);
    (label, None, Some(old_text), Some(new_text))
}

fn summarize_raw_pair(
    old_raw: &str,
    new_raw: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let spans = word_diff(old_raw, new_raw);
    if let Some(res) = summarize_modified_spans(&spans) {
        return res;
    }
    (
        None,
        Some(normalize_excerpt(old_raw, MAX_EXCERPT_CHARS)),
        Some(normalize_excerpt(new_raw, MAX_EXCERPT_CHARS)),
    )
}

fn summarize_equation_pair(
    old_eq: &str,
    new_eq: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let old_tokens: Vec<&str> = old_eq.split_whitespace().collect();
    let new_tokens: Vec<&str> = new_eq.split_whitespace().collect();
    let diff = similar::TextDiff::configure().diff_slices(&old_tokens, &new_tokens);
    let mut old_groups: Vec<String> = Vec::new();
    let mut new_groups: Vec<String> = Vec::new();
    let mut cur_old: Vec<&str> = Vec::new();
    let mut cur_new: Vec<&str> = Vec::new();
    let mut first_context: Option<String> = None;
    let mut prev_equal: Option<&str> = None;

    for change in diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            similar::ChangeTag::Equal => {
                if !cur_old.is_empty() {
                    old_groups.push(cur_old.join(" "));
                    cur_old.clear();
                }
                if !cur_new.is_empty() {
                    new_groups.push(cur_new.join(" "));
                    cur_new.clear();
                }
                prev_equal = Some(val);
            }
            similar::ChangeTag::Delete => {
                if first_context.is_none() {
                    if let Some(prev) = prev_equal {
                        if prev != "$" {
                            first_context = Some(format!("… {prev}"));
                        }
                    }
                }
                cur_old.push(val);
            }
            similar::ChangeTag::Insert => {
                if first_context.is_none() {
                    if let Some(prev) = prev_equal {
                        if prev != "$" {
                            first_context = Some(format!("… {prev}"));
                        }
                    }
                }
                cur_new.push(val);
            }
        }
    }
    if !cur_old.is_empty() {
        old_groups.push(cur_old.join(" "));
    }
    if !cur_new.is_empty() {
        new_groups.push(cur_new.join(" "));
    }

    if old_groups.is_empty() && new_groups.is_empty() {
        return (
            None,
            Some(normalize_excerpt(old_eq, MAX_EXCERPT_CHARS)),
            Some(normalize_excerpt(new_eq, MAX_EXCERPT_CHARS)),
        );
    }

    let old_ex = (!old_groups.is_empty())
        .then(|| normalize_excerpt(&old_groups.join(" … "), MAX_EXCERPT_CHARS));
    let new_ex = (!new_groups.is_empty())
        .then(|| normalize_excerpt(&new_groups.join(" … "), MAX_EXCERPT_CHARS));
    (first_context, old_ex, new_ex)
}

fn summarize_single_block(
    block: &Block,
    tag: DiffTag,
) -> Option<(&'static str, String, bool)> {
    match block {
        Block::Paragraph { source_text } => {
            if is_label_only(source_text) {
                return None;
            }
            Some(("Text", normalize_excerpt(source_text, MAX_EXCERPT_CHARS), false))
        }
        Block::Heading { body_text, .. } => {
            Some(("Heading", normalize_excerpt(body_text, MAX_EXCERPT_CHARS), true))
        }
        Block::ListItem { body_text } | Block::EnumItem { body_text, .. } => {
            Some(("List", normalize_excerpt(body_text, MAX_EXCERPT_CHARS), false))
        }
        Block::TermItem { term, description } => {
            let combined = format!("{term}: {description}");
            Some(("Term", normalize_excerpt(&combined, MAX_EXCERPT_CHARS), false))
        }
        Block::Equation { content, .. } => {
            Some(("Equation", normalize_excerpt(content, MAX_EXCERPT_CHARS), true))
        }
        Block::RawBlock { content } => {
            Some(("Code", normalize_excerpt(content, MAX_EXCERPT_CHARS), true))
        }
        Block::FuncCall { content } => {
            if is_boilerplate_funccall(content) || is_added_figure_wrapper(content) {
                return None;
            }
            if tag == DiffTag::Deleted && content.trim_start().starts_with("#figure") {
                return None;
            }
            let (label, excerpt) = funccall_kind_and_text(content);
            Some((label, excerpt, true))
        }
        Block::Parbreak => None,
    }
}

fn summarize_modified_spans(
    spans: &[DiffSpan],
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let mut old_parts: Vec<&str> = Vec::new();
    let mut new_parts: Vec<&str> = Vec::new();
    let mut first_change_idx: Option<usize> = None;

    for (idx, span) in spans.iter().enumerate() {
        let trimmed = span.text.trim();
        if trimmed.is_empty() || is_label_only(&span.text) {
            continue;
        }
        match span.tag {
            SpanTag::Equal => {}
            SpanTag::Deleted => {
                first_change_idx.get_or_insert(idx);
                old_parts.push(trimmed);
            }
            SpanTag::Inserted => {
                first_change_idx.get_or_insert(idx);
                new_parts.push(trimmed);
            }
        }
    }

    if old_parts.is_empty() && new_parts.is_empty() {
        return None;
    }

    let old_ex = (!old_parts.is_empty())
        .then(|| normalize_excerpt(&old_parts.join(" … "), MAX_EXCERPT_CHARS));
    let new_ex = (!new_parts.is_empty())
        .then(|| normalize_excerpt(&new_parts.join(" … "), MAX_EXCERPT_CHARS));

    let single_side_len = match (&old_ex, &new_ex) {
        (Some(o), None) => Some(o.chars().count()),
        (None, Some(n)) => Some(n.chars().count()),
        _ => None,
    };
    let context = if single_side_len.is_some_and(|len| len < 35) {
        first_change_idx
            .and_then(|idx| idx.checked_sub(1))
            .and_then(|prev| spans.get(prev))
            .filter(|s| s.tag == SpanTag::Equal)
            .and_then(|s| trailing_context_excerpt(&s.text, 28))
    } else {
        None
    };

    Some((context, old_ex, new_ex))
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

    #[test]
    fn test_render_with_summary_emits_anchors_safely() {
        let results = vec![
            DiffResult::Modified {
                kind: BlockKind::Heading { depth: 1 },
                spans: vec![
                    DiffSpan {
                        tag: SpanTag::Deleted,
                        text: "Old Title".into(),
                    },
                    DiffSpan {
                        tag: SpanTag::Inserted,
                        text: "New Title".into(),
                    },
                ],
            },
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<sec:title>".into(),
            }),
            DiffResult::Modified {
                kind: BlockKind::Paragraph,
                spans: vec![
                    DiffSpan {
                        tag: SpanTag::Equal,
                        text: "See ".into(),
                    },
                    DiffSpan {
                        tag: SpanTag::Deleted,
                        text: "old".into(),
                    },
                    DiffSpan {
                        tag: SpanTag::Inserted,
                        text: "new".into(),
                    },
                    DiffSpan {
                        tag: SpanTag::Equal,
                        text: " value.".into(),
                    },
                ],
            },
            DiffResult::Added(Block::Equation {
                content: "$ x = 1 $".into(),
                block: true,
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<eq:x>".into(),
            }),
        ];

        let (output, anchors) =
            render_with_citations_and_summary(&results, &BTreeSet::new(), "typdiff:t0:");
        assert_eq!(anchors.len(), 3);
        assert_eq!(anchors[0].block_label, "Heading");
        assert_eq!(anchors[0].old_excerpt.as_deref(), Some("Old Title"));
        assert_eq!(anchors[0].new_excerpt.as_deref(), Some("New Title"));
        // Anchor for heading must follow <sec:title>, not sit before it.
        assert!(output.contains("<sec:title>\n#metadata(none) <typdiff:t0:0>"));
        // Inline paragraph anchor placed right before first diff span.
        assert!(output.contains("See #metadata(none) <typdiff:t0:1>#diff-deleted[old]#diff-added[new] value."));
        // Equation label attaches inside #diff-added[...] before metadata anchor.
        assert!(output.contains("#diff-added[$ x = 1 $ <eq:x>]\n\n#metadata(none) <typdiff:t0:2>"));
    }

    #[test]
    fn test_summary_pairs_equations_in_both_orders_without_duplicates() {
        // Case 1: Added before Deleted
        let added_first = vec![
            DiffResult::Added(Block::Equation {
                content: "$ phi_3 = a sin theta $".into(),
                block: true,
            }),
            DiffResult::Deleted(Block::Equation {
                content: "$ phi_3 = a cos theta $".into(),
                block: true,
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<eq:phi3>".into(),
            }),
        ];
        let (out1, anchors1) =
            render_with_citations_and_summary(&added_first, &BTreeSet::new(), "typdiff:e1:");
        assert_eq!(anchors1.len(), 1);
        assert_eq!(anchors1[0].kind, SummaryChangeKind::Modified);
        assert_eq!(anchors1[0].block_label, "Equation");
        assert_eq!(anchors1[0].old_excerpt.as_deref(), Some("cos"));
        assert_eq!(anchors1[0].new_excerpt.as_deref(), Some("sin"));
        assert!(out1.contains("#diff-added[$ phi_3 = a sin theta $ <eq:phi3>]"));
        assert!(out1.contains("#diff-deleted[$ phi_3 = a cos theta $]\n\n#metadata(none) <typdiff:e1:0>"));

        // Case 2: Deleted before Added
        let deleted_first = vec![
            DiffResult::Deleted(Block::Equation {
                content: "$ phi_3 = a cos theta $".into(),
                block: true,
            }),
            DiffResult::Added(Block::Equation {
                content: "$ phi_3 = a sin theta $".into(),
                block: true,
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<eq:phi3>".into(),
            }),
        ];
        let (out2, anchors2) =
            render_with_citations_and_summary(&deleted_first, &BTreeSet::new(), "typdiff:e2:");
        assert_eq!(anchors2.len(), 1, "expected 1 merged anchor, got: {anchors2:?}");
        assert_eq!(anchors2[0].kind, SummaryChangeKind::Modified);
        assert_eq!(anchors2[0].old_excerpt.as_deref(), Some("cos"));
        assert_eq!(anchors2[0].new_excerpt.as_deref(), Some("sin"));
        assert!(out2.contains("#diff-added[$ phi_3 = a sin theta $ <eq:phi3>]\n\n#metadata(none) <typdiff:e2:0>"));
        assert_eq!(out2.matches("#metadata(none)").count(), 1);
    }

    #[test]
    fn test_summary_pairs_consecutive_equations_by_similarity() {
        let results = vec![
            DiffResult::Added(Block::Equation {
                content: "$ alpha = x + 2 $".into(),
                block: true,
            }),
            DiffResult::Added(Block::Equation {
                content: "$ beta = y * 9 $".into(),
                block: true,
            }),
            DiffResult::Deleted(Block::Equation {
                content: "$ alpha = x + 1 $".into(),
                block: true,
            }),
            DiffResult::Deleted(Block::Equation {
                content: "$ beta = y * 8 $".into(),
                block: true,
            }),
        ];
        let (out, anchors) =
            render_with_citations_and_summary(&results, &BTreeSet::new(), "typdiff:eqs:");
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].old_excerpt.as_deref(), Some("1"));
        assert_eq!(anchors[0].new_excerpt.as_deref(), Some("2"));
        assert_eq!(anchors[1].old_excerpt.as_deref(), Some("8"));
        assert_eq!(anchors[1].new_excerpt.as_deref(), Some("9"));
        assert_eq!(out.matches("#metadata(none)").count(), 2);
    }

    #[test]
    fn test_summary_funccall_short_cap_long_diff_and_table_note_diff() {
        // 1. short-cap with unchanged short caption and modified long caption inside grid figure
        let old_fig = r#"#figure(
  grid(columns: 2, subfig("(a)", "./a.pdf"), subfig("(b)", "./b.pdf")),
  caption: short-cap([Short title unchanged])[Detailed description (see Section @sec:COBE-SST2).],
)"#;
        let new_fig = r#"#figure(
  grid(columns: 2, subfig("(a)", "./a.pdf"), subfig("(b)", "./b.pdf")),
  caption: short-cap([Short title unchanged])[Detailed description (see @sec:COBE-SST2).],
)"#;
        let fig_results = vec![
            DiffResult::Added(Block::FuncCall {
                content: new_fig.into(),
            }),
            DiffResult::Deleted(Block::FuncCall {
                content: old_fig.into(),
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<fig:grid>".into(),
            }),
        ];
        let (_out, fig_anchors) =
            render_with_citations_and_summary(&fig_results, &BTreeSet::new(), "typdiff:f:");
        assert_eq!(fig_anchors.len(), 1);
        assert_eq!(fig_anchors[0].kind, SummaryChangeKind::Modified);
        assert_eq!(fig_anchors[0].block_label, "Figure");
        assert_eq!(fig_anchors[0].figure_label.as_deref(), Some("fig:grid"));
        assert_eq!(fig_anchors[0].old_excerpt.as_deref(), Some("Section"));
        assert_eq!(fig_anchors[0].new_excerpt, None);

        // 2. #figure([ #table(...) #align(...)[ ... ] ]) with unchanged captions and modified note
        let old_tbl = r#"#figure(
  [
    #table(columns: 2, [CTL], [Clim])
    #align(left)[
      #set text(size: 9pt)
      _Note_. Represented by $phi_1$ in Equations 3–7.
    ]
  ],
  caption: short-cap([Experiment settings])[Theoretical experiment settings.],
)"#;
        let new_tbl = r#"#figure(
  [
    #table(columns: 2, [CTL], [Clim])
    #align(left)[
      #set text(size: 9pt)
      _Note_. Represented by $phi_1$ in #Eq(<eq:TideQBO_equ>)–#Eq(<eq:TideQBO_eqT>).
    ]
  ],
  caption: short-cap([Experiment settings])[Theoretical experiment settings.],
)"#;
        let tbl_results = vec![
            DiffResult::Deleted(Block::FuncCall {
                content: old_tbl.into(),
            }),
            DiffResult::Added(Block::FuncCall {
                content: new_tbl.into(),
            }),
            DiffResult::Unchanged(Block::Paragraph {
                source_text: "<tab:TideQBO_explist>".into(),
            }),
        ];
        let (tbl_out, tbl_anchors) =
            render_with_citations_and_summary(&tbl_results, &BTreeSet::new(), "typdiff:t:");
        assert_eq!(tbl_anchors.len(), 1);
        assert_eq!(tbl_anchors[0].kind, SummaryChangeKind::Modified);
        assert_eq!(tbl_anchors[0].block_label, "Table");
        assert_eq!(tbl_anchors[0].figure_label.as_deref(), Some("tab:TideQBO_explist"));
        assert_eq!(tbl_anchors[0].old_excerpt.as_deref(), Some("Equations 3–7"));
        assert_eq!(
            tbl_anchors[0].new_excerpt.as_deref(),
            Some("#Eq(<eq:TideQBO_equ>)–#Eq(<eq:TideQBO_eqT>)")
        );
        assert!(tbl_out.contains("<tab:TideQBO_explist>\n#metadata(none) <typdiff:t:0>"));
    }

    #[test]
    fn test_summary_funccall_image_only_ignores_added_caption_wrapper() {
        let old_fig = r#"#figure(
  image("./Figs/fig1.pdf"),
  caption: short-cap([My clean short title])[Long caption.],
)"#;
        let new_fig = r#"#figure(
  image("/.typdiff-figures/figure-0000.pdf"),
  caption: short-cap([#text(fill: rgb("0067c0"))[#underline[My clean short title]]])[Long caption.],
)"#;
        let (lbl, _ctx, old_ex, new_ex) = summarize_funccall_pair(old_fig, new_fig);
        assert_eq!(lbl, "Figure");
        assert_eq!(old_ex, None);
        assert_eq!(new_ex.as_deref(), Some("My clean short title"));
    }
}
