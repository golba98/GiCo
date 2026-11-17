//! Intelligence-overhaul regression tests: CMake build-tree validation,
//! presets, subdir executable discovery, runtime-argument evidence, intent
//! classification, general planners, intent-aware verification and the
//! structured prompt contract.

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

fn write_exec(dir: &Path, name: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, "binary").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn plan_in(dir: &Path, request: &str) -> Option<plan::Plan> {
    let raw = engine().scan(dir).unwrap();
    plan::plan(&raw, request)
}

fn cache_txt(root: &Path, build_rel: &str, home: &str) -> String {
    format!(
        "# CMakeCache\nCMAKE_HOME_DIRECTORY:INTERNAL={home}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\nCMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_BUILD_TYPE:STRING=Release\n",
        root.join(build_rel).display()
    )
}

// --- CMake build-tree intelligence -----------------------------------------

#[test]
fn cmake_stale_cache_from_renamed_project_is_never_selected() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    // Simulate the renamed-away project: cache points at the old location.
    write(
        tmp.path(),
        "build/CMakeCache.txt",
        &cache_txt(tmp.path(), "build", "/home/user/Old Project Name"),
    );
    let p = plan_in(tmp.path(), "compile and run this project").expect("plan");
    assert!(
        !p.command.contains("--build build ") && !p.command.contains("./build/"),
        "stale 'build' tree must not be used: {}",
        p.command
    );
    assert!(
        p.command.contains("build-gico"),
        "fresh dir expected: {}",
        p.command
    );
    assert!(
        p.note.as_deref().unwrap_or("").contains("stale"),
        "stale note expected: {:?}",
        p.note
    );
}

#[test]
fn cmake_valid_custom_named_build_dir_is_preferred() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "build-gico-test/CMakeCache.txt",
        &cache_txt(
            tmp.path(),
            "build-gico-test",
            &tmp.path().display().to_string(),
        ),
    );
    write_exec(tmp.path(), "build-gico-test/myapp");
    let p = plan_in(tmp.path(), "compile and run this project").expect("plan");
    assert_eq!(
        p.command, "cmake --build build-gico-test && ./build-gico-test/myapp",
        "got: {}",
        p.command
    );
}

#[test]
fn cmake_moved_build_tree_is_detected_stale() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    // Cache claims a different build dir than where it lives.
    write(
        tmp.path(),
        "build/CMakeCache.txt",
        &format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL=/elsewhere/build\n",
            tmp.path().display()
        ),
    );
    let p = plan_in(tmp.path(), "build the project").expect("plan");
    assert!(
        p.command.contains("build-gico"),
        "moved tree must be stale: {}",
        p.command
    );
    assert!(p.note.as_deref().unwrap_or("").contains("stale"));
}

#[test]
fn cmake_presets_are_used_when_nothing_is_configured() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "CMakePresets.json",
        r#"{"version": 3, "configurePresets": [{"name": "dev", "binaryDir": "build-dev", "generator": "Ninja"}]}"#,
    );
    let p = plan_in(tmp.path(), "build the project").expect("plan");
    assert!(
        p.command.contains("cmake --preset dev"),
        "got: {}",
        p.command
    );
    assert!(
        p.command.contains("cmake --build build-dev"),
        "got: {}",
        p.command
    );
}

#[test]
fn cmake_executable_in_subdirectory_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_subdirectory(tools)\n",
    );
    write(
        tmp.path(),
        "tools/CMakeLists.txt",
        "add_executable(mytool main.cpp)\n",
    );
    write(tmp.path(), "tools/main.cpp", "int main(){}");
    write(
        tmp.path(),
        "build/CMakeCache.txt",
        &cache_txt(tmp.path(), "build", &tmp.path().display().to_string()),
    );
    write_exec(tmp.path(), "build/tools/mytool");
    let p = plan_in(tmp.path(), "run this").expect("plan");
    assert!(
        p.command.contains("./build/tools/mytool"),
        "subdir executable must be found: {}",
        p.command
    );
}

#[test]
fn cmake_library_only_project_explains_no_executable() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_library(mylib STATIC lib.cpp)\n",
    );
    write(tmp.path(), "lib.cpp", "int f(){}");
    let p = plan_in(tmp.path(), "compile and run this project").expect("plan");
    assert!(
        p.command.starts_with("cmake"),
        "build expected: {}",
        p.command
    );
    assert!(
        !p.command.contains("&& ./"),
        "no run step for library: {}",
        p.command
    );
    assert!(p
        .note
        .as_deref()
        .unwrap_or("")
        .contains("no executable target"));
}

// --- Runtime-argument intelligence -----------------------------------------

#[test]
fn build_and_run_uses_verified_sample_input_from_readme() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "README.md",
        "# MyApp\n\nUsage: myapp <input-image> [options]\n\nExample:\n    build/myapp assets/samples/tester.png --output out.png\n",
    );
    write(tmp.path(), "assets/samples/tester.png", "png");
    write(
        tmp.path(),
        "build/CMakeCache.txt",
        &cache_txt(tmp.path(), "build", &tmp.path().display().to_string()),
    );
    write_exec(tmp.path(), "build/myapp");
    let p = plan_in(tmp.path(), "compile and run this project").expect("plan");
    assert!(
        p.command.contains("assets/samples/tester.png"),
        "verified sample input expected: {}",
        p.command
    );
    assert!(
        p.command.contains("--output"),
        "documented output flag expected: {}",
        p.command
    );
    assert!(
        !p.command.contains("assets/test.png"),
        "invented path: {}",
        p.command
    );
}

#[test]
fn required_positional_without_sample_builds_only_and_explains() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "README.md",
        "Usage: myapp <input-image> --output out.png\n",
    );
    let p = plan_in(tmp.path(), "compile and run this project").expect("plan");
    assert!(
        p.command.starts_with("cmake"),
        "build expected: {}",
        p.command
    );
    assert!(
        !p.command.contains("&& ./"),
        "must not propose a failing run: {}",
        p.command
    );
    let note = p.note.as_deref().unwrap_or("");
    assert!(note.contains("requires an input argument"), "note: {note}");
}

// --- Intent classification ---------------------------------------------------

#[test]
fn intent_classification_table() {
    let cases: &[(&str, Intent)] = &[
        ("compile and run this C++ project", Intent::BuildAndRun),
        ("build the project", Intent::Build),
        ("rebuild everything", Intent::Build),
        ("fix the build", Intent::Build),
        ("run this", Intent::Run),
        ("launch the app", Intent::Run),
        ("run the tests", Intent::Test),
        ("test the project", Intent::Test),
        ("clean the build artifacts", Intent::Clean),
        ("what's in this directory?", Intent::ListDir),
        ("ls", Intent::ListDir),
        ("find all cpp files", Intent::SearchFiles),
        ("search for python files", Intent::SearchFiles),
        ("where is the config file", Intent::SearchFiles),
        ("show me the largest files", Intent::LargestFiles),
        ("which files take the most space", Intent::LargestFiles),
        ("git status", Intent::GitStatus),
        ("show recent git changes", Intent::GitDiff),
        ("show the git log", Intent::GitLog),
        ("what's using port 3000?", Intent::InspectPort),
        (
            "which process is listening on port 8080",
            Intent::InspectPort,
        ),
        ("show running processes", Intent::InspectProcess),
        (
            "what processes are using the most cpu",
            Intent::InspectProcess,
        ),
        ("install htop", Intent::InstallPackage),
        ("how do I install ffmpeg", Intent::InstallPackage),
        ("how much disk space is left", Intent::DiskUsage),
        ("check memory usage", Intent::MemoryInfo),
        ("explain what ls -la does", Intent::Explain),
        (
            "fix this command: `foo --bar` error: command not found",
            Intent::RepairCommand,
        ),
        ("tell me a joke", Intent::Ambiguous),
    ];
    for (req, want) in cases {
        let got = intent::classify_str(req);
        assert_eq!(got, *want, "request: {req:?}");
    }
}

#[test]
fn intent_helpers_extract_arguments() {
    assert_eq!(
        intent::search_pattern("find all cpp files").as_deref(),
        Some("*.cpp")
    );
    assert_eq!(
        intent::search_pattern("locate python files").as_deref(),
        Some("*.py")
    );
    assert_eq!(intent::port_number("what's using port 3000?"), Some(3000));
    assert_eq!(intent::port_number("listening on port 8080"), Some(8080));
    assert_eq!(
        intent::package_name("install htop"),
        Some("htop".to_string())
    );
}

// --- General planners --------------------------------------------------------

#[test]
fn general_planners_use_installed_tools() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "a.txt", "x");

    let p = plan_in(tmp.path(), "what's using port 3000?").expect("plan");
    assert!(p.command.contains(":3000"), "got: {}", p.command);
    assert!(
        p.command.contains("ss")
            || p.command.contains("lsof")
            || p.command.contains("netstat")
            || p.command.contains("/proc/net"),
        "must use an installed tool: {}",
        p.command
    );

    let p = plan_in(tmp.path(), "show me the largest files").expect("plan");
    assert!(
        p.command.contains("sort"),
        "size-aware sort expected: {}",
        p.command
    );

    let p = plan_in(tmp.path(), "what's in this directory?").expect("plan");
    assert_eq!(p.command, "ls -la");

    let p = plan_in(tmp.path(), "find all cpp files").expect("plan");
    assert!(
        p.command.contains("find . -type f -name '*.cpp'"),
        "got: {}",
        p.command
    );

    let p = plan_in(tmp.path(), "how much disk space is left").expect("plan");
    assert_eq!(p.command, "df -h");

    let p = plan_in(tmp.path(), "check memory usage").expect("plan");
    assert_eq!(p.command, "free -h");

    let p = plan_in(tmp.path(), "install htop").expect("plan");
    assert!(p.command.ends_with("install htop"), "got: {}", p.command);
    assert!(
        ["dnf", "apt", "pacman", "zypper", "apk"]
            .iter()
            .any(|pm| p.command.contains(pm)),
        "native package manager expected: {}",
        p.command
    );
}

#[test]
fn git_planners_require_a_repo() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "a.txt", "x");
    // Not a repo: no deterministic git command.
    assert!(plan_in(tmp.path(), "git status").is_none());

    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), "a.txt", "x");
    let st = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(st.success());
    let p = plan_in(repo.path(), "show me the git status").expect("plan");
    assert_eq!(p.command, "git status");
    let p = plan_in(repo.path(), "show recent git changes").expect("plan");
    assert_eq!(p.command, "git diff");
    let p = plan_in(repo.path(), "show the git log").expect("plan");
    assert_eq!(p.command, "git log --oneline -10");
}

// --- Intent-aware verification ----------------------------------------------

#[test]
fn bare_cd_is_rejected_for_build_requests() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "int main(){}");
    let raw = engine().scan(tmp.path()).unwrap();
    let issues = verify::intent_issues("compile and run this C++ project", "cd /tmp");
    assert!(!issues.is_empty(), "cd must be flagged for a build request");
    let mut rep = verify::verify("cd /tmp", tmp.path(), &raw.entries);
    rep.issues.extend(issues);
    assert_eq!(rep.verdict(), Verdict::Blocked);
}

#[test]
fn compile_without_run_is_flagged_for_build_and_run() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    let issues = verify::intent_issues(
        "compile and run this project",
        "cmake -S . -B build && cmake --build build",
    );
    assert!(
        issues.iter().any(|i| i.message.contains("run")),
        "missing run step must be flagged: {issues:?}"
    );
}

// --- Structured prompt contract ----------------------------------------------

#[test]
fn prompt_contains_intent_and_verified_constraints() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "CMakeLists.txt",
        "project(x)\nadd_executable(myapp main.cpp)\n",
    );
    write(tmp.path(), "main.cpp", "int main(){}");
    write(
        tmp.path(),
        "build/CMakeCache.txt",
        &cache_txt(tmp.path(), "build", "/old/renamed-away path"),
    );
    let ctx = engine()
        .collect_at(tmp.path(), "compile and run this project")
        .unwrap();
    let prompt = engine().render_prompt(&ctx);
    assert!(prompt.contains("## Intent"), "prompt: {prompt}");
    assert!(prompt.contains("build and then run"), "prompt: {prompt}");
    assert!(
        prompt.contains("## Verified constraints"),
        "prompt: {prompt}"
    );
    assert!(
        prompt.contains("stale CMake build dir build"),
        "prompt: {prompt}"
    );
    assert!(
        prompt.contains("Executable targets: myapp"),
        "prompt: {prompt}"
    );
    assert!(
        prompt.len() < 8000,
        "prompt must stay compact: {} chars",
        prompt.len()
    );
}
