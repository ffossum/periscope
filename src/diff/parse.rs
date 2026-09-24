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
    // Body lines the current hunk still expects, from its header's counts. The
    // body is delimited by these rather than by line prefixes, because a body
    // line is indistinguishable from file metadata on its own: a removed `--`
    // comment reads as `--- ...`, exactly like a `--- a/path` header.
    let mut want_old = 0usize;
    let mut want_new = 0usize;

    for line in raw.lines() {
        if line.starts_with("diff --git") {
            files.extend(cur.take());
            cur = Some(ParsedFile {
                title: title_from_diff_git(line),
                ..ParsedFile::default()
            });
            in_hunk = false;
            (want_old, want_new) = (0, 0);
            continue;
        }

        if line.starts_with("@@") {
            let file = cur.get_or_insert_with(ParsedFile::default);
            let hunk = parse_hunk_header(line);
            (old_ln, new_ln) = (hunk.old_start, hunk.new_start);
            (want_old, want_new) = (hunk.old_count, hunk.new_count);
            in_hunk = true;
            file.rows.push(ParsedRow::Hunk(line.to_string()));
            continue;
        }

        // The trailing "\ No newline at end of file" isn't counted by the
        // header, so allow it once the body is otherwise exhausted.
        let no_newline = in_hunk && line.starts_with('\\');
        if want_old == 0 && want_new == 0 && !no_newline {
            in_hunk = false;
            if is_file_meta(line) {
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
            } else {
                // Stray content (e.g. non-diff input): show it verbatim.
                let file = cur.get_or_insert_with(ParsedFile::default);
                file.rows.push(ParsedRow::Verbatim(line.to_string()));
            }
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
                want_new = want_new.saturating_sub(1);
            }
            Some('-') => {
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Removed,
                    old: Some(old_ln),
                    new: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
                want_old = want_old.saturating_sub(1);
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
                want_old = want_old.saturating_sub(1);
                want_new = want_new.saturating_sub(1);
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

/// The `-a,b +c,d` ranges of a hunk header: where each side starts and how many
/// lines of it the hunk body covers.
#[derive(Clone, Copy, PartialEq, Debug)]
struct HunkRange {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

/// Extract the old and new ranges from `@@ -a,b +c,d @@`.
///
/// An omitted count means one line, per the unified-diff format.
fn parse_hunk_header(line: &str) -> HunkRange {
    let mut hunk = HunkRange {
        old_start: 1,
        old_count: 1,
        new_start: 1,
        new_count: 1,
    };
    // Only the range section between the `@@` markers holds the numbers; the
    // trailing function context can contain stray `+`/`-` tokens (e.g. Rust's
    // `->`) that must not be mistaken for line numbers.
    let range = line.split("@@").nth(1).unwrap_or(line);
    for token in range.split_whitespace() {
        let (rest, start, count) = if let Some(rest) = token.strip_prefix('-') {
            (rest, &mut hunk.old_start, &mut hunk.old_count)
        } else if let Some(rest) = token.strip_prefix('+') {
            (rest, &mut hunk.new_start, &mut hunk.new_count)
        } else {
            continue;
        };
        let mut parts = rest.split(',');
        let Some(n) = parts.next().and_then(|n| n.parse().ok()) else {
            continue;
        };
        *start = n;
        *count = parts.next().map_or(1, |c| c.parse().unwrap_or(1));
    }
    hunk
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_ignores_arrow_in_context() {
        // Rust's `->` in the function-context suffix must not be mistaken for a
        // line-number token. (Regression test.)
        let hunk = parse_hunk_header("@@ -300,7 +300,8 @@ fn bar() -> Result<()>");
        assert_eq!(
            hunk,
            HunkRange {
                old_start: 300,
                old_count: 7,
                new_start: 300,
                new_count: 8,
            }
        );
    }

    #[test]
    fn hunk_header_without_counts() {
        // An omitted count means a single line.
        let hunk = parse_hunk_header("@@ -42 +57 @@");
        assert_eq!(
            hunk,
            HunkRange {
                old_start: 42,
                old_count: 1,
                new_start: 57,
                new_count: 1,
            }
        );
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
    fn body_lines_that_look_like_file_headers_stay_content() {
        // A removed `--` comment (Lua, Haskell, SQL...) reads as `--- ...`, and
        // an added `++` one as `+++ ...`; neither is a file header inside a
        // hunk body. (Regression test.)
        let raw = "\
diff --git a/a.lua b/a.lua
--- a/a.lua
+++ b/a.lua
@@ -1,2 +1,2 @@
--- old comment
+++ new comment
 ctx
";
        let files = parse(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "a.lua");
        assert_eq!(
            content(&files[0]),
            vec![
                (SideKind::Removed, Some(1), None, "-- old comment"),
                (SideKind::Added, None, Some(1), "++ new comment"),
                (SideKind::Context, Some(2), Some(2), "ctx"),
            ],
        );
    }

    #[test]
    fn metadata_after_an_exhausted_hunk_is_still_metadata() {
        // The counts end the body, so the next file's headers parse normally.
        let raw = "\
diff --git a/a.lua b/a.lua
--- a/a.lua
+++ b/a.lua
@@ -1 +1 @@
--- old
\\ No newline at end of file
+-- new
diff --git a/b.lua b/b.lua
--- a/b.lua
+++ b/b.lua
@@ -7 +7 @@
-x
+y
";
        let files = parse(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].title, "b.lua");
        assert_eq!(
            content(&files[1]),
            vec![
                (SideKind::Removed, Some(7), None, "x"),
                (SideKind::Added, None, Some(7), "y"),
            ],
        );
        // The "\ No newline" marker is dropped rather than shown verbatim.
        assert!(
            !files[0]
                .rows
                .iter()
                .any(|r| matches!(r, ParsedRow::Verbatim(_)))
        );
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
