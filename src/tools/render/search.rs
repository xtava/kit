use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use ignore::WalkBuilder;
use tokio::sync::Notify;

use crate::tui::{Frecency, FuzzyIndex, SearchMode, Suggestion};

pub(crate) struct SearchIndex {
    index: FuzzyIndex<FileEntry>,
}

impl SearchIndex {
    pub(crate) fn discover(root: &Path, wake: Arc<Notify>) -> Self {
        let mut index = FuzzyIndex::new(SearchMode::Path, move || wake.notify_one());
        index.replace(indexed_entries(discover_entries(root)));
        Self { index }
    }

    pub(crate) fn refresh(&mut self, root: &Path) -> usize {
        self.index.replace(indexed_entries(discover_entries(root)));
        self.index.len()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn suggestions(
        &mut self,
        query: &str,
        current: Option<&Path>,
        show_git_ignored: bool,
        frecency: &Frecency<PathBuf>,
    ) -> Option<Vec<Suggestion>> {
        let needle = query.strip_prefix("./").unwrap_or(query);
        if needle.is_empty() {
            return Some(self.frecent_suggestions(current, show_git_ignored, frecency));
        }

        let mut ranked = self
            .index
            .search(needle)?
            .into_iter()
            .filter(|matched| show_git_ignored || !matched.item.ignored)
            .map(|matched| {
                let frequency = frecency.score(&matched.item.path);
                (matched.score, frequency, matched.item)
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.display.cmp(&right.2.display))
        });
        Some(
            ranked.into_iter().map(|(_, _, entry)| suggestion(&entry, current, frecency)).collect(),
        )
    }

    fn frecent_suggestions(
        &self,
        current: Option<&Path>,
        show_git_ignored: bool,
        frecency: &Frecency<PathBuf>,
    ) -> Vec<Suggestion> {
        let mut entries = self
            .index
            .items()
            .filter(|entry| show_git_ignored || !entry.ignored)
            .map(|entry| (frecency.score(&entry.path), entry))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| left.1.display.cmp(&right.1.display))
        });
        entries.into_iter().map(|(_, entry)| suggestion(entry, current, frecency)).collect()
    }

    #[cfg(test)]
    fn with_entries(entries: Vec<FileEntry>) -> Self {
        let mut index = FuzzyIndex::new(SearchMode::Path, || {});
        index.replace(indexed_entries(entries));
        Self { index }
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::with_entries(Vec::new())
    }
}

fn indexed_entries(entries: Vec<FileEntry>) -> impl Iterator<Item = (FileEntry, String)> {
    entries.into_iter().map(|entry| {
        let text = entry.display.clone();
        (entry, text)
    })
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: PathBuf,
    display: String,
    bytes: u64,
    ignored: bool,
}

impl FileEntry {
    fn new(root: &Path, path: PathBuf, ignored: bool) -> Self {
        let display = display_path(root, &path);
        let bytes = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Self { path, display, bytes, ignored }
    }
}

fn discover_entries(root: &Path) -> Vec<FileEntry> {
    let mut entries = WalkBuilder::new(root)
        .follow_links(false)
        .standard_filters(true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| is_markdown(entry.path()))
        .map(|entry| FileEntry::new(root, entry.into_path(), false))
        .collect::<Vec<_>>();

    let mut known = entries.iter().map(|entry| entry.path.clone()).collect::<HashSet<_>>();
    for relative in git_ignored_markdown(root) {
        let path = root.join(relative);
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        if !path.starts_with(root)
            || !path.is_file()
            || !is_markdown(&path)
            || !known.insert(path.clone())
        {
            continue;
        }
        entries.push(FileEntry::new(root, path, true));
    }

    entries.sort_by(|left, right| left.display.to_lowercase().cmp(&right.display.to_lowercase()));
    entries
}

fn suggestion(
    entry: &FileEntry,
    current: Option<&Path>,
    frecency: &Frecency<PathBuf>,
) -> Suggestion {
    let mut hint = Vec::with_capacity(4);
    if entry.ignored {
        hint.push("ignored".to_owned());
    }
    if current == Some(entry.path.as_path()) {
        hint.push("open".to_owned());
    }
    if let Some(frequency) = frecency.describe(&entry.path) {
        hint.push(frequency);
    }
    hint.push(format_bytes(entry.bytes));
    Suggestion::new(entry.display.clone(), hint.join(" · "))
}

fn git_ignored_markdown(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            ":(icase)*.md",
            ":(icase)*.markdown",
            ":(icase)*.mdown",
            ":(icase)*.mkd",
            ":(icase)*.mdx",
        ])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .map(PathBuf::from)
        .collect()
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase),
        Some(extension)
            if matches!(extension.as_str(), "md" | "markdown" | "mdown" | "mkd" | "mdx")
    )
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes >= MIB as u64 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= KIB as u64 {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("kit-render-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn refresh_rebuilds_the_discovered_file_set() {
        let root = temp_dir("index-refresh");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("one.md"), "# One").unwrap();
        let mut index = SearchIndex::discover(&root, Arc::new(Notify::new()));
        assert_eq!(index.len(), 1);

        std::fs::remove_file(root.join("one.md")).unwrap();
        std::fs::write(root.join("two.md"), "# Two").unwrap();
        assert_eq!(index.refresh(&root), 1);
        assert_eq!(index.index.items().next().unwrap().display, "two.md");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_classifies_gitignored_markdown() {
        let root = temp_dir("ignored-discovery");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let status = Command::new("git").args(["init", "--quiet"]).current_dir(&root).status();
        assert!(status.is_ok_and(|status| status.success()));
        std::fs::write(root.join("README.md"), "# Read me").unwrap();
        std::fs::write(root.join("docs/guide.markdown"), "# Guide").unwrap();
        std::fs::write(root.join("ignored.md"), "# Ignore me").unwrap();
        std::fs::write(root.join("notes.txt"), "not Markdown").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.md\n").unwrap();

        let index = SearchIndex::discover(&root, Arc::new(Notify::new()));
        let paths = index.index.items().map(|entry| entry.display.as_str()).collect::<Vec<_>>();
        assert_eq!(paths, vec!["docs/guide.markdown", "ignored.md", "README.md"]);
        assert!(index.index.items().find(|entry| entry.display == "ignored.md").unwrap().ignored);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_query_is_ordered_by_frecency() {
        let first = FileEntry {
            path: PathBuf::from("/alpha.md"),
            display: "alpha.md".to_owned(),
            bytes: 1,
            ignored: false,
        };
        let second = FileEntry {
            path: PathBuf::from("/zeta.md"),
            display: "zeta.md".to_owned(),
            bytes: 1,
            ignored: false,
        };
        let mut index = SearchIndex::with_entries(vec![first, second]);
        let mut frecency = Frecency::default();
        frecency.record(PathBuf::from("/zeta.md"));

        let suggestions = index.suggestions("", None, true, &frecency).unwrap();
        assert_eq!(suggestions[0].insert, "zeta.md");
        assert!(suggestions[0].hint.contains("1×"));
    }
}
