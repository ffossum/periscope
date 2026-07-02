//! Parse a unified diff into per-file groups of plain, unstyled rows.
//!
//! This step is pure text-to-data: no syntax highlighting, no side-by-side
//! pairing, no styling. Those happen in [`super::build`].

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum SideKind {
    Context,
    Removed,
    Added,
}

/// A single parsed line of a diff, before any styling or pairing.
pub(super) enum ParsedRow {
    /// A hunk header (`@@ ... @@`).
    Hunk(String),
    /// A line shown verbatim full-width: file metadata or stray non-diff input.
    Verbatim(String),
    /// A content line tagged with its role and 1-based line numbers. `old` is
    /// set for removed/context lines, `new` for added/context lines.
    Content {
        kind: SideKind,
        old: Option<usize>,
        new: Option<usize>,
        text: String,
    },
}

/// One file's diff as plain data, before syntax highlighting and pairing.
#[derive(Default)]
pub(super) struct ParsedFile {
    /// The current (new) path; becomes the block title.
    pub(super) title: String,
    /// The previous path when the file was renamed, else `None`.
    pub(super) rename_from: Option<String>,
    pub(super) rows: Vec<ParsedRow>,
}

/// Parse a unified diff into per-file groups of plain, unstyled rows.
///
/// This step is pure text-to-data: no syntax highlighting, no side-by-side
/// pairing, no styling. Those happen in [`super::build::build_files`].
pub(super) fn parse(raw: &str) -> Vec<ParsedFile> {
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut cur: Option<ParsedFile> = None;
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    let mut in_hunk = false;

    for line in raw.lines() {
        if line.starts_with("diff --git") {
            files.extend(cur.take());
            cur = Some(ParsedFile {
                title: title_from_diff_git(line),
                ..ParsedFile::default()
            });
            in_hunk = false;
            continue;
        }

        if is_file_meta(line) {
            in_hunk = false;
            if let Some(rest) = line.strip_prefix("rename from ") {
                cur.get_or_insert_with(ParsedFile::default).rename_from =
                    Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                cur.get_or_insert_with(ParsedFile::default).title = rest.trim().to_string();
            } else if let Some(rest) = line
                .strip_prefix("+++ ")
                .or_else(|| line.strip_prefix("--- "))
                && let Some(path) = clean_path(rest)
            {
                // Use the +++/--- paths to refine the title.
                let file = cur.get_or_insert_with(ParsedFile::default);
                if line.starts_with("+++ ") || file.title.is_empty() {
                    file.title = path;
                }
            }
            continue;
        }

        if line.starts_with("@@") {
            let file = cur.get_or_insert_with(ParsedFile::default);
            (old_ln, new_ln) = parse_hunk_header(line);
            in_hunk = true;
            file.rows.push(ParsedRow::Hunk(line.to_string()));
            continue;
        }

        if !in_hunk {
            // Stray content (e.g. non-diff input): show it verbatim.
            let file = cur.get_or_insert_with(ParsedFile::default);
            file.rows.push(ParsedRow::Verbatim(line.to_string()));
            continue;
        }

        let file = cur.get_or_insert_with(ParsedFile::default);
        match line.chars().next() {
            Some('+') => {
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Added,
                    old: None,
                    new: Some(new_ln),
                    text: line[1..].to_string(),
                });
                new_ln += 1;
            }
            Some('-') => {
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Removed,
                    old: Some(old_ln),
                    new: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
            }
            Some('\\') => {} // "\ No newline at end of file"
            _ => {
                // Context line (leading space, or an empty line).
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Context,
                    old: Some(old_ln),
                    new: Some(new_ln),
                    text: line.strip_prefix(' ').unwrap_or(line).to_string(),
                });
                old_ln += 1;
                new_ln += 1;
            }
        }
    }

    files.extend(cur);
    files
}

/// Pull the new-file path out of a `diff --git a/<path> b/<path>` line.
fn title_from_diff_git(line: &str) -> String {
    line.split_whitespace()
        .last()
        .map(|t| t.strip_prefix("b/").unwrap_or(t).to_string())
        .unwrap_or_default()
}

/// Whether a line is file-level metadata (not part of a hunk body).
fn is_file_meta(line: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "index ",
        "--- ",
        "+++ ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "rename ",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// Strip the leading `a/` or `b/` from a diff path, or `None` for `/dev/null`.
fn clean_path(raw: &str) -> Option<String> {
    let path = raw.trim();
    let path = path
        .strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path);
    (path != "/dev/null").then(|| path.to_string())
}

/// Extract the starting old and new line numbers from `@@ -a,b +c,d @@`.
fn parse_hunk_header(line: &str) -> (usize, usize) {
    let mut old = 1;
    let mut new = 1;
    // Only the range section between the `@@` markers holds the numbers; the
    // trailing function context can contain stray `+`/`-` tokens (e.g. Rust's
    // `->`) that must not be mistaken for line numbers.
    let range = line.split("@@").nth(1).unwrap_or(line);
    for token in range.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            if let Some(n) = rest.split(',').next().and_then(|n| n.parse().ok()) {
                old = n;
            }
        } else if let Some(rest) = token.strip_prefix('+')
            && let Some(n) = rest.split(',').next().and_then(|n| n.parse().ok())
        {
            new = n;
        }
    }
    (old, new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_ignores_arrow_in_context() {
        // Rust's `->` in the function-context suffix must not be mistaken for a
        // line-number token. (Regression test.)
        let (old, new) = parse_hunk_header("@@ -300,7 +300,8 @@ fn bar() -> Result<()>");
        assert_eq!((old, new), (300, 300));
    }

    #[test]
    fn hunk_header_without_counts() {
        let (old, new) = parse_hunk_header("@@ -42 +57 @@");
        assert_eq!((old, new), (42, 57));
    }

    /// Pull out only the `Content` rows for terser assertions.
    fn content(file: &ParsedFile) -> Vec<(SideKind, Option<usize>, Option<usize>, &str)> {
        file.rows
            .iter()
            .filter_map(|r| match r {
                ParsedRow::Content {
                    kind,
                    old,
                    new,
                    text,
                } => Some((*kind, *old, *new, text.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn assigns_line_numbers_from_hunk_start() {
        let raw = "\
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -300,3 +300,3 @@ fn bar() -> Result<()>
 ctx
-old line
+new line
";
        let files = parse(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "foo.rs");
        assert_eq!(
            content(&files[0]),
            vec![
                (SideKind::Context, Some(300), Some(300), "ctx"),
                (SideKind::Removed, Some(301), None, "old line"),
                (SideKind::Added, None, Some(301), "new line"),
            ],
        );
    }

    #[test]
    fn title_falls_back_to_old_path_for_deletions() {
        let raw = "\
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = parse(raw);
        assert_eq!(files[0].title, "gone.rs");
    }

    #[test]
    fn non_diff_input_is_kept_verbatim() {
        let files = parse("just some text\nnot a diff\n");
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .rows
                .iter()
                .all(|r| matches!(r, ParsedRow::Verbatim(_)))
        );
    }
}
