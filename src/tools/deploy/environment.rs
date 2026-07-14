use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Validated dotenv values for one Target, with secret values redacted from Debug output.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct TargetEnvironment {
    values: BTreeMap<String, String>,
}

/// Loaded Target environments keyed by stable Target ID.
#[derive(Clone, Default)]
pub struct TargetEnvironments {
    targets: BTreeMap<String, TargetEnvironment>,
}

#[derive(Debug, Error)]
pub enum EnvironmentError {
    #[error("read dotenv file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse dotenv file {} at line {line}: {message}", path.display())]
    Parse { path: PathBuf, line: usize, message: String },
}

#[derive(Debug, Error)]
#[error("line {line}: {message}")]
pub struct DotenvParseError {
    pub line: usize,
    pub message: String,
}

impl TargetEnvironment {
    pub fn load(path: &Path) -> Result<Self, EnvironmentError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|source| EnvironmentError::Read { path: path.to_path_buf(), source })?;
        parse_dotenv(&raw).map_err(|error| EnvironmentError::Parse {
            path: path.to_path_buf(),
            line: error.line,
            message: error.message,
        })
    }

    pub fn resolve(&self, name: &str) -> Option<String> {
        self.resolve_with(name, |name| std::env::var(name))
    }

    pub fn child_values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().filter_map(|(name, value)| {
            std::env::var_os(name).is_none().then_some((name.as_str(), value.as_str()))
        })
    }

    fn resolve_with<F>(&self, name: &str, process_value: F) -> Option<String>
    where
        F: FnOnce(&str) -> Result<String, std::env::VarError>,
    {
        match process_value(name) {
            Ok(value) => (!value.trim().is_empty()).then_some(value),
            Err(std::env::VarError::NotPresent) => {
                self.values.get(name).filter(|value| !value.trim().is_empty()).cloned()
            }
            Err(std::env::VarError::NotUnicode(_)) => None,
        }
    }
}

impl fmt::Debug for TargetEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TargetEnvironment")
            .field("value_count", &self.values.len())
            .finish()
    }
}

impl TargetEnvironments {
    pub fn insert(&mut self, target_id: String, environment: TargetEnvironment) {
        self.targets.insert(target_id, environment);
    }

    pub fn get(&self, target_id: &str) -> Option<&TargetEnvironment> {
        self.targets.get(target_id)
    }
}

impl fmt::Debug for TargetEnvironments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TargetEnvironments")
            .field("target_count", &self.targets.len())
            .finish()
    }
}

pub fn parse_dotenv(raw: &str) -> Result<TargetEnvironment, DotenvParseError> {
    let mut values = BTreeMap::new();
    for (index, raw_line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_name, raw_value)) = line.split_once('=') else {
            return Err(parse_error(line_number, "expected KEY=VALUE"));
        };
        let name = raw_name.trim();
        if !valid_name(name) {
            return Err(parse_error(
                line_number,
                "key must start with a letter or '_' and contain only letters, numbers, or '_'",
            ));
        }
        let value = parse_value(raw_value.trim(), line_number)?;
        values.insert(name.to_owned(), value);
    }
    Ok(TargetEnvironment { values })
}

fn parse_value(value: &str, line: usize) -> Result<String, DotenvParseError> {
    if value.contains('\0') {
        return Err(parse_error(line, "value must not contain a NUL byte"));
    }
    let first = value.chars().next();
    let last = value.chars().last();
    match (first, last) {
        (Some(quote @ ('\'' | '"')), Some(end)) if quote == end && value.len() >= 2 => {
            Ok(value[quote.len_utf8()..value.len() - end.len_utf8()].to_owned())
        }
        (Some('\'' | '"'), _) | (_, Some('\'' | '"')) => {
            Err(parse_error(line, "value has unmatched surrounding quote"))
        }
        _ => Ok(value.to_owned()),
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_error(line: usize, message: &str) -> DotenvParseError {
    DotenvParseError { line, message: message.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_blanks_whitespace_and_quotes() -> Result<(), DotenvParseError> {
        let environment = parse_dotenv(
            "\n# deployment secrets\n PLAIN = value \nDOUBLE=\"two words\"\nSINGLE='three words'\nEMPTY=\n",
        )?;

        assert_eq!(environment.values.get("PLAIN").map(String::as_str), Some("value"));
        assert_eq!(environment.values.get("DOUBLE").map(String::as_str), Some("two words"));
        assert_eq!(environment.values.get("SINGLE").map(String::as_str), Some("three words"));
        assert_eq!(environment.values.get("EMPTY").map(String::as_str), Some(""));
        Ok(())
    }

    #[test]
    fn process_environment_overrides_file_value() -> Result<(), DotenvParseError> {
        let environment = parse_dotenv("TOKEN=file-token")?;
        let resolved = environment.resolve_with("TOKEN", |_| Ok("process-token".to_owned()));

        assert_eq!(resolved.as_deref(), Some("process-token"));
        Ok(())
    }

    #[test]
    fn rejects_malformed_lines_and_unmatched_quotes() {
        let malformed = parse_dotenv("MISSING_EQUALS");
        let unmatched = parse_dotenv("TOKEN='open");

        assert!(malformed.is_err_and(|error| error.line == 1));
        assert!(unmatched.is_err_and(|error| error.message.contains("unmatched")));
    }

    #[test]
    fn missing_file_error_names_the_path() {
        let path = std::env::temp_dir()
            .join(format!("kit-deploy-env-missing-{}-placeholder.env", std::process::id()));
        let error = TargetEnvironment::load(&path);

        assert!(error.is_err_and(|error| error.to_string().contains(&path.display().to_string())));
    }
}
