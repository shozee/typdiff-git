mod cli;
mod git;
mod tools;
mod typdiff;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::Cli;
use git::{is_worktree, Change, ChangeKind, Repository};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

#[derive(Clone, Debug)]
struct FigureReplacement {
    replacement: PathBuf,
    kind: ChangeKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FigureDefinition {
    label: String,
    image_path: Option<PathBuf>,
    caption: Option<String>,
    source_file: PathBuf,
}

#[derive(Clone, Debug)]
struct PlacedDeletedFigure {
    label: String,
}

#[derive(Clone, Debug)]
struct SummaryEntry {
    anchor: String,
    file: PathBuf,
    kind: String,
    block_label: String,
    figure_label: Option<String>,
    context_excerpt: Option<String>,
    old_excerpt: Option<String>,
    new_excerpt: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(cli.repository.as_deref())?;
    if is_worktree(&cli.old) {
        bail!("WORKTREE can be used only as the newer revision");
    }
    repo.verify_revision(&cli.old)?;
    repo.verify_revision(&cli.new)?;
    tools::require("typst")?;
    let image_functions = normalized_image_functions(&cli.image_functions)?;

    let debug_repo_prefix = cli
        .debug_dir
        .as_ref()
        .and_then(|path| repository_relative_path(repo.root(), path));
    let output = absolute_output(&cli.output)?;

    if cli.watch {
        if !is_worktree(&cli.new) {
            bail!("--watch requires the newer revision to be WORKTREE (or -)");
        }
        match generate_diff(&cli, &repo, &image_functions, &output, debug_repo_prefix.as_deref()) {
            Ok(_) => {}
            Err(e) => eprintln!("error building initial diff: {e:#}"),
        }
        eprintln!(
            "Watching for changes in {}... (Press Ctrl+C to exit)",
            repo.root().display()
        );
        watch_loop(
            &cli,
            &repo,
            &image_functions,
            &output,
            debug_repo_prefix.as_deref(),
        )?;
    } else {
        generate_diff(
            &cli,
            &repo,
            &image_functions,
            &output,
            debug_repo_prefix.as_deref(),
        )?;
    }

    Ok(())
}

fn watch_loop(
    cli: &Cli,
    repo: &Repository,
    image_functions: &[String],
    output: &Path,
    debug_repo_prefix: Option<&Path>,
) -> Result<()> {
    let mut last_mtime = get_latest_mtime(repo.root(), output, cli.debug_dir.as_deref());
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let current_mtime = get_latest_mtime(repo.root(), output, cli.debug_dir.as_deref());
        if current_mtime > last_mtime {
            // Debounce to allow multiple quick saves / file system events to settle
            std::thread::sleep(Duration::from_millis(150));
            last_mtime = get_latest_mtime(repo.root(), output, cli.debug_dir.as_deref());
            eprintln!("\nChange detected, updating {}...", output.display());
            let start = Instant::now();
            match generate_diff(cli, repo, image_functions, output, debug_repo_prefix) {
                Ok(_) => {
                    eprintln!(
                        "Successfully updated {} in {:.2?}",
                        output.display(),
                        start.elapsed()
                    );
                }
                Err(e) => {
                    eprintln!("Error updating diff: {e:#}");
                }
            }
        }
    }
}

fn get_latest_mtime(
    root: &Path,
    output: &Path,
    debug_dir: Option<&Path>,
) -> Option<SystemTime> {
    let output_abs = output.canonicalize().ok().unwrap_or_else(|| output.to_path_buf());
    let debug_abs = debug_dir.and_then(|d| d.canonicalize().ok().or_else(|| Some(d.to_path_buf())));

    let mut latest = None;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let file_name = e.file_name().to_string_lossy();
            if file_name == ".git"
                || file_name == "target"
                || file_name.starts_with(".#")
                || file_name.ends_with('~')
                || file_name.ends_with(".swp")
                || file_name.ends_with(".tmp")
            {
                return false;
            }
            let path = e.path();
            if path == output || path == output_abs || e.file_name() == output.file_name().unwrap_or_default() {
                return false;
            }
            if let Some(debug) = &debug_abs {
                if path == debug.as_path() {
                    return false;
                }
            }
            true
        })
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    latest = match latest {
                        None => Some(mtime),
                        Some(prev) => Some(prev.max(mtime)),
                    };
                }
            }
        }
    }
    latest
}

fn generate_diff(
    cli: &Cli,
    repo: &Repository,
    image_functions: &[String],
    output: &Path,
    debug_repo_prefix: Option<&Path>,
) -> Result<()> {
    let (workspace, _temporary_guard) = if let Some(debug_dir) = &cli.debug_dir {
        let debug_dir = absolute_output(debug_dir)?;
        if debug_dir.exists() {
            fs::remove_dir_all(&debug_dir).with_context(|| {
                format!("clearing debug directory {}", debug_dir.display())
            })?;
        }
        fs::create_dir_all(&debug_dir)?;
        eprintln!("debug workspace: {}", debug_dir.display());
        (debug_dir, None)
    } else {
        let temporary = TempDir::new().context("creating temporary workspace")?;
        let path = temporary.path().to_path_buf();
        if !cli.watch {
            eprintln!("temporary workspace: {} (removed when the command exits)", path.display());
        }
        (path, Some(temporary))
    };
    let old_tree = workspace.join("old");
    let new_tree = workspace.join("new");
    let report_tree = workspace.join("report");
    repo.export_revision(&cli.old, &old_tree)?;
    repo.export_revision(&cli.new, &new_tree)?;
    copy_tree(&new_tree, &report_tree)?;

    // Ensure any deleted bibliography entries from old revision are present in report_tree
    sync_old_bib_entries_to_report(&old_tree, &report_tree)?;

    // Collect all citation keys from old, new, and report trees
    let citation_keys = collect_citation_keys_from_trees(&[&old_tree, &new_tree, &report_tree]);
    if !citation_keys.is_empty() {
        eprintln!("indexed {} bibliography citation key(s)", citation_keys.len());
    }

    // Mark newly-added figure definitions by label, independently of whether the
    // underlying image asset is tracked, generated, reused, or referenced through a
    // custom wrapper. This is more reliable than asset-only detection for Added figures.
    let old_figure_labels: BTreeSet<String> = collect_figure_definitions(
        &old_tree,
        image_functions,
    )?
    .into_iter()
    .map(|definition| definition.label)
    .collect();
    let added_figure_labels = mark_added_figure_definitions(
        &report_tree,
        &old_figure_labels,
    )?;
    if !added_figure_labels.is_empty() {
        eprintln!(
            "marked {} added figure definition(s) by label:",
            added_figure_labels.len()
        );
        for label in &added_figure_labels {
            eprintln!("  - <{}>", label);
        }
    }

    let main_file = report_tree.join(&cli.main);
    if !main_file.is_file() {
        bail!("main Typst document does not exist in the newer revision: {}", cli.main.display());
    }

    let mut changes = repo.changed_typst_files(&cli.old, &cli.new)?;
    changes.retain(|change| {
        cli.matches(change) && !change_is_below(change, debug_repo_prefix)
    });

    // Prepare and rewrite changed figure assets before running typdiff.  This is
    // important for newly-added figures: once typdiff has wrapped a newly-added
    // figure block, reliably rewriting its image path becomes much harder.
    let mut figures = repo.changed_figure_files(&cli.old, &cli.new)?;
    figures.retain(|change| !change_is_below(change, debug_repo_prefix));
    let mut marked_figures = prepare_figure_comparisons(
        &figures,
        &old_tree,
        &new_tree,
        &report_tree,
        &workspace,
    )?;

    // Independently compare every image/PDF path referenced by both revisions.
    // This is the reliable fallback for a same-name, same-path asset whose bytes were
    // replaced in WORKTREE but which was not surfaced by Git name-status processing.
    let mut modified_figure_assets: HashMap<(String, usize), PathBuf> = HashMap::new();
    add_same_path_content_comparisons(
        &old_tree,
        &new_tree,
        &report_tree,
        &workspace,
        image_functions,
        &mut marked_figures,
        &mut modified_figure_assets,
    )?;

    // A figure can be modified without either asset being modified in Git: the
    // #figure definition may simply switch from one existing PDF/image path to
    // another. Detect this by matching old/new figure definitions by label and
    // comparing their resolved image paths.
    add_referenced_asset_comparisons(
        &old_tree,
        &new_tree,
        &report_tree,
        &workspace,
        image_functions,
        &mut marked_figures,
    )?;

    rewrite_image_references(&report_tree, &marked_figures, image_functions)?;

    let mut replaced = 0usize;
    let mut summary_entries: Vec<SummaryEntry> = Vec::new();
    let mut typdiff_processed_files: BTreeSet<PathBuf> = BTreeSet::new();
    for change in &changes {
        match change.kind {
            ChangeKind::Deleted => {
                eprintln!(
                    "note: deleted file omitted from newer full-document report: {}",
                    change.old_path.display()
                );
                if cli.summary_page {
                    summary_entries.push(SummaryEntry {
                        anchor: String::new(),
                        file: change.old_path.clone(),
                        kind: "Deleted".to_string(),
                        block_label: "File".to_string(),
                        figure_label: None,
                        context_excerpt: None,
                        old_excerpt: Some(format!(
                            "Removed file {}",
                            normalize_path(&change.old_path)
                        )),
                        new_excerpt: None,
                    });
                }
            }
            ChangeKind::Added => {
                let empty = workspace.join(format!("empty-{replaced}.typ"));
                fs::write(&empty, "")?;
                let rewritten_new = report_tree.join(&change.new_path);
                let prefix = cli
                    .summary_page
                    .then(|| format!("typdiff:s{replaced}:"));
                let anchors = run_typdiff(
                    &empty,
                    &rewritten_new,
                    &rewritten_new,
                    &citation_keys,
                    prefix.as_deref(),
                )?;
                typdiff_processed_files.insert(change.new_path.clone());
                append_summary_anchors(&mut summary_entries, &change.new_path, anchors);
                replaced += 1;
            }
            ChangeKind::Modified | ChangeKind::Renamed => {
                let old_source = old_tree.join(&change.old_path);
                let rewritten_new = report_tree.join(&change.new_path);
                let prefix = cli
                    .summary_page
                    .then(|| format!("typdiff:s{replaced}:"));
                let anchors = run_typdiff(
                    &old_source,
                    &rewritten_new,
                    &rewritten_new,
                    &citation_keys,
                    prefix.as_deref(),
                )?;
                typdiff_processed_files.insert(change.new_path.clone());
                append_summary_anchors(&mut summary_entries, &change.new_path, anchors);
                replaced += 1;
            }
        }
    }
    // typdiff may reconstruct added code blocks. Run the path rewriter once more on
    // its output so added/modified figure assets still point at the generated framed
    // comparison PDFs. The pass is idempotent because already-rewritten paths no longer
    // match the original asset paths.
    rewrite_image_references(&report_tree, &marked_figures, image_functions)?;
    rewrite_modified_figures_by_label(
        &report_tree,
        &modified_figure_assets,
    )?;

    if cli.summary_page {
        collect_summary_figure_entries(
            &report_tree,
            &added_figure_labels,
            &typdiff_processed_files,
            &cli.notices_file,
            &cli.summary_file,
            &mut summary_entries,
        )?;
    }

    let removed_references =
        removed_figure_references(&old_tree, &new_tree, image_functions)?;
    let removed_definitions =
        removed_figure_definitions(&old_tree, &new_tree, image_functions)?;
    let removed_label_references =
        removed_figure_label_references(&old_tree, &new_tree)?;
    if !removed_references.is_empty() {
        eprintln!("detected {} removed figure path reference(s):", removed_references.len());
        for path in &removed_references {
            eprintln!("  - {}", path.display());
        }
    }
    if !removed_definitions.is_empty() {
        eprintln!("detected {} removed figure definition label(s):", removed_definitions.len());
        for definition in &removed_definitions {
            eprintln!("  - <{}> in {}", definition.label, definition.source_file.display());
        }
    }
    if !removed_label_references.is_empty() {
        eprintln!("detected {} removed #Fig/#Figure reference label(s):", removed_label_references.len());
        for label in &removed_label_references {
            eprintln!("  - <{}>", label);
        }
    }
    let mut delfig_counter = 0usize;
    let placed_deleted = place_deleted_figure_notices(
        &report_tree,
        &removed_definitions,
        &removed_label_references,
        cli.summary_page,
        &mut delfig_counter,
        cli.summary_page.then_some(&mut summary_entries),
    )?;
    if !placed_deleted.is_empty() {
        eprintln!("placed {} deleted-figure notice(s) near their former references", placed_deleted.len());
    }
    write_debug_manifest(
        &workspace,
        &removed_references,
        &removed_definitions,
        &removed_label_references,
    )?;
    let deleted_figures = write_deleted_figure_notices(
        &main_file,
        &report_tree,
        &cli.notices_file,
        &figures,
        &removed_references,
        &removed_definitions,
        &removed_label_references,
        &placed_deleted,
        &mut delfig_counter,
        cli.summary_page.then_some(&mut summary_entries),
    )?;

    if cli.summary_page {
        write_summary_page(
            &main_file,
            &report_tree,
            &cli.summary_file,
            &cli.old,
            &cli.new,
            &summary_entries,
        )?;
    }

    if let Some(parent) = output.parent() { fs::create_dir_all(parent)?; }
    tools::run(
        "typst",
        [
            OsStr::new("compile"),
            OsStr::new("--root"),
            report_tree.as_os_str(),
            main_file.as_os_str(),
            output.as_os_str(),
        ],
    )?;

    if cli.debug_dir.is_some() {
        println!("debug report source: {}", main_file.display());
        println!("debug manifest: {}", workspace.join("deleted-figures.txt").display());
        println!(
            "debug notice source: {}",
            report_tree.join(&cli.notices_file).display()
        );
        if cli.summary_page {
            println!(
                "debug summary source: {}",
                report_tree.join(&cli.summary_file).display()
            );
        }
    }
    println!(
        "created {} from {} ({} child files, {} figures highlighted, and {} deleted-figure notices)",
        output.display(),
        cli.main.display(),
        replaced,
        marked_figures.len(),
        deleted_figures
    );
    Ok(())
}

fn write_debug_manifest(
    workspace: &Path,
    removed_references: &BTreeSet<PathBuf>,
    removed_definitions: &[FigureDefinition],
    removed_label_references: &BTreeSet<String>,
) -> Result<()> {
    let mut text = String::new();
    text.push_str("Removed figure labels:\n");
    if removed_definitions.is_empty() {
        text.push_str("  (none)\n");
    }
    for definition in removed_definitions {
        text.push_str(&format!(
            "  <{}> source={} image={}\n",
            definition.label,
            definition.source_file.display(),
            definition
                .image_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unknown)".to_string())
        ));
    }
    text.push_str("\nRemoved #Fig/#Figure label references:\n");
    if removed_label_references.is_empty() {
        text.push_str("  (none)\n");
    }
    for label in removed_label_references {
        text.push_str(&format!("  <{}>\n", label));
    }
    text.push_str("\nRemoved figure path references:\n");
    if removed_references.is_empty() {
        text.push_str("  (none)\n");
    }
    for path in removed_references {
        text.push_str(&format!("  {}\n", path.display()));
    }
    fs::write(workspace.join("deleted-figures.txt"), text)?;
    Ok(())
}

fn removed_figure_label_references(
    old_tree: &Path,
    new_tree: &Path,
) -> Result<BTreeSet<String>> {
    let old = collect_figure_label_references(old_tree)?;
    let new = collect_figure_label_references(new_tree)?;
    Ok(old.difference(&new).cloned().collect())
}

fn collect_figure_label_references(root: &Path) -> Result<BTreeSet<String>> {
    let mut labels = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        labels.extend(extract_figure_reference_labels(&source));
    }
    Ok(labels)
}

fn extract_figure_reference_labels(source: &str) -> BTreeSet<String> {
    let mut labels = BTreeSet::new();
    for function in ["#Fig", "#Figure", "#Figs", "#Figures"] {
        let mut offset = 0usize;
        while let Some(relative) = source[offset..].find(function) {
            let start = offset + relative;
            let after_name = start + function.len();
            // Avoid matching #Figure when scanning #Fig.
            if source.as_bytes().get(after_name).is_some_and(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
            }) {
                offset = after_name;
                continue;
            }
            let mut open = after_name;
            while open < source.len() && source.as_bytes()[open].is_ascii_whitespace() {
                open += 1;
            }
            if source.as_bytes().get(open) != Some(&b'(') {
                offset = after_name;
                continue;
            }
            let Some(close) = find_matching_delimiter(source, open, '(', ')') else {
                offset = open + 1;
                continue;
            };
            let body = &source[open + 1..close];
            let mut body_offset = 0usize;
            while let Some(label_start_relative) = body[body_offset..].find("<fig:") {
                let label_start = body_offset + label_start_relative + 1;
                let Some(label_end_relative) = body[label_start..].find('>') else {
                    break;
                };
                let label_end = label_start + label_end_relative;
                labels.insert(body[label_start..label_end].trim().to_string());
                body_offset = label_end + 1;
            }
            offset = close + 1;
        }
    }
    labels
}

fn mark_added_figure_definitions(
    report_tree: &Path,
    old_labels: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut marked_labels = BTreeSet::new();

    for entry in walkdir::WalkDir::new(report_tree) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
            || entry.path().starts_with(report_tree.join(".typdiff-figures"))
        {
            continue;
        }

        let source = fs::read_to_string(entry.path())?;
        let definitions = find_figure_definition_ranges(&source);
        let mut edits = Vec::new();

        for (start, end, label) in definitions {
            if old_labels.contains(&label) {
                continue;
            }
            edits.push((start, end, label));
        }

        if edits.is_empty() {
            continue;
        }

        let mut updated = source;
        for (start, end, label) in edits.into_iter().rev() {
            let original = &updated[start..end];
            let wrapped = added_figure_definition_markup(original);
            updated.replace_range(start..end, &wrapped);
            marked_labels.insert(label);
        }
        fs::write(entry.path(), updated)?;
        eprintln!("wrapped added figure definition(s) in {}", entry.path().display());
    }

    Ok(marked_labels)
}

fn find_figure_definition_ranges(source: &str) -> Vec<(usize, usize, String)> {
    find_figure_definition_ranges_inner(source, false)
}

fn find_figure_definition_ranges_inner(
    source: &str,
    all_labels: bool,
) -> Vec<(usize, usize, String)> {
    let mut result = Vec::new();
    let mut offset = 0usize;

    while let Some(relative) = source[offset..].find("#figure") {
        let start = offset + relative;
        if is_inside_line_comment(source, start) {
            offset = start + "#figure".len();
            continue;
        }
        let mut open = start + "#figure".len();
        while open < source.len() && source.as_bytes()[open].is_ascii_whitespace() {
            open += 1;
        }
        if source.as_bytes().get(open) != Some(&b'(') {
            offset = open.max(start + 1);
            continue;
        }
        let Some(close) = find_matching_delimiter(source, open, '(', ')') else {
            offset = open + 1;
            continue;
        };

        let label_open = skip_whitespace_and_comments(source, close + 1);
        if source.as_bytes().get(label_open) != Some(&b'<') {
            offset = close + 1;
            continue;
        }
        let Some(label_end_relative) = source[label_open + 1..].find('>') else {
            offset = close + 1;
            continue;
        };
        let label_close = label_open + 1 + label_end_relative;
        let label = source[label_open + 1..label_close].trim().to_string();
        if (all_labels && typst_syntax::is_valid_label_literal_id(&label))
            || label.starts_with("fig:")
        {
            result.push((start, label_close + 1, label));
        }
        offset = label_close + 1;
    }

    result
}

fn is_inside_line_comment(source: &str, pos: usize) -> bool {
    let line_start = source[..pos].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..pos].trim_start().starts_with("//")
}

fn skip_whitespace_and_comments(source: &str, mut idx: usize) -> usize {
    let bytes = source.as_bytes();
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            idx += 1;
        } else if source[idx..].starts_with("//") {
            if let Some(nl) = source[idx..].find('\n') {
                idx += nl + 1;
            } else {
                idx = bytes.len();
            }
        } else if source[idx..].starts_with("/*") {
            if let Some(end) = source[idx + 2..].find("*/") {
                idx += 2 + end + 2;
            } else {
                idx = bytes.len();
            }
        } else {
            break;
        }
    }
    idx
}

fn added_figure_definition_markup(figure_source: &str) -> String {
    format!(
        r#"#block(
  width: 100%,
  inset: 7pt,
  stroke: 1pt + rgb("0067c0"),
  fill: rgb("f3f8ff"),
  radius: 2pt,
  breakable: false,
)[
  #text(fill: rgb("0067c0"), weight: "bold")[Added figure]
  #v(4pt)
  {}
]"#,
        figure_source
    )
}

fn removed_figure_definitions(
    old_tree: &Path,
    new_tree: &Path,
    image_functions: &[String],
) -> Result<Vec<FigureDefinition>> {
    let old = collect_figure_definitions(old_tree, image_functions)?;
    let new = collect_figure_definitions(new_tree, image_functions)?;
    let new_labels: BTreeSet<&str> = new.iter().map(|figure| figure.label.as_str()).collect();
    Ok(old
        .into_iter()
        .filter(|figure| !new_labels.contains(figure.label.as_str()))
        .collect())
}

fn collect_figure_definitions(
    root: &Path,
    image_functions: &[String],
) -> Result<Vec<FigureDefinition>> {
    let mut definitions = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        let source_dir = entry.path().parent().unwrap_or(root);
        let source_file = entry.path().strip_prefix(root)?.to_path_buf();
        let mut offset = 0usize;
        while let Some(relative) = source[offset..].find("#figure") {
            let start = offset + relative;
            let mut open = start + "#figure".len();
            while open < source.len() && source.as_bytes()[open].is_ascii_whitespace() {
                open += 1;
            }
            if source.as_bytes().get(open) != Some(&b'(') {
                offset = open;
                continue;
            }
            let Some(close) = find_matching_delimiter(&source, open, '(', ')') else {
                offset = open + 1;
                continue;
            };
            let mut label_start = close + 1;
            while label_start < source.len()
                && source.as_bytes()[label_start].is_ascii_whitespace()
            {
                label_start += 1;
            }
            if source.as_bytes().get(label_start) != Some(&b'<') {
                offset = close + 1;
                continue;
            }
            let Some(label_relative_end) = source[label_start + 1..].find('>') else {
                offset = close + 1;
                continue;
            };
            let label_end = label_start + 1 + label_relative_end;
            let label = source[label_start + 1..label_end].trim().to_string();
            if !label.starts_with("fig:") {
                offset = label_end + 1;
                continue;
            }
            let block = &source[start..=close];
            let image_path = extract_direct_image_paths(block, image_functions)
                .into_iter()
                .find_map(|path| resolve_project_path(root, source_dir, &path));
            let caption = extract_caption_content(block);
            definitions.push(FigureDefinition {
                label,
                image_path,
                caption,
                source_file: source_file.clone(),
            });
            offset = label_end + 1;
        }
    }
    Ok(definitions)
}

fn extract_caption_content(figure_block: &str) -> Option<String> {
    let caption_pos = figure_block.find("caption:")? + "caption:".len();
    let remaining = &figure_block[caption_pos..];
    let open_relative = remaining.find('[')?;
    let open = caption_pos + open_relative;
    let close = find_matching_delimiter(figure_block, open, '[', ']')?;
    let caption = figure_block[open + 1..close].trim();
    if caption.is_empty() {
        None
    } else {
        Some(caption.to_string())
    }
}

fn deleted_caption_markup(caption: &str) -> String {
    format!("  #v(3pt)\n  {}\n", caption)
}

fn removed_figure_references(
    old_tree: &Path,
    new_tree: &Path,
    image_functions: &[String],
) -> Result<BTreeSet<PathBuf>> {
    let old = collect_figure_references(old_tree, image_functions)?;
    let new = collect_figure_references(new_tree, image_functions)?;
    Ok(old.difference(&new).cloned().collect())
}

fn collect_figure_references(
    root: &Path,
    image_functions: &[String],
) -> Result<BTreeSet<PathBuf>> {
    let mut references = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        let source_dir = entry.path().parent().unwrap_or(root);
        for image_path in extract_direct_image_paths(&source, image_functions) {
            if let Some(project_path) = resolve_project_path(root, source_dir, &image_path) {
                references.insert(project_path);
            }
        }
    }
    Ok(references)
}

fn extract_direct_image_paths(source: &str, image_functions: &[String]) -> Vec<String> {
    let mut paths = BTreeSet::new();

    // First, honor explicitly registered image functions.
    for function in image_functions {
        let mut offset = 0usize;
        while let Some((_, path_start, path_end)) = find_image_call(source, function, offset) {
            paths.insert(source[path_start..path_end].to_string());
            offset = path_end + 1;
        }
    }

    // Also scan string literals whose suffix is a supported figure extension. This
    // catches wrappers where the path is not the first argument, named arguments such
    // as `path: "..."`, and project-specific helpers not registered on the CLI.
    for literal in extract_string_literals(source) {
        if is_figure_path(&literal) {
            paths.insert(literal);
        }
    }

    paths.into_iter().collect()
}

fn extract_string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'"' && quote != b'\'' {
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        let mut escaped = false;
        while index < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == quote {
                if let Ok(value) = std::str::from_utf8(&bytes[start..index]) {
                    result.push(value.to_string());
                }
                index += 1;
                break;
            }
            index += 1;
        }
    }
    result
}

fn is_figure_path(value: &str) -> bool {
    let without_query = value.split(['?', '#']).next().unwrap_or(value);
    Path::new(without_query)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "svg" | "pdf"
            )
        })
}

fn find_image_call(
    source: &str,
    function: &str,
    offset: usize,
) -> Option<(usize, usize, usize)> {
    let bytes = source.as_bytes();
    let mut search_from = offset;
    while let Some(relative) = source[search_from..].find(function) {
        let function_start = search_from + relative;
        let function_end = function_start + function.len();
        let valid_left = function_start == 0
            || !is_identifier_byte(bytes[function_start - 1]);
        let valid_right = function_end >= bytes.len()
            || !is_identifier_byte(bytes[function_end]);
        if !valid_left || !valid_right {
            search_from = function_end;
            continue;
        }
        let mut index = function_end;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'(') {
            search_from = function_end;
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let quote = *bytes.get(index)?;
        if quote != b'"' && quote != b'\'' {
            search_from = index;
            continue;
        }
        let path_start = index + 1;
        index = path_start;
        let mut escaped = false;
        while index < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == quote {
                return Some((function_start, path_start, index));
            }
            index += 1;
        }
        return None;
    }
    None
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn resolve_project_path(root: &Path, source_dir: &Path, image_path: &str) -> Option<PathBuf> {
    // URL and data sources are not repository figure paths.
    if image_path.contains("://") || image_path.starts_with("data:") {
        return None;
    }
    let candidate = if let Some(stripped) = image_path.strip_prefix('/') {
        root.join(stripped)
    } else {
        source_dir.join(image_path)
    };
    let normalized = normalize_lexically(&candidate);
    normalized.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn place_deleted_figure_notices(
    report_tree: &Path,
    removed_definitions: &[FigureDefinition],
    removed_label_references: &BTreeSet<String>,
    emit_anchors: bool,
    delfig_counter: &mut usize,
    mut summary_entries: Option<&mut Vec<SummaryEntry>>,
) -> Result<Vec<PlacedDeletedFigure>> {
    let definitions: HashMap<&str, &FigureDefinition> = removed_definitions
        .iter()
        .map(|definition| (definition.label.as_str(), definition))
        .collect();
    let mut placed = Vec::new();

    for label in removed_label_references {
        let Some((source_path, insertion)) = find_deleted_reference_location(report_tree, label)? else {
            continue;
        };
        let rel_source = source_path
            .strip_prefix(report_tree)
            .unwrap_or(&source_path)
            .to_path_buf();
        let definition = definitions.get(label.as_str()).copied();
        let anchor = if emit_anchors {
            let a = format!("typdiff:delfig-{}", *delfig_counter);
            *delfig_counter += 1;
            Some(a)
        } else {
            None
        };
        let box_markup = inline_deleted_figure_box(label, definition, anchor.as_deref());
        let mut source = fs::read_to_string(&source_path)?;
        source.insert_str(insertion, &box_markup);
        fs::write(&source_path, source)?;
        if let (Some(entries), Some(anchor_id)) = (summary_entries.as_deref_mut(), anchor.as_ref()) {
            let excerpt = definition
                .and_then(|d| d.caption.as_deref())
                .map(clean_caption_excerpt)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    definition
                        .and_then(|d| d.image_path.as_ref())
                        .map(|p| normalize_path(p))
                })
                .unwrap_or_else(|| format!("<{label}>"));
            entries.push(SummaryEntry {
                anchor: anchor_id.clone(),
                file: rel_source.clone(),
                kind: "Deleted".to_string(),
                block_label: "Figure".to_string(),
                figure_label: Some(label.clone()),
                context_excerpt: None,
                old_excerpt: Some(excerpt),
                new_excerpt: None,
            });
        }
        placed.push(PlacedDeletedFigure {
            label: label.clone(),
        });
    }
    Ok(placed)
}

fn find_deleted_reference_location(
    report_tree: &Path,
    label: &str,
) -> Result<Option<(PathBuf, usize)>> {
    let escaped = format!("\\<{}>", label);
    let plain = format!("<{}>", label);
    for entry in walkdir::WalkDir::new(report_tree) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
            || entry.path().file_name() == Some(OsStr::new(".typdiff-notices.typ"))
            || entry.path().file_name() == Some(OsStr::new(".typdiff-summary.typ"))
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        let Some(label_pos) = source.find(&escaped).or_else(|| source.find(&plain)) else {
            continue;
        };
        // Prefer the end of the paragraph containing the deleted prose reference. This
        // keeps the notice close to the original discussion without splitting a sentence.
        let insertion = paragraph_end(&source, label_pos);
        return Ok(Some((entry.path().to_path_buf(), insertion)));
    }
    Ok(None)
}

fn paragraph_end(source: &str, position: usize) -> usize {
    if let Some(relative) = source[position..].find("\n\n") {
        position + relative + 2
    } else if let Some(relative) = source[position..].find('\n') {
        position + relative + 1
    } else {
        source.len()
    }
}

fn inline_deleted_figure_box(
    label: &str,
    definition: Option<&FigureDefinition>,
    anchor: Option<&str>,
) -> String {
    let anchor_line = anchor
        .map(|a| format!("  #metadata(none) <{a}>\n"))
        .unwrap_or_default();
    let file = definition
        .and_then(|definition| definition.image_path.as_ref())
        .map(|path| format!(
            "  File: #raw(\"{}\")
",
            escape_typst_string(&normalize_path(path))
        ))
        .unwrap_or_default();
    let caption = definition
        .and_then(|definition| definition.caption.as_deref())
        .map(|caption| {
            format!(
                "  Previous caption:
{}",
                deleted_caption_markup(caption)
            )
        })
        .unwrap_or_default();
    format!(
        r#"
#block(
  width: 100%, inset: 7pt,
  stroke: 1pt + rgb("b3261e"), fill: rgb("fff4f2"),
  radius: 2pt, breakable: false,
)[
{}  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  Label: #raw("{}")
{}{}  #v(5pt)
  #text(weight: "bold")[
    This figure was present near this location in the previous version but has been removed from the revised document.
  ]
]
#v(5pt)

"#,
        anchor_line,
        escape_typst_string(label),
        file,
        caption,
    )
}

fn write_deleted_figure_notices(
    main_file: &Path,
    report_tree: &Path,
    notices_file: &Path,
    figures: &[Change],
    removed_references: &BTreeSet<PathBuf>,
    removed_definitions: &[FigureDefinition],
    removed_label_references: &BTreeSet<String>,
    placed_deleted: &[PlacedDeletedFigure],
    delfig_counter: &mut usize,
    mut summary_entries: Option<&mut Vec<SummaryEntry>>,
) -> Result<usize> {
    // A "deleted figure" can mean either that the binary asset was deleted or that
    // its image(...) reference disappeared while the asset remained in the repository.
    let mut deleted_paths: BTreeSet<PathBuf> = figures
        .iter()
        .filter(|figure| matches!(figure.kind, ChangeKind::Deleted))
        .map(|figure| figure.old_path.clone())
        .collect();
    deleted_paths.extend(removed_references.iter().cloned());
    for definition in removed_definitions {
        if let Some(path) = &definition.image_path {
            deleted_paths.insert(path.clone());
        }
    }

    if deleted_paths.is_empty()
        && removed_definitions.is_empty()
        && removed_label_references.is_empty()
    {
        return Ok(0);
    }

    let placed_labels_for_appendix: BTreeSet<&str> = placed_deleted
        .iter()
        .map(|placed| placed.label.as_str())
        .collect();
    let unplaced_definition_count = removed_definitions
        .iter()
        .filter(|definition| !placed_labels_for_appendix.contains(definition.label.as_str()))
        .count();
    let unplaced_reference_count = removed_label_references
        .iter()
        .filter(|label| {
            !placed_labels_for_appendix.contains(label.as_str())
                && !removed_definitions.iter().any(|definition| definition.label == **label)
        })
        .count();
    let definition_paths_all: BTreeSet<PathBuf> = removed_definitions
        .iter()
        .filter_map(|definition| definition.image_path.clone())
        .collect();
    let path_only_count = deleted_paths.difference(&definition_paths_all).count();
    if unplaced_definition_count == 0 && unplaced_reference_count == 0 && path_only_count == 0 {
        return Ok(placed_deleted.len());
    }

    let mut appendix = String::from(
        r#"

#pagebreak()
= Deleted Figures

#text(fill: rgb("666666"), size: 9pt)[
  The following figures were present in the previous version but have been removed from the revised document.
]

"#,
    );

    let definition_paths: BTreeSet<PathBuf> = removed_definitions
        .iter()
        .filter_map(|definition| definition.image_path.clone())
        .collect();

    for definition in removed_definitions {
        if placed_deleted.iter().any(|placed| placed.label == definition.label) {
            continue;
        }
        let anchor_line = if let Some(entries) = summary_entries.as_deref_mut() {
            let anchor = format!("typdiff:delfig-{}", *delfig_counter);
            *delfig_counter += 1;
            let excerpt = definition
                .caption
                .as_deref()
                .map(clean_caption_excerpt)
                .filter(|s| !s.is_empty())
                .or_else(|| definition.image_path.as_ref().map(|p| normalize_path(p)))
                .unwrap_or_else(|| format!("<{}>", definition.label));
            entries.push(SummaryEntry {
                anchor: anchor.clone(),
                file: definition.source_file.clone(),
                kind: "Deleted".to_string(),
                block_label: "Figure".to_string(),
                figure_label: Some(definition.label.clone()),
                context_excerpt: None,
                old_excerpt: Some(excerpt),
                new_excerpt: None,
            });
            format!("  #metadata(none) <{anchor}>\n")
        } else {
            String::new()
        };
        let file_line = definition
            .image_path
            .as_ref()
            .map(|path| format!("  File: #raw(\"{}\")\n", escape_typst_string(&normalize_path(path))))
            .unwrap_or_default();
        let caption_line = definition
            .caption
            .as_deref()
            .map(|caption| format!("  Previous caption:\n{}", deleted_caption_markup(caption)))
            .unwrap_or_default();
        appendix.push_str(&format!(
            r#"#block(
  width: 100%, inset: 7pt, stroke: 1pt + rgb("b3261e"),
  fill: rgb("fff4f2"), radius: 2pt, breakable: false,
)[
{}  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  Label: #raw("{}")
{}{}  Source: #raw("{}")
  #v(5pt)
  #text(weight: "bold")[
    This figure definition was present in the previous version but has been removed from the revised document.
  ]
]
#v(5pt)

"#,
            anchor_line,
            escape_typst_string(&definition.label),
            file_line,
            caption_line,
            escape_typst_string(&normalize_path(&definition.source_file)),
        ));
    }

    let definition_labels: BTreeSet<&str> = removed_definitions
        .iter()
        .map(|definition| definition.label.as_str())
        .collect();
    let placed_labels: BTreeSet<&str> = placed_deleted
        .iter()
        .map(|placed| placed.label.as_str())
        .collect();
    for label in removed_label_references {
        if placed_labels.contains(label.as_str()) {
            continue;
        }
        if definition_labels.contains(label.as_str()) {
            continue;
        }
        let anchor_line = if let Some(entries) = summary_entries.as_deref_mut() {
            let anchor = format!("typdiff:delfig-{}", *delfig_counter);
            *delfig_counter += 1;
            entries.push(SummaryEntry {
                anchor: anchor.clone(),
                file: notices_file.to_path_buf(),
                kind: "Deleted".to_string(),
                block_label: "Figure".to_string(),
                figure_label: Some(label.clone()),
                context_excerpt: None,
                old_excerpt: Some(format!("<{label}>")),
                new_excerpt: None,
            });
            format!("  #metadata(none) <{anchor}>\n")
        } else {
            String::new()
        };
        appendix.push_str(&format!(
            r#"#block(
  width: 100%, inset: 7pt, stroke: 1pt + rgb("b3261e"),
  fill: rgb("fff4f2"), radius: 2pt, breakable: false,
)[
{}  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  Label: #raw("{}")
  The figure reference and its corresponding figure were present in the previous version but have been removed from the revised document.
]
#v(5pt)

"#,
            anchor_line,
            escape_typst_string(label)
        ));
    }

    for path in deleted_paths.difference(&definition_paths) {
        let path = normalize_path(path);
        let anchor_line = if let Some(entries) = summary_entries.as_deref_mut() {
            let anchor = format!("typdiff:delfig-{}", *delfig_counter);
            *delfig_counter += 1;
            entries.push(SummaryEntry {
                anchor: anchor.clone(),
                file: notices_file.to_path_buf(),
                kind: "Deleted".to_string(),
                block_label: "Figure".to_string(),
                figure_label: None,
                context_excerpt: None,
                old_excerpt: Some(path.clone()),
                new_excerpt: None,
            });
            format!("  #metadata(none) <{anchor}>\n")
        } else {
            String::new()
        };
        appendix.push_str(&format!(
            r#"#block(
  width: 100%, inset: 7pt, stroke: 1pt + rgb("b3261e"),
  fill: rgb("fff4f2"), radius: 2pt, breakable: false,
)[
{}  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  The figure #raw("{}") was present in the previous version but has been removed from the revised document.
]
#v(5pt)

"#,
            anchor_line,
            escape_typst_string(&path)
        ));
    }

    let notices_path = report_tree.join(notices_file);
    if let Some(parent) = notices_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&notices_path, &appendix)
        .with_context(|| format!("writing {}", notices_path.display()))?;

    // Include the generated notices explicitly from the compilation entry point.
    // Keeping them in a separate file makes debugging straightforward and avoids
    // ambiguity about whether appending content after the user's main document is
    // affected by a surrounding show/template construct.
    let include_path = if notices_file.is_absolute() {
        normalize_path(notices_file)
    } else {
        format!("/{}", normalize_path(notices_file))
    };
    let mut main = fs::read_to_string(main_file)
        .with_context(|| format!("reading {}", main_file.display()))?;
    main.push_str(&format!("\n\n#include \"{}\"\n", include_path));
    fs::write(main_file, main)
        .with_context(|| format!("including deleted-figure notices from {}", main_file.display()))?;
    eprintln!("deleted-figure notice source: {}", notices_path.display());

    let reference_only_count = removed_label_references
        .iter()
        .filter(|label| {
            !placed_labels.contains(label.as_str())
                && !definition_labels.contains(label.as_str())
        })
        .count();
    let unplaced_definition_count = removed_definitions
        .iter()
        .filter(|definition| !placed_labels.contains(definition.label.as_str()))
        .count();
    Ok(
        placed_deleted.len()
            + unplaced_definition_count
            + reference_only_count
            + deleted_paths.difference(&definition_paths).count(),
    )
}

fn escape_typst_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn repository_relative_path(repository_root: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    normalize_lexically(&absolute)
        .strip_prefix(normalize_lexically(repository_root))
        .ok()
        .map(Path::to_path_buf)
}

fn change_is_below(change: &Change, prefix: Option<&Path>) -> bool {
    let Some(prefix) = prefix else {
        return false;
    };
    change.old_path.starts_with(prefix) || change.new_path.starts_with(prefix)
}

fn rewrite_modified_figures_by_label(
    report_tree: &Path,
    modified_assets: &HashMap<(String, usize), PathBuf>,
) -> Result<()> {
    if modified_assets.is_empty() {
        return Ok(());
    }

    for entry in walkdir::WalkDir::new(report_tree) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
            || entry.path().starts_with(report_tree.join(".typdiff-figures"))
        {
            continue;
        }
        let original = fs::read_to_string(entry.path())?;
        let ranges = find_figure_definition_ranges(&original);
        let mut edits = Vec::new();
        for (start, end, label) in ranges {
            for ((asset_label, asset_index), replacement) in modified_assets {
                if asset_label == &label {
                    edits.push((
                        start,
                        end,
                        label.clone(),
                        *asset_index,
                        normalize_path(replacement),
                    ));
                }
            }
        }
        if edits.is_empty() {
            continue;
        }

        let mut updated = original;
        // For one figure with multiple subfig assets, apply higher indices first so
        // replacing an earlier string does not invalidate later byte ranges.
        edits.sort_by_key(|(start, _, _, asset_index, _)| (*start, *asset_index));
        for (start, end, label, asset_index, replacement) in edits.into_iter().rev() {
            let figure_source = &updated[start..end];
            let rewritten = replace_nth_figure_asset(
                figure_source,
                asset_index,
                &replacement,
            );
            if rewritten == figure_source {
                eprintln!(
                    "warning: found modified figure <{}>[{}] but could not replace its asset",
                    label,
                    asset_index
                );
                continue;
            }
            updated.replace_range(start..end, &rewritten);
            eprintln!(
                "rewrote modified figure by label <{}>[{}] in {}",
                label,
                asset_index,
                entry.path().display()
            );
        }
        fs::write(entry.path(), updated)?;
    }
    Ok(())
}

fn replace_nth_figure_asset(
    figure_source: &str,
    target_index: usize,
    replacement: &str,
) -> String {
    let mut figure_index = 0usize;
    for (start, end, value) in extract_string_literal_ranges(figure_source) {
        if !is_figure_path(&value) {
            continue;
        }
        if figure_index == target_index {
            let mut result = figure_source.to_string();
            result.replace_range(start..end, replacement);
            return result;
        }
        figure_index += 1;
    }
    figure_source.to_string()
}

fn extract_string_literal_ranges(source: &str) -> Vec<(usize, usize, String)> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'"' && quote != b'\'' {
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        let mut escaped = false;
        while index < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == quote {
                if let Ok(value) = std::str::from_utf8(&bytes[start..index]) {
                    result.push((start, index, value.to_string()));
                }
                index += 1;
                break;
            }
            index += 1;
        }
    }
    result
}

fn add_same_path_content_comparisons(
    old_tree: &Path,
    new_tree: &Path,
    report_tree: &Path,
    workspace: &Path,
    image_functions: &[String],
    replacements: &mut HashMap<PathBuf, FigureReplacement>,
    modified_assets: &mut HashMap<(String, usize), PathBuf>,
) -> Result<()> {
    let old_references = collect_figure_references(old_tree, image_functions)?;
    let new_references = collect_figure_references(new_tree, image_functions)?;
    let new_assets_by_label = collect_figure_assets(new_tree, image_functions)?;
    let common: Vec<PathBuf> = old_references
        .intersection(&new_references)
        .cloned()
        .collect();

    eprintln!(
        "checking {} image/PDF path(s) referenced by both revisions",
        common.len()
    );

    let output_dir = report_tree.join(".typdiff-figures");
    fs::create_dir_all(&output_dir)?;
    let mut comparison_index = 0usize;

    for project_path in common {
        let old_asset = old_tree.join(&project_path);
        let new_asset = new_tree.join(&project_path);
        if !old_asset.is_file() || !new_asset.is_file() {
            continue;
        }

        let changed = files_differ(&old_asset, &new_asset)?;
        eprintln!(
            "same-path figure check: {} content_changed={}",
            project_path.display(),
            changed
        );
        if !changed {
            continue;
        }

        // If Git's changed-file path already produced a comparison, retain it. The
        // important part is that the replacement remains registered for source rewrite.
        if let Some(existing) = replacements.get(&project_path) {
            for (label, paths) in &new_assets_by_label {
                if paths.contains(&project_path) {
                    for (asset_index, path) in paths.iter().enumerate() {
                        if path == &project_path {
                            modified_assets.insert(
                                (label.clone(), asset_index),
                                existing.replacement.clone(),
                            );
                        }
                    }
                }
            }
            eprintln!(
                "same-path modified figure already registered: {}",
                project_path.display()
            );
            continue;
        }

        let work = workspace.join(format!("same-path-figure-{comparison_index:04}"));
        fs::create_dir_all(&work)?;
        let comparison_typ = work.join("comparison.typ");
        let comparison_pdf = output_dir.join(format!(
            "same-path-figure-{comparison_index:04}.pdf"
        ));
        fs::write(
            &comparison_typ,
            modified_figure_source(&old_asset, &new_asset),
        )?;
        tools::run(
            "typst",
            [
                OsStr::new("compile"),
                OsStr::new("--root"),
                OsStr::new("/"),
                comparison_typ.as_os_str(),
                comparison_pdf.as_os_str(),
            ],
        )?;

        let comparison_project_path = PathBuf::from(format!(
            "/.typdiff-figures/same-path-figure-{comparison_index:04}.pdf"
        ));
        replacements.insert(
            project_path.clone(),
            FigureReplacement {
                replacement: comparison_project_path.clone(),
                kind: ChangeKind::Modified,
            },
        );
        for (label, paths) in &new_assets_by_label {
            if paths.contains(&project_path) {
                for (asset_index, path) in paths.iter().enumerate() {
                    if path == &project_path {
                        modified_assets.insert(
                            (label.clone(), asset_index),
                            comparison_project_path.clone(),
                        );
                    }
                }
            }
        }
        eprintln!(
            "marked same-path modified figure: {}",
            project_path.display()
        );
        comparison_index += 1;
    }

    Ok(())
}

fn add_referenced_asset_comparisons(
    old_tree: &Path,
    new_tree: &Path,
    report_tree: &Path,
    workspace: &Path,
    image_functions: &[String],
    replacements: &mut HashMap<PathBuf, FigureReplacement>,
) -> Result<()> {
    let old_figures = collect_figure_assets(old_tree, image_functions)?;
    let new_figures = collect_figure_assets(new_tree, image_functions)?;
    let old_by_label: HashMap<&str, &Vec<PathBuf>> = old_figures
        .iter()
        .map(|(label, paths)| (label.as_str(), paths))
        .collect();

    let output_dir = report_tree.join(".typdiff-figures");
    fs::create_dir_all(&output_dir)?;
    let mut comparison_index = 0usize;

    for (label, new_paths) in &new_figures {
        let Some(old_paths) = old_by_label.get(label.as_str()).copied() else {
            continue;
        };

        // Match assets by their position inside the same figure definition. This also
        // supports multi-panel figures containing two or more image/PDF calls.
        let common = old_paths.len().min(new_paths.len());
        for asset_index in 0..common {
            let old_project_path = &old_paths[asset_index];
            let new_project_path = &new_paths[asset_index];
            let old_asset = old_tree.join(old_project_path);
            let new_asset = new_tree.join(new_project_path);

            if !old_asset.is_file() || !new_asset.is_file() {
                eprintln!(
                    "warning: cannot compare figure asset <{}>[{}]: old={} new={}",
                    label,
                    asset_index,
                    old_asset.display(),
                    new_asset.display()
                );
                continue;
            }

            let path_changed = old_project_path != new_project_path;
            let content_changed = files_differ(&old_asset, &new_asset)?;
            eprintln!(
                "figure asset check <{}>[{}]: old={} new={} path_changed={} content_changed={}",
                label,
                asset_index,
                old_project_path.display(),
                new_project_path.display(),
                path_changed,
                content_changed
            );
            if !path_changed && !content_changed {
                continue;
            }

            // A Git name-status change may already have created a comparison for this
            // newer path. Keep that comparison instead of producing a duplicate.
            if replacements.contains_key(new_project_path) {
                eprintln!(
                    "modified figure already registered <{}>[{}]: {}",
                    label,
                    asset_index,
                    new_project_path.display()
                );
                continue;
            }

            let work = workspace.join(format!(
                "referenced-figure-{comparison_index:04}"
            ));
            fs::create_dir_all(&work)?;
            let comparison_typ = work.join("comparison.typ");
            let comparison_pdf = output_dir.join(format!(
                "referenced-figure-{comparison_index:04}.pdf"
            ));
            fs::write(
                &comparison_typ,
                modified_figure_source(&old_asset, &new_asset),
            )?;
            tools::run(
                "typst",
                [
                    OsStr::new("compile"),
                    OsStr::new("--root"),
                    OsStr::new("/"),
                    comparison_typ.as_os_str(),
                    comparison_pdf.as_os_str(),
                ],
            )?;

            replacements.insert(
                new_project_path.clone(),
                FigureReplacement {
                    replacement: PathBuf::from(format!(
                        "/.typdiff-figures/referenced-figure-{comparison_index:04}.pdf"
                    )),
                    kind: ChangeKind::Modified,
                },
            );
            eprintln!(
                "marked modified figure <{}>[{}]: {} -> {}",
                label,
                asset_index,
                old_project_path.display(),
                new_project_path.display()
            );
            comparison_index += 1;
        }
    }

    Ok(())
}

fn collect_figure_assets(
    root: &Path,
    _image_functions: &[String],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut figures = HashMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
        {
            continue;
        }
        let source = fs::read_to_string(entry.path())?;
        let source_dir = entry.path().parent().unwrap_or(root);
        for (start, end, label) in find_figure_definition_ranges(&source) {
            let figure_source = &source[start..end];
            let mut paths = Vec::new();

            // Preserve lexical source order. BTreeSet-based extraction sorts paths and
            // breaks the correspondence between subfig a/b and their asset positions.
            for (_, _, raw_path) in extract_string_literal_ranges(figure_source) {
                if !is_figure_path(&raw_path) {
                    continue;
                }
                if let Some(path) = resolve_project_path(root, source_dir, &raw_path) {
                    paths.push(path);
                }
            }
            if !paths.is_empty() {
                figures.insert(label, paths);
            }
        }
    }
    Ok(figures)
}

fn files_differ(old: &Path, new: &Path) -> Result<bool> {
    let old_metadata = fs::metadata(old)
        .with_context(|| format!("reading metadata for {}", old.display()))?;
    let new_metadata = fs::metadata(new)
        .with_context(|| format!("reading metadata for {}", new.display()))?;
    if old_metadata.len() != new_metadata.len() {
        return Ok(true);
    }

    // Compare bytes rather than timestamps. Git archives and copied WORKTREE files can
    // have unrelated mtimes, while equal-size PDFs can still have different contents.
    let old_bytes = fs::read(old)
        .with_context(|| format!("reading figure asset {}", old.display()))?;
    let new_bytes = fs::read(new)
        .with_context(|| format!("reading figure asset {}", new.display()))?;
    Ok(old_bytes != new_bytes)
}

fn prepare_figure_comparisons(
    figures: &[Change],
    old_tree: &Path,
    new_tree: &Path,
    report_tree: &Path,
    temp: &Path,
) -> Result<HashMap<PathBuf, FigureReplacement>> {
    let output_dir = report_tree.join(".typdiff-figures");
    fs::create_dir_all(&output_dir)?;
    let mut replacements = HashMap::new();

    for (index, figure) in figures.iter().enumerate() {
        if matches!(figure.kind, ChangeKind::Deleted) {
            eprintln!("note: deleted figure omitted from newer full-document report: {}", figure.old_path.display());
            continue;
        }
        let new_path = new_tree.join(&figure.new_path);
        if !new_path.is_file() { continue; }

        let work = temp.join(format!("figure-{index:04}"));
        fs::create_dir_all(&work)?;
        let comparison_typ = work.join("comparison.typ");
        let comparison_pdf = output_dir.join(format!("figure-{index:04}.pdf"));
        let source = match figure.kind {
            ChangeKind::Added => added_figure_source(&new_path),
            ChangeKind::Modified | ChangeKind::Renamed => {
                let old_path = old_tree.join(&figure.old_path);
                if old_path.is_file() {
                    modified_figure_source(&old_path, &new_path)
                } else {
                    added_figure_source(&new_path)
                }
            }
            ChangeKind::Deleted => unreachable!(),
        };
        fs::write(&comparison_typ, source)?;
        tools::run(
            "typst",
            [
                OsStr::new("compile"),
                OsStr::new("--root"),
                OsStr::new("/"),
                comparison_typ.as_os_str(),
                comparison_pdf.as_os_str(),
            ],
        )?;
        replacements.insert(
            figure.new_path.clone(),
            FigureReplacement {
                replacement: PathBuf::from(format!("/.typdiff-figures/figure-{index:04}.pdf")),
                kind: figure.kind,
            },
        );
    }
    Ok(replacements)
}

fn modified_figure_source(old: &Path, new: &Path) -> String {
    format!(r#"#set page(width: 190mm, height: auto, margin: 8mm)
#set text(size: 9pt)
#block(width: 100%, inset: 5pt, stroke: 1pt + rgb("b3261e"), radius: 2pt)[
  #text(fill: rgb("b3261e"), weight: "bold")[Previous version]
  #v(4pt)
  #align(center)[#image("{}", width: 100%)]
]
#v(5mm)
#block(width: 100%, inset: 5pt, stroke: 1pt + rgb("0067c0"), radius: 2pt)[
  #text(fill: rgb("0067c0"), weight: "bold")[Revised version]
  #v(4pt)
  #align(center)[#image("{}", width: 100%)]
]
"#, typst_string(old), typst_string(new))
}

fn added_figure_source(new: &Path) -> String {
    format!(r#"#set page(width: 190mm, height: auto, margin: 8mm)
#set text(size: 9pt)
#block(width: 100%, inset: 5pt, stroke: 1pt + rgb("0067c0"), radius: 2pt)[
  #text(fill: rgb("0067c0"), weight: "bold")[Added figure]
  #v(4pt)
  #align(center)[#image("{}", width: 100%)]
]
"#, typst_string(new))
}

fn rewrite_image_references(
    root: &Path,
    replacements: &HashMap<PathBuf, FigureReplacement>,
    image_functions: &[String],
) -> Result<()> {
    if replacements.is_empty() { return Ok(()); }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.path().extension() != Some(OsStr::new("typ")) { continue; }
        if entry.path().starts_with(root.join(".typdiff-figures")) { continue; }
        let original = fs::read_to_string(entry.path())?;
        let directory = entry.path().parent().unwrap_or(root);
        let mut updated = original.clone();
        for (project_path, replacement) in replacements {
            let replacement_path = normalize_path(&replacement.replacement);
            let absolute_spelling = format!("/{}", normalize_path(project_path));
            if matches!(replacement.kind, ChangeKind::Added) {
                updated = mark_added_caption(&updated, &absolute_spelling, image_functions);
            }
            updated = replace_image_path(
                &updated,
                &absolute_spelling,
                &replacement_path,
                image_functions,
            );
            if let Ok(relative_project) = directory.strip_prefix(root) {
                if let Some(relative_spelling) = relative_path(relative_project, project_path) {
                    if matches!(replacement.kind, ChangeKind::Added) {
                        updated = mark_added_caption(&updated, &relative_spelling, image_functions);
                    }
                    updated = replace_image_path(
                        &updated,
                        &relative_spelling,
                        &replacement_path,
                        image_functions,
                    );
                }
            }
        }
        if updated != original {
            eprintln!("rewrote figure reference(s) in {}", entry.path().display());
            fs::write(entry.path(), updated)?;
        }
    }
    Ok(())
}

fn mark_added_caption(
    source: &str,
    image_path: &str,
    image_functions: &[String],
) -> String {
    let mut edits = Vec::new();

    for function in image_functions {
        let mut search_from = 0usize;
        while let Some((image_start, path_start, path_end)) =
            find_image_call(source, function, search_from)
        {
            if &source[path_start..path_end] != image_path {
                search_from = path_end + 1;
                continue;
            }
            if let Some((content_start, content_end)) = find_caption_block(source, image_start) {
                edits.push((content_start, content_end));
            }
            search_from = path_end + 1;
        }
    }

    edits.sort_unstable();
    edits.dedup();
    let mut result = source.to_string();
    for (start, end) in edits.into_iter().rev() {
        result.insert_str(end, "]]" );
        result.insert_str(
            start,
            "#text(fill: rgb(\"0067c0\"))[#underline[",
        );
    }
    result
}

fn find_caption_block(source: &str, image_start: usize) -> Option<(usize, usize)> {
    let figure_start = source[..image_start].rfind("#figure(")?;
    let open_paren = figure_start + "#figure".len();
    let figure_end = find_matching_delimiter(source, open_paren, '(', ')')?;
    if image_start >= figure_end {
        return None;
    }

    let caption_relative = source[image_start..figure_end].find("caption:")?;
    let caption_start = image_start + caption_relative + "caption:".len();
    let block_relative = source[caption_start..figure_end].find('[')?;
    let open_bracket = caption_start + block_relative;
    let close_bracket = find_matching_delimiter(source, open_bracket, '[', ']')?;
    Some((open_bracket + 1, close_bracket))
}

fn find_matching_delimiter(
    source: &str,
    open_byte: usize,
    open: char,
    close: char,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in source[open_byte..].char_indices() {
        let index = open_byte + offset;
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
        } else if character == open {
            depth += 1;
        } else if character == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn replace_image_path(
    source: &str,
    old_path: &str,
    new_path: &str,
    image_functions: &[String],
) -> String {
    let mut edits = Vec::new();
    for function in image_functions {
        let mut offset = 0usize;
        while let Some((_, path_start, path_end)) = find_image_call(source, function, offset) {
            if &source[path_start..path_end] == old_path {
                edits.push((path_start, path_end));
            }
            offset = path_end + 1;
        }
    }
    edits.sort_unstable();
    edits.dedup();
    let mut result = source.to_string();
    for (start, end) in edits.into_iter().rev() {
        result.replace_range(start..end, new_path);
    }
    // Fallback for custom wrappers or named path arguments. Exact quoted literals are
    // safe to replace and avoid depending on the wrapper's argument layout.
    result = result.replace(
        &format!("\"{}\"", old_path),
        &format!("\"{}\"", new_path),
    );
    result = result.replace(
        &format!("'{}'", old_path),
        &format!("'{}'", new_path),
    );
    result
}

fn normalized_image_functions(extra: &[String]) -> Result<Vec<String>> {
    let mut functions = vec!["image".to_string()];
    for function in extra {
        let function = function.trim().trim_start_matches('#');
        if function.is_empty()
            || !function
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            bail!("invalid --image-function name: {function}");
        }
        if !functions.iter().any(|existing| existing == function) {
            functions.push(function.to_string());
        }
    }
    Ok(functions)
}

fn relative_path(from_dir: &Path, target: &Path) -> Option<String> {
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = target.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts = vec!["..".to_string(); from.len().saturating_sub(common)];
    parts.extend(to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    Some(if parts.is_empty() { ".".into() } else { parts.join("/") })
}

fn normalize_path(path: &Path) -> String { path.to_string_lossy().replace('\\', "/") }
fn typst_string(path: &Path) -> String { normalize_path(path).replace('"', "\\\"") }

fn run_typdiff(
    old: &Path,
    new: &Path,
    target: &Path,
    citation_keys: &BTreeSet<String>,
    summary_prefix: Option<&str>,
) -> Result<Vec<typdiff::render::SummaryAnchor>> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read both inputs before replacing target. The newer input may be the same
    // report-tree file as target after figure-reference rewriting.
    let old_source = fs::read_to_string(old)
        .with_context(|| format!("reading {}", old.display()))?;
    let new_source = fs::read_to_string(new)
        .with_context(|| format!("reading {}", new.display()))?;
    let (old_source, new_source) =
        neutralize_one_sided_references(&old_source, &new_source, citation_keys);

    let old_blocks: Vec<_> = typdiff::parse::parse(&old_source)
        .into_iter()
        .filter(|b| !matches!(b, typdiff::Block::Parbreak))
        .collect();
    let new_blocks: Vec<_> = typdiff::parse::parse(&new_source)
        .into_iter()
        .filter(|b| !matches!(b, typdiff::Block::Parbreak))
        .collect();

    let diff_results = typdiff::diff::diff(&old_blocks, &new_blocks);
    let (diff_output, anchors) = match summary_prefix {
        Some(prefix) => typdiff::render::render_with_citations_and_summary(
            &diff_results,
            citation_keys,
            prefix,
        ),
        None => (
            typdiff::render::render_with_citations(&diff_results, citation_keys),
            Vec::new(),
        ),
    };

    let sanitized_output = repair_typdiff_brackets(&diff_output);
    let sanitized_output = repair_typdiff_labels(&sanitized_output);

    fs::write(target, sanitized_output)
        .with_context(|| format!("writing visual diff to {}", target.display()))?;
    Ok(anchors)
}

fn append_summary_anchors(
    entries: &mut Vec<SummaryEntry>,
    file: &Path,
    anchors: Vec<typdiff::render::SummaryAnchor>,
) {
    for anchor in anchors {
        let kind = match anchor.kind {
            typdiff::render::SummaryChangeKind::Modified => "Modified",
            typdiff::render::SummaryChangeKind::Added => "Added",
            typdiff::render::SummaryChangeKind::Deleted => "Deleted",
        };
        entries.push(SummaryEntry {
            anchor: anchor.label,
            file: file.to_path_buf(),
            kind: kind.to_string(),
            block_label: anchor.block_label.to_string(),
            figure_label: anchor.figure_label,
            context_excerpt: anchor.context_excerpt,
            old_excerpt: anchor.old_excerpt,
            new_excerpt: anchor.new_excerpt,
        });
    }
}

fn clean_caption_excerpt(caption: &str) -> String {
    let mut cur = caption.trim();
    loop {
        if let Some(rest) = cur.strip_prefix("short-cap") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                if let Some(open) = rest.find('[') {
                    if let Some(close) = find_matching_delimiter(rest, open, '[', ']') {
                        cur = rest[open + 1..close].trim();
                        continue;
                    }
                }
            }
        }
        let stripped = cur.strip_prefix('#').unwrap_or(cur);
        if let Some(after_name) = stripped
            .strip_prefix("text")
            .or_else(|| stripped.strip_prefix("underline"))
            .or_else(|| stripped.strip_prefix("emph"))
            .or_else(|| stripped.strip_prefix("strong"))
        {
            let mut rest = after_name.trim_start();
            if rest.starts_with('(') {
                if let Some(close_p) = find_matching_delimiter(rest, 0, '(', ')') {
                    rest = rest[close_p + 1..].trim_start();
                }
            }
            if rest.starts_with('[') {
                if let Some(close_b) = find_matching_delimiter(rest, 0, '[', ']') {
                    if close_b + 1 == rest.len() {
                        cur = rest[1..close_b].trim();
                        continue;
                    }
                }
            }
        }
        break;
    }
    let collapsed = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_chars = 90usize;
    if collapsed.chars().count() <= max_chars {
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

fn collect_summary_figure_entries(
    report_tree: &Path,
    added_figure_labels: &BTreeSet<String>,
    typdiff_processed_files: &BTreeSet<PathBuf>,
    notices_file: &Path,
    summary_file: &Path,
    summary_entries: &mut Vec<SummaryEntry>,
) -> Result<()> {
    let notices_path = report_tree.join(notices_file);
    let summary_path = report_tree.join(summary_file);
    let mut extra_counter = 0usize;

    let mut typ_files = Vec::new();
    for entry in walkdir::WalkDir::new(report_tree) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension() != Some(OsStr::new("typ"))
            || entry.path().starts_with(report_tree.join(".typdiff-figures"))
            || entry.path() == notices_path
            || entry.path() == summary_path
        {
            continue;
        }
        typ_files.push(entry.path().to_path_buf());
    }
    typ_files.sort();

    for path in typ_files {
        let rel_path = path.strip_prefix(report_tree)?.to_path_buf();
        let source = fs::read_to_string(&path)?;
        let ranges = find_figure_definition_ranges_inner(&source, true);
        let mut covered_ranges: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());

        for (start, end, label) in ranges {
            covered_ranges.push((start, end));
            let block = &source[start..end];
            let is_added = added_figure_labels.contains(&label);
            let has_rewritten_fig = block.contains("/.typdiff-figures/");
            if !is_added && !has_rewritten_fig {
                continue;
            }
            let base_label = if label.starts_with("tab:") || block.contains("table(") {
                "Table"
            } else {
                "Figure"
            };
            let caption_ex = extract_caption_content(block)
                .map(|c| clean_caption_excerpt(&c))
                .filter(|s| !s.is_empty());

            if let Some(existing) = summary_entries
                .iter_mut()
                .find(|e| e.figure_label.as_deref() == Some(label.as_str()))
            {
                if has_rewritten_fig && !is_added && existing.old_excerpt.is_some() {
                    existing.block_label = format!("{base_label} (image + text)");
                } else {
                    existing.block_label = base_label.to_string();
                }
                if existing.new_excerpt.is_none() {
                    existing.new_excerpt = caption_ex.or_else(|| Some(format!("<{label}>")));
                }
                existing.anchor = label.clone();
                continue;
            }

            summary_entries.push(SummaryEntry {
                anchor: label.clone(),
                file: rel_path.clone(),
                kind: if is_added { "Added" } else { "Modified" }.to_string(),
                block_label: base_label.to_string(),
                figure_label: Some(label.clone()),
                context_excerpt: None,
                old_excerpt: None,
                new_excerpt: caption_ex.or_else(|| Some(format!("<{label}>"))),
            });
        }

        // Unlabeled image changes inside files already processed by run_typdiff are
        // already anchored there; only scan outside-figure assets in untouched files.
        if typdiff_processed_files.contains(&rel_path) {
            continue;
        }

        // Also detect rewritten /.typdiff-figures/... calls outside labeled #figure(...) blocks.
        let mut extra_insertions: Vec<(usize, String)> = Vec::new();
        let mut unanchored_extras: Vec<String> = Vec::new();
        for (lit_start, lit_end, literal) in extract_string_literal_ranges(&source) {
            if !literal.contains("/.typdiff-figures/") {
                continue;
            }
            if is_inside_line_comment(&source, lit_start) {
                continue;
            }
            if covered_ranges
                .iter()
                .any(|&(s, e)| lit_start >= s && lit_end <= e)
            {
                continue;
            }
            let file_name = Path::new(&literal)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&literal)
                .to_string();
            if let Some(hash_pos) = find_enclosing_markup_hash(&source, lit_start) {
                let anchor = format!("typdiff:fig-extra-{extra_counter}");
                extra_counter += 1;
                extra_insertions.push((hash_pos, format!("#metadata(none) <{anchor}>")));
                summary_entries.push(SummaryEntry {
                    anchor,
                    file: rel_path.clone(),
                    kind: "Modified".to_string(),
                    block_label: "Figure".to_string(),
                    figure_label: None,
                    context_excerpt: None,
                    old_excerpt: None,
                    new_excerpt: Some(file_name),
                });
            } else {
                unanchored_extras.push(file_name);
            }
        }
        if !extra_insertions.is_empty() {
            extra_insertions.sort_by_key(|(pos, _)| *pos);
            extra_insertions.dedup_by_key(|(pos, _)| *pos);
            let mut updated = source;
            for (pos, marker) in extra_insertions.into_iter().rev() {
                updated.insert_str(pos, &marker);
            }
            fs::write(&path, updated)?;
        }
        for file_name in unanchored_extras {
            summary_entries.push(SummaryEntry {
                anchor: String::new(),
                file: rel_path.clone(),
                kind: "Modified".to_string(),
                block_label: "Figure".to_string(),
                figure_label: None,
                context_excerpt: None,
                old_excerpt: None,
                new_excerpt: Some(file_name),
            });
        }
    }

    Ok(())
}

fn find_enclosing_markup_hash(source: &str, target_byte: usize) -> Option<usize> {
    fn walk(
        node: &typst_syntax::SyntaxNode,
        offset: usize,
        in_markup: bool,
        target: usize,
    ) -> Option<usize> {
        use typst_syntax::SyntaxKind;
        let children: Vec<_> = node.children().collect();
        if in_markup {
            let mut child_offset = offset;
            let mut i = 0usize;
            while i < children.len() {
                let child = children[i];
                if child.kind() == SyntaxKind::Hash && i + 1 < children.len() {
                    let next = children[i + 1];
                    let expr_start = child_offset + child.len();
                    let expr_end = expr_start + next.len();
                    if target >= expr_start && target < expr_end {
                        return Some(child_offset);
                    }
                }
                if target >= child_offset && target < child_offset + child.len() {
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
                    return walk(child, child_offset, child_markup, target);
                }
                child_offset += child.len();
                i += 1;
            }
            None
        } else {
            let mut child_offset = offset;
            for child in children {
                if target >= child_offset && target < child_offset + child.len() {
                    let child_markup = child.kind() == SyntaxKind::ContentBlock;
                    return walk(child, child_offset, child_markup, target);
                }
                child_offset += child.len();
            }
            None
        }
    }

    let root = typst_syntax::parse(source);
    walk(&root, 0, true, target_byte)
}

fn write_summary_page(
    main_file: &Path,
    report_tree: &Path,
    summary_file: &Path,
    old_rev: &str,
    new_rev: &str,
    entries: &[SummaryEntry],
) -> Result<()> {
    let mut entries_typ = String::new();
    for entry in entries {
        let file = escape_typst_string(&normalize_path(&entry.file));
        let kind = escape_typst_string(&entry.kind);
        let block = escape_typst_string(&entry.block_label);
        let anchor = escape_typst_string(&entry.anchor);
        let ctx = escape_typst_string(entry.context_excerpt.as_deref().unwrap_or(""));
        let old_ex = escape_typst_string(entry.old_excerpt.as_deref().unwrap_or(""));
        let new_ex = escape_typst_string(entry.new_excerpt.as_deref().unwrap_or(""));
        entries_typ.push_str(&format!(
            "    (anchor: \"{anchor}\", file: \"{file}\", kind: \"{kind}\", block: \"{block}\", ctx: \"{ctx}\", old: \"{old_ex}\", new: \"{new_ex}\"),\n"
        ));
    }

    let summary_source = format!(
        r##"#page(
  margin: (x: 16mm, y: 16mm),
  numbering: none,
  header: none,
  footer: none,
)[
  #set par(first-line-indent: 0pt, justify: false, spacing: 0.45em, leading: 0.4em)
  #set text(size: 9pt)
  #let typdiff-format-page(loc) = {{
    let num = counter(page).at(loc)
    let pat = loc.page-numbering()
    if pat == none {{
      [front #numbering("1", ..num)]
    }} else {{
      numbering(pat, ..num)
    }}
  }}
  #let typdiff-plain-text(body) = {{
    if body == none {{ "" }}
    else if type(body) == str {{ body }}
    else if body.func() == footnote or body.func() == metadata {{ "" }}
    else if body.has("text") {{ body.text }}
    else if body.func() == [ ].func() or body.func() == [~].func() {{ " " }}
    else if body.has("children") {{ body.children.map(typdiff-plain-text).join("") }}
    else if body.has("body") {{ typdiff-plain-text(body.body) }}
    else if body.has("child") {{ typdiff-plain-text(body.child) }}
    else {{ "" }}
  }}
  #let typdiff-section-of(loc) = {{
    let hs = query(selector(heading).before(loc))
    if hs.len() > 0 {{
      let txt = typdiff-plain-text(hs.last().body).trim()
      let parts = txt.clusters()
      if parts.len() > 60 {{
        parts.slice(0, 60).join("") + "…"
      }} else {{
        txt
      }}
    }} else {{
      ""
    }}
  }}

  #text(size: 16pt, weight: "bold")[Revision Summary]
  #v(2pt)
  #text(size: 8.5pt, fill: rgb("#555555"))[
    Comparing #raw("{}") → #raw("{}")
    #h(1em) | #h(1em)
    {} change(s)
  ]
  #v(6pt)

  #context {{
    let entries = (
{}    )
    if entries.len() == 0 {{
      text(fill: rgb("#555555"))[No differences detected between the two revisions.]
    }} else {{
      let resolved = ()
      for (seq, e) in entries.enumerate() {{
        let hits = if e.anchor != "" {{ query(label(e.anchor)) }} else {{ () }}
        if hits.len() > 0 {{
          let loc = hits.first().location()
          resolved.push((entry: e, loc: loc, page: loc.page(), y: loc.position().y.pt(), seq: seq))
        }} else {{
          resolved.push((entry: e, loc: none, page: 999999, y: 0.0, seq: seq))
        }}
      }}
      let sorted = resolved.sorted(key: it => (it.page, it.y, it.seq))
      table(
        columns: (auto, auto, 1fr, auto),
        stroke: 0.4pt + rgb("#dddddd"),
        inset: (x: 5pt, y: 4pt),
        align: (right, left, left, right),
        fill: (col, row) => if row == 0 {{ rgb("#f3f4f6") }} else {{ none }},
        table.header(
          [*\#*],
          [*Change*],
          [*Location & Excerpt*],
          [*Page*],
        ),
        ..sorted.enumerate().map(((idx, item)) => {{
          let e = item.entry
          let kind-color = if e.kind == "Added" {{
            rgb("#0067c0")
          }} else if e.kind == "Deleted" {{
            rgb("#b3261e")
          }} else {{
            rgb("#6b21a8")
          }}
          let sec = if item.loc != none {{ typdiff-section-of(item.loc) }} else {{ "" }}
          let loc-header = if sec != "" {{
            [#raw(e.file) #text(fill: rgb("#666666"))[· #sec]]
          }} else {{
            [#raw(e.file)]
          }}
          let excerpt-body = {{
            if e.ctx != "" {{
              text(fill: rgb("#666666"), e.ctx)
              [ ]
            }}
            if e.old != "" {{
              strike(text(fill: rgb("#cc0000"), e.old))
            }}
            if e.old != "" and e.new != "" {{
              [ → ]
            }}
            if e.new != "" {{
              underline(text(fill: rgb("#0000ff"), e.new))
            }}
          }}
          let page-cell = if item.loc != none {{
            link(item.loc)[#text(fill: rgb("#0067c0"), weight: "bold")[p. #typdiff-format-page(item.loc)]]
          }} else {{
            text(fill: rgb("#888888"))[—]
          }}
          (
            [#(idx + 1)],
            [
              #text(fill: kind-color, weight: "bold")[#e.kind]\
              #text(size: 8pt, fill: rgb("#555555"))[#e.block]
            ],
            [
              #text(size: 8pt, fill: rgb("#555555"))[#loc-header]\
              #excerpt-body
            ],
            page-cell,
          )
        }}).flatten()
      )
    }}
  }}
]
#counter(page).update(1)
"##,
        escape_typst_string(old_rev),
        escape_typst_string(new_rev),
        entries.len(),
        entries_typ,
    );

    let summary_path = report_tree.join(summary_file);
    if let Some(parent) = summary_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&summary_path, summary_source)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    let include_path = if summary_file.is_absolute() {
        normalize_path(summary_file)
    } else {
        format!("/{}", normalize_path(summary_file))
    };
    let main = fs::read_to_string(main_file)
        .with_context(|| format!("reading {}", main_file.display()))?;
    let include_stmt = format!("#include \"{}\"\n", include_path);
    let updated = if main.starts_with("#!") {
        if let Some(nl) = main.find('\n') {
            let mut s = String::with_capacity(main.len() + include_stmt.len());
            s.push_str(&main[..=nl]);
            s.push_str(&include_stmt);
            s.push_str(&main[nl + 1..]);
            s
        } else {
            format!("{main}\n{include_stmt}")
        }
    } else {
        format!("{include_stmt}{main}")
    };
    fs::write(main_file, updated)
        .with_context(|| format!("including summary page from {}", main_file.display()))?;
    eprintln!("summary page source: {}", summary_path.display());
    Ok(())
}
fn neutralize_one_sided_references(
    old: &str,
    new: &str,
    citation_keys: &BTreeSet<String>,
) -> (String, String) {
    let old_tokens = collect_reference_tokens(old, citation_keys);
    let new_tokens = collect_reference_tokens(new, citation_keys);
    let old_only: BTreeSet<String> = old_tokens.difference(&new_tokens).cloned().collect();

    // Only references that disappear from the document must be rendered literally.
    // New-only references must remain executable Typst so calls such as
    // #Fig(<fig:new>) expand to their resolved text (for example, "Fig. 1.1") inside
    // diff-added markup rather than appearing as raw source text.
    (replace_tokens_with_raw(old, &old_only), new.to_string())
}

fn collect_reference_tokens(
    source: &str,
    citation_keys: &BTreeSet<String>,
) -> BTreeSet<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = BTreeSet::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '@' {
            let start = index;
            index += 1;
            let key = if chars.get(index) == Some(&'<') {
                index += 1;
                let k_start = index;
                while index < chars.len() && chars[index] != '>' { index += 1; }
                let k = chars[k_start..index].iter().collect::<String>();
                if index < chars.len() { index += 1; }
                k
            } else {
                let k_start = index;
                while index < chars.len() && is_label_char(chars[index]) { index += 1; }
                while index > k_start && (chars[index - 1] == '.' || chars[index - 1] == ':') {
                    index -= 1;
                }
                chars[k_start..index].iter().collect::<String>()
            };
            if citation_keys.contains(&key) {
                // Known bibliography citations must remain executable Typst.
                continue;
            }
            if index > start + 1 {
                tokens.insert(chars[start..index].iter().collect());
            }
            continue;
        }
        if chars[index] == '#' && chars.get(index + 1).is_some_and(|c| c.is_uppercase()) {
            let start = index;
            let mut cursor = index + 1;
            while cursor < chars.len()
                && (chars[cursor].is_alphanumeric() || matches!(chars[cursor], '_' | '-'))
            { cursor += 1; }
            while cursor < chars.len() && chars[cursor].is_whitespace() { cursor += 1; }
            if chars.get(cursor) == Some(&'(') {
                if let Some(end) = find_char_call_end(&chars, cursor) {
                    let token: String = chars[start..end].iter().collect();
                    if token.contains('<') && token.contains('>') {
                        tokens.insert(token);
                        index = end;
                        continue;
                    }
                }
            }
        }
        index += 1;
    }
    tokens
}

fn collect_citation_keys_from_trees(trees: &[&Path]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for tree in trees {
        if !tree.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(tree).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("bib") {
                if let Ok(content) = fs::read_to_string(path) {
                    keys.extend(extract_bib_keys(&content));
                }
            } else if ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml") {
                if let Ok(content) = fs::read_to_string(path) {
                    keys.extend(extract_yaml_citation_keys(&content));
                }
            }
        }
    }
    keys
}

fn extract_bib_keys(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            i += 1;
            let type_start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            let entry_type = &content[type_start..i].to_ascii_lowercase();
            if entry_type == "comment"
                || entry_type == "preamble"
                || entry_type == "string"
                || entry_type.is_empty()
            {
                continue;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'{' || bytes[i] == b'(') {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                let key_start = i;
                while i < bytes.len()
                    && bytes[i] != b','
                    && bytes[i] != b'}'
                    && bytes[i] != b')'
                    && !bytes[i].is_ascii_whitespace()
                {
                    i += 1;
                }
                if i > key_start {
                    let key = content[key_start..i].trim();
                    if !key.is_empty() {
                        keys.insert(key.to_string());
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    keys
}

fn extract_yaml_citation_keys(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for line in content.lines() {
        if line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with('#')
            || line.trim().is_empty()
        {
            continue;
        }
        if let Some(colon) = line.find(':') {
            let key = line[..colon].trim();
            if !key.is_empty() && !key.contains(' ') {
                keys.insert(key.to_string());
            }
        }
    }
    keys
}

fn sync_old_bib_entries_to_report(old_tree: &Path, report_tree: &Path) -> Result<()> {
    if !old_tree.exists() || !report_tree.exists() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(old_tree).into_iter().filter_map(|e| e.ok()) {
        let old_path = entry.path();
        if !old_path.is_file() {
            continue;
        }
        let ext = old_path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !ext.eq_ignore_ascii_case("bib") {
            continue;
        }
        let rel = old_path.strip_prefix(old_tree)?;
        let report_path = report_tree.join(rel);
        if !report_path.exists() {
            if let Some(parent) = report_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(old_path, &report_path)?;
            continue;
        }
        let old_content = fs::read_to_string(old_path)?;
        let report_content = fs::read_to_string(&report_path)?;
        let report_keys = extract_bib_keys(&report_content);

        let mut to_append = String::new();
        let bytes = old_content.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@' {
                let entry_start = i;
                i += 1;
                let type_start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
                {
                    i += 1;
                }
                let entry_type = &old_content[type_start..i].to_ascii_lowercase();
                if entry_type == "comment"
                    || entry_type == "preamble"
                    || entry_type == "string"
                    || entry_type.is_empty()
                {
                    continue;
                }
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && (bytes[i] == b'{' || bytes[i] == b'(') {
                    let open_char = bytes[i];
                    let close_char = if open_char == b'{' { b'}' } else { b')' };
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    let key_start = i;
                    while i < bytes.len()
                        && bytes[i] != b','
                        && bytes[i] != close_char
                        && !bytes[i].is_ascii_whitespace()
                    {
                        i += 1;
                    }
                    let key = old_content[key_start..i].trim();
                    let mut depth = 1usize;
                    while i < bytes.len() && depth > 0 {
                        if bytes[i] == open_char {
                            depth += 1;
                        } else if bytes[i] == close_char {
                            depth -= 1;
                        }
                        i += 1;
                    }
                    if !key.is_empty() && !report_keys.contains(key) {
                        to_append.push('\n');
                        to_append.push_str(&old_content[entry_start..i]);
                        to_append.push('\n');
                    }
                }
            } else {
                i += 1;
            }
        }
        if !to_append.is_empty() {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&report_path)?;
            f.write_all(to_append.as_bytes())?;
        }
    }
    Ok(())
}

fn find_char_call_end(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in chars.iter().copied().enumerate().skip(open) {
        if in_string {
            if escaped { escaped = false; }
            else if character == '\\' { escaped = true; }
            else if character == '"' { in_string = false; }
            continue;
        }
        match character {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 { return Some(index + 1); }
            }
            _ => {}
        }
    }
    None
}

fn is_label_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')
}

fn replace_tokens_with_raw(source: &str, tokens: &BTreeSet<String>) -> String {
    let mut result = source.to_string();
    let mut ordered: Vec<&String> = tokens.iter().collect();
    ordered.sort_by_key(|token| std::cmp::Reverse(token.len()));
    for token in ordered {
        let fence = raw_fence(token);
        let wrapped = format!("{fence}{token}{fence}");
        let is_bare_ref = token.starts_with('@') && !token.starts_with("@<");
        let mut out = String::with_capacity(result.len());
        let mut cursor = 0usize;
        while let Some(rel) = result[cursor..].find(token.as_str()) {
            let start = cursor + rel;
            let end = start + token.len();
            let prev_char = result[..start].chars().next_back();
            let preceded_by_raw_or_escape = matches!(prev_char, Some('`') | Some('\\'));
            let extends_label = is_bare_ref && is_ref_continued_at(&result[end..]);
            if preceded_by_raw_or_escape || extends_label {
                out.push_str(&result[cursor..end]);
            } else {
                out.push_str(&result[cursor..start]);
                out.push_str(&wrapped);
            }
            cursor = end;
        }
        out.push_str(&result[cursor..]);
        result = out;
    }
    result
}

fn is_ref_continued_at(rest: &str) -> bool {
    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_alphanumeric() || matches!(first, '_' | '-') {
        return true;
    }
    if matches!(first, '.' | ':') {
        return chars.next().is_some_and(is_label_char);
    }
    false
}

fn raw_fence(value: &str) -> String {
    let mut length = 1usize;
    while value.contains(&"`".repeat(length)) { length += 1; }
    "`".repeat(length)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() { fs::create_dir_all(&target)?; }
        else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn absolute_output(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() { Ok(path.to_path_buf()) } else { Ok(std::env::current_dir()?.join(path)) }
}

fn repair_typdiff_brackets(source: &str) -> String {
    let unclosed_ranges = unclosed_diff_block_ranges(source);
    if unclosed_ranges.is_empty() {
        return source.to_string();
    }
    let mut result = Vec::new();
    let mut line_start = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let line_end = line_start + trimmed.len();
        let intersects_unclosed = unclosed_ranges
            .iter()
            .any(|&(start, end)| start <= line_end && line_start < end);
        if intersects_unclosed {
            result.push(fix_line_bracket_balance(trimmed));
        } else {
            result.push(trimmed.to_string());
        }
        line_start += line.len();
    }
    if source.is_empty() {
        return String::new();
    }
    let mut joined = result.join("\n");
    if source.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

fn unclosed_diff_block_ranges(source: &str) -> Vec<(usize, usize)> {
    fn walk(node: &typst_syntax::SyntaxNode, offset: usize, ranges: &mut Vec<(usize, usize)>) {
        let is_diff_call = node.kind() == typst_syntax::SyntaxKind::FuncCall
            && node.children().next().is_some_and(|callee| {
                callee.kind() == typst_syntax::SyntaxKind::Ident
                    && matches!(callee.text().as_str(), "diff-added" | "diff-deleted")
            });
        if is_diff_call {
            let has_unclosed = node.children().any(|child| {
                child.kind() == typst_syntax::SyntaxKind::Args
                    && child.children().any(|arg| {
                        arg.kind() == typst_syntax::SyntaxKind::ContentBlock
                            && !arg
                                .children()
                                .last()
                                .is_some_and(|last| last.kind() == typst_syntax::SyntaxKind::RightBracket)
                    })
            });
            if has_unclosed {
                ranges.push((offset, offset + node.len()));
            }
        }
        let mut child_offset = offset;
        for child in node.children() {
            walk(child, child_offset, ranges);
            child_offset += child.len();
        }
    }

    let root = typst_syntax::parse(source);
    let mut ranges = Vec::new();
    walk(&root, 0, &mut ranges);
    ranges
}

fn fix_line_bracket_balance(line: &str) -> String {
    let mut unescaped_open = 0usize;
    let mut unescaped_close = 0usize;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '[' {
            unescaped_open += 1;
        } else if c == ']' {
            unescaped_close += 1;
        }
    }

    if unescaped_open <= unescaped_close {
        return line.to_string();
    }

    let mut excess = unescaped_open - unescaped_close;
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut in_diff = false;
    let mut candidate_indices = Vec::new();

    while i < n {
        if chars[i] == '\\' && i + 1 < n {
            i += 2;
            continue;
        }

        let remaining: String = chars[i..].iter().take(15).collect();
        if remaining.starts_with("#diff-deleted[") || remaining.starts_with("#diff-added[") {
            in_diff = true;
            i += "#diff-deleted[".len();
            continue;
        }

        if in_diff {
            if chars[i] == '[' {
                let mut j = i.checked_sub(1);
                while let Some(idx) = j {
                    if chars[idx].is_whitespace() {
                        j = idx.checked_sub(1);
                    } else {
                        break;
                    }
                }
                let mut is_ident = false;
                if let Some(mut idx) = j {
                    while chars[idx].is_alphanumeric() || matches!(chars[idx], '_' | '-') {
                        if let Some(prev) = idx.checked_sub(1) {
                            idx = prev;
                        } else {
                            break;
                        }
                    }
                    if chars[idx] == '#' {
                        is_ident = true;
                    }
                }

                if !is_ident {
                    candidate_indices.push(i);
                }
            }
        }
        i += 1;
    }

    let mut to_escape = std::collections::BTreeSet::new();
    for &idx in candidate_indices.iter().rev() {
        if excess == 0 {
            break;
        }
        to_escape.insert(idx);
        excess -= 1;
    }

    let mut output = String::with_capacity(line.len() + to_escape.len());
    for (idx, &c) in chars.iter().enumerate() {
        if to_escape.contains(&idx) {
            output.push('\\');
        }
        output.push(c);
    }
    output
}

fn repair_typdiff_labels(source: &str) -> String {
    let blocks = collect_diff_block_close_offsets(source);

    let mut result = source.to_string();
    for close_bracket in blocks.into_iter().rev() {
        let after = &result[close_bracket + 1..];
        let chars: Vec<(usize, char)> = after.char_indices().collect();
        let mut idx = 0;
        let mut newlines = 0;
        while idx < chars.len() {
            let c = chars[idx].1;
            if c == ' ' || c == '\t' {
                idx += 1;
            } else if c == '\n' || c == '\r' {
                newlines += 1;
                if newlines > 1 {
                    break;
                }
                idx += 1;
            } else {
                break;
            }
        }
        if newlines > 1 || idx >= chars.len() {
            continue;
        }

        let mut labels = Vec::new();
        let mut last_label_end_in_after = 0;
        while idx < chars.len() && chars[idx].1 == '<' {
            let label_start = chars[idx].0;
            let mut cursor = idx + 1;
            while cursor < chars.len() && is_label_char(chars[cursor].1) {
                cursor += 1;
            }
            if cursor < chars.len() && chars[cursor].1 == '>' {
                let label_end = chars[cursor].0 + 1;
                labels.push(after[label_start..label_end].to_string());
                last_label_end_in_after = label_end;
                idx = cursor + 1;
                while idx < chars.len() && (chars[idx].1 == ' ' || chars[idx].1 == '\t') {
                    idx += 1;
                }
            } else {
                break;
            }
        }

        if !labels.is_empty() {
            let labels_str = labels.join("");
            let had_space_before_label = chars[0].1 == ' ' || chars[0].1 == '\t';
            let content_ends_with_space = result[..close_bracket].ends_with(char::is_whitespace);
            let to_insert = if !content_ends_with_space && had_space_before_label {
                format!(" {labels_str}")
            } else {
                labels_str
            };

            result.drain(close_bracket + 1..close_bracket + 1 + last_label_end_in_after);
            result.insert_str(close_bracket, &to_insert);
        }
    }

    result
}

fn collect_diff_block_close_offsets(source: &str) -> Vec<usize> {
    fn walk(node: &typst_syntax::SyntaxNode, offset: usize, closes: &mut Vec<usize>) {
        let is_diff_call = node.kind() == typst_syntax::SyntaxKind::FuncCall
            && node.children().next().is_some_and(|callee| {
                callee.kind() == typst_syntax::SyntaxKind::Ident
                    && matches!(callee.text().as_str(), "diff-added" | "diff-deleted")
            });
        let mut child_offset = offset;
        for child in node.children() {
            if is_diff_call && child.kind() == typst_syntax::SyntaxKind::Args {
                let mut arg_offset = child_offset;
                for arg in child.children() {
                    walk(arg, arg_offset, closes);
                    if arg.kind() == typst_syntax::SyntaxKind::ContentBlock {
                        let is_closed = arg.children().last().is_some_and(|last| {
                            last.kind() == typst_syntax::SyntaxKind::RightBracket
                        });
                        if is_closed {
                            closes.push(arg_offset + arg.len() - 1);
                        }
                    }
                    arg_offset += arg.len();
                }
            } else {
                walk(child, child_offset, closes);
            }
            child_offset += child.len();
        }
    }

    let root = typst_syntax::parse(source);
    let mut closes = Vec::new();
    walk(&root, 0, &mut closes);
    closes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repair_typdiff_brackets_unclosed() {
        let broken = "one #diff-deleted[[]two three#diff-deleted[\\]] four";
        let fixed = repair_typdiff_brackets(broken);
        assert_eq!(fixed, "one #diff-deleted[\\[]two three#diff-deleted[\\]] four");
    }

    #[test]
    fn test_repair_typdiff_brackets_balanced() {
        let ok = "one #diff-deleted[[two]] four";
        let fixed = repair_typdiff_brackets(ok);
        assert_eq!(fixed, ok);
    }

    #[test]
    fn test_repair_typdiff_brackets_preserves_multiline_raw_math_and_strings() {
        let multiline = "#diff-added[line one [a\nline two b]]";
        assert_eq!(repair_typdiff_brackets(multiline), multiline);

        let raw_and_math = "#diff-deleted[`[` and #text(\"[\") and $ [0, 1) $]";
        assert_eq!(repair_typdiff_brackets(raw_and_math), raw_and_math);
    }

    #[test]
    fn test_repair_typdiff_labels_with_quotes_escapes_and_raw() {
        let source = r#"#diff-added[5" display with \] and `[`]<fig:test>"#;
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, r#"#diff-added[5" display with \] and `[`<fig:test>]"#);
    }

    #[test]
    fn test_replace_tokens_with_raw_respects_ref_boundaries() {
        let mut citation_keys = BTreeSet::new();
        citation_keys.insert("cite2024".to_string());
        let old = "See @fig:1 and @fig:10 and @fig:1_extra and @cite2024.";
        let new = "See @fig:10 and @fig:1_extra.";
        let (neutralized, _) = neutralize_one_sided_references(old, new, &citation_keys);
        assert!(neutralized.contains("`@fig:1`"));
        assert!(neutralized.contains("@fig:10"));
        assert!(!neutralized.contains("`@fig:1`0"));
        assert!(neutralized.contains("@fig:1_extra"));
        assert!(!neutralized.contains("`@fig:1`_extra"));
    }

    #[test]
    fn test_repair_typdiff_labels_after_added() {
        let source = "#diff-deleted[#figure(old)]#diff-added[#figure(new)]<fig:test>";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, "#diff-deleted[#figure(old)]#diff-added[#figure(new)<fig:test>]");
    }

    #[test]
    fn test_repair_typdiff_labels_with_space() {
        let source = "#diff-added[#figure(new) ] <fig:test>";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, "#diff-added[#figure(new) <fig:test>]");
    }

    #[test]
    fn test_repair_typdiff_labels_deleted_only() {
        let source = "#diff-deleted[#figure(old)]<fig:old>";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, "#diff-deleted[#figure(old)<fig:old>]");
    }

    #[test]
    fn test_repair_typdiff_labels_nested_brackets() {
        let source = "#diff-added[#figure(caption: [Short [nested] title])[body]]<fig:nested>";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, "#diff-added[#figure(caption: [Short [nested] title])[body]<fig:nested>]");
    }

    #[test]
    fn test_repair_typdiff_labels_no_label() {
        let source = "#diff-added[hello world] next text";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, source);
    }

    #[test]
    fn test_repair_typdiff_labels_parbreak_ignored() {
        let source = "#diff-added[hello]\n\n<fig:detached>";
        let fixed = repair_typdiff_labels(source);
        assert_eq!(fixed, source);
    }

    #[test]
    fn test_extract_bib_keys() {
        let bib = r#"
@article{smirnov2015,
  title = {A test},
  author = {Smirnov, A.},
  year = 2015
}
@misc{Derber:1989,
  title = {Another test}
}
@comment{ignore_me, text = {comment}}
"#;
        let keys = extract_bib_keys(bib);
        assert!(keys.contains("smirnov2015"));
        assert!(keys.contains("Derber:1989"));
        assert!(!keys.contains("ignore_me"));
    }

    #[test]
    fn test_neutralize_one_sided_references_preserves_citations() {
        let mut citation_keys = BTreeSet::new();
        citation_keys.insert("smirnov2015".to_string());

        let old = "as shown by @smirnov2015 and @old_figure.";
        let new = "as shown by @new_figure.";

        let (neutralized_old, _) = neutralize_one_sided_references(old, new, &citation_keys);
        // @smirnov2015 should NOT be wrapped in backticks because it is in citation_keys
        assert!(neutralized_old.contains("@smirnov2015"));
        assert!(!neutralized_old.contains("`@smirnov2015`"));
        // @old_figure should be wrapped in backticks because it disappeared and is not a citation
        assert!(neutralized_old.contains("`@old_figure`"));
    }

    #[test]
    fn test_summary_page_cli_generates_compilable_pdf_with_logical_pages_and_figures() {
        use std::process::Command;

        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        };

        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);

        let svg_v1 = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40" fill="red"/></svg>"##;
        let svg_v2 = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40" fill="blue"/></svg>"##;
        fs::write(root.join("fig_mod.svg"), svg_v1).unwrap();
        fs::write(root.join("fig_del.svg"), svg_v1).unwrap();

        fs::write(
            root.join("main.typ"),
            r#"#set page(numbering: "i")
#counter(page).update(1)
#include "/front.typ"
#pagebreak()
#set page(numbering: "1")
#counter(page).update(1)
#set heading(numbering: "1.")
#include "/chap1.typ"
"#,
        )
        .unwrap();
        fs::write(root.join("front.typ"), "Old abstract sentence.\n").unwrap();
        fs::write(
            root.join("chap1.typ"),
            r#"= Chapter One <sec:ch1>

First paragraph with old word.

#figure(image("/fig_mod.svg"), caption: [Modified fig caption]) <fig:mod>

#figure(image("/fig_del.svg"), caption: [Deleted fig caption]) <fig:del>

See #figure([old], caption: [Old tbl]) <tab:tbl1>.
"#,
        )
        .unwrap();
        // Keep chap1 valid Typst for both revisions.
        fs::write(
            root.join("chap1.typ"),
            r#"= Chapter One <sec:ch1>

First paragraph with old word.

#figure(image("/fig_mod.svg"), caption: [Modified fig caption]) <fig:mod>

#figure(image("/fig_del.svg"), caption: [Deleted fig caption]) <fig:del>

#figure(table(columns: 2, [A], [B]), caption: [My table]) <tab:tbl1>

Reference to @fig:del here.
"#,
        )
        .unwrap();

        git(&["add", "."]);
        git(&["commit", "-m", "v1"]);

        // Make v2 changes in worktree.
        fs::write(root.join("fig_mod.svg"), svg_v2).unwrap();
        fs::write(root.join("fig_add.svg"), svg_v2).unwrap();
        fs::write(root.join("front.typ"), "New abstract sentence.\n").unwrap();
        fs::write(
            root.join("chap1.typ"),
            r#"= Chapter One <sec:ch1>

First paragraph with new word.

#figure(image("/fig_mod.svg"), caption: [Modified fig caption]) <fig:mod>

#figure(image("/fig_add.svg"), caption: [Added fig caption]) <fig:add>

#figure(table(columns: 2, [A], [C]), caption: [My table]) <tab:tbl1>

Reference removed.
"#,
        )
        .unwrap();

        let debug_dir = root.join(".debug");
        let out_pdf = root.join("diff-summary.pdf");
        let cli = Cli {
            old: "HEAD".to_string(),
            new: "WORKTREE".to_string(),
            main: PathBuf::from("main.typ"),
            output: out_pdf.clone(),
            watch: false,
            repository: Some(root.to_path_buf()),
            paths: Vec::new(),
            excludes: Vec::new(),
            image_functions: Vec::new(),
            debug_dir: Some(debug_dir.clone()),
            notices_file: PathBuf::from(".typdiff-notices.typ"),
            summary_page: true,
            summary_file: PathBuf::from(".typdiff-summary.typ"),
        };
        let repo = Repository::discover(Some(root)).unwrap();
        let image_functions = normalized_image_functions(&cli.image_functions).unwrap();
        let debug_repo_prefix = repository_relative_path(repo.root(), &debug_dir);
        generate_diff(
            &cli,
            &repo,
            &image_functions,
            &out_pdf,
            debug_repo_prefix.as_deref(),
        )
        .unwrap();

        assert!(out_pdf.is_file(), "expected output PDF to be created");
        let summary_typ = fs::read_to_string(debug_dir.join("report/.typdiff-summary.typ")).unwrap();
        assert!(summary_typ.contains("\"old\""), "summary missing modified old excerpt: {summary_typ}");
        assert!(summary_typ.contains("\"new\""), "summary missing modified new excerpt: {summary_typ}");
        assert!(summary_typ.contains("Modified fig caption"), "summary missing modified figure: {summary_typ}");
        assert!(summary_typ.contains("Added fig caption"), "summary missing added figure: {summary_typ}");
        assert!(!summary_typ.contains("#underline[Added fig caption]"), "caption wrapper should be stripped: {summary_typ}");
        assert!(!summary_typ.contains("fig-extra-"), "should not emit duplicate figure entry: {summary_typ}");
        assert!(summary_typ.contains("Deleted fig caption"), "summary missing deleted figure: {summary_typ}");
        assert!(summary_typ.contains("block: \"Table\""), "summary missing modified table kind: {summary_typ}");
        assert!(summary_typ.contains("old: \"B\", new: \"C\""), "summary missing table cell diff: {summary_typ}");
        assert!(summary_typ.contains("typdiff-format-page"), "summary missing logical page formatter");
    }
}
