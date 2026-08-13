use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::Value;
use url::Url;

use super::protocol::{
    Diagnostic, DiagnosticAuthority, DiagnosticCode, DiagnosticLocation, DiagnosticPosition,
    DiagnosticRange, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticSource,
    DiagnosticSummary, DiagnosticTag, MAX_DIAGNOSTIC_ITEMS, MAX_DIAGNOSTIC_MESSAGE_BYTES,
    MAX_DIAGNOSTIC_RELATED_INFORMATION,
};

pub struct NormalizedDiagnostics {
    pub items: Vec<Diagnostic>,
    pub summary: DiagnosticSummary,
}

pub struct CompilerDiagnostics {
    pub normalized: NormalizedDiagnostics,
    pub classified: bool,
}

pub fn normalize_document_report(
    result: &Value,
    file: &Path,
    workspace: &Path,
) -> Result<NormalizedDiagnostics> {
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .context("native tsgo diagnostic report omitted its kind")?;
    if kind != "full" {
        bail!("native tsgo returned unsupported diagnostic report kind {kind:?}");
    }
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .context("native tsgo diagnostic report omitted its items")?;
    let diagnostics = items
        .iter()
        .map(|item| normalize_lsp_diagnostic(item, file, workspace))
        .collect::<Result<Vec<_>>>()?;
    Ok(finalize(diagnostics))
}

pub fn parse_compiler_output(
    stdout: &str,
    stderr: &str,
    workspace: &Path,
    project: &Path,
) -> Result<CompilerDiagnostics> {
    let mut items = Vec::new();
    let mut classified = true;
    parse_compiler_stream(stdout, workspace, project, &mut items, &mut classified)?;
    parse_compiler_stream(stderr, workspace, project, &mut items, &mut classified)?;
    Ok(CompilerDiagnostics { normalized: finalize(items), classified })
}

fn parse_compiler_stream(
    output: &str,
    workspace: &Path,
    project: &Path,
    items: &mut Vec<Diagnostic>,
    classified: &mut bool,
) -> Result<()> {
    let mut active = None;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(parsed) = parse_source_diagnostic(line)? {
            finish_compiler_record(&mut active, items);
            active = Some(CompilerDiagnosticRecord {
                location: DiagnosticLocation::SourcePoint {
                    file: normalize_compiler_path(parsed.file, workspace),
                    position: DiagnosticPosition { line: parsed.line, character: parsed.character },
                },
                severity: parsed.severity,
                code: parsed.code,
                message: parsed.message.to_owned(),
                related: Vec::new(),
                related_omitted: 0,
            });
            continue;
        }
        if let Some(parsed) = parse_global_diagnostic(line)? {
            finish_compiler_record(&mut active, items);
            active = Some(CompilerDiagnosticRecord {
                location: DiagnosticLocation::Project { config: project.to_path_buf() },
                severity: parsed.severity,
                code: parsed.code,
                message: parsed.message.to_owned(),
                related: Vec::new(),
                related_omitted: 0,
            });
            continue;
        }
        if let Some(parsed) = parse_related_compiler_location(line)? {
            if let Some(record) = active.as_mut() {
                let (message, message_truncated) = bounded_message(parsed.message);
                if record.related.len() < MAX_DIAGNOSTIC_RELATED_INFORMATION {
                    record.related.push(DiagnosticRelatedInformation {
                        location: DiagnosticLocation::SourcePoint {
                            file: normalize_compiler_path(parsed.file, workspace),
                            position: DiagnosticPosition {
                                line: parsed.line,
                                character: parsed.character,
                            },
                        },
                        message,
                        message_truncated,
                    });
                } else {
                    record.related_omitted = record.related_omitted.saturating_add(1);
                }
                continue;
            }
        }
        if line.chars().next().is_some_and(char::is_whitespace) {
            if let Some(record) = active.as_mut() {
                record.message.push('\n');
                record.message.push_str(line.trim_end());
                continue;
            }
        }
        finish_compiler_record(&mut active, items);
        *classified = false;
    }
    finish_compiler_record(&mut active, items);
    Ok(())
}

struct CompilerDiagnosticRecord {
    location: DiagnosticLocation,
    severity: DiagnosticSeverity,
    code: i64,
    message: String,
    related: Vec<DiagnosticRelatedInformation>,
    related_omitted: usize,
}

fn finish_compiler_record(
    active: &mut Option<CompilerDiagnosticRecord>,
    items: &mut Vec<Diagnostic>,
) {
    let Some(record) = active.take() else {
        return;
    };
    let (message, message_truncated) = bounded_message(&record.message);
    items.push(Diagnostic {
        id: String::new(),
        authority: DiagnosticAuthority::Compiler,
        location: record.location,
        severity: record.severity,
        code: DiagnosticCode::Number { value: record.code },
        source: DiagnosticSource::Named { name: "ts".to_owned() },
        message,
        message_truncated,
        tags: Vec::new(),
        related: record.related,
        related_omitted: record.related_omitted,
    });
}

pub fn merge(parts: impl IntoIterator<Item = NormalizedDiagnostics>) -> NormalizedDiagnostics {
    let mut items = Vec::new();
    let mut observed = 0usize;
    let mut truncated_details = 0usize;
    for part in parts {
        observed = observed.saturating_add(part.summary.total);
        truncated_details = truncated_details.saturating_add(part.summary.truncated_details);
        items.extend(part.items);
    }
    let mut normalized = finalize_with_total(items, observed);
    normalized.summary.truncated_details = truncated_details;
    normalized
}

fn normalize_lsp_diagnostic(item: &Value, file: &Path, workspace: &Path) -> Result<Diagnostic> {
    let range = normalize_range(item.get("range").context("diagnostic omitted its range")?)?;
    let severity = match item.get("severity").and_then(Value::as_u64) {
        None => DiagnosticSeverity::Unspecified,
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Information,
        Some(4) => DiagnosticSeverity::Hint,
        Some(value) => DiagnosticSeverity::Unknown { value },
    };
    let code = match item.get("code") {
        None | Some(Value::Null) => DiagnosticCode::Absent,
        Some(Value::Number(value)) => DiagnosticCode::Number {
            value: value.as_i64().context("diagnostic numeric code exceeds i64")?,
        },
        Some(Value::String(value)) => DiagnosticCode::Text { value: value.clone() },
        Some(_) => bail!("diagnostic code is neither a number nor string"),
    };
    let source = match item.get("source") {
        None | Some(Value::Null) => DiagnosticSource::Absent,
        Some(Value::String(name)) => DiagnosticSource::Named { name: name.clone() },
        Some(_) => bail!("diagnostic source is not a string"),
    };
    let (message, message_truncated) = bounded_message(
        item.get("message").and_then(Value::as_str).context("diagnostic omitted its message")?,
    );
    let mut tags = item
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tag| match tag.as_u64().context("diagnostic tag is not an integer")? {
            1 => Ok(DiagnosticTag::Unnecessary),
            2 => Ok(DiagnosticTag::Deprecated),
            value => Ok(DiagnosticTag::Unknown { value }),
        })
        .collect::<Result<Vec<_>>>()?;
    tags.sort();
    tags.dedup();
    let related_items = item
        .get("relatedInformation")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let related_omitted = related_items.len().saturating_sub(MAX_DIAGNOSTIC_RELATED_INFORMATION);
    let related = related_items
        .iter()
        .take(MAX_DIAGNOSTIC_RELATED_INFORMATION)
        .map(|related| normalize_related(related, workspace))
        .collect::<Result<Vec<_>>>()?;
    Ok(Diagnostic {
        id: String::new(),
        authority: DiagnosticAuthority::LanguageService,
        location: DiagnosticLocation::SourceRange { file: public_path(workspace, file), range },
        severity,
        code,
        source,
        message,
        message_truncated,
        tags,
        related,
        related_omitted,
    })
}

fn normalize_related(item: &Value, workspace: &Path) -> Result<DiagnosticRelatedInformation> {
    let location = item.get("location").context("related diagnostic omitted its location")?;
    let uri = location
        .get("uri")
        .and_then(Value::as_str)
        .context("related diagnostic location omitted its URI")?;
    let file = Url::parse(uri)
        .with_context(|| format!("parse related diagnostic URI {uri}"))?
        .to_file_path()
        .map_err(|()| anyhow!("related diagnostic uses a non-file URI: {uri}"))?;
    let range = normalize_range(
        location.get("range").context("related diagnostic location omitted its range")?,
    )?;
    let (message, message_truncated) = bounded_message(
        item.get("message")
            .and_then(Value::as_str)
            .context("related diagnostic omitted its message")?,
    );
    Ok(DiagnosticRelatedInformation {
        location: DiagnosticLocation::SourceRange { file: public_path(workspace, &file), range },
        message,
        message_truncated,
    })
}

fn normalize_range(value: &Value) -> Result<DiagnosticRange> {
    Ok(DiagnosticRange {
        start: normalize_position(value.get("start").context("range omitted its start")?)?,
        end: normalize_position(value.get("end").context("range omitted its end")?)?,
    })
}

fn normalize_position(value: &Value) -> Result<DiagnosticPosition> {
    let line = value.get("line").and_then(Value::as_u64).context("position omitted its line")?;
    let character =
        value.get("character").and_then(Value::as_u64).context("position omitted its character")?;
    Ok(DiagnosticPosition {
        line: u32::try_from(line).context("diagnostic line exceeds u32")?.saturating_add(1),
        character: u32::try_from(character)
            .context("diagnostic character exceeds u32")?
            .saturating_add(1),
    })
}

fn finalize(items: Vec<Diagnostic>) -> NormalizedDiagnostics {
    let observed = items.len();
    finalize_with_total(items, observed)
}

fn finalize_with_total(mut items: Vec<Diagnostic>, observed: usize) -> NormalizedDiagnostics {
    items.sort_by_key(diagnostic_sort_key);
    items.dedup_by(|left, right| diagnostic_sort_key(left) == diagnostic_sort_key(right));
    let unique_total = observed.max(items.len());
    let mut summary = DiagnosticSummary { total: unique_total, ..Default::default() };
    for item in &items {
        summary.truncated_details = summary
            .truncated_details
            .saturating_add(usize::from(item.message_truncated))
            .saturating_add(item.related_omitted)
            .saturating_add(
                item.related.iter().filter(|related| related.message_truncated).count(),
            );
        match item.severity {
            DiagnosticSeverity::Error => summary.errors += 1,
            DiagnosticSeverity::Warning => summary.warnings += 1,
            DiagnosticSeverity::Information => summary.information += 1,
            DiagnosticSeverity::Hint => summary.hints += 1,
            DiagnosticSeverity::Unspecified => summary.unspecified += 1,
            DiagnosticSeverity::Unknown { .. } => summary.unknown += 1,
        }
    }
    items.truncate(MAX_DIAGNOSTIC_ITEMS);
    for (index, item) in items.iter_mut().enumerate() {
        item.id = format!("d{}", index + 1);
    }
    summary.returned = items.len();
    summary.omitted = summary.total.saturating_sub(summary.returned);
    NormalizedDiagnostics { items, summary }
}

fn diagnostic_sort_key(item: &Diagnostic) -> String {
    format!(
        "{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{}",
        item.location, item.severity, item.code, item.source, item.message
    )
}

fn public_path(workspace: &Path, file: &Path) -> PathBuf {
    file.strip_prefix(workspace).map(Path::to_path_buf).unwrap_or_else(|_| file.to_path_buf())
}

fn normalize_compiler_path(raw: &str, workspace: &Path) -> PathBuf {
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() { path } else { workspace.join(path) };
    let normalized = absolute.canonicalize().unwrap_or(absolute);
    public_path(workspace, &normalized)
}

struct SourceCompilerDiagnostic<'a> {
    file: &'a str,
    line: u32,
    character: u32,
    severity: DiagnosticSeverity,
    code: i64,
    message: &'a str,
}

struct GlobalCompilerDiagnostic<'a> {
    severity: DiagnosticSeverity,
    code: i64,
    message: &'a str,
}

struct RelatedCompilerLocation<'a> {
    file: &'a str,
    line: u32,
    character: u32,
    message: &'a str,
}

fn parse_source_diagnostic(line: &str) -> Result<Option<SourceCompilerDiagnostic<'_>>> {
    let Some((prefix, rest)) = line.rsplit_once("): ") else {
        return Ok(None);
    };
    let Some(open) = prefix.rfind('(') else {
        return Ok(None);
    };
    let Some((line_number, character)) = prefix[open + 1..].split_once(',') else {
        return Ok(None);
    };
    let Some(global) = parse_global_diagnostic(rest)? else {
        return Ok(None);
    };
    Ok(Some(SourceCompilerDiagnostic {
        file: &prefix[..open],
        line: line_number.parse().context("parse compiler diagnostic line")?,
        character: character.parse().context("parse compiler diagnostic character")?,
        severity: global.severity,
        code: global.code,
        message: global.message,
    }))
}

fn parse_global_diagnostic(line: &str) -> Result<Option<GlobalCompilerDiagnostic<'_>>> {
    let (severity, rest) = if let Some(rest) = line.strip_prefix("error TS") {
        (DiagnosticSeverity::Error, rest)
    } else if let Some(rest) = line.strip_prefix("warning TS") {
        (DiagnosticSeverity::Warning, rest)
    } else {
        return Ok(None);
    };
    let Some((code, message)) = rest.split_once(": ") else {
        return Ok(None);
    };
    Ok(Some(GlobalCompilerDiagnostic {
        severity,
        code: code.parse().context("parse compiler diagnostic code")?,
        message,
    }))
}

fn parse_related_compiler_location(line: &str) -> Result<Option<RelatedCompilerLocation<'_>>> {
    let Some((prefix, message)) = line.rsplit_once("): ") else {
        return Ok(None);
    };
    let Some(open) = prefix.rfind('(') else {
        return Ok(None);
    };
    let Some((line_number, character)) = prefix[open + 1..].split_once(',') else {
        return Ok(None);
    };
    let Ok(line_number) = line_number.parse() else {
        return Ok(None);
    };
    let Ok(character) = character.parse() else {
        return Ok(None);
    };
    Ok(Some(RelatedCompilerLocation {
        file: &prefix[..open],
        line: line_number,
        character,
        message,
    }))
}

fn bounded_message(value: &str) -> (String, bool) {
    if value.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return (value.to_owned(), false);
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES.saturating_sub('…'.len_utf8());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (format!("{}…", &value[..end]), true)
}
