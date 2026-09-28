use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const WORKTREE: &str = "WORKTREE";

#[derive(Clone, Copy, Debug)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
        })
    }
}

#[derive(Debug)]
pub struct Change {
    pub kind: ChangeKind,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
}

pub struct Repository {
    root: PathBuf,
}

impl Repository {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn discover(start: Option<&Path>) -> Result<Self> {
        let start = start.unwrap_or_else(|| Path::new("."));
        let output = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("running git")?;
        if !output.status.success() {
            bail!(
                "not inside a Git repository: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(Self {
            root: PathBuf::from(String::from_utf8(output.stdout)?.trim()),
        })
    }

    pub fn verify_revision(&self, rev: &str) -> Result<()> {
        if is_worktree(rev) {
            return Ok(());
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("invalid Git revision: {rev}");
        }
        Ok(())
    }

    pub fn changed_typst_files(&self, old: &str, new: &str) -> Result<Vec<Change>> {
        self.changed_files(old, new, &["*.typ"])
    }

    pub fn changed_figure_files(&self, old: &str, new: &str) -> Result<Vec<Change>> {
        self.changed_files(
            old,
            new,
            &["*.png", "*.jpg", "*.jpeg", "*.webp", "*.svg", "*.pdf"],
        )
    }

    fn changed_files(&self, old: &str, new: &str, patterns: &[&str]) -> Result<Vec<Change>> {
        if is_worktree(old) {
            bail!("WORKTREE is supported only as the newer revision");
        }

        let mut command = Command::new("git");
        command.arg("-C").arg(&self.root);
        if is_worktree(new) {
            command.args(["diff", "--name-status", "-z", "--find-renames", old, "--"]);
        } else {
            command.args([
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                old,
                new,
                "--",
            ]);
        }
        command.args(patterns);

        let output = command.output().context("listing changed files")?;
        if !output.status.success() {
            bail!("git diff failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        let mut changes = parse_name_status_z(&output.stdout)?;

        // `git diff` deliberately excludes untracked files. Include non-ignored
        // untracked files when the newer side is the working tree.
        if is_worktree(new) {
            let tracked_paths: HashSet<PathBuf> = changes
                .iter()
                .flat_map(|change| [change.old_path.clone(), change.new_path.clone()])
                .collect();
            for path in self.untracked_files()? {
                if matches_patterns(&path, patterns) && !tracked_paths.contains(&path) {
                    changes.push(Change {
                        kind: ChangeKind::Added,
                        old_path: path.clone(),
                        new_path: path,
                    });
                }
            }
        }
        Ok(changes)
    }

    pub fn export_revision(&self, rev: &str, destination: &Path) -> Result<()> {
        if is_worktree(rev) {
            return self.export_worktree(destination);
        }

        fs::create_dir_all(destination)?;
        let mut archive = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["archive", "--format=tar", rev])
            .stdout(Stdio::piped())
            .spawn()
            .context("starting git archive")?;
        let stdout = archive.stdout.take().context("capturing git archive output")?;
        let status = Command::new("tar")
            .arg("-xf")
            .arg("-")
            .arg("-C")
            .arg(destination)
            .stdin(stdout)
            .status()
            .context("extracting revision archive")?;
        let git_status = archive.wait()?;
        if !git_status.success() || !status.success() {
            bail!("failed to export revision {rev}");
        }
        Ok(())
    }

    fn export_worktree(&self, destination: &Path) -> Result<()> {
        fs::create_dir_all(destination)?;
        // When --debug-dir is located inside the repository, the destination itself is
        // an untracked directory. Exclude its top-level workspace before enumerating the
        // worktree, otherwise old/ and new/ recursively copy one another.
        let excluded_workspace = destination
            .strip_prefix(&self.root)
            .ok()
            .and_then(|relative| relative.components().next())
            .map(|component| PathBuf::from(component.as_os_str()));

        // Include tracked files plus non-ignored untracked files. Ignored build
        // products, `.git`, and deleted tracked files are not copied.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
            .output()
            .context("listing working-tree files")?;
        if !output.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        for field in output.stdout.split(|byte| *byte == 0).filter(|f| !f.is_empty()) {
            let relative = PathBuf::from(std::str::from_utf8(field)?);
            if excluded_workspace
                .as_ref()
                .is_some_and(|workspace| relative.starts_with(workspace))
            {
                continue;
            }
            let source = self.root.join(&relative);
            if !source.is_file() {
                continue;
            }
            let target = destination.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target).with_context(|| {
                format!("copying working-tree file {}", relative.display())
            })?;
        }
        Ok(())
    }

    fn untracked_files(&self) -> Result<Vec<PathBuf>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .output()
            .context("listing untracked files")?;
        if !output.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| Ok(PathBuf::from(std::str::from_utf8(field)?)))
            .collect()
    }
}

pub fn is_worktree(rev: &str) -> bool {
    rev.eq_ignore_ascii_case(WORKTREE) || rev == "-"
}

fn matches_patterns(path: &Path, patterns: &[&str]) -> bool {
    let extension = path.extension().and_then(OsStrExt::to_str_lowercase);
    patterns.iter().any(|pattern| {
        pattern
            .strip_prefix("*.")
            .is_some_and(|expected| extension.as_deref() == Some(expected))
    })
}

struct OsStrExt;
impl OsStrExt {
    fn to_str_lowercase(value: &std::ffi::OsStr) -> Option<String> {
        value.to_str().map(str::to_ascii_lowercase)
    }
}

fn parse_name_status_z(bytes: &[u8]) -> Result<Vec<Change>> {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut index = 0;
    let mut result = Vec::new();

    while index < fields.len() {
        let status = std::str::from_utf8(fields[index])?;
        index += 1;
        let code = status.chars().next().context("missing git status code")?;
        match code {
            'A' | 'M' | 'D' => {
                let path = PathBuf::from(std::str::from_utf8(
                    fields.get(index).context("missing path")?,
                )?);
                index += 1;
                let kind = match code {
                    'A' => ChangeKind::Added,
                    'M' => ChangeKind::Modified,
                    _ => ChangeKind::Deleted,
                };
                result.push(Change {
                    kind,
                    old_path: path.clone(),
                    new_path: path,
                });
            }
            'R' => {
                let old_path = PathBuf::from(std::str::from_utf8(
                    fields.get(index).context("missing old rename path")?,
                )?);
                index += 1;
                let new_path = PathBuf::from(std::str::from_utf8(
                    fields.get(index).context("missing new rename path")?,
                )?);
                index += 1;
                result.push(Change {
                    kind: ChangeKind::Renamed,
                    old_path,
                    new_path,
                });
            }
            other => bail!("unsupported git change status: {other}"),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modification_and_rename() {
        let changes = parse_name_status_z(b"M\0a.typ\0R100\0old.typ\0new.typ\0").unwrap();
        assert_eq!(changes.len(), 2);
        assert!(matches!(changes[1].kind, ChangeKind::Renamed));
    }

    #[test]
    fn recognizes_worktree_aliases() {
        assert!(is_worktree("WORKTREE"));
        assert!(is_worktree("worktree"));
        assert!(is_worktree("-"));
        assert!(!is_worktree("HEAD"));
    }
}
