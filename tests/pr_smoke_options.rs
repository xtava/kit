//! Smoke tests for Option and Result handling.

#[test]
fn unwrap_or_returns_default_for_none() {
    let value: Option<i32> = None;
    assert_eq!(value.unwrap_or(7), 7);
}

#[test]
fn map_skips_none() {
    let value: Option<i32> = None;
    assert_eq!(value.map(|n| n * 2), None);
}

#[test]
fn result_ok_is_ok() {
    let result: Result<u8, &str> = Ok(200);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 200);
}
