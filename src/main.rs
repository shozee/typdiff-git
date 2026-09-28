mod cli;
mod git;
mod tools;

use anyhow::{bail, Context, Result};
use clap::Parser;
use cli::Cli;
use git::{is_worktree, Change, ChangeKind, Repository};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
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
    tools::require("typdiff")?;
    tools::require("typst")?;
    let image_functions = normalized_image_functions(&cli.image_functions)?;

    let debug_repo_prefix = cli
        .debug_dir
        .as_ref()
        .and_then(|path| repository_relative_path(repo.root(), path));

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
        eprintln!("temporary workspace: {} (removed when the command exits)", path.display());
        (path, Some(temporary))
    };
    let old_tree = workspace.join("old");
    let new_tree = workspace.join("new");
    let report_tree = workspace.join("report");
    repo.export_revision(&cli.old, &old_tree)?;
    repo.export_revision(&cli.new, &new_tree)?;
    copy_tree(&new_tree, &report_tree)?;

    // Mark newly-added figure definitions by label, independently of whether the
    // underlying image asset is tracked, generated, reused, or referenced through a
    // custom wrapper. This is more reliable than asset-only detection for Added figures.
    let old_figure_labels: BTreeSet<String> = collect_figure_definitions(
        &old_tree,
        &image_functions,
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
        cli.matches(change) && !change_is_below(change, debug_repo_prefix.as_deref())
    });

    // Prepare and rewrite changed figure assets before running typdiff.  This is
    // important for newly-added figures: once typdiff has wrapped a newly-added
    // figure block, reliably rewriting its image path becomes much harder.
    let mut figures = repo.changed_figure_files(&cli.old, &cli.new)?;
    figures.retain(|change| !change_is_below(change, debug_repo_prefix.as_deref()));
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
        &image_functions,
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
        &image_functions,
        &mut marked_figures,
    )?;

    rewrite_image_references(&report_tree, &marked_figures, &image_functions)?;

    let mut replaced = 0usize;
    for change in &changes {
        if change.old_path == cli.main || change.new_path == cli.main {
            eprintln!("warning: using newer {} as the document entry point; its own edits are not highlighted", cli.main.display());
            continue;
        }
        match change.kind {
            ChangeKind::Deleted => eprintln!(
                "note: deleted file omitted from newer full-document report: {}",
                change.old_path.display()
            ),
            ChangeKind::Added => {
                let empty = workspace.join(format!("empty-{replaced}.typ"));
                fs::write(&empty, "")?;
                let rewritten_new = report_tree.join(&change.new_path);
                run_typdiff(&empty, &rewritten_new, &rewritten_new)?;
                replaced += 1;
            }
            ChangeKind::Modified | ChangeKind::Renamed => {
                let old_source = old_tree.join(&change.old_path);
                let rewritten_new = report_tree.join(&change.new_path);
                run_typdiff(&old_source, &rewritten_new, &rewritten_new)?;
                replaced += 1;
            }
        }
    }
    // typdiff may reconstruct added code blocks. Run the path rewriter once more on
    // its output so added/modified figure assets still point at the generated framed
    // comparison PDFs. The pass is idempotent because already-rewritten paths no longer
    // match the original asset paths.
    rewrite_image_references(&report_tree, &marked_figures, &image_functions)?;
    rewrite_modified_figures_by_label(
        &report_tree,
        &modified_figure_assets,
    )?;

    let removed_references =
        removed_figure_references(&old_tree, &new_tree, &image_functions)?;
    let removed_definitions =
        removed_figure_definitions(&old_tree, &new_tree, &image_functions)?;
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
    let placed_deleted = place_deleted_figure_notices(
        &report_tree,
        &removed_definitions,
        &removed_label_references,
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
    )?;

    let output = absolute_output(&cli.output)?;
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
    let mut result = Vec::new();
    let mut offset = 0usize;

    while let Some(relative) = source[offset..].find("#figure") {
        let start = offset + relative;
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

        let mut label_open = close + 1;
        while label_open < source.len()
            && source.as_bytes()[label_open].is_ascii_whitespace()
        {
            label_open += 1;
        }
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
        if label.starts_with("fig:") {
            result.push((start, label_close + 1, label));
        }
        offset = label_close + 1;
    }

    result
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
        let definition = definitions.get(label.as_str()).copied();
        let box_markup = inline_deleted_figure_box(label, definition);
        let mut source = fs::read_to_string(&source_path)?;
        source.insert_str(insertion, &box_markup);
        fs::write(&source_path, source)?;
        placed.push(PlacedDeletedFigure { label: label.clone() });
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
) -> String {
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
  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  Label: #raw("{}")
{}{}  #v(5pt)
  #text(weight: "bold")[
    This figure was present near this location in the previous version but has been removed from the revised document.
  ]
]
#v(5pt)

"#,
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
  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
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
        appendix.push_str(&format!(
            r#"#block(
  width: 100%, inset: 7pt, stroke: 1pt + rgb("b3261e"),
  fill: rgb("fff4f2"), radius: 2pt, breakable: false,
)[
  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  Label: #raw("{}")
  The figure reference and its corresponding figure were present in the previous version but have been removed from the revised document.
]
#v(5pt)

"#,
            escape_typst_string(label)
        ));
    }

    for path in deleted_paths.difference(&definition_paths) {
        let path = normalize_path(path);
        appendix.push_str(&format!(
            r#"#block(
  width: 100%, inset: 7pt, stroke: 1pt + rgb("b3261e"),
  fill: rgb("fff4f2"), radius: 2pt, breakable: false,
)[
  #text(fill: rgb("b3261e"), weight: "bold")[Deleted figure]
  #v(3pt)
  The figure #raw("{}") was present in the previous version but has been removed from the revised document.
]
#v(5pt)

"#,
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
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

fn run_typdiff(old: &Path, new: &Path, target: &Path) -> Result<()> {
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
        neutralize_one_sided_references(&old_source, &new_source);

    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document.typ");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let old_sanitized = parent.join(format!(".{file_name}.typdiff-old.typ"));
    let new_sanitized = parent.join(format!(".{file_name}.typdiff-new.typ"));
    let temporary = parent.join(format!(".{file_name}.typdiff-output.typ"));

    fs::write(&old_sanitized, old_source)?;
    fs::write(&new_sanitized, new_source)?;

    let result = tools::run(
        "typdiff",
        [
            old_sanitized.as_os_str(),
            new_sanitized.as_os_str(),
            OsStr::new("-o"),
            temporary.as_os_str(),
        ],
    );
    if let Err(error) = result {
        let _ = fs::remove_file(&old_sanitized);
        let _ = fs::remove_file(&new_sanitized);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    fs::rename(&temporary, target)
        .with_context(|| format!("replacing {} with visual diff", target.display()))?;
    let _ = fs::remove_file(old_sanitized);
    let _ = fs::remove_file(new_sanitized);
    Ok(())
}
fn neutralize_one_sided_references(old: &str, new: &str) -> (String, String) {
    let old_tokens = collect_reference_tokens(old);
    let new_tokens = collect_reference_tokens(new);
    let old_only: BTreeSet<String> = old_tokens.difference(&new_tokens).cloned().collect();

    // Only references that disappear from the document must be rendered literally.
    // New-only references must remain executable Typst so calls such as
    // #Fig(<fig:new>) expand to their resolved text (for example, "Fig. 1.1") inside
    // diff-added markup rather than appearing as raw source text.
    (replace_tokens_with_raw(old, &old_only), new.to_string())
}

fn collect_reference_tokens(source: &str) -> BTreeSet<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = BTreeSet::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '@' {
            let start = index;
            index += 1;
            if chars.get(index) == Some(&'<') {
                index += 1;
                while index < chars.len() && chars[index] != '>' { index += 1; }
                if index < chars.len() { index += 1; }
            } else {
                while index < chars.len() && is_label_char(chars[index]) { index += 1; }
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
        result = result.replace(token, &format!("{fence}{token}{fence}"));
    }
    result
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
