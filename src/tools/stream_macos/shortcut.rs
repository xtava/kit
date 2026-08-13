use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use directories::BaseDirs;
use serde::Serialize;
use serde_json::{json, Value};

use crate::framework::AtomicFileWriter;

pub(super) const SHORTCUT_LABEL: &str = "Kit Stream Slot · Cmd+Shift+M";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ShortcutStatus {
    Installed,
    Missing,
}

struct LoadedConfig {
    raw: Vec<u8>,
    document: Value,
}

pub(super) fn status(executable: &Path) -> Result<ShortcutStatus> {
    if !executable.is_file() {
        return Ok(ShortcutStatus::Missing);
    }
    let path = karabiner_path()?;
    if !path.exists() {
        return Ok(ShortcutStatus::Missing);
    }
    let loaded = load_path(&path)?;
    let profiles = profiles(&loaded.document)?;
    let selected = selected_profile_index(profiles)?;
    let desired = managed_rule(executable);
    let locations = managed_rule_locations(&loaded.document);
    let installed = locations.len() == 1
        && locations[0].0 == selected
        && profiles[selected]
            .pointer("/complex_modifications/rules")
            .and_then(Value::as_array)
            .and_then(|rules| rules.get(locations[0].1))
            == Some(&desired);
    Ok(if installed { ShortcutStatus::Installed } else { ShortcutStatus::Missing })
}

pub(super) fn install(executable: &Path) -> Result<bool> {
    validate_executable(executable)?;
    let path = karabiner_path()?;
    let directory = path.parent().context("Karabiner config has no parent directory")?;
    let writer = AtomicFileWriter::new(directory, ".kit-stream-shortcut.lock", ".kit-stream");
    let _lock = writer.lock()?;
    let mut loaded = load_path(&path)?;
    let desired = managed_rule(executable);
    let profiles = profiles(&loaded.document)?;
    let selected_index = selected_profile_index(profiles)?;
    let managed = managed_rule_locations(&loaded.document);
    if managed.len() == 1
        && managed[0].0 == selected_index
        && profiles[selected_index]
            .pointer("/complex_modifications/rules")
            .and_then(Value::as_array)
            .and_then(|rules| rules.get(managed[0].1))
            == Some(&desired)
    {
        return Ok(false);
    }

    let selected_rules = profiles[selected_index]
        .pointer("/complex_modifications/rules")
        .and_then(Value::as_array)
        .context("selected Karabiner profile has no complex_modifications.rules array")?;
    if selected_rules.iter().filter(|rule| !is_managed_rule(rule)).any(rule_captures_shortcut) {
        bail!("Cmd+Shift+M is already captured by another Karabiner rule");
    }

    let insertion_index = selected_rules.iter().position(is_managed_rule);
    let mutable_profiles = profiles_mut(&mut loaded.document)?;
    for profile in mutable_profiles.iter_mut() {
        if let Some(rules) =
            profile.pointer_mut("/complex_modifications/rules").and_then(Value::as_array_mut)
        {
            rules.retain(|rule| !is_managed_rule(rule));
        }
    }
    let selected_rules = mutable_profiles[selected_index]
        .pointer_mut("/complex_modifications/rules")
        .and_then(Value::as_array_mut)
        .context("selected Karabiner profile has no complex_modifications.rules array")?;
    let insertion_index = insertion_index.unwrap_or(selected_rules.len()).min(selected_rules.len());
    selected_rules.insert(insertion_index, desired);

    create_backup_once(&path, &loaded.raw)?;
    save_locked(&writer, &path, &loaded.raw, &loaded.document)?;
    Ok(true)
}

pub(super) fn remove() -> Result<bool> {
    let path = karabiner_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let directory = path.parent().context("Karabiner config has no parent directory")?;
    let writer = AtomicFileWriter::new(directory, ".kit-stream-shortcut.lock", ".kit-stream");
    let _lock = writer.lock()?;
    let mut loaded = load_path(&path)?;
    let mut removed = false;
    for profile in profiles_mut(&mut loaded.document)? {
        if let Some(rules) =
            profile.pointer_mut("/complex_modifications/rules").and_then(Value::as_array_mut)
        {
            let before = rules.len();
            rules.retain(|rule| !is_managed_rule(rule));
            removed |= rules.len() != before;
        }
    }
    if removed {
        create_backup_once(&path, &loaded.raw)?;
        save_locked(&writer, &path, &loaded.raw, &loaded.document)?;
    }
    Ok(removed)
}

fn managed_rule(executable: &Path) -> Value {
    let executable_text = executable.to_string_lossy();
    let executable = shell_words::quote(&executable_text);
    let log_path = shortcut_log_path();
    let log_text = log_path.to_string_lossy();
    let log = shell_words::quote(&log_text);
    json!({
        "description": SHORTCUT_LABEL,
        "manipulators": [{
            "type": "basic",
            "from": {
                "key_code": "m",
                "modifiers": {
                    "mandatory": ["left_command", "left_shift"],
                    "optional": ["caps_lock"]
                }
            },
            "to": [{
                "shell_command": format!("{executable} stream toggle > {log} 2>&1"),
                "repeat": false
            }]
        }]
    })
}

fn rule_captures_shortcut(rule: &Value) -> bool {
    rule["manipulators"]
        .as_array()
        .is_some_and(|manipulators| manipulators.iter().any(manipulator_captures_shortcut))
}

fn manipulator_captures_shortcut(manipulator: &Value) -> bool {
    let from = &manipulator["from"];
    let captures_m = from["key_code"] == "m" || from["any"] == "key_code";
    if !captures_m {
        return false;
    }
    let mandatory = modifier_values(from.pointer("/modifiers/mandatory"));
    let optional = modifier_values(from.pointer("/modifiers/optional"));
    let mut consumes_command = false;
    let mut consumes_shift = false;
    for modifier in mandatory {
        match modifier {
            "left_command" | "left_gui" | "command" => consumes_command = true,
            "left_shift" | "shift" => consumes_shift = true,
            "caps_lock" => {}
            _ => return false,
        }
    }
    let optional_any = optional.contains(&"any");
    let allows_command = optional_any
        || optional
            .iter()
            .any(|modifier| matches!(*modifier, "left_command" | "left_gui" | "command"));
    let allows_shift =
        optional_any || optional.iter().any(|modifier| matches!(*modifier, "left_shift" | "shift"));
    (consumes_command || allows_command) && (consumes_shift || allows_shift)
}

fn modifier_values(value: Option<&Value>) -> Vec<&str> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
}

fn managed_rule_locations(document: &Value) -> Vec<(usize, usize)> {
    document["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(profile_index, profile)| {
            profile
                .pointer("/complex_modifications/rules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(move |(rule_index, rule)| {
                    is_managed_rule(rule).then_some((profile_index, rule_index))
                })
        })
        .collect()
}

fn is_managed_rule(rule: &Value) -> bool {
    rule["description"] == SHORTCUT_LABEL
}

fn profiles(document: &Value) -> Result<&Vec<Value>> {
    document["profiles"].as_array().context("Karabiner config has no profiles array")
}

fn profiles_mut(document: &mut Value) -> Result<&mut Vec<Value>> {
    document["profiles"].as_array_mut().context("Karabiner config has no profiles array")
}

fn selected_profile_index(profiles: &[Value]) -> Result<usize> {
    profiles
        .iter()
        .position(|profile| profile["selected"] == true)
        .context("Karabiner config has no selected profile")
}

fn validate_executable(executable: &Path) -> Result<()> {
    if !executable.is_absolute() {
        bail!("the Kit executable used by Karabiner must be absolute");
    }
    if !executable.is_file() {
        bail!("Kit executable {} does not exist", executable.display());
    }
    Ok(())
}

fn load_path(path: &Path) -> Result<LoadedConfig> {
    ensure_regular_config(path)?;
    let raw = std::fs::read(path)
        .with_context(|| format!("read Karabiner configuration {}", path.display()))?;
    let document =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(LoadedConfig { raw, document })
}

fn ensure_regular_config(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect Karabiner configuration {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("Karabiner configuration {} must not be a symlink", path.display());
    }
    if !metadata.is_file() {
        bail!("Karabiner configuration {} is not a regular file", path.display());
    }
    Ok(())
}

fn save_locked(
    writer: &AtomicFileWriter,
    path: &Path,
    original: &[u8],
    document: &Value,
) -> Result<()> {
    ensure_regular_config(path)?;
    let current = std::fs::read(path)
        .with_context(|| format!("re-read Karabiner configuration {}", path.display()))?;
    if current != original {
        bail!("Karabiner configuration changed during installation; no changes were written");
    }
    let mut bytes = serde_json::to_vec_pretty(document).context("serialize Karabiner config")?;
    bytes.push(b'\n');
    writer.replace(path, &bytes)?;
    Ok(())
}

fn create_backup_once(path: &Path, raw: &[u8]) -> Result<()> {
    let backup = path.with_file_name("karabiner.json.kit-stream-backup");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = match options.open(&backup) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("create {}", backup.display())),
    };
    if let Err(error) = file.write_all(raw).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&backup);
        return Err(error).with_context(|| format!("write {}", backup.display()));
    }
    if let Some(directory) = backup.parent() {
        std::fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync {}", directory.display()))?;
    }
    Ok(())
}

fn karabiner_path() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("karabiner/karabiner.json"));
    }
    Ok(BaseDirs::new()
        .context("resolve home directory")?
        .home_dir()
        .join(".config/karabiner/karabiner.json"))
}

fn shortcut_log_path() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join("Library/Logs/kit-stream-shortcut.log"))
        .unwrap_or_else(|| PathBuf::from("/tmp/kit-stream-shortcut.log"))
}
