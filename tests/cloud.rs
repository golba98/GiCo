use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gico"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("GICO_BACKEND")
        .env_remove("GICO_ENDPOINT")
        .env_remove("GICO_MODEL")
        .output()
        .unwrap()
}

#[test]
fn deterministic_cloud_preview_and_preload_issue_no_responses_requests() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::write(root.join("list"), "10\n2\n").unwrap();
    let o = cli(
        root,
        &[
            "--backend",
            "openai",
            "--proposal-json",
            "--verbose",
            "sort list numerically",
        ],
    );
    assert!(o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("provider response"));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["command"], "sort -n -- list");
    assert!(cli(root, &["--backend", "openai", "preload"])
        .status
        .success());
    assert!(!root.join("data/gico/openai-usage.json").exists());
}

#[test]
fn profiles_preserve_explicit_local_model_and_cloud_reasoning() {
    use gico::config::Config;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    let mut c = Config {
        model: "local-special".into(),
        endpoint: "http://127.0.0.1:11555".into(),
        ..Default::default()
    };
    c.select_backend("openai");
    assert_eq!(c.model, "gpt-6-luna");
    assert_eq!(c.reasoning_effort, "none");
    c.model = "gpt-6.1-terra".into();
    c.reasoning_effort = "low".into();
    c.save(&p).unwrap();
    let mut c: Config = toml::from_str(&fs::read_to_string(p).unwrap()).unwrap();
    c.select_backend("ollama");
    assert_eq!(c.model, "local-special");
    assert_eq!(c.endpoint, "http://127.0.0.1:11555");
    c.select_backend("openai");
    assert_eq!(c.reasoning_effort, "low");
    assert_eq!(c.model, "gpt-6.1-terra");
}
