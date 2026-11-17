use gico::llm::{Backend, LlmClient};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;

const SMALL: &str = "qwen2.5-coder:0.5b";

/// Check each request in order, with a fresh connection per response.
fn ollama(replies: Vec<(&'static str, &'static str)>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for (method, body) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = BufReader::new(&stream);
            let mut line = String::new();
            request.read_line(&mut line).unwrap();
            assert!(line.starts_with(method), "{line}");
            let mut length = 0;
            loop {
                line.clear();
                assert!(request.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            if method.starts_with("POST") {
                let mut payload = vec![0; length];
                request.read_exact(&mut payload).unwrap();
                let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(json["model"], SMALL);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (url, server)
}

#[test]
fn discovers_installed_small_model_once_across_generation_rounds() {
    let chat = ("POST /api/chat ", r#"{"message":{"content":"pwd"}}"#);
    let (url, server) = ollama(vec![
        (
            "GET /api/tags ",
            r#"{"models":[{"name":"other:7b"},{"name":"qwen2.5-coder:0.5b"}]}"#,
        ),
        chat,
        chat,
    ]);
    let client = LlmClient::new(Backend::Ollama, &url, "");
    assert_eq!(client.complete("first").unwrap(), "pwd");
    assert_eq!(client.complete("second").unwrap(), "pwd");
    server.join().unwrap();
}

#[test]
fn missing_small_model_gives_setup_instruction() {
    let (url, server) = ollama(vec![(
        "GET /api/tags ",
        r#"{"models":[{"name":"other:7b"}]}"#,
    )]);
    let client = LlmClient::new(Backend::Ollama, &url, "");
    assert!(client
        .complete("hello")
        .unwrap_err()
        .0
        .contains("ollama pull qwen2.5-coder:0.5b"));
    server.join().unwrap();
}
