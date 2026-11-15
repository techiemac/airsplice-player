//! The playlist as a tree: entries inside followed folders are shown under
//! rows for those folders (and their subfolders), which collapse. Display
//! only: the playlist order is what plays.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Folder { path: PathBuf, name: String, depth: usize, count: usize, collapsed: bool },
    Entry { ix: usize, depth: usize },
}

/// The folders an entry sits in, outermost first: the followed folder
/// holding it, then each subfolder down to its own. Empty for an entry
/// added on its own.
fn chain(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(root) = roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.components().count()) else {
        return Vec::new();
    };
    let mut folders = vec![root.clone()];
    if let Some(parent) = path.parent()
        && let Ok(inner) = parent.strip_prefix(root)
    {
        let mut at = root.clone();
        for part in inner.components() {
            at.push(part);
            folders.push(at.clone());
        }
    }
    folders
}

fn name(folder: &Path) -> String {
    folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| folder.display().to_string())
}

/// The rows to show for `playlist`, given the followed folders and the
/// collapsed ones.
pub fn rows(playlist: &[PathBuf], roots: &[PathBuf], collapsed: &dyn Fn(&Path) -> bool) -> Vec<Row> {
    let mut all = Vec::new();
    let mut open: Vec<PathBuf> = Vec::new();
    for (ix, path) in playlist.iter().enumerate() {
        let folders = chain(path, roots);
        let same = open.iter().zip(&folders).take_while(|(a, b)| a == b).count();
        open.truncate(same);
        for folder in &folders[same..] {
            all.push(Row::Folder {
                path: folder.clone(),
                name: name(folder),
                depth: open.len(),
                count: 0,
                collapsed: collapsed(folder),
            });
            open.push(folder.clone());
        }
        all.push(Row::Entry { ix, depth: open.len() });
    }
    // A folder row counts the entries after it until the tree climbs back
    // out of it.
    for i in 0..all.len() {
        if let Row::Folder { depth, .. } = all[i] {
            let n = all[i + 1..]
                .iter()
                .take_while(|r| match r {
                    Row::Folder { depth: d, .. } | Row::Entry { depth: d, .. } => *d > depth,
                })
                .filter(|r| matches!(r, Row::Entry { .. }))
                .count();
            if let Row::Folder { count, .. } = &mut all[i] {
                *count = n;
            }
        }
    }
    // Hide what is under a collapsed folder.
    let mut hidden_below: Option<usize> = None;
    all.into_iter()
        .filter(|r| {
            let (depth, folds) = match r {
                Row::Folder { depth, collapsed, .. } => (*depth, *collapsed),
                Row::Entry { depth, .. } => (*depth, false),
            };
            if hidden_below.is_some_and(|d| depth > d) {
                return false;
            }
            hidden_below = folds.then_some(depth);
            true
        })
        .collect()
}

/// The folders that must be open for `path` to be visible.
pub fn ancestors_of(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    chain(path, roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn describe(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Folder { name, depth, count, collapsed, .. } => {
                    format!("{}{}{name} ({count})", "  ".repeat(*depth), if *collapsed { "+" } else { "-" })
                }
                Row::Entry { ix, depth } => format!("{}{ix}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn entries_sit_under_their_folders() {
        let playlist = [p("/m/clips/a.mp4"), p("/m/clips/b.mp4"), p("/m/clips/day2/c.mp4"), p("/m/clips/day2/x/d.mp4"), p("/elsewhere/e.mp4")];
        let rows = rows(&playlist, &[p("/m/clips")], &|_| false);
        assert_eq!(describe(&rows), ["-clips (4)", "  0", "  1", "  -day2 (2)", "    2", "    -x (1)", "      3", "4"]);
    }

    #[test]
    fn collapsed_folders_hide_what_is_under_them() {
        let playlist = [p("/m/clips/a.mp4"), p("/m/clips/day2/c.mp4"), p("/m/clips/day2/d.mp4"), p("/m/clips/z.mp4")];
        let collapsed = HashSet::from([p("/m/clips/day2")]);
        let rows = rows(&playlist, &[p("/m/clips")], &|f| collapsed.contains(f));
        assert_eq!(describe(&rows), ["-clips (4)", "  0", "  +day2 (2)", "  3"], "z.mp4 is back at the folder's own level");
        let collapsed = HashSet::from([p("/m/clips")]);
        assert_eq!(describe(&super::rows(&playlist, &[p("/m/clips")], &|f| collapsed.contains(f))), ["+clips (4)"]);
    }

    #[test]
    fn a_folder_split_by_other_entries_shows_twice() {
        // The playlist order is what plays; the tree follows it.
        let playlist = [p("/m/a/1.mp4"), p("/x/2.mp4"), p("/m/a/3.mp4")];
        assert_eq!(describe(&rows(&playlist, &[p("/m/a")], &|_| false)), ["-a (1)", "  0", "1", "-a (1)", "  2"]);
    }

    #[test]
    fn a_nested_followed_folder_uses_the_innermost_root() {
        let rows = rows(&[p("/m/a/b/1.mp4")], &[p("/m/a"), p("/m/a/b")], &|_| false);
        assert_eq!(describe(&rows), ["-b (1)", "  0"]);
        assert_eq!(ancestors_of(&p("/m/a/b/c/1.mp4"), &[p("/m/a")]), [p("/m/a"), p("/m/a/b"), p("/m/a/b/c")]);
    }
}
