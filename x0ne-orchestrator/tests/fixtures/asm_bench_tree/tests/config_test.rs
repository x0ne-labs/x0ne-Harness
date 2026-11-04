use toy::config::Config;

#[test]
fn test_load_reads_pairs() {
    let dir = std::env::temp_dir().join("toy-config");
    std::fs::write(&dir, "a = 1\nb = 2\n").unwrap();
    let c = Config::load(dir.to_str().unwrap());
    assert_eq!(c.get("a"), "1");
}

#[test]
fn test_missing_key_panics() {
    // TODO: should return Option
}
