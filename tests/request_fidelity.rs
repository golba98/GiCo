use gico::{
    context::ContextEngine,
    harness::{pipeline, Harness},
    plan,
    verify::Verdict,
};
use std::{fs, path::Path, process::Command};

fn candidate(root: &Path, request: &str, command: Option<&str>) -> pipeline::Candidate {
    let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
    let command = command
        .map(String::from)
        .unwrap_or_else(|| plan::plan(&raw, request).expect(request).command);
    pipeline::verify_candidate(&mut Harness::new(3), &raw, request, command, None)
}

#[test]
fn query_pairs_preserve_scope_format_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("cold room")).unwrap();
    fs::write(root.join("cold room/π's readings"), "100\n2\n20\n").unwrap();
    fs::write(root.join("labels"), "a.b\naxb\na.b\n").unwrap();
    for (request, out) in [
        ("sort \"cold room/π's readings\" numerically", "2\n20\n100"),
        ("sort \"cold room/π's readings\" lexically", "100\n2\n20"),
        (
            "show the highest integer in \"cold room/π's readings\"",
            "100",
        ),
        (
            "show the smallest integer in \"cold room/π's readings\"",
            "2",
        ),
        (
            "print lines containing the literal 'a.b' in labels",
            "a.b\na.b",
        ),
        (
            "print lines matching the regex 'a.b' in labels",
            "a.b\naxb\na.b",
        ),
        ("print lines excluding a.b in labels", "axb"),
        ("count lines containing a.b in labels", "2"),
        ("print the line numbers containing a.b in labels", "1\n3"),
    ] {
        let c = candidate(root, request, None);
        assert_ne!(
            c.report.verdict(),
            Verdict::Blocked,
            "{request}: {:?}",
            c.report
        );
        let actual = Command::new("sh")
            .arg("-c")
            .arg(&c.command)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(actual.status.success());
        assert_eq!(String::from_utf8(actual.stdout).unwrap().trim_end(), out);
    }
}

#[test]
fn destination_change_or_alternate_effect_needs_new_approval() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    fs::write(root.join("values"), "10\n2").unwrap();
    let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
    let req = "write sorted lines from values to sorted.txt";
    let c = candidate(root, req, None);
    assert_ne!(c.report.verdict(), Verdict::Blocked);
    for edit in [
        "sort values",
        "sort -n values > sorted.txt",
        "sort values > different.txt",
    ] {
        assert_eq!(
            candidate(root, req, Some(edit)).report.verdict(),
            Verdict::Blocked
        );
    }
    fs::write(root.join("sorted.txt"), "new unrelated data").unwrap();
    assert!(pipeline::authorize(&mut Harness::new(3), &raw, req, &c, Some("")).is_err());
    assert_eq!(
        fs::read_to_string(root.join("sorted.txt")).unwrap(),
        "new unrelated data"
    );
}

#[test]
fn project_run_steps_use_the_subcommand_and_verified_manifest() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='moss'\nversion='0.1.0'",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
    let req = "build and run this project";
    assert_ne!(
        candidate(root, req, None).report.verdict(),
        Verdict::Blocked
    );
    for cmd in [
        "cargo build",
        "cargo build && cargo test",
        "cargo build && echo 'cargo run'",
    ] {
        assert_eq!(
            candidate(root, req, Some(cmd)).report.verdict(),
            Verdict::Blocked
        );
    }
    fs::write(
        root.join("package.json"),
        r#"{"scripts":{"start":"node main.js","build":"tsc"}}"#,
    )
    .unwrap();
    assert!(!candidate(root, req, Some("npm run build && npm start"))
        .report
        .issues
        .iter()
        .any(|e| e.message.contains("only builds")));
    assert!(candidate(root, req, Some("npm run build && npm run dev"))
        .report
        .issues
        .iter()
        .any(|e| e.message.contains("only builds")));
}

#[test]
fn ordinary_words_do_not_imply_project_operations() {
    for req in [
        "give me the latest two commits",
        "find the symbolic links",
        "read the final line of the symlink p",
    ] {
        assert!(!matches!(
            gico::intent::classify_str(req),
            gico::intent::Intent::Test | gico::intent::Intent::Build
        ));
    }
}

#[test]
fn printing_or_filtering_cannot_replace_its_input() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("sensor"), "i\ni\n").unwrap();
    for cmd in [
        "sed -i s/i/x/ sensor",
        "sort -o sensor sensor",
        "cp sensor shadow",
        "awk '1' sensor > shadow && mv shadow sensor",
    ] {
        let c = candidate(
            root.path(),
            "display each distinct reading in sensor",
            Some(cmd),
        );
        assert_eq!(c.report.verdict(), Verdict::Blocked, "{cmd}");
    }
    let c = candidate(
        root.path(),
        "collapse repeated adjacent rows in sensor without rearranging anything",
        None,
    );
    assert_eq!(c.command, "uniq -- sensor");
}

#[test]
fn command_options_and_predicates_are_not_input_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("x"), "310\n").unwrap();
    fs::write(root.join("data"), "1.4\n").unwrap();
    for command in [
        "find . -type f ! -name bad",
        "grep -F -e 1.4 -- data",
        "tail -n 3 -- data",
    ] {
        let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
        assert_ne!(
            gico::verify::verify(command, root, &raw.entries).verdict(),
            Verdict::Blocked,
            "{command}"
        );
    }
    let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
    assert_eq!(
        gico::verify::verify("tail -n 3 -- ./missing", root, &raw.entries).verdict(),
        Verdict::Blocked
    );
    assert!(matches!(
        pipeline::prepare(
            &mut Harness::new(3),
            &raw,
            "please reverse these rows",
            |_, _| panic!("ungrounded")
        ),
        pipeline::Preparation::Clarify(_)
    ));
}

#[test]
fn character_count_is_independent_of_the_calling_shell_locale() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ink"), "é🖊").unwrap();
    for (unit, expected) in [("characters", "2"), ("bytes", "6")] {
        let request = format!("count {unit} in ink");
        let plan = candidate(dir.path(), &request, None);
        assert_ne!(plan.report.verdict(), Verdict::Blocked);
        let out = Command::new("sh")
            .args(["-c", &plan.command])
            .env("LC_ALL", "C")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), expected);
    }
}

#[test]
fn virtual_workspace_launches_the_verified_gui_subcommand_with_features() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for member in ["screen", "worker"] {
        fs::create_dir_all(root.join(format!("packages/{member}/src"))).unwrap();
    }
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"packages/*\"]\n",
    )
    .unwrap();
    fs::write(
        root.join("README.md"),
        "```sh\ncargo run --release --bin canvas -- draw\n```\n",
    )
    .unwrap();
    fs::write(root.join("packages/screen/Cargo.toml"), "[package]\nname = 'sketch'\nversion = '0.1.0'\nautobins = false\n[features]\nvisual = []\n[[bin]]\nname = 'canvas'\npath = 'src/main.rs'\nrequired-features = ['visual']\n").unwrap();
    fs::write(root.join("packages/screen/src/main.rs"), "enum Command {\n/// Open the graphical window.\nDraw(Options),\n}\nfn main() { match cli() { Command::Draw(args) => show(args) } }").unwrap();
    fs::write(
        root.join("packages/worker/Cargo.toml"),
        "[package]\nname = 'daemon'\nversion = '0.1.0'\n",
    )
    .unwrap();
    fs::write(root.join("packages/worker/src/main.rs"), "fn main() {}").unwrap();
    let request = "I'd like to see the GUI in this Rust build";
    let preview = candidate(root, request, None);
    assert_ne!(preview.report.verdict(), Verdict::Blocked);
    assert_eq!(
        preview.command,
        "cargo run --release --bin canvas --features visual -- draw"
    );
    for wrong in ["cargo run --release", "cargo run --release --bin daemon"] {
        assert_eq!(
            candidate(root, request, Some(wrong)).report.verdict(),
            Verdict::Blocked
        );
    }
    let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
    fs::write(root.join("packages/screen/src/main.rs"), "fn main() {}").unwrap();
    assert!(pipeline::authorize(
        &mut Harness::new(4),
        &raw,
        request,
        &preview,
        Some(&preview.command)
    )
    .is_err());
}

#[test]
fn genuine_target_ambiguity_accepts_a_number_without_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='scope'\nversion='0.1.0'",
    )
    .unwrap();
    fs::create_dir_all(root.join("src/bin")).unwrap();
    fs::write(root.join("src/bin/first.rs"), "fn main(){}").unwrap();
    fs::write(root.join("src/bin/second.rs"), "fn main(){}").unwrap();
    let raw = ContextEngine::new(Default::default()).scan(root).unwrap();
    let response = pipeline::prepare(
        &mut Harness::new(4),
        &raw,
        "run this Rust program",
        |_, _| panic!("known options"),
    );
    assert!(
        matches!(response, pipeline::Preparation::Clarify(s) if s.contains("1 scope/first") && s.contains("2 scope/second"))
    );
    assert_eq!(
        candidate(root, "run this Rust program\nClarification: 2", None).command,
        "cargo run --bin second"
    );
}

#[test]
fn navigation_combines_the_project_name_and_documented_purpose() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("Development/Signal Emulator");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("Cargo.toml"), "[workspace]\nmembers=[]").unwrap();
    fs::write(
        project.join("README.md"),
        "# Signal Emulator\n\nInspect a handheld calculator on the desktop.\n",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_gico"))
        .args([
            "--backend",
            "ollama",
            "--no-cache",
            "--proposal-json",
            "take me to that calculator emulator",
        ])
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", home.path().join("data"))
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(output["command"], format!("cd '{}'", project.display()));
}
