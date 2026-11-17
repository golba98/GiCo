//! Deterministic command-planning + intent-aware validation regression tests.

use std::path::Path;

use gico::context::{ContextEngine, EngineConfig};
use gico::intent::{self, Intent};
use gico::plan;
use gico::verify::{self, Verdict};

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

fn plan_in(dir: &Path, request: &str) -> Option<plan::Plan> {
    let raw = engine().scan(dir).unwrap();
    plan::plan(&raw, request)
}

// --- CMake ---------------------------------------------------------------

#[test]
fn cmake_unconfigured_build_and_run_uses_declared_target() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.25)\nproject(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    let p = plan_in(tmp.path(), "compile and run this C++ project").expect("plan");
    assert_eq!(
        p.command, "cmake -S . -B build && cmake --build build && ./build/myapp",
        "got: {}",
        p.command
    );
    assert!(p.note.is_none());
}

#[test]
fn cmake_configured_build_dir_is_reused() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    // existing configured build tree with the built executable
    write(tmp.path(), "build/CMakeCache.txt", "# cache");
    let exe = tmp.path().join("build/myapp");
    std::fs::write(&exe, "binary").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let p = plan_in(tmp.path(), "compile and run this C++ project").expect("plan");
    assert_eq!(
        p.command, "cmake --build build && ./build/myapp",
        "got: {}",
        p.command
    );
}

#[test]
fn cmake_library_only_project_builds_but_explains_no_run() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_library(mylib STATIC lib.cpp)\n",
    );
    write(tmp.path(), "lib.cpp", "int f(){}");
    let p = plan_in(tmp.path(), "compile and run this C++ project").expect("plan");
    assert_eq!(p.command, "cmake -S . -B build && cmake --build build");
    let note = p.note.expect("library-only note");
    assert!(note.contains("no executable target"), "{note}");
}

#[test]
fn cmake_build_only_request_does_not_append_run() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    let p = plan_in(tmp.path(), "build this project").unwrap();
    assert_eq!(p.command, "cmake -S . -B build && cmake --build build");
}

#[test]
fn shell_quote_handles_spaces_and_apostrophes() {
    use gico::context::fs::shell_quote;
    assert_eq!(shell_quote("build"), "build");
    assert_eq!(shell_quote("my app"), "'my app'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
    assert_eq!(shell_quote("a/b-c_d.e"), "a/b-c_d.e");
}

#[test]
fn cmake_detection_beats_stray_python_scripts() {
    // The 4-IT Pixel Forger failure mode: CMake project with scripts/*.py
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(app main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(tmp.path(), "scripts/check.py", "print(1)");
    let raw = engine().scan(tmp.path()).unwrap();
    let p = raw.project.expect("project detected");
    assert_eq!(p.kind, "C/C++", "misdetected as {}", p.kind);
    assert_eq!(p.build_system.as_deref(), Some("CMake"));
    assert_eq!(p.executable_targets, vec!["app".to_string()]);
}

// --- other project types ---------------------------------------------------

#[test]
fn cargo_build_and_run() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    );
    write(tmp.path(), "src/main.rs", "fn main(){}");
    let p = plan_in(tmp.path(), "compile and run this rust project").unwrap();
    assert_eq!(p.command, "cargo build && cargo run");
}

#[test]
fn make_build_and_run_uses_run_target() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "Makefile",
        "all:\n\tgcc main.c -o app\nrun:\n\t./app\n",
    );
    write(tmp.path(), "main.c", "int main(){}");
    let p = plan_in(tmp.path(), "build and run this project").unwrap();
    assert_eq!(p.command, "make && make run");
}

#[test]
fn npm_build_and_run() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"x","scripts":{"build":"tsc","start":"node i.js"}}"#,
    );
    write(tmp.path(), "package-lock.json", "{}");
    let p = plan_in(tmp.path(), "build and run this project").unwrap();
    assert_eq!(p.command, "npm run build && npm start");
}

#[test]
fn python_run_uses_real_entrypoint() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "requirements.txt", "requests\n");
    write(tmp.path(), "main.py", "print(1)");
    let p = plan_in(tmp.path(), "run this python project").unwrap();
    assert_eq!(p.command, "python3 main.py");
}

#[test]
fn plain_cpp_fallback_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "int main(){}");
    write(tmp.path(), "Wallet.cpp", "// w");
    let p = plan_in(tmp.path(), "compile and run this C++ project").unwrap();
    assert_eq!(p.command, "g++ *.cpp -o output && ./output");
}

#[test]
fn unknown_intent_returns_no_plan() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "int main(){}");
    assert!(plan_in(tmp.path(), "what files are here").is_none());
}

// --- intent-aware validation ----------------------------------------------

#[test]
fn cd_command_is_rejected_for_build_request() {
    let issues = verify::intent_issues(
        "compile and run this C++ project",
        "cd /home/example/Development/5-C++/4-IT Pixel Forger",
    );
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0].message.contains("irrelevant"),
        "{:?}",
        issues[0].message
    );
}

#[test]
fn bare_shell_is_rejected_for_build_request() {
    let issues = verify::intent_issues("compile and run this C++ project", "sh");
    assert!(!issues.is_empty());
    assert!(issues[0].message.contains("irrelevant"));
}

#[test]
fn unrelated_command_is_rejected_for_build_request() {
    let issues = verify::intent_issues("compile and run this C++ project", "ls -la");
    assert!(
        !issues.is_empty(),
        "ls -la must not satisfy a build request"
    );
    assert!(issues[0].message.contains("does not address the request"));
}

#[test]
fn proper_build_command_passes_intent_check() {
    assert!(verify::intent_issues(
        "compile and run this C++ project",
        "cmake -S . -B build && cmake --build build && ./build/it-pixel-cli"
    )
    .is_empty());
    assert!(verify::intent_issues(
        "compile and run this C++ project",
        "g++ *.cpp -o output && ./output"
    )
    .is_empty());
}

#[test]
fn non_build_requests_are_not_intent_checked() {
    assert!(verify::intent_issues("list all files", "ls -la").is_empty());
    assert!(verify::intent_issues("what is this directory", "pwd").is_empty());
}

// --- end-to-end verify of planned commands ---------------------------------

#[test]
fn planned_cmake_command_verifies_clean() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    let raw = engine().scan(tmp.path()).unwrap();
    let p = plan::plan(&raw, "compile and run this project").unwrap();
    let mut rep = verify::verify(&p.command, tmp.path(), &raw.entries);
    rep.issues.extend(verify::intent_issues(
        "compile and run this C++ project",
        &p.command,
    ));
    assert_eq!(rep.verdict(), Verdict::Valid, "issues: {:?}", rep.issues);
}

#[test]
fn detect_intent_mapping() {
    assert_eq!(
        intent::classify_str("compile and run this C++ project"),
        Intent::BuildAndRun
    );
    assert_eq!(intent::classify_str("build the project"), Intent::Build);
    assert_eq!(intent::classify_str("run the app"), Intent::Run);
    assert_eq!(intent::classify_str("run the tests"), Intent::Test);
    assert_eq!(intent::classify_str("clean the build"), Intent::Clean);
    assert_eq!(
        intent::classify_str("show me the files here"),
        Intent::ListDir
    );
    assert_eq!(
        intent::classify_str("find all cpp files"),
        Intent::SearchFiles
    );
    assert_eq!(
        intent::classify_str("what's using port 3000"),
        Intent::InspectPort
    );
    assert_eq!(
        intent::classify_str("show recent git changes"),
        Intent::GitDiff
    );
    assert_eq!(
        intent::classify_str("find the largest files"),
        Intent::LargestFiles
    );
    assert_eq!(
        intent::classify_str("how much disk space is left"),
        Intent::DiskUsage
    );
    assert_eq!(
        intent::classify_str("how do I install htop"),
        Intent::InstallPackage
    );
    assert_eq!(intent::classify_str("florb the widget"), Intent::Ambiguous);
}
