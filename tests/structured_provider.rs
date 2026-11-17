use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    time::Duration,
};

#[test]
fn live_cli_can_request_one_scoped_read_before_answering() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let backend = std::thread::spawn(move || {
        for (round, reply) in [
            json!({"kind":"inspect","tool":"read","value":"README.md"}),
            json!({"kind":"answer","text":"The signal is violet."}),
        ]
        .iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut req = BufReader::new(&stream);
            let mut line = String::new();
            req.read_line(&mut line).unwrap();
            assert!(line.starts_with("POST /api/chat "));
            let mut length = 0;
            loop {
                line.clear();
                assert!(req.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            req.read_exact(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(request["format"]["oneOf"].is_array());
            assert_eq!(request["keep_alive"], "300s");
            assert_eq!(request["options"]["num_ctx"], 4096);
            assert_eq!(request["think"], false);
            let prompt = request["messages"][0]["content"].as_str().unwrap();
            assert_eq!(prompt.contains("signal is violet"), round == 1);
            let body = json!({"message":{"content":reply.to_string()},"done":true}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
    std::fs::write(root.join("README.md"), "Our signal is violet.").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gico"))
        .args([
            "--model",
            "qwen3.5:4b-q4_K_M",
            "--endpoint",
            &endpoint,
            "--proposal-json",
            "what is our signal colour?",
        ])
        .current_dir(&root)
        .env("HOME", home.path())
        .env_remove("GICO_MOCK_COMMAND")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["kind"],
        "answer"
    );
    backend.join().unwrap();
}
