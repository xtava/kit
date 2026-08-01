use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::SecretReference;

/// Validated masking-on `op run` values with contents redacted from Debug output.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct OpEnvironment {
    values: BTreeMap<String, OpEnvironmentValue>,
}

#[derive(Clone, Eq, PartialEq)]
enum OpEnvironmentValue {
    Literal(String),
    Reference(SecretReference),
}

#[derive(Debug, Error)]
pub enum EnvironmentFileError {
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

impl OpEnvironment {
    pub fn load(path: &Path) -> Result<Self, EnvironmentFileError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|source| EnvironmentFileError::Read { path: path.to_path_buf(), source })?;
        parse_dotenv(&raw).map_err(|error| EnvironmentFileError::Parse {
            path: path.to_path_buf(),
            line: error.line,
            message: error.message,
        })
    }

    /// Literal values absent from the parent environment are passed to `op run`. References are
    /// kept out of the process environment and written only to its scoped refs file.
    pub fn child_values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().filter_map(|(name, value)| match value {
            OpEnvironmentValue::Literal(value) if std::env::var_os(name).is_none() => {
                Some((name.as_str(), value.as_str()))
            }
            OpEnvironmentValue::Literal(_) | OpEnvironmentValue::Reference(_) => None,
        })
    }

    pub fn references(&self) -> BTreeMap<String, SecretReference> {
        self.values
            .iter()
            .filter_map(|(name, value)| match value {
                OpEnvironmentValue::Reference(reference) => {
                    Some((name.clone(), reference.clone()))
                }
                OpEnvironmentValue::Literal(_) => None,
            })
            .collect()
    }

    pub fn is_references_only(&self) -> bool {
        !self.values.is_empty()
            && self
                .values
                .values()
                .all(|value| matches!(value, OpEnvironmentValue::Reference(_)))
    }

    /// Return exactly one configured reference for an in-process API consumer.
    pub fn reference(&self, name: &str) -> Option<&SecretReference> {
        match self.values.get(name) {
            Some(OpEnvironmentValue::Reference(reference)) => Some(reference),
            Some(OpEnvironmentValue::Literal(_)) | None => None,
        }
    }
}

impl fmt::Debug for OpEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpEnvironment")
            .field("value_count", &self.values.len())
            .field(
                "reference_count",
                &self
                    .values
                    .values()
                    .filter(|value| matches!(value, OpEnvironmentValue::Reference(_)))
                    .count(),
            )
            .finish()
    }
}

pub fn parse_dotenv(raw: &str) -> Result<OpEnvironment, DotenvParseError> {
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
        let value = if value.starts_with("op://") {
            OpEnvironmentValue::Reference(SecretReference::new(value).map_err(|error| {
                parse_error(line_number, &format!("invalid 1Password reference: {error}"))
            })?)
        } else {
            OpEnvironmentValue::Literal(value)
        };
        values.insert(name.to_owned(), value);
    }
    Ok(OpEnvironment { values })
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

        let literal = |name| match environment.values.get(name) {
            Some(OpEnvironmentValue::Literal(value)) => Some(value.as_str()),
            Some(OpEnvironmentValue::Reference(_)) | None => None,
        };
        assert_eq!(literal("PLAIN"), Some("value"));
        assert_eq!(literal("DOUBLE"), Some("two words"));
        assert_eq!(literal("SINGLE"), Some("three words"));
        assert_eq!(literal("EMPTY"), Some(""));
        Ok(())
    }

    #[test]
    fn separates_literal_child_values_from_onepassword_references(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let environment =
            parse_dotenv("PLAIN=value\nOTHER=two words\nTOKEN=op://Deploy/example/token")?;

        assert_eq!(
            environment.child_values().collect::<Vec<_>>(),
            [("OTHER", "two words"), ("PLAIN", "value")]
        );
        assert_eq!(
            environment.references().get("TOKEN").map(SecretReference::as_str),
            Some("op://Deploy/example/token")
        );
        Ok(())
    }

    #[test]
    fn debug_reports_shape_without_environment_values_or_reference_paths(
    ) -> Result<(), DotenvParseError> {
        let environment = parse_dotenv(
            "PLAIN=literal-value-sentinel\nTOKEN=op://SensitiveVault/SensitiveItem/password",
        )?;
        let debug = format!("{environment:?}");

        assert!(debug.contains("value_count: 2"));
        assert!(debug.contains("reference_count: 1"));
        assert!(!debug.contains("literal-value-sentinel"));
        assert!(!debug.contains("SensitiveVault"));
        assert!(!debug.contains("SensitiveItem"));
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
        let error = OpEnvironment::load(&path);

        assert!(error.is_err_and(|error| error.to_string().contains(&path.display().to_string())));
    }
}
