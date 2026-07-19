use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

use crate::onepassword::SecretReference;

pub const PROJECT_CONFIG: &str = ".kit/ops.toml";
pub const SCHEMA_VERSION: u32 = 1;
const NO_MASKING_ENV: &str = "OP_RUN_NO_MASKING";

pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: OpsConfig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsConfig {
    pub(super) version: u32,
    pub(super) ops: Vec<Operation>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub(super) id: String,
    pub(super) command: CommandSpec,
    #[serde(default)]
    pub(super) refs: BTreeMap<String, SecretReference>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub(super) program: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
}

impl fmt::Debug for LoadedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedConfig")
            .field("path", &self.path)
            .field("config", &self.config)
            .finish()
    }
}

impl fmt::Debug for OpsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpsConfig")
            .field("version", &self.version)
            .field("operation_count", &self.ops.len())
            .finish()
    }
}

impl fmt::Debug for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Operation")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("reference_count", &self.refs.len())
            .finish()
    }
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("arg_count", &self.args.len())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(
        "no ops config found; searched:\n{searched}\ncreate .kit/ops.toml or pass --config <path>"
    )]
    Missing { searched: String },
    #[error("read ops config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse ops config {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("invalid ops config {}:\n{}", path.display(), issues.join("\n"))]
    Invalid { path: PathBuf, issues: Vec<String> },
}

impl LoadedConfig {
    pub fn load(
        explicit: Option<PathBuf>,
        project_dir: PathBuf,
        xdg_path: PathBuf,
    ) -> Result<Self, ConfigError> {
        let path = match explicit {
            Some(path) => path,
            None => {
                let project_path = project_dir.join(PROJECT_CONFIG);
                if project_path.is_file() {
                    project_path
                } else if xdg_path.is_file() {
                    xdg_path
                } else {
                    let searched = [project_path, xdg_path]
                        .into_iter()
                        .map(|path| format!("  - {}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(ConfigError::Missing { searched });
                }
            }
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        let config = parse_and_validate(&raw, path.clone())?;
        Ok(Self { path, config })
    }
}

impl OpsConfig {
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.ops.iter().find(|operation| operation.id == id)
    }
}

fn parse_and_validate(raw: &str, path: PathBuf) -> Result<OpsConfig, ConfigError> {
    let config = toml::from_str::<OpsConfig>(raw).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        // `Display` includes a source excerpt. Keep only the structured message so an invalid
        // literal value is never copied into an error or agent transcript.
        message: source.message().to_owned(),
    })?;
    let issues = validate(&config);
    if issues.is_empty() {
        Ok(config)
    } else {
        Err(ConfigError::Invalid { path, issues })
    }
}

fn validate(config: &OpsConfig) -> Vec<String> {
    let mut issues = Vec::new();
    if config.version != SCHEMA_VERSION {
        issues.push(format!(
            "- version {} is unsupported; expected {SCHEMA_VERSION}",
            config.version
        ));
    }
    if config.ops.is_empty() {
        issues.push("- ops must contain at least one operation".to_owned());
    }

    let mut ids = HashSet::new();
    for (index, operation) in config.ops.iter().enumerate() {
        let label = format!("ops[{index}]");
        if !valid_id(&operation.id) {
            issues.push(format!("- {label}.id must use only letters, numbers, '.', '_' or '-'"));
        } else if !ids.insert(operation.id.as_str()) {
            issues.push(format!("- duplicate operation id '{}'", operation.id));
        }
        if operation.command.program.trim().is_empty() {
            issues.push(format!("- {label}.command.program must not be empty"));
        } else if is_op_program(&operation.command.program) {
            issues.push(format!(
                "- {label}.command.program must not invoke op; Kit owns the only op boundary"
            ));
        }
        if operation.command.args.iter().any(|arg| masking_opt_out(arg)) {
            issues.push(format!(
                "- {label}.command.args must not contain --no-masking or {NO_MASKING_ENV}"
            ));
        }
        for name in operation.refs.keys() {
            if !valid_environment_name(name) {
                issues.push(format!(
                    "- {label}.refs key '{name}' must start with a letter or '_' and contain only letters, numbers, or '_'"
                ));
            }
            if name.eq_ignore_ascii_case(NO_MASKING_ENV) {
                issues.push(format!("- {label}.refs must not set {NO_MASKING_ENV}"));
            }
        }
    }
    issues
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn masking_opt_out(argument: &str) -> bool {
    argument == "--no-masking"
        || argument.starts_with("--no-masking=")
        || argument
            .split_once('=')
            .map_or(argument, |(name, _)| name)
            .eq_ignore_ascii_case(NO_MASKING_ENV)
}

fn is_op_program(program: &str) -> bool {
    Path::new(program).file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.eq_ignore_ascii_case("op") || name.eq_ignore_ascii_case("op.exe")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const VALID: &str = r#"
version = 1

[[ops]]
id = "deploy-marketing"
command = { program = "kit", args = ["deploy", "marketing"] }
[ops.refs]
CLOUDFLARE_API_TOKEN = "op://Deploy/cloudflare/api_token"
"#;

    fn temp_root(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("kit-ops-config-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn parses_refs_only_operations() -> Result<(), ConfigError> {
        let config = parse_and_validate(VALID, PathBuf::from("ops.toml"))?;
        let operation = config.operation("deploy-marketing").expect("operation");

        assert_eq!(operation.command.program, "kit");
        assert_eq!(operation.command.args, ["deploy", "marketing"]);
        assert_eq!(
            operation.refs.get("CLOUDFLARE_API_TOKEN").map(SecretReference::as_str),
            Some("op://Deploy/cloudflare/api_token")
        );
        Ok(())
    }

    #[test]
    fn rejects_literal_values_and_masking_opt_outs() {
        let literal = VALID
            .replace("op://Deploy/cloudflare/api_token", "resolved-value-must-not-enter-config");
        let env_opt_out = VALID.replace("CLOUDFLARE_API_TOKEN =", "op_run_no_masking =");
        let flag_opt_out =
            VALID.replace("[\"deploy\", \"marketing\"]", "[\"deploy\", \"--no-masking\"]");

        let literal_error =
            parse_and_validate(&literal, PathBuf::from("ops.toml")).expect_err("literal must fail");
        assert!(matches!(&literal_error, ConfigError::Parse { .. }));
        assert!(!literal_error.to_string().contains("resolved-value-must-not-enter-config"));
        assert!(parse_and_validate(&env_opt_out, PathBuf::from("ops.toml"))
            .expect_err("masking environment opt-out must fail")
            .to_string()
            .contains(NO_MASKING_ENV));
        assert!(parse_and_validate(&flag_opt_out, PathBuf::from("ops.toml"))
            .expect_err("masking flag opt-out must fail")
            .to_string()
            .contains("--no-masking"));
    }

    #[test]
    fn debug_never_prints_command_args_or_reference_strings() -> Result<(), ConfigError> {
        let raw = VALID.replace("\"marketing\"", "\"argument-sentinel\"");
        let config = parse_and_validate(&raw, PathBuf::from("ops.toml"))?;
        let debug = format!("{config:?}");
        let operation_debug = format!("{:?}", config.operation("deploy-marketing").unwrap());

        assert!(!debug.contains("api_token"));
        assert!(!operation_debug.contains("argument-sentinel"));
        assert!(!operation_debug.contains("api_token"));
        Ok(())
    }

    #[test]
    fn project_config_precedes_xdg_config() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("discovery");
        let project = root.join("project");
        let project_config = project.join(PROJECT_CONFIG);
        let xdg_config = root.join("xdg/ops.toml");
        std::fs::create_dir_all(project_config.parent().expect("project parent"))?;
        std::fs::create_dir_all(xdg_config.parent().expect("xdg parent"))?;
        std::fs::write(&project_config, VALID)?;
        std::fs::write(&xdg_config, VALID.replace("deploy-marketing", "xdg"))?;

        let loaded = LoadedConfig::load(None, project, xdg_config)?;

        assert_eq!(loaded.path, project_config);
        assert!(loaded.config.operation("deploy-marketing").is_some());
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
