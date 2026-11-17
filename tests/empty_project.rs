//! Empty / non-buildable project detection: informational responses instead
//! of fabricated commands.

use std::path::Path;

use gico::context::{self, ContextEngine, EngineConfig};

fn engine() -> ContextEngine {
    ContextEngine::new(EngineConfig {
        cache_dir: None,
        ..Default::default()
    })
}

fn write(dir: &Path, name: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, "x").unwrap();
}

#[test]
fn empty_directory_is_informational() {
    let tmp = tempfile::tempdir().unwrap();
    let scan = engine().scan(tmp.path()).unwrap();
    let r =
        context::informational_response("compile and run the C++ project in this directory", &scan);
    let (title, detail) = r.expect("should short-circuit");
    assert!(title.contains("No buildable C++ project found"), "{title}");
    assert!(detail.contains("empty"), "{detail}");
}

#[test]
fn build_only_directory_is_distinguished_from_empty() {
    // The reported case: 4-IT Pixel Generator/ contains only build/src and build/tests
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("4-IT Pixel Generator");
    write(&dir, "build/src/.keep");
    write(&dir, "build/tests/.keep");
    let scan = engine().scan(&dir).unwrap();
    let (title, detail) =
        context::informational_response("Compile and run the C++ project in this directory", &scan)
            .expect("should short-circuit");
    assert!(title.contains("C++"), "{title}");
    assert!(
        detail.contains("build/"),
        "should mention excluded build dir: {detail}"
    );
}

#[test]
fn directory_with_sources_is_not_short_circuited() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp");
    let scan = engine().scan(tmp.path()).unwrap();
    assert!(context::informational_response("compile and run this C++ project", &scan).is_none());
}

#[test]
fn non_build_request_is_not_short_circuited() {
    let tmp = tempfile::tempdir().unwrap();
    let scan = engine().scan(tmp.path()).unwrap();
    assert!(context::informational_response("list files here", &scan).is_none());
    assert!(context::informational_response("what is the weather", &scan).is_none());
}

#[test]
fn only_docs_no_sources_is_informational() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "README.md");
    let scan = engine().scan(tmp.path()).unwrap();
    let (title, detail) =
        context::informational_response("build this rust project", &scan).unwrap();
    assert!(title.contains("Rust"), "{title}");
    assert!(detail.contains("No source files"), "{detail}");
}
