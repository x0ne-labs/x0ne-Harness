use toy::store::Store;

#[test]
fn test_insert_then_get() {
    let mut s = Store::default();
    let id = s.insert("x".into());
    assert_eq!(s.get(id).unwrap(), "x");
}

#[test]
fn test_remove_returns_value() {
    let mut s = Store::default();
    let id = s.insert("y".into());
    assert_eq!(s.remove(id), "y");
}
