//! Smoke tests for iterator adapters.

#[test]
fn filter_keeps_even_numbers() {
    let evens: Vec<i32> = (1..=6).filter(|n| n % 2 == 0).collect();
    assert_eq!(evens, vec![2, 4, 6]);
}

#[test]
fn sum_totals_range() {
    let total: i32 = (1..=4).sum();
    assert_eq!(total, 10);
}

#[test]
fn any_finds_matching_element() {
    let values = [3, 5, 8, 13];
    assert!(values.iter().any(|n| n % 2 == 0));
}
