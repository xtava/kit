use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::model::{Catalog, InvalidSkill, Skill, SkillName};

const MAX_LIBRARY_ENTRIES: usize = 4_096;
const MAX_SKILL_BYTES: u64 = 1_048_576;

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

pub(super) fn load_catalog(library: &Path) -> Result<Catalog> {
    let library = library
        .canonicalize()
        .with_context(|| format!("resolve Skills library {}", library.display()))?;
    let metadata = fs::metadata(&library)
        .with_context(|| format!("inspect Skills library {}", library.display()))?;
    if !metadata.is_dir() {
        bail!("Skills library is not a directory: {}", library.display());
    }

    let mut candidates = Vec::new();
    let mut invalid = Vec::new();
    let mut entry_count = 0;
    for entry in fs::read_dir(&library)
        .with_context(|| format!("read Skills library {}", library.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", library.display()))?;
        entry_count += 1;
        if entry_count > MAX_LIBRARY_ENTRIES {
            bail!(
                "Skills library contains more than {MAX_LIBRARY_ENTRIES} entries: {}",
                library.display()
            );
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspect Skills library entry {}", entry.path().display()))?;
        if file_type.is_dir() || file_type.is_symlink() {
            candidates.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
                file_type,
            ));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let mut skills = Vec::new();
    for (directory, path, file_type) in candidates {
        let result = if file_type.is_symlink() {
            Err(anyhow::anyhow!("canonical skill entries must be real directories, not symlinks"))
        } else {
            load_skill(&path, &directory)
        };
        match result {
            Ok(skill) => skills.push(skill),
            Err(error) => {
                invalid.push(InvalidSkill { directory, path, error: format!("{error:#}") })
            }
        }
    }
    invalid.sort_by(|left, right| left.directory.cmp(&right.directory));

    Ok(Catalog::new(library, skills, invalid))
}

pub(super) fn create_skill(library: &Path, name: SkillName, description: &str) -> Result<Skill> {
    let library = library
        .canonicalize()
        .with_context(|| format!("resolve Skills library {}", library.display()))?;
    validate_description(description)?;

    let path = library.join(name.as_str());
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("skill {:?} already exists at {}", name.as_str(), path.display());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create skill directory {}", path.display()));
        }
    }

    let skill_file = path.join("SKILL.md");
    let result = write_new_skill(&skill_file, &name, description)
        .and_then(|()| load_skill(&path, name.as_str()));
    match result {
        Ok(skill) => Ok(skill),
        Err(error) => {
            let remove_file_error = match fs::remove_file(&skill_file) {
                Ok(()) => None,
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => None,
                Err(cleanup) => Some(cleanup),
            };
            let remove_dir_error = fs::remove_dir(&path).err();
            match (remove_file_error, remove_dir_error) {
                (None, None) => Err(error),
                (file, directory) => Err(error).context(format!(
                    "cleanup failed after skill creation error (file: {}; directory: {})",
                    cleanup_label(file),
                    cleanup_label(directory)
                )),
            }
        }
    }
}

fn cleanup_label(error: Option<std::io::Error>) -> String {
    error.map_or_else(|| "ok".to_owned(), |error| error.to_string())
}

fn write_new_skill(path: &Path, name: &SkillName, description: &str) -> Result<()> {
    let quoted_description =
        serde_json::to_string(description.trim()).context("encode skill description")?;
    let source = format!(
        "---\nname: {}\ndescription: {}\n---\n\n# {}\n\nDescribe how to use this skill.\n",
        name.as_str(),
        quoted_description,
        name.as_str()
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(source.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    file.sync_all().with_context(|| format!("sync {}", path.display()))?;
    Ok(())
}

fn load_skill(path: &Path, directory: &str) -> Result<Skill> {
    let directory_name = SkillName::parse(directory.to_owned())
        .with_context(|| format!("invalid canonical skill directory name {directory:?}"))?;
    let skill_file = path.join("SKILL.md");
    let metadata = fs::symlink_metadata(&skill_file)
        .with_context(|| format!("inspect required {}", skill_file.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("required {} must be a real file", skill_file.display());
    }
    if metadata.len() > MAX_SKILL_BYTES {
        bail!(
            "{} is {} bytes; the Skills manager limit is {MAX_SKILL_BYTES} bytes",
            skill_file.display(),
            metadata.len()
        );
    }

    let mut source = String::new();
    File::open(&skill_file)
        .with_context(|| format!("open {}", skill_file.display()))?
        .take(MAX_SKILL_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("read UTF-8 skill document {}", skill_file.display()))?;
    if source.len() as u64 > MAX_SKILL_BYTES {
        bail!(
            "{} grew beyond the {MAX_SKILL_BYTES}-byte limit while reading",
            skill_file.display()
        );
    }

    let normalized = source.replace("\r\n", "\n");
    let (frontmatter, markdown) = split_document(&normalized)?;
    let frontmatter: Frontmatter = yaml_serde::from_str(frontmatter)
        .with_context(|| format!("parse YAML frontmatter in {}", skill_file.display()))?;
    let name = SkillName::parse(frontmatter.name)
        .with_context(|| format!("invalid frontmatter name in {}", skill_file.display()))?;
    if name != directory_name {
        bail!(
            "frontmatter name {:?} must match parent directory {:?}",
            name.as_str(),
            directory_name.as_str()
        );
    }
    validate_description(&frontmatter.description)?;
    if let Some(compatibility) = &frontmatter.compatibility {
        if compatibility.chars().count() > 500 {
            bail!("frontmatter compatibility must not exceed 500 characters");
        }
    }
    if frontmatter.license.as_deref().is_some_and(|value| value.trim().is_empty()) {
        bail!("frontmatter license must not be empty when present");
    }
    if frontmatter.allowed_tools.as_deref().is_some_and(|value| value.trim().is_empty()) {
        bail!("frontmatter allowed-tools must not be empty when present");
    }
    let _ = frontmatter.metadata;

    Ok(Skill::new(name, frontmatter.description, path.to_path_buf(), markdown.to_owned()))
}

fn split_document(source: &str) -> Result<(&str, &str)> {
    let source = source.strip_prefix("\u{feff}").unwrap_or(source);
    let rest = source
        .strip_prefix("---\n")
        .context("SKILL.md must begin with a YAML frontmatter delimiter (`---`)")?;
    if let Some(index) = rest.find("\n---\n") {
        return Ok((&rest[..index], &rest[index + 5..]));
    }
    if let Some(frontmatter) = rest.strip_suffix("\n---") {
        return Ok((frontmatter, ""));
    }
    bail!("SKILL.md frontmatter is missing its closing `---` delimiter")
}

fn validate_description(description: &str) -> Result<()> {
    let count = description.chars().count();
    if description.trim().is_empty() || count > 1_024 {
        bail!("skill description must contain between 1 and 1024 characters");
    }
    Ok(())
}
