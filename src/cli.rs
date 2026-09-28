use crate::git::Change;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// Older Git revision (commit, tag, or branch)
    pub old: String,
    /// Newer Git revision, or WORKTREE/- for staged, unstaged, and untracked files
    pub new: String,
    /// Root Typst document to compile from the newer revision
    #[arg(short, long, default_value = "main.typ")]
    pub main: PathBuf,
    /// Output PDF
    #[arg(short, long, default_value = "revision-diff.pdf")]
    pub output: PathBuf,
    /// Repository path; defaults to the current directory
    #[arg(short = 'C', long)]
    pub repository: Option<PathBuf>,
    /// Include only changed paths below this prefix (repeatable)
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Exclude changed paths below this prefix (repeatable)
    #[arg(long = "exclude")]
    pub excludes: Vec<PathBuf>,
    /// Additional Typst function whose first string argument is an image path (repeatable)
    #[arg(long = "image-function", value_name = "NAME")]
    pub image_functions: Vec<String>,
    /// Persist old/new/report trees and generated Typst files in this directory
    #[arg(long = "debug-dir", value_name = "DIR")]
    pub debug_dir: Option<PathBuf>,
    /// Typst file included at the end of main.typ for generated revision notices
    #[arg(long = "notices-file", default_value = ".typdiff-notices.typ")]
    pub notices_file: PathBuf,
}

impl Cli {
    pub fn matches(&self, change: &Change) -> bool {
        let candidates = [&change.old_path, &change.new_path];
        let included = self.paths.is_empty()
            || self.paths.iter().any(|prefix| candidates.iter().any(|path| path.starts_with(prefix)));
        let excluded = self.excludes.iter().any(|prefix| candidates.iter().any(|path| path.starts_with(prefix)));
        included && !excluded
    }
}
