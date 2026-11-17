//! Project detection tests: C/C++, Make, CMake, Cargo, npm/pnpm/yarn/bun,
//! Go, Python, empty dirs, and build-command prioritization.

use std::path::Path;

use gico::context::{ContextEngine, EngineConfig};

fn engine() -> ContextEngine {
    ContextEngine::new(EngineConfig {
        cache_dir: None,
        ..Default::default()
    })
}

fn write(dir: &Path, name: &str, contents: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, contents).unwrap();
}

fn project(dir: &Path) -> Option<gico::context::ProjectInfo> {
    engine().collect_at(dir, "build").unwrap().project
}

#[test]
fn single_file_cpp_no_build_system_suggests_compiler() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "int main(){}");
    let p = project(tmp.path()).expect("should detect C++");
    assert_eq!(p.kind, "C/C++");
    assert_eq!(p.build_system, None);
    let build = p.build_command.expect("compiler available in test env");
    assert!(
        build.contains("main.cpp") || build.contains("*.cpp"),
        "build: {build}"
    );
    assert!(build.starts_with("g++") || build.starts_with("clang++") || build.starts_with("c++"));
}

#[test]
fn multi_file_cpp_detects_all_sources_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    for f in [
        "main.cpp",
        "Wallet.cpp",
        "OrderBook.cpp",
        "OrderBookEntry.cpp",
        "MerkelMain.cpp",
        "CSVReader.cpp",
    ] {
        write(tmp.path(), f, "// x");
    }
    let ctx = engine()
        .collect_at(tmp.path(), "compile and run this C++ project")
        .unwrap();
    let p = ctx.project.unwrap();
    assert_eq!(p.kind, "C/C++");
    let listing: Vec<&str> = ctx.files.iter().map(|f| f.path.as_str()).collect();
    for f in [
        "main.cpp",
        "Wallet.cpp",
        "OrderBook.cpp",
        "OrderBookEntry.cpp",
        "MerkelMain.cpp",
        "CSVReader.cpp",
    ] {
        assert!(listing.contains(&f), "missing {f} in {listing:?}");
    }
}

#[test]
fn makefile_project_prefers_make() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.c", "int main(){}");
    write(tmp.path(), "Makefile", "all:\n\tgcc main.c -o app\n");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.build_system.as_deref(), Some("Make"));
    assert_eq!(p.build_command.as_deref(), Some("make"));
}

#[test]
fn cmake_project_prefers_cmake() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.10)\nproject(x)\n",
    );
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.build_system.as_deref(), Some("CMake"));
    let b = p.build_command.unwrap();
    assert!(b.contains("cmake --build"), "build: {b}");
}

#[test]
fn cargo_project_prefers_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    );
    write(tmp.path(), "src/main.rs", "fn main(){}");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.kind, "Rust");
    assert_eq!(p.build_system.as_deref(), Some("Cargo"));
    assert_eq!(p.build_command.as_deref(), Some("cargo build"));
    assert_eq!(p.run_command.as_deref(), Some("cargo run"));
}

#[test]
fn npm_project_uses_npm_and_scripts() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"x","scripts":{"build":"tsc","dev":"tsc -w"}}"#,
    );
    write(tmp.path(), "package-lock.json", "{}");
    write(tmp.path(), "index.ts", "console.log(1)");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.build_system.as_deref(), Some("npm"));
    assert_eq!(p.build_command.as_deref(), Some("npm run build"));
    assert_eq!(p.run_command.as_deref(), Some("npm run dev"));
}

#[test]
fn package_manager_detected_from_lockfiles() {
    for (lock, pm) in [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"x","scripts":{"build":"tsc"}}"#,
        );
        write(tmp.path(), lock, "{}");
        let p = project(tmp.path()).unwrap();
        assert_eq!(p.build_system.as_deref(), Some(pm), "lockfile {lock}");
        assert!(p.build_command.as_deref().unwrap().starts_with(pm));
    }
}

#[test]
fn go_project_detected() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "go.mod", "module example.com/x\n\ngo 1.22\n");
    write(tmp.path(), "main.go", "package main\nfunc main(){}");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.kind, "Go");
    assert_eq!(p.build_command.as_deref(), Some("go build ./..."));
}

#[test]
fn python_project_run_command_uses_real_file() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "requirements.txt", "requests\n");
    write(tmp.path(), "main.py", "print('hi')");
    write(tmp.path(), "util.py", "x=1");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.kind, "Python");
    assert_eq!(p.run_command.as_deref(), Some("python3 main.py"));
}

#[test]
fn python_single_file_run_command() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "script.py", "print(1)");
    let p = project(tmp.path()).unwrap();
    assert_eq!(p.run_command.as_deref(), Some("python3 script.py"));
}

#[test]
fn empty_dir_has_no_project() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(project(tmp.path()).is_none());
}

#[test]
fn heavy_dirs_are_not_scanned() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "node_modules/dep/index.js", "x");
    write(tmp.path(), "target/debug/app", "x");
    write(tmp.path(), "main.rs", "fn main(){}");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    );
    let ctx = engine().collect_at(tmp.path(), "build").unwrap();
    let listing = serde_json::to_string(&ctx.files).unwrap();
    assert!(!listing.contains("node_modules"));
    assert!(!listing.contains("target/debug"));
    assert!(ctx.project.is_some());
}
