use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Command, Output},
};

fn cli(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gico"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("ZDOTDIR", home)
        .env("GICO_ENDPOINT", "http://127.0.0.1:1")
        .env_remove("GICO_MODEL")
        .env_remove("GICO_MOCK_COMMAND")
        .output()
        .unwrap()
}
fn decision(home: &Path, request: &str) -> Value {
    let o = cli(home, home, &["--proposal-json", request]);
    serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{o:?}"))
}

#[test]
fn alias_tracks_a_moved_project_and_keeps_authored_corrections() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let first = home.join("Development/Old Workspace");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(
        first.join("package.json"),
        r#"{"name":"fixture","description":"original purpose","main":"entry.js"}"#,
    )
    .unwrap();
    std::fs::write(first.join("entry.js"), "").unwrap();
    assert!(cli(home, &first, &["alias", "Solstice"]).status.success());
    let dir = std::fs::read_dir(home.join("data/gico/projects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let prefs = dir.join("preferences.json");
    let bytes=serde_json::to_vec(&json!({"aliases":["Solstice"],"purpose":"user correction","roles":{"entry.js":"start here"}})).unwrap();
    std::fs::write(&prefs, &bytes).unwrap();
    let moved = home.join("Development/New Site");
    std::fs::rename(first, &moved).unwrap();
    std::fs::write(dir.join("context.json"), "broken").unwrap();
    let info = decision(home, "what is the purpose of project Solstice?");
    assert_eq!(info["kind"], "answer");
    assert!(info["text"].as_str().unwrap().contains("user correction"));
    std::fs::write(moved.join("new.js"), "").unwrap();
    std::fs::write(moved.join("package.json"), r#"{"main":"new.js"}"#).unwrap();
    let info = decision(home, "where does Solstice start?");
    assert!(info["text"].as_str().unwrap().contains("new.js"));
    assert_eq!(std::fs::read(&prefs).unwrap(), bytes);
    let nav = decision(home, "go to Solstice");
    assert!(nav["command"].as_str().unwrap().contains("New Site"));
}

#[test]
fn invalid_config_is_actionable_and_hook_removal_works_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".config/gico")).unwrap();
    std::fs::write(home.join(".config/gico/config.toml"), "model = [").unwrap();
    let out = cli(home, home, &["status"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("invalid"));
    let before = "export THEME=custom\n";
    std::fs::write(home.join(".zshrc"),format!("{before}# >>> GiCo interactive shell >>>\n(( $+commands[gico] )) && eval \"$(command gico init-zsh)\"\n# <<< GiCo interactive shell <<<\n")).unwrap();
    for _ in 0..2 {
        assert!(cli(home, home, &["setup", "--remove-zsh"]).status.success());
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".zshrc")).unwrap(),
        before
    );
}

#[test]
fn path_operands_are_checked_in_a_directory_with_spaces() {
    use gico::verify::{verify, Verdict};
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Two Words");
    std::fs::create_dir(&dir).unwrap();
    for invalid in ["basename $(pwd)", "dirname", "touch"] {
        assert_eq!(verify(invalid, &dir, &[]).verdict(), Verdict::Blocked);
    }
    for valid in [
        r#"basename "$PWD""#,
        "basename 'literal file'",
        "printf '%s' '$(pwd)'",
    ] {
        assert_ne!(verify(valid, &dir, &[]).verdict(), Verdict::Blocked);
    }
}

#[test]
fn natural_navigation_distinguishes_source_from_a_build_only_remnant() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let parent = home.join("Development/5-C++");
    let old = parent.join("Atlas Generator");
    let new = parent.join("Atlas Studio");
    std::fs::create_dir_all(old.join("build")).unwrap();
    for (tree, src) in [("build-old", &old), ("verified", &new)] {
        let path = new.join(tree);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("CMakeCache.txt"),
            format!(
                "CMAKE_HOME_DIRECTORY:INTERNAL={}\nCMAKE_CACHEFILE_DIR:INTERNAL={}\n",
                src.display(),
                src.join(tree).display()
            ),
        )
        .unwrap();
    }
    std::fs::write(new.join("CMakeLists.txt"), "project(atlas)").unwrap();
    assert!(decision(home, "go to atlas gen cpp")["command"]
        .as_str()
        .unwrap()
        .contains("Atlas Studio"));
    let explicit = format!("go to {}", old.display());
    assert!(decision(home, &explicit)["command"]
        .as_str()
        .unwrap()
        .contains("Atlas Generator"));
}

#[test]
fn an_explicit_subdirectory_takes_precedence_over_its_project_name() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("Development/Atlas");
    let destination = source.join("assets");
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(source.join("package.json"), "{}").unwrap();
    let query = format!("go to {}", destination.display());
    let result = decision(home.path(), &query);
    assert_eq!(result["command"], format!("cd {}", destination.display()));
}
