//! The deterministic injection detector against a corpus of real cases
//! and real READMEs. Prints the detection and false-positive rates.

use std::path::Path;

use x0ne_orchestrator::injection_guard::{rule_list, scan};

fn corpus(kind: &str) -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/injection-corpus")
        .join(kind);
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            // Line 1 cites the source; it is not part of the sample.
            let body = text
                .split_once('\n')
                .map(|(_, b)| b)
                .unwrap_or("")
                .to_string();
            (p.file_name().unwrap().to_string_lossy().into_owned(), body)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn the_corpus_is_detected_without_false_positives() {
    let inject = corpus("inject");
    let legit = corpus("legit");
    assert!(inject.len() >= 15 && legit.len() >= 15, "corpus too small");

    let mut missed = Vec::new();
    for (name, body) in &inject {
        let found = scan(body);
        if found.is_empty() {
            missed.push(name.clone());
        } else {
            println!("hit  {name}: {}", rule_list(&found));
        }
    }
    let mut false_pos = Vec::new();
    for (name, body) in &legit {
        let found = scan(body);
        if !found.is_empty() {
            println!("FP   {name}: {} {:?}", rule_list(&found), found[0].excerpt);
            false_pos.push(name.clone());
        }
    }
    println!(
        "detection {}/{} ; false positives {}/{}",
        inject.len() - missed.len(),
        inject.len(),
        false_pos.len(),
        legit.len()
    );
    assert!(missed.is_empty(), "missed: {missed:?}");
    assert!(false_pos.is_empty(), "false positives: {false_pos:?}");
}
