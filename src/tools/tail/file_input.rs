use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PastedInput {
    Text(String),
    Files(Vec<PathBuf>),
    Ambiguous { raw: String, existing: Vec<PathBuf>, missing: Vec<String> },
}

pub fn classify(raw: String) -> PastedInput {
    let candidates = tokens(&raw);
    if candidates.is_empty() {
        return PastedInput::Text(raw);
    }
    let mut existing = Vec::new();
    let mut missing = Vec::new();
    for candidate in candidates {
        match path_from_token(&candidate) {
            Some(path) if path.is_file() => existing.push(path),
            _ => missing.push(candidate),
        }
    }
    match (existing.is_empty(), missing.is_empty()) {
        (false, true) => PastedInput::Files(existing),
        (true, _) => PastedInput::Text(raw),
        (false, false) => PastedInput::Ambiguous { raw, existing, missing },
    }
}

fn tokens(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut characters = trimmed.chars().peekable();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Some(active), current) if current == active => quote = None,
            (None, '\'' | '"') if token.is_empty() => quote = Some(character),
            (None, current) if current.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            (None, '\\') if characters.peek().is_some_and(|next| next.is_whitespace()) => {
                token.push(characters.next().expect("peeked escaped whitespace"));
            }
            (_, current) => token.push(current),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn path_from_token(token: &str) -> Option<PathBuf> {
    if token.starts_with("file://") {
        return url::Url::parse(token).ok()?.to_file_path().ok();
    }
    Some(PathBuf::from(token))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn multiline_words_remain_text() {
        assert_eq!(classify("hello\nworld".into()), PastedInput::Text("hello\nworld".into()));
    }

    #[test]
    fn quoted_and_file_url_paths_are_files() {
        let temp = test_directory();
        let first = temp.join("a b.txt");
        let second = temp.join("c.txt");
        fs::write(&first, "a").unwrap();
        fs::write(&second, "c").unwrap();
        let raw = format!("'{}' {}", first.display(), url::Url::from_file_path(&second).unwrap());
        assert_eq!(classify(raw), PastedInput::Files(vec![first, second]));
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn mixed_paths_require_a_decision() {
        let temp = test_directory();
        let path = temp.join("exists");
        fs::write(&path, "x").unwrap();
        assert!(matches!(
            classify(format!("{} missing", path.display())),
            PastedInput::Ambiguous { .. }
        ));
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn tokenization_preserves_windows_paths_and_unix_escaped_spaces() {
        assert_eq!(
            tokens(r#""C:\Users\Ari\My File.txt" C:\tmp\other.txt"#),
            [r#"C:\Users\Ari\My File.txt"#, r#"C:\tmp\other.txt"#]
        );
        assert_eq!(tokens(r#"/tmp/My\ File.txt"#), ["/tmp/My File.txt"]);
    }

    fn test_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kit-tail-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }
}
