//! Regression test for the original failure: a small model invented
//! `wallet.cpp` instead of recognizing `Wallet.cpp` and the other real
//! C++ files. With verified context supplied, the exact names must appear
//! in the prompt, and the verifier must catch the invented name if the
//! model still produces it.

use std::path::Path;

use gico::context::{ContextEngine, EngineConfig};
use gico::verify;

fn engine() -> ContextEngine {
    ContextEngine::new(EngineConfig {
        cache_dir: None,
        ..Default::default()
    })
}

fn setup_merkle_like_project(dir: &Path) {
    for f in [
        "main.cpp",
        "Wallet.cpp",
        "OrderBook.cpp",
        "OrderBookEntry.cpp",
        "MerkelMain.cpp",
        "CSVReader.cpp",
    ] {
        std::fs::write(dir.join(f), "// source\n").unwrap();
    }
}

#[test]
fn context_supplies_exact_capitalized_filenames() {
    let tmp = tempfile::tempdir().unwrap();
    setup_merkle_like_project(tmp.path());
    let ctx = engine()
        .collect_at(tmp.path(), "compile and run this C++ project")
        .unwrap();
    let prompt = engine().render_prompt(&ctx);
    for f in [
        "main.cpp",
        "Wallet.cpp",
        "OrderBook.cpp",
        "OrderBookEntry.cpp",
        "MerkelMain.cpp",
        "CSVReader.cpp",
    ] {
        assert!(prompt.contains(f), "prompt missing {f}");
    }
    assert!(
        !prompt.contains("wallet.cpp"),
        "prompt must not contain the invented name"
    );
    // The detected context must tell the model there is no build system and a compiler exists.
    assert!(prompt.contains("Build system: none detected"));
    assert!(prompt.contains("g++") || prompt.contains("clang++") || prompt.contains("Compilers:"));
}

#[test]
fn verifier_catches_invented_wallet_cpp_from_model_output() {
    let tmp = tempfile::tempdir().unwrap();
    setup_merkle_like_project(tmp.path());
    // Simulated small-model output that repeats the original mistake.
    let bad = "g++ wallet.cpp -o output && ./output";
    let scan = engine().scan(tmp.path()).unwrap();
    let report = verify::verify(bad, tmp.path(), &scan.entries);
    assert!(report.has_errors());
    let msg = format!("{:?}", report.issues);
    assert!(msg.contains("Wallet.cpp"), "issues: {msg}");
}

#[test]
fn verifier_accepts_correct_glob_command() {
    let tmp = tempfile::tempdir().unwrap();
    setup_merkle_like_project(tmp.path());
    let good = "g++ *.cpp -o output && ./output";
    let scan = engine().scan(tmp.path()).unwrap();
    let report = verify::verify(good, tmp.path(), &scan.entries);
    assert!(!report.has_errors(), "issues: {:?}", report.issues);
}

#[test]
fn llm_output_extraction_handles_fences_and_dollar() {
    assert_eq!(
        gico::llm::extract_command("```sh\ng++ *.cpp -o output && ./output\n```"),
        "g++ *.cpp -o output && ./output"
    );
    assert_eq!(gico::llm::extract_command("$ ls -la"), "ls -la");
    assert_eq!(gico::llm::extract_command("cargo build"), "cargo build");
}
