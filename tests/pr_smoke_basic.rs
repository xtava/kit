//! Smoke tests exercising basic arithmetic and collection behavior.
//! Self-contained on purpose so this PR is safe to open and revert.

#[test]
fn addition_is_commutative() {
    assert_eq!(2 + 3, 3 + 2);
}

#[test]
fn multiplication_identity_holds() {
    assert_eq!(42 * 1, 42);
}

#[test]
fn vec_push_grows_length() {
    let mut values = vec![1, 2, 3];
    values.push(4);
    assert_eq!(values.len(), 4);
    assert_eq!(values.last(), Some(&4));
}
