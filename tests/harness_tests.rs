//! Tests for the integrated harness components and the new behaviors:
//! GUI/CLI target classification, directory navigation, and cd verification.

use std::path::Path;

use gico::context::cmake::{self, TargetKind};
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

/// CMake project with a CLI target and a GUI target in a gui/ subdirectory,
/// mirroring real projects like IT Pixel Forger.
fn setup_cmake_cli_gui(d: &Path) {
    write(
        d,
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.20)\nproject(x)\nadd_executable(mytool-cli src/main.cpp)\nadd_subdirectory(src/gui)\nadd_subdirectory(tests)\n",
    );
    write(d, "src/main.cpp", "int main(){return 0;}\n");
    write(
        d,
        "src/gui/CMakeLists.txt",
        "find_package(Qt6 REQUIRED COMPONENTS Widgets)\nadd_executable(mytool-gui main.cpp)\nset_target_properties(mytool-gui PROPERTIES WIN32_EXECUTABLE ON)\ntarget_link_libraries(mytool-gui PRIVATE Qt6::Widgets)\n",
    );
    write(d, "src/gui/main.cpp", "int main(){return 0;}\n");
    write(
        d,
        "tests/CMakeLists.txt",
        "add_executable(mytool_tests main.cpp)\n",
    );
    write(d, "tests/main.cpp", "int main(){return 0;}\n");
}

#[test]
fn cmake_target_kinds_classify_cli_gui_test() {
    let tmp = tempfile::tempdir().unwrap();
    setup_cmake_cli_gui(tmp.path());
    let kinds = cmake::target_kinds(tmp.path());
    let kind_of = |n: &str| kinds.iter().find(|t| t.name == n).map(|t| t.kind);
    assert_eq!(kind_of("mytool-cli"), Some(TargetKind::Cli));
    assert_eq!(kind_of("mytool-gui"), Some(TargetKind::Gui));
    assert_eq!(kind_of("mytool_tests"), Some(TargetKind::Test));
}

#[test]
fn gui_launch_request_picks_gui_target_not_cli() {
    let tmp = tempfile::tempdir().unwrap();
    setup_cmake_cli_gui(tmp.path());
    // configured build tree with both binaries built
    let cache = format!(
        "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\n",
        tmp.path().display(),
        tmp.path().join("build").display()
    );
    write(tmp.path(), "build/CMakeCache.txt", &cache);
    for exe in ["build/mytool-cli", "build/src/gui/mytool-gui"] {
        let p = tmp.path().join(exe);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let p = plan_in(
        tmp.path(),
        "find the graphical application built by this project and launch it",
    )
    .expect("plan");
    assert!(
        p.command.contains("mytool-gui"),
        "must launch the GUI target: {}",
        p.command
    );
    assert!(
        !p.command.contains("mytool-cli"),
        "must not launch the CLI: {}",
        p.command
    );
    assert!(p.note.as_deref().unwrap_or("").contains("GUI"));
}

#[test]
fn plain_run_prefers_cli_target() {
    let tmp = tempfile::tempdir().unwrap();
    setup_cmake_cli_gui(tmp.path());
    let cache = format!(
        "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\n",
        tmp.path().display(),
        tmp.path().join("build").display()
    );
    write(tmp.path(), "build/CMakeCache.txt", &cache);
    for exe in ["build/mytool-cli", "build/src/gui/mytool-gui"] {
        let p = tmp.path().join(exe);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let p = plan_in(tmp.path(), "run this").expect("plan");
    assert!(
        p.command.contains("mytool-cli"),
        "run should prefer CLI: {}",
        p.command
    );
    assert!(
        !p.command.contains("mytool-gui"),
        "run must not pick GUI: {}",
        p.command
    );
}

// --- Directory navigation ----------------------------------------------------

#[test]
fn navigate_dir_tests_serial() {
    // These mutate $HOME, so they must run in one test (no parallel env races).
    let fake_home = tempfile::tempdir().unwrap();
    write(
        fake_home.path(),
        "Development/9-Rust/SomeCrate/Cargo.toml",
        "[package]\nname=\"x\"\n",
    );
    write(
        fake_home.path(),
        "Development/1-JavaScript/web/package.json",
        "{}",
    );
    write(fake_home.path(), "readme.txt", "nothing relevant\n");
    let old_home = std::env::var_os("HOME");
    // SAFETY: single test mutating HOME; restored before assertions end.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    // 1. resolve from filesystem evidence
    let req = "change to my Rust development directory";
    assert_eq!(intent::classify_str(req), Intent::NavigateDir);
    let raw = engine().scan(fake_home.path()).unwrap();
    let p = plan::plan(&raw, req).expect("plan");
    assert!(p.command.starts_with("cd "), "got: {}", p.command);
    assert!(
        p.command.contains("9-Rust"),
        "must resolve to the Rust dir: {}",
        p.command
    );
    assert!(!p.command.contains("1-JavaScript"), "got: {}", p.command);
    let issues = verify::intent_issues(req, &p.command);
    assert!(issues.is_empty(), "issues: {issues:?}");

    // 2. unresolvable request explains instead of inventing a path
    let raw = engine().scan(fake_home.path()).unwrap();
    let p = plan::plan(&raw, "go to my quantum-zebra workshop").expect("plan");
    assert!(p.command.is_empty(), "must not invent a cd: {}", p.command);
    assert!(p.note.as_deref().unwrap_or("").contains("could not find"));

    // 3. cd to a missing path is blocked by intent-aware verification
    let issues = verify::intent_issues("change to my Rust directory", "cd /definitely/not/here");
    assert!(!issues.is_empty());
    let mut rep = verify::verify("cd /definitely/not/here", fake_home.path(), &raw.entries);
    rep.issues.extend(issues);
    assert_eq!(rep.verdict(), Verdict::Blocked);

    match old_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[test]
fn gui_planning_declines_when_only_the_cli_is_built() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    setup_cmake_cli_gui(root);
    write(
        root,
        "build/CMakeCache.txt",
        &format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\n",
            root.display(),
            root.join("build").display()
        ),
    );
    write(root, "build/mytool-cli", "#!/bin/sh\nexit 0\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        root.join("build/mytool-cli"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let plan = plan_in(root, "launch the graphical application").unwrap();
    assert!(plan.command.is_empty());
    assert!(plan.note.unwrap().contains("No verified GUI"));
}

#[test]
fn plan_uses_the_selected_targets_docs_without_running_its_help() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "CMakeLists.txt", "project(x)\n# add_executable(faux main.cpp)\nadd_executable(a-test main.cpp)\nadd_executable(converter main.cpp)\nadd_library(core STATIC main.cpp)\nadd_test(NAME smoke COMMAND a-test)\n");
    write(root, "main.cpp", "int main(){}");
    write(root, "README.md", "Usage: a-test <other-args>\nUsage: converter <input>\nExample: build/converter 'data/my sample.png'\n");
    write(root, "data/my sample.png", "sample");
    write(
        root,
        "build/CMakeCache.txt",
        &format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\n",
            root.display(),
            root.join("build").display()
        ),
    );
    write(root, "build/converter", "#!/bin/sh\ntouch ran-unapproved\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        root.join("build/converter"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let plan = plan_in(root, "run this").unwrap();
    assert_eq!(plan.command, "./build/converter 'data/my sample.png'");
    assert!(!root.join("ran-unapproved").exists());
    let kinds = cmake::target_kinds(root);
    assert!(kinds
        .iter()
        .any(|target| target.name == "core" && target.kind == TargetKind::Library));
    assert!(!kinds.iter().any(|target| target.name == "faux"));
}
