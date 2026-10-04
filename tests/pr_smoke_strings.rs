//! Smoke tests for common string operations.

#[test]
fn starts_with_detects_prefix() {
    assert!("kit cdp".starts_with("kit"));
}

#[test]
fn trim_removes_surrounding_whitespace() {
    assert_eq!("  hello  ".trim(), "hello");
}

#[test]
fn split_collects_expected_parts() {
    let parts: Vec<&str> = "a,b,c".split(',').collect();
    assert_eq!(parts, vec!["a", "b", "c"]);
}
