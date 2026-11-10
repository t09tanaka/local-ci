use shard_example::add;

#[test]
fn adds() {
    assert_eq!(add(2, 2), 4);
}

#[test]
fn fails_on_purpose_when_asked() {
    if std::env::var_os("SHARD_EXAMPLE_FAIL").is_some() {
        panic!("asked to fail");
    }
}
