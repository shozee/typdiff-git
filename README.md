# typdiff-git

A Git-aware wrapper for [sou1118/typdiff](https://github.com/sou1118/typdiff) that generates full-document Typst revision PDFs with visual diffs for text, figures, and subfigures.

`typdiff-git` compares two Git revisions, or a Git revision and the current working tree, then compiles the complete Typst project from its root document. Text changes are handled by `typdiff`, while added, modified, and deleted figures receive dedicated visual markers.

## Features

- Compare two commits, tags, or branches
- Compare a Git revision with the current working tree
- Include staged, unstaged, and non-ignored untracked files
- Compile a complete multi-file Typst document from `main.typ`
- Preserve document structure, references, numbering, bibliography, and page layout
- Show added and deleted text using `typdiff` markup
- Keep newly added cross-reference functions executable, such as `#Fig(<fig:example>)`
- Mark newly added figures with a blue frame
- Show modified figures vertically as **Previous version** and **Revised version**
- Detect figure changes when:
  - the asset path changes
  - the path remains unchanged but the file contents change
- Compare individual assets inside multi-panel figures and `subfig` layouts
- Leave unchanged subfigures untouched
- Insert deleted-figure notices near their former references
- Include the previous caption in deleted-figure notices when available
- Fall back to a final **Deleted Figures** section when an inline insertion point cannot be found
- Preserve intermediate old, new, and report trees for debugging

## Example output

### Added figure

A newly added `#figure(...) <fig:...>` definition is wrapped in a blue box labeled **Added figure**.

### Modified figure

A modified image or PDF is replaced by a vertical comparison:

```text
Previous version
[old figure]

Revised version
[new figure]
```

The comparison uses the original horizontal width, making it suitable for detailed scientific figures that would be difficult to read side by side.

### Deleted figure

A deleted figure is represented near its former reference by a red notice containing, when available:

- the former figure label
- the previous asset path
- the previous caption
- a statement that the figure was removed from the revised document

## Requirements

- Git
- Rust and Cargo
- [Typst](https://typst.app/)
- [typdiff](https://github.com/sou1118/typdiff)
- `tar`

Install `typdiff` from crates.io:

```bash
cargo install typdiff
```

Make sure `typst`, `typdiff`, and `git` are available in `PATH`:

```bash
typst --version
typdiff --version
git --version
```

## Installation

Install from crates.io:

```bash
cargo install typdiff-git
```

Or clone your repository and build the release binary:

```bash
git clone https://github.com/YOUR_ACCOUNT/typdiff-git.git
cd typdiff-git
cargo build --release
```

Install the binary in a user-local directory:

```bash
install -m 755 target/release/typdiff-git ~/.local/bin/typdiff-git
```

Ensure `~/.local/bin` is in `PATH`.

## Basic usage

```bash
typdiff-git OLD_REV NEW_REV \
  --main main.typ \
  -o revision-diff.pdf
```

For example:

```bash
typdiff-git submitted-version HEAD \
  --main main.typ \
  -o revision-diff.pdf
```

The output is a complete PDF based on the newer revision, not a collection of separately compiled chapter PDFs.

## Compare with the working tree

Use `WORKTREE` as the newer revision:

```bash
typdiff-git HEAD WORKTREE \
  --main main.typ \
  -o working-tree-diff.pdf
```

This includes:

- staged changes
- unstaged changes
- tracked-file deletions
- non-ignored untracked files

Ignored files and `.git/` are excluded.

`WORKTREE` is supported only as the newer side because the report uses the newer document structure as its base.

You may also compare an older submitted revision directly with the current working tree:

```bash
typdiff-git submitted-version WORKTREE \
  --main main.typ \
  -o current-revision.pdf
```

## Multi-file Typst projects

A typical project may look like this:

```text
main.typ
chap_intro.typ
chap_methods.typ
chap_results.typ
references.bib
Figs/
```

`main.typ` may include child documents:

```typst
#include "chap_intro.typ"
#include "chap_methods.typ"
#include "chap_results.typ"
```

`typdiff-git` exports both revisions, copies the complete newer tree into a temporary report tree, replaces changed child sources with visual diff sources, and compiles `main.typ` once.

This preserves:

- chapter order
- page settings
- counters and numbering
- bibliography
- labels and cross-references
- headers and footers
- appendices

## Cross-reference functions

Projects often define custom reference helpers such as:

```typst
#Fig(<fig:example>)
#Figure(<fig:example>)
```

Newly added references remain executable Typst. Therefore, an added call such as:

```typst
#Fig(<fig:example>)
```

is rendered as its resolved text, for example:

```text
Fig. 1.1
```

rather than displayed as literal Typst source.

References that exist only in the old revision are neutralized before `typdiff` runs. This prevents unresolved deleted labels and invalid escaping from breaking compilation.

## Figure handling

Supported figure asset extensions are:

```text
.png
.jpg
.jpeg
.webp
.svg
.pdf
```

### Added figures

A figure label that exists only in the newer revision is treated as an added figure:

```typst
#figure(
  image("Figs/new-result.pdf"),
  caption: [A newly added result.],
) <fig:new-result>
```

The complete figure definition is wrapped in a blue **Added figure** box.

Label-based detection means the figure can still be recognized when:

- the asset already existed in the repository
- an existing asset is reused in a new figure
- the figure is composed using a helper such as `subfig`

### Modified figures

A figure is treated as modified when either:

1. its referenced asset path changes, or
2. its asset path stays the same but the file contents differ between revisions.

For same-path changes, the old and new files are compared byte for byte. File timestamps are not used.

The generated comparison places the old asset above the new asset:

```text
Previous version
[old asset]

Revised version
[new asset]
```

### Subfigures and multi-panel figures

For a multi-panel figure such as:

```typst
#figure(
  (
    subfig("Figs/panel-a.pdf", ...),
    subfig("Figs/panel-b.pdf", ...),
  ),
  caption: [A two-panel comparison.],
) <fig:panels>
```

assets are tracked in source order:

```text
asset 0 = panel-a.pdf
asset 1 = panel-b.pdf
```

If only panel A changes, only asset 0 is replaced with a previous/revised comparison. Panel B remains unchanged.

### Deleted figures

A figure definition removed from the newer revision is detected by its `<fig:...>` label. A red **Deleted figure** box is inserted near the paragraph containing the removed `#Fig(...)` or `#Figure(...)` reference.

The box can include:

- the former label
- the previous image path
- the previous caption

The previous caption is shown without additional coloring or strike-through.

If the former location cannot be determined, the notice is added to a final **Deleted Figures** section.

## Custom image functions

The built-in `image(...)` function is always recognized.

If a project uses a custom function whose first quoted string argument is an image path, register it with `--image-function`:

```bash
typdiff-git HEAD WORKTREE \
  --main main.typ \
  --image-function my-image \
  --image-function panel-image \
  -o revision.pdf
```

Example:

```typst
#my-image("Figs/example.pdf", width: 90%)
```

The option is repeatable.

## Include and exclude paths

Limit processing to selected paths:

```bash
typdiff-git OLD_REV NEW_REV \
  --main main.typ \
  --path chapters \
  -o revision.pdf
```

Exclude style, template, or generated files that should not be passed through `typdiff`:

```bash
typdiff-git OLD_REV NEW_REV \
  --main main.typ \
  --exclude page_settings.typ \
  --exclude template.typ \
  --exclude generated \
  -o revision.pdf
```

This is recommended for Typst files that primarily define functions, styles, or templates. They remain available from the newer project tree, but their code is not decorated with text-diff markup.

## Debugging

By default, intermediate files are created in a temporary directory and removed when the command exits.

Use `--debug-dir` to preserve them:

```bash
typdiff-git HEAD WORKTREE \
  --main main.typ \
  --debug-dir .typdiff-debug \
  -o working-tree-diff.pdf
```

The debug tree contains:

```text
.typdiff-debug/
├── old/                    # exported old revision
├── new/                    # exported new revision or WORKTREE
├── report/                 # exact project tree used for compilation
│   ├── main.typ
│   ├── changed-child.typ
│   ├── .typdiff-figures/   # generated figure comparisons
│   └── .typdiff-notices.typ
└── deleted-figures.txt
```

The exact source compiled by Typst is:

```text
.typdiff-debug/report/main.typ
```

Useful checks include:

```bash
grep -R -n "Added figure" .typdiff-debug/report --include='*.typ'
```

```bash
grep -R -n "Deleted figure" .typdiff-debug/report --include='*.typ'
```

```bash
grep -R -n ".typdiff-figures" .typdiff-debug/report --include='*.typ'
```

```bash
find .typdiff-debug/report/.typdiff-figures -type f -name '*.pdf' -print
```

Add the debug directory to `.gitignore`:

```gitignore
.typdiff-debug/
```

## Typical command

```bash
typdiff-git HEAD WORKTREE \
  --main main.typ \
  --exclude page_settings.typ \
  --debug-dir .typdiff-debug \
  -o working-tree-diff.pdf
```

## How it works

1. Validate the old and new revisions.
2. Export the old revision.
3. Export the new revision or current working tree.
4. Copy the complete newer project into a report tree.
5. Detect added figure definitions by label.
6. Detect modified image and PDF assets by path and file contents.
7. Generate previous/revised figure-comparison PDFs.
8. Rewrite only affected figure or subfigure asset references.
9. Run `typdiff` for changed child `.typ` documents.
10. Insert deleted-figure notices near their former references.
11. Compile the report-tree `main.typ` once with Typst.

## Limitations

- `main.typ` is used as the newer structural entry point. Text written directly in `main.typ` is not currently highlighted because rewriting the entry point can interfere with imports, includes, and document-level configuration.
- Image paths assembled dynamically may not be resolved statically:

```typst
#image("Figs/" + filename)
```

- Image paths stored only in runtime data structures may require project-specific handling.
- Deleted figures are positioned using removed figure references when available. If no reliable location is found, they are listed in the final appendix.
- Git LFS assets must be available as real image or PDF files in both exported trees. A pointer file cannot be rendered by Typst.
- Figure-comparison blocks increase document height and can change page breaks.

## Relationship to typdiff

This project is not a fork of `typdiff`. It invokes `typdiff` as an external source-level diff engine and adds Git-aware project orchestration, full-document compilation, and visual figure comparison.

Upstream project:

- [sou1118/typdiff](https://github.com/sou1118/typdiff)

Issues concerning Typst source parsing and text-diff rendering may belong upstream. Git revision handling, working-tree snapshots, full-project compilation, and figure lifecycle visualization are implemented in `typdiff-git`.

## Contributing

Bug reports and pull requests are welcome. Forks and independent extensions of this project are even more welcome.
This project was created primarily to address an urgent practical need: generating revision documents for an academic manuscript under a tight deadline. Please note that I am not a Rust expert, and much of the code was developed with the assistance of AI tools. The implementation may therefore contain unconventional design choices or areas that could benefit from review and improvement.
Please test the tool carefully before using its output for important submissions.

When reporting an issue, please include:

- the command used
- the relevant execution log
- whether the newer side is a commit or `WORKTREE`
- a minimal Typst example if possible
- the contents of the relevant files under `.typdiff-debug/report/`

Please do not include confidential manuscripts or unpublished data in public issues.

## License

Apache-2.0.
