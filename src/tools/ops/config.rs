use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::onepassword::{EnvironmentFileError, OpEnvironment};

pub const PROJECT_CONFIG: &str = ".kit/ops.toml";
pub const SCHEMA_VERSION: u32 = 3;
const NO_MASKING_ENV: &str = "OP_RUN_NO_MASKING";

pub struct LoadedConfig {
    pub path: PathBuf,
    pub base_dir: PathBuf,
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
    pub(super) working_dir: Option<PathBuf>,
    pub(super) env_file: PathBuf,
    pub(super) command: CommandSpec,
    #[serde(default)]
    pub(super) parameters: Vec<ParameterSpec>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub(super) program: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct ParameterSpec {
    pub(super) name: String,
    pub(super) environment: String,
    #[serde(flatten)]
    pub(super) kind: ParameterKind,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ParameterKind {
    Email,
    Integer {
        #[serde(default)]
        minimum: Option<i64>,
        #[serde(default)]
        maximum: Option<i64>,
    },
    String {
        #[serde(default = "default_minimum_length")]
        minimum_length: usize,
        #[serde(default = "default_maximum_length")]
        maximum_length: usize,
    },
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
            .field("has_working_dir", &self.working_dir.is_some())
            .field("command", &self.command)
            .field("parameter_count", &self.parameters.len())
            .field("environment", &"<refs-only file>")
            .finish()
    }
}

impl fmt::Debug for ParameterSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParameterSpec")
            .field("name", &self.name)
            .field("environment", &self.environment)
            .field("kind", &self.kind)
            .finish()
    }
}

impl fmt::Debug for ParameterKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Email => formatter.write_str("Email"),
            Self::Integer { .. } => formatter.write_str("Integer"),
            Self::String { .. } => formatter.write_str("String"),
        }
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
    #[error("load env_file for operation '{operation}': {source}")]
    Environment {
        operation: String,
        #[source]
        source: EnvironmentFileError,
    },
    #[error("operation '{operation}' env_file must contain only op:// references")]
    LiteralEnvironment { operation: String },
}

#[derive(Debug, Error)]
pub enum ParameterError {
    #[error("operation '{operation}' requires --input-json with: {required}")]
    MissingInput { operation: String, required: String },
    #[error("operation '{operation}' does not declare public parameters")]
    UnexpectedInput { operation: String },
    #[error("parse public operation input: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("public operation input must be one JSON object")]
    NotObject,
    #[error("operation input contains undeclared parameter '{0}'")]
    Undeclared(String),
    #[error("operation input is missing parameter '{0}'")]
    Missing(String),
    #[error("operation parameter '{parameter}' {message}")]
    Invalid { parameter: String, message: String },
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
        let base_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or(project_dir);
        Ok(Self { path, base_dir, config })
    }

    pub fn environment(&self, operation: &Operation) -> Result<OpEnvironment, ConfigError> {
        let path = if operation.env_file.is_absolute() {
            operation.env_file.clone()
        } else {
            self.base_dir.join(&operation.env_file)
        };
        let environment =
            OpEnvironment::load(&path).map_err(|source| ConfigError::Environment {
                operation: operation.id.clone(),
                source,
            })?;
        if !environment.is_references_only() {
            return Err(ConfigError::LiteralEnvironment {
                operation: operation.id.clone(),
            });
        }
        Ok(environment)
    }

    pub fn working_directory(&self, operation: &Operation) -> PathBuf {
        match operation.working_dir.as_deref() {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => self.base_dir.join(path),
            None => self.base_dir.clone(),
        }
    }
}

impl OpsConfig {
    pub fn operation(&self, id: &str) -> Option<&Operation> {
        self.ops.iter().find(|operation| operation.id == id)
    }
}

impl Operation {
    pub fn resolve_parameters(
        &self,
        input: Option<&[u8]>,
    ) -> Result<BTreeMap<String, String>, ParameterError> {
        if self.parameters.is_empty() {
            return match input {
                Some(_) => {
                    Err(ParameterError::UnexpectedInput { operation: self.id.clone() })
                }
                None => Ok(BTreeMap::new()),
            };
        }
        let input = input.ok_or_else(|| ParameterError::MissingInput {
            operation: self.id.clone(),
            required: self
                .parameters
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        })?;
        let value = serde_json::from_slice::<Value>(input)?;
        let object = value.as_object().ok_or(ParameterError::NotObject)?;
        for name in object.keys() {
            if !self.parameters.iter().any(|parameter| parameter.name == *name) {
                return Err(ParameterError::Undeclared(name.clone()));
            }
        }
        let mut environment = BTreeMap::new();
        for parameter in &self.parameters {
            let value = object
                .get(&parameter.name)
                .ok_or_else(|| ParameterError::Missing(parameter.name.clone()))?;
            environment.insert(
                parameter.environment.clone(),
                parameter.kind.resolve(&parameter.name, value)?,
            );
        }
        Ok(environment)
    }
}

impl ParameterKind {
    fn resolve(&self, parameter: &str, value: &Value) -> Result<String, ParameterError> {
        match self {
            Self::Email => {
                let value = required_string(parameter, value)?;
                let normalized = value.trim().to_lowercase();
                if !valid_email(&normalized) {
                    return Err(invalid_parameter(
                        parameter,
                        "must be a valid email address",
                    ));
                }
                Ok(normalized)
            }
            Self::Integer { minimum, maximum } => {
                let value = value
                    .as_i64()
                    .ok_or_else(|| invalid_parameter(parameter, "must be a JSON integer"))?;
                if minimum.is_some_and(|minimum| value < minimum)
                    || maximum.is_some_and(|maximum| value > maximum)
                {
                    return Err(invalid_parameter(
                        parameter,
                        "is outside its configured range",
                    ));
                }
                Ok(value.to_string())
            }
            Self::String { minimum_length, maximum_length } => {
                let value = required_string(parameter, value)?;
                let length = value.chars().count();
                if length < *minimum_length || length > *maximum_length {
                    return Err(invalid_parameter(
                        parameter,
                        "is outside its configured length range",
                    ));
                }
                Ok(value.to_owned())
            }
        }
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
        if operation.env_file.as_os_str().to_string_lossy().trim().is_empty() {
            issues.push(format!("- {label}.env_file must not be empty"));
        }
        if operation
            .working_dir
            .as_ref()
            .is_some_and(|path| path.as_os_str().to_string_lossy().trim().is_empty())
        {
            issues.push(format!("- {label}.working_dir must not be empty"));
        }
        let mut parameter_names = HashSet::new();
        let mut parameter_environments = HashSet::new();
        for (parameter_index, parameter) in operation.parameters.iter().enumerate() {
            let parameter_label = format!("{label}.parameters[{parameter_index}]");
            if !valid_id(&parameter.name) {
                issues.push(format!("- {parameter_label}.name must be a valid parameter id"));
            } else if !parameter_names.insert(parameter.name.as_str()) {
                issues.push(format!(
                    "- {label}.parameters contains duplicate name '{}'",
                    parameter.name
                ));
            }
            if !valid_environment_name(&parameter.environment) {
                issues.push(format!(
                    "- {parameter_label}.environment must be a valid environment name"
                ));
            } else if parameter.environment.eq_ignore_ascii_case(NO_MASKING_ENV) {
                issues.push(format!(
                    "- {parameter_label}.environment must not set {NO_MASKING_ENV}"
                ));
            } else if !parameter_environments.insert(parameter.environment.as_str()) {
                issues.push(format!(
                    "- {label}.parameters contains duplicate environment '{}'",
                    parameter.environment
                ));
            }
            match &parameter.kind {
                ParameterKind::Integer { minimum: Some(minimum), maximum: Some(maximum) }
                    if minimum > maximum =>
                {
                    issues.push(format!(
                        "- {parameter_label} minimum must not exceed maximum"
                    ));
                }
                ParameterKind::String { minimum_length, maximum_length }
                    if minimum_length > maximum_length || *maximum_length > 65_536 =>
                {
                    issues.push(format!("- {parameter_label} length bounds are invalid"));
                }
                ParameterKind::Email
                | ParameterKind::Integer { .. }
                | ParameterKind::String { .. } => {}
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

fn valid_email(value: &str) -> bool {
    if value.len() > 320 || value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && domain.len() <= 253
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

fn required_string<'a>(parameter: &str, value: &'a Value) -> Result<&'a str, ParameterError> {
    value
        .as_str()
        .ok_or_else(|| invalid_parameter(parameter, "must be a JSON string"))
}

fn invalid_parameter(parameter: &str, message: &str) -> ParameterError {
    ParameterError::Invalid { parameter: parameter.to_owned(), message: message.to_owned() }
}

const fn default_minimum_length() -> usize {
    1
}

const fn default_maximum_length() -> usize {
    4096
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
version = 2

[[ops]]
id = "deploy-marketing"
env_file = "production.env"
command = { program = "kit", args = ["deploy", "marketing"] }
"#;

    fn temp_root(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("kit-ops-config-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn parses_refs_only_environment_operations() -> Result<(), ConfigError> {
        let config = parse_and_validate(VALID, PathBuf::from("ops.toml"))?;
        let operation = config.operation("deploy-marketing").expect("operation");

        assert_eq!(operation.command.program, "kit");
        assert_eq!(operation.command.args, ["deploy", "marketing"]);
        assert_eq!(operation.env_file, PathBuf::from("production.env"));
        Ok(())
    }

    #[test]
    fn rejects_masking_opt_outs() {
        let flag_opt_out =
            VALID.replace("[\"deploy\", \"marketing\"]", "[\"deploy\", \"--no-masking\"]");

        assert!(parse_and_validate(&flag_opt_out, PathBuf::from("ops.toml"))
            .expect_err("masking flag opt-out must fail")
            .to_string()
            .contains("--no-masking"));
    }

    #[test]
    fn debug_never_prints_command_args_or_reference_strings() -> Result<(), ConfigError> {
        let raw = VALID
            .replace("\"marketing\"", "\"argument-sentinel\"")
            .replace("production.env", "sensitive-reference-catalog.env");
        let config = parse_and_validate(&raw, PathBuf::from("ops.toml"))?;
        let debug = format!("{config:?}");
        let operation_debug = format!("{:?}", config.operation("deploy-marketing").unwrap());

        assert!(!debug.contains("sensitive-reference-catalog"));
        assert!(!operation_debug.contains("argument-sentinel"));
        assert!(!operation_debug.contains("sensitive-reference-catalog"));
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
