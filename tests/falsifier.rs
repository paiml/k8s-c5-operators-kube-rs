// The falsifier: a test that runs but calls nothing from the crate. If coverage
// tooling ever reports this as covered, the 100% figure on tests/unit.rs means nothing.
#[test]
fn calls_nothing() {
    assert_eq!(2 + 2, 4);
}
