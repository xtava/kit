use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::model::{
    ChangeGroup, ChangeKind, DiffDocument, DiffInput, SourceSnapshot, SpecialState,
};

#[derive(Clone, Debug)]
struct StatusEntry {
    status: [u8; 2],
    submodule: String,
    head_oid: Option<String>,
    index_oid: Option<String>,
    path: PathBuf,
    original_path: Option<PathBuf>,
    record: RecordKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordKind {
    Ordinary,
    RenameOrCopy,
    Unmerged,
    Untracked,
}

pub fn load_repository(cwd: &Path) -> Result<Vec<DiffDocument>> {
    let root = repository_root(cwd)?;
    let output = git_output(&root, &["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
    let entries = parse_status(&output.stdout)?;
    let mut documents = Vec::new();
    for entry in entries {
        append_documents(&mut documents, &root, entry);
    }
    documents.sort_by(|left, right| {
        group_order(left.group)
            .cmp(&group_order(right.group))
            .then_with(|| left.display_path().cmp(&right.display_path()))
    });
    Ok(documents)
}

fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let output = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = trim_line_ending(&output.stdout);
    if root.is_empty() {
        bail!("git rev-parse returned an empty repository root");
    }
    path_from_bytes(root).context("decode Git repository root")
}

fn append_documents(documents: &mut Vec<DiffDocument>, root: &Path, entry: StatusEntry) {
    if entry.record == RecordKind::Unmerged {
        documents.push(DiffDocument::build(DiffInput {
            group: ChangeGroup::Changes,
            kind: ChangeKind::Conflict,
            old_path: Some(entry.path.clone()),
            new_path: Some(entry.path),
            old: SourceSnapshot::Absent,
            new: SourceSnapshot::Absent,
            special: Some(SpecialState::Conflict),
        }));
        return;
    }

    if entry.submodule.starts_with('S') {
        for (group, code) in
            [(ChangeGroup::Staged, entry.status[0]), (ChangeGroup::Changes, entry.status[1])]
        {
            if is_changed(code) {
                documents.push(DiffDocument::build(DiffInput {
                    group,
                    kind: ChangeKind::Submodule,
                    old_path: Some(entry.path.clone()),
                    new_path: Some(entry.path.clone()),
                    old: SourceSnapshot::Absent,
                    new: SourceSnapshot::Absent,
                    special: Some(SpecialState::Submodule { state: entry.submodule.clone() }),
                }));
            }
        }
        return;
    }

    if entry.record == RecordKind::Untracked {
        documents.push(DiffDocument::build(DiffInput {
            group: ChangeGroup::Changes,
            kind: ChangeKind::Untracked,
            old_path: None,
            new_path: Some(entry.path.clone()),
            old: SourceSnapshot::Absent,
            new: read_worktree(root, &entry.path),
            special: None,
        }));
        return;
    }

    if is_changed(entry.status[0]) {
        let kind = change_kind(entry.status[0]);
        let (old_path, new_path) =
            comparison_paths(kind, &entry.path, entry.original_path.as_deref());
        documents.push(DiffDocument::build(DiffInput {
            group: ChangeGroup::Staged,
            kind,
            old_path,
            new_path,
            old: read_blob(root, entry.head_oid.as_deref()),
            new: read_blob(root, entry.index_oid.as_deref()),
            special: None,
        }));
    }

    if is_changed(entry.status[1]) {
        let kind = change_kind(entry.status[1]);
        let (old_path, new_path) =
            comparison_paths(kind, &entry.path, entry.original_path.as_deref());
        documents.push(DiffDocument::build(DiffInput {
            group: ChangeGroup::Changes,
            kind,
            old_path,
            new_path,
            old: read_blob_filtered(root, entry.index_oid.as_deref(), &entry.path),
            new: if kind == ChangeKind::Deleted {
                SourceSnapshot::Absent
            } else {
                read_worktree(root, &entry.path)
            },
            special: None,
        }));
    }
}

fn comparison_paths(
    kind: ChangeKind,
    path: &Path,
    original_path: Option<&Path>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => (None, Some(path.to_path_buf())),
        ChangeKind::Deleted => (Some(path.to_path_buf()), None),
        ChangeKind::Renamed | ChangeKind::Copied => {
            (Some(original_path.unwrap_or(path).to_path_buf()), Some(path.to_path_buf()))
        }
        _ => (Some(path.to_path_buf()), Some(path.to_path_buf())),
    }
}

fn read_blob(root: &Path, oid: Option<&str>) -> SourceSnapshot {
    let Some(oid) = oid else {
        return SourceSnapshot::Absent;
    };
    match git_output(root, &["cat-file", "blob", oid]) {
        Ok(output) => SourceSnapshot::Bytes(Arc::from(output.stdout)),
        Err(error) => SourceSnapshot::Unavailable(error.to_string()),
    }
}

fn read_blob_filtered(root: &Path, oid: Option<&str>, path: &Path) -> SourceSnapshot {
    let Some(oid) = oid else {
        return SourceSnapshot::Absent;
    };
    let mut path_argument = OsString::from("--path=");
    path_argument.push(path.as_os_str());
    let arguments = [
        OsString::from("cat-file"),
        OsString::from("--filters"),
        path_argument,
        OsString::from(oid),
    ];
    match git_output_os(root, &arguments) {
        Ok(output) => SourceSnapshot::Bytes(Arc::from(output.stdout)),
        Err(error) => SourceSnapshot::Unavailable(error.to_string()),
    }
}

fn read_worktree(root: &Path, path: &Path) -> SourceSnapshot {
    let absolute = root.join(path);
    let result = fs::symlink_metadata(&absolute).and_then(|metadata| {
        if metadata.file_type().is_symlink() {
            read_link_bytes(&absolute)
        } else {
            fs::read(&absolute)
        }
    });
    match result {
        Ok(bytes) => SourceSnapshot::Bytes(Arc::from(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SourceSnapshot::Absent,
        Err(error) => SourceSnapshot::Unavailable(format!("read {}: {error}", absolute.display())),
    }
}

#[cfg(unix)]
fn read_link_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;

    fs::read_link(path).map(|target| target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn read_link_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    fs::read_link(path).map(|target| target.to_string_lossy().into_owned().into_bytes())
}

fn parse_status(input: &[u8]) -> Result<Vec<StatusEntry>> {
    let mut entries = Vec::new();
    let mut records = input.split(|byte| *byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        match record[0] {
            b'1' => entries.push(parse_ordinary(record)?),
            b'2' => {
                let original_path = records
                    .next()
                    .context("porcelain v2 rename/copy record is missing its original path")?;
                entries.push(parse_rename(record, original_path)?);
            }
            b'u' => entries.push(parse_unmerged(record)?),
            b'?' => entries.push(parse_untracked(record)?),
            b'!' => {}
            marker => bail!("unknown porcelain v2 record marker {:?}", marker as char),
        }
    }
    Ok(entries)
}

fn parse_ordinary(record: &[u8]) -> Result<StatusEntry> {
    let fields = split_fields(record, 9, "ordinary")?;
    Ok(StatusEntry {
        status: parse_status_code(fields[1])?,
        submodule: ascii(fields[2], "submodule state")?,
        head_oid: parse_oid(fields[6])?,
        index_oid: parse_oid(fields[7])?,
        path: path_from_bytes(fields[8])?,
        original_path: None,
        record: RecordKind::Ordinary,
    })
}

fn parse_rename(record: &[u8], original_path: &[u8]) -> Result<StatusEntry> {
    let fields = split_fields(record, 10, "rename/copy")?;
    Ok(StatusEntry {
        status: parse_status_code(fields[1])?,
        submodule: ascii(fields[2], "submodule state")?,
        head_oid: parse_oid(fields[6])?,
        index_oid: parse_oid(fields[7])?,
        path: path_from_bytes(fields[9])?,
        original_path: Some(path_from_bytes(original_path)?),
        record: RecordKind::RenameOrCopy,
    })
}

fn parse_unmerged(record: &[u8]) -> Result<StatusEntry> {
    let fields = split_fields(record, 11, "unmerged")?;
    Ok(StatusEntry {
        status: parse_status_code(fields[1])?,
        submodule: ascii(fields[2], "submodule state")?,
        head_oid: None,
        index_oid: None,
        path: path_from_bytes(fields[10])?,
        original_path: None,
        record: RecordKind::Unmerged,
    })
}

fn parse_untracked(record: &[u8]) -> Result<StatusEntry> {
    let fields = split_fields(record, 2, "untracked")?;
    Ok(StatusEntry {
        status: [b'?', b'?'],
        submodule: "N...".to_owned(),
        head_oid: None,
        index_oid: None,
        path: path_from_bytes(fields[1])?,
        original_path: None,
        record: RecordKind::Untracked,
    })
}

fn split_fields<'a>(record: &'a [u8], count: usize, kind: &str) -> Result<Vec<&'a [u8]>> {
    let fields: Vec<_> = record.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() != count {
        bail!("invalid porcelain v2 {kind} record: expected {count} fields, got {}", fields.len());
    }
    Ok(fields)
}

fn parse_status_code(field: &[u8]) -> Result<[u8; 2]> {
    field.try_into().map_err(|_| anyhow::anyhow!("invalid porcelain v2 status code"))
}

fn parse_oid(field: &[u8]) -> Result<Option<String>> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_hexdigit) {
        bail!("invalid Git object id {:?}", String::from_utf8_lossy(field));
    }
    if field.iter().all(|byte| *byte == b'0') {
        return Ok(None);
    }
    Ok(Some(ascii(field, "Git object id")?))
}

fn ascii(field: &[u8], name: &str) -> Result<String> {
    if !field.is_ascii() {
        bail!("non-ASCII {name} in porcelain v2 output");
    }
    Ok(String::from_utf8(field.to_vec()).expect("checked ASCII"))
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes).context("Git path is not valid UTF-8")?))
}

fn change_kind(code: u8) -> ChangeKind {
    match code {
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'T' => ChangeKind::TypeChanged,
        b'U' => ChangeKind::Conflict,
        _ => ChangeKind::Modified,
    }
}

fn is_changed(code: u8) -> bool {
    !matches!(code, b'.' | b' ')
}

fn group_order(group: ChangeGroup) -> u8 {
    match group {
        ChangeGroup::Staged => 0,
        ChangeGroup::Changes => 1,
    }
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    while bytes.last().is_some_and(|byte| matches!(byte, b'\n' | b'\r')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    let arguments: Vec<_> = args.iter().map(OsString::from).collect();
    git_output_os(cwd, &arguments)
}

fn git_output_os(cwd: &Path, args: &[OsString]) -> Result<Output> {
    let command = args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>().join(" ");
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("run git {command} in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            command,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::tools::diff::model::DiffBody;

    static NEXT_REPO: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_porcelain_v2_record_shapes_without_quoting_paths() {
        let zero = "0".repeat(40);
        let oid = "1".repeat(40);
        let input = format!(
            "1 .M N... 100644 100644 100644 {oid} {oid} path with spaces.rs\0\
             2 R. N... 100644 100644 100644 {oid} {oid} R100 renamed.rs\0old name.rs\0\
             u UU N... 100644 100644 100644 100644 {oid} {oid} {oid} conflict.rs\0\
             ? untracked file.rs\0\
             1 A. N... 000000 100644 100644 {zero} {oid} added.rs\0"
        );

        let entries = parse_status(input.as_bytes()).unwrap();

        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].path, Path::new("path with spaces.rs"));
        assert_eq!(entries[1].record, RecordKind::RenameOrCopy);
        assert_eq!(entries[1].original_path.as_deref(), Some(Path::new("old name.rs")));
        assert_eq!(entries[2].record, RecordKind::Unmerged);
        assert_eq!(entries[3].record, RecordKind::Untracked);
        assert_eq!(entries[4].head_oid, None);
        assert_eq!(change_kind(b'C'), ChangeKind::Copied);
    }

    #[test]
    fn rejects_unknown_or_truncated_porcelain_records() {
        assert!(parse_status(b"x nonsense\0").is_err());
        assert!(parse_status(b"2 R. N...\0missing-old\0").is_err());
        assert!(parse_status(b"2 R. N... 1 2 3 4 5 R100 new\0").is_err());
    }

    #[test]
    fn loads_staged_unstaged_mixed_untracked_rename_and_delete_without_mutation() {
        let repo = TestRepo::new();
        repo.write(".gitattributes", "filtered.txt text eol=crlf\n");
        repo.write("staged.txt", "base\n");
        repo.write("staged-delete.txt", "delete from index\n");
        repo.write("mixed.txt", "base\n");
        repo.write("rename.txt", "rename me\n");
        repo.write("delete.txt", "delete me\n");
        repo.write("filtered.txt", "base\nsame\n");
        repo.write("type.txt", "plain file\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "base"]);

        repo.write("staged.txt", "staged\n");
        repo.write("staged-added.txt", "new in index\n");
        repo.write("mixed.txt", "index\n");
        repo.git(&["add", "staged.txt", "staged-added.txt", "mixed.txt"]);
        repo.git(&["rm", "staged-delete.txt"]);
        repo.write("mixed.txt", "worktree\n");
        repo.git(&["mv", "rename.txt", "renamed.txt"]);
        fs::remove_file(repo.path.join("delete.txt")).unwrap();
        repo.write_bytes("filtered.txt", b"base\r\nchanged\r\n");
        fs::remove_file(repo.path.join("type.txt")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("staged.txt", repo.path.join("type.txt")).unwrap();
        repo.write("untracked file.txt", "new\n");
        repo.write("unusual\npath.txt", "new\n");

        let before = repo.evidence();
        let documents = load_repository(&repo.path).unwrap();
        let after = repo.evidence();

        assert_eq!(before, after, "loading the viewer source must be read-only");
        assert!(has(&documents, ChangeGroup::Staged, "staged.txt", ChangeKind::Modified));
        assert!(has(&documents, ChangeGroup::Staged, "staged-added.txt", ChangeKind::Added));
        assert!(has(&documents, ChangeGroup::Staged, "staged-delete.txt", ChangeKind::Deleted));
        assert!(has(&documents, ChangeGroup::Staged, "mixed.txt", ChangeKind::Modified));
        assert!(has(&documents, ChangeGroup::Changes, "mixed.txt", ChangeKind::Modified));
        assert!(has(&documents, ChangeGroup::Staged, "renamed.txt", ChangeKind::Renamed));
        assert!(has(&documents, ChangeGroup::Changes, "delete.txt", ChangeKind::Deleted));
        assert!(has(&documents, ChangeGroup::Changes, "untracked file.txt", ChangeKind::Untracked));
        assert!(has(&documents, ChangeGroup::Changes, "unusual\npath.txt", ChangeKind::Untracked));
        #[cfg(unix)]
        assert!(has(&documents, ChangeGroup::Changes, "type.txt", ChangeKind::TypeChanged));
        let filtered = documents
            .iter()
            .find(|document| {
                document.group == ChangeGroup::Changes
                    && document.display_path() == Some(Path::new("filtered.txt"))
            })
            .expect("filtered worktree document");
        let DiffBody::Text(filtered) = &filtered.body else {
            panic!("filtered worktree document should be text");
        };
        assert_eq!(filtered.old.line(0), "base\r\n");
        assert_eq!(filtered.new.line(0), "base\r\n");
        assert!(documents
            .iter()
            .all(|document| !matches!(document.body, DiffBody::Unavailable(_))));
    }

    #[test]
    fn represents_merge_conflicts_explicitly() {
        let repo = TestRepo::new();
        repo.write("conflict.txt", "base\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "base"]);
        repo.git(&["checkout", "-b", "other"]);
        repo.write("conflict.txt", "theirs\n");
        repo.git(&["commit", "-am", "theirs"]);
        repo.git(&["checkout", "master"]);
        repo.write("conflict.txt", "ours\n");
        repo.git(&["commit", "-am", "ours"]);
        assert!(!repo.git_status(&["merge", "other"]).success());

        let documents = load_repository(&repo.path).unwrap();

        assert!(has(&documents, ChangeGroup::Changes, "conflict.txt", ChangeKind::Conflict));
        assert!(documents
            .iter()
            .any(|document| matches!(document.body, DiffBody::Special(SpecialState::Conflict))));
    }

    #[test]
    fn represents_submodule_status_without_reading_nested_content() {
        let entry = StatusEntry {
            status: [b'.', b'M'],
            submodule: "S.MU".to_owned(),
            head_oid: None,
            index_oid: None,
            path: PathBuf::from("nested"),
            original_path: None,
            record: RecordKind::Ordinary,
        };
        let mut documents = Vec::new();

        append_documents(&mut documents, Path::new("/does/not/exist"), entry);

        assert_eq!(documents.len(), 1);
        assert!(matches!(
            documents[0].body,
            DiffBody::Special(SpecialState::Submodule { ref state }) if state == "S.MU"
        ));
    }

    fn has(documents: &[DiffDocument], group: ChangeGroup, path: &str, kind: ChangeKind) -> bool {
        documents.iter().any(|document| {
            document.group == group
                && document.kind == kind
                && document.display_path() == Some(Path::new(path))
        })
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Evidence {
        status: Vec<u8>,
        unstaged: Vec<u8>,
        staged: Vec<u8>,
        index: Vec<u8>,
    }

    struct TestRepo {
        path: PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kit-diff-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            let repo = Self { path };
            repo.git(&["init", "-b", "master"]);
            repo.git(&["config", "user.name", "Kit Test"]);
            repo.git(&["config", "user.email", "kit@example.test"]);
            repo
        }

        fn write(&self, path: &str, contents: &str) {
            self.write_bytes(path, contents.as_bytes());
        }

        fn write_bytes(&self, path: &str, contents: &[u8]) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn git(&self, args: &[&str]) {
            let status = self.git_status(args);
            assert!(status.success(), "git {args:?} failed");
        }

        fn git_status(&self, args: &[&str]) -> std::process::ExitStatus {
            Command::new("git")
                .args(args)
                .current_dir(&self.path)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
        }

        fn evidence(&self) -> Evidence {
            Evidence {
                status: git_output(
                    &self.path,
                    &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
                )
                .unwrap()
                .stdout,
                unstaged: git_output(&self.path, &["diff", "--binary"]).unwrap().stdout,
                staged: git_output(&self.path, &["diff", "--cached", "--binary"]).unwrap().stdout,
                index: fs::read(self.path.join(".git/index")).unwrap(),
            }
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
