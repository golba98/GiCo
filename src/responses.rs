//! Native OpenAI Responses transport. Only the existing local approval pipeline dispatches commands.
use crate::llm::{response_body, LlmError};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const ENDPOINT: &str = "https://api.openai.com/v1";
pub const MODEL: &str = "gpt-6-luna";

pub fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    if matches!(
        endpoint.trim_end_matches('/'),
        ENDPOINT | "https://api.openai.com"
    ) {
        Ok(())
    } else {
        Err("native openai uses https://api.openai.com/v1; use llamacpp for a compatible local server".into())
    }
}

#[derive(Default, Clone)]
pub struct Usage {
    pub attempts: u32,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub latency_ms: u128,
    pub estimated_usd: Option<f64>,
}

fn estimate(model: &str, usage: &Value) -> Option<f64> {
    // Standard short-context prices per million tokens (2026-11-18).
    // Cache-write charges vary by retention policy; skip estimates with reported writes.
    let (input, cached, output) = match model {
        "gpt-6-luna" => (0.10, 0.01, 0.50),
        "gpt-6.1-terra" => (1.00, 0.05, 5.00),
        _ => return None,
    };
    let n = usage["input_tokens"].as_u64()?;
    let c = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    if n > 272000
        || usage["input_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .unwrap_or(0)
            > 0
    {
        return None;
    }
    Some(
        (n.saturating_sub(c) as f64 * input
            + c as f64 * cached
            + usage["output_tokens"].as_u64()? as f64 * output)
            / 1e6,
    )
}

fn body(
    model: &str,
    reasoning: &str,
    prompt: &str,
    format: Option<&Value>,
    limit: u32,
) -> Result<Value, LlmError> {
    let mut data = json!({"model":model,"input":prompt,"store":false,"max_output_tokens":limit,"reasoning":{"effort":reasoning}});
    if let Some(schema) = format {
        let variants = schema["oneOf"]
            .as_array()
            .ok_or_else(|| LlmError("expected a terminal decision schema".into()))?;
        let mut tools = Vec::new();
        for variant in variants {
            let kind = variant["properties"]["kind"]["const"]
                .as_str()
                .ok_or_else(|| LlmError("invalid decision name".into()))?;
            let mut parameters = variant.clone();
            parameters["properties"]
                .as_object_mut()
                .ok_or_else(|| LlmError("invalid decision fields".into()))?
                .remove("kind");
            parameters["required"]
                .as_array_mut()
                .ok_or_else(|| LlmError("invalid required fields".into()))?
                .retain(|v| v != "kind");
            tools
                .push(json!({"type":"function","name":kind,"strict":true,"parameters":parameters}));
        }
        data["tools"] = Value::Array(tools);
        data["tool_choice"] = json!("required");
        data["parallel_tool_calls"] = json!(false);
        data["instructions"]=json!("Select one terminal decision function. Inspection is read-only. A proposal waits for user approval.");
    }
    Ok(data)
}

pub(crate) async fn key() -> Result<String, LlmError> {
    tokio::task::spawn_blocking(crate::auth::credential)
        .await
        .map_err(|_| LlmError("credential lookup interrupted".into()))?
        .map_err(LlmError)
}

pub(crate) fn status_error(status: reqwest::StatusCode, quota: bool) -> LlmError {
    let reason = match status.as_u16() {
        401 => "authentication rejected; check OPENAI_API_KEY or run gico auth login",
        403 | 404 => "access denied or model unavailable; verify the configured project and model",
        429 if quota => "project quota exhausted; check API billing",
        429 => "rate limit reached; try again later",
        400 | 422 => {
            "request rejected; check model, reasoning effort, output limit and structured schema"
        }
        500..=599 => "OpenAI service error",
        _ => "request unsuccessful",
    };
    LlmError(format!("OpenAI HTTP {status}: {reason}"))
}

pub(crate) async fn complete(
    client: &crate::llm::LlmClient,
    prompt: &str,
    schema: Option<&Value>,
) -> Result<String, LlmError> {
    validate_endpoint(&client.endpoint).map_err(LlmError)?;
    let key = key().await?;
    // Only this production entry point can obtain the user's credential.
    transport(client, prompt, schema, &client.endpoint, &key).await
}

async fn transport(
    client: &crate::llm::LlmClient,
    prompt: &str,
    schema: Option<&Value>,
    endpoint: &str,
    key: &str,
) -> Result<String, LlmError> {
    let start = Instant::now();
    let payload = body(
        &client.model,
        &client.reasoning,
        prompt,
        schema,
        client.max_tokens,
    )?;
    for attempt in 0..2 {
        client.cloud_usage.borrow_mut().attempts += 1;
        let response = client
            .http
            .post(format!("{endpoint}/responses"))
            .bearer_auth(key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                LlmError(
                    if e.is_timeout() {
                        "OpenAI request timed out"
                    } else {
                        "could not reach the OpenAI Responses API"
                    }
                    .into(),
                )
            })?;
        let status = response.status();
        let backoff = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let bytes =
            response_body(response, if status.is_success() { 1_048_576 } else { 8192 }).await?;
        let data = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        if !status.is_success() {
            let quota = data["error"]["code"] == "insufficient_quota";
            if attempt == 0
                && !quota
                && matches!(status.as_u16(), 429 | 503)
                && backoff.unwrap_or(1) <= 2
            {
                tokio::time::sleep(Duration::from_millis(backoff.map_or(250, |n| n * 1000))).await;
                continue;
            }
            return Err(status_error(status, quota));
        }
        let usage = &data["usage"];
        let stats = Usage {
            attempts: client.cloud_usage.borrow().attempts,
            latency_ms: start.elapsed().as_millis(),
            input: usage["input_tokens"].as_u64().unwrap_or(0),
            output: usage["output_tokens"].as_u64().unwrap_or(0),
            cached: usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
            reasoning: usage["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0),
            estimated_usd: estimate(&client.model, usage),
        };
        client.cloud_usage.replace(stats);
        if data["status"] == "incomplete" {
            return Err(LlmError(
                "OpenAI response incomplete; check the output limit".into(),
            ));
        }
        if data["status"] != "completed" {
            return Err(LlmError("OpenAI returned no completed response".into()));
        }
        return decision(&data, schema);
    }
    Err(LlmError("OpenAI request exhausted its retry budget".into()))
}

fn decision(data: &Value, schema: Option<&Value>) -> Result<String, LlmError> {
    let items = data["output"]
        .as_array()
        .ok_or_else(|| LlmError("OpenAI returned no output items".into()))?;
    let mut text = String::new();
    let mut action = None;
    for item in items {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if part["type"] == "refusal" {
                        return Err(LlmError("OpenAI declined this request".into()));
                    }
                    if part["type"] == "output_text" {
                        text.push_str(part["text"].as_str().unwrap_or(""));
                    }
                }
            }
            Some("function_call") => {
                let name = item["name"].as_str().unwrap_or("");
                if action.is_some()
                    || !schema.and_then(|s| s["oneOf"].as_array()).is_some_and(|v| {
                        v.iter()
                            .any(|variant| variant["properties"]["kind"]["const"] == name)
                    })
                {
                    return Err(LlmError(
                        "unexpected or multiple OpenAI decision calls".into(),
                    ));
                }
                let mut fields: serde_json::Map<String, Value> =
                    serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                        .map_err(|_| LlmError("malformed OpenAI decision arguments".into()))?;
                if fields
                    .insert("kind".into(), Value::String(name.into()))
                    .is_some()
                {
                    return Err(LlmError("conflicting OpenAI decision fields".into()));
                }
                action = Some(Value::Object(fields).to_string());
            }
            _ => {}
        }
    }
    if schema.is_some() {
        action.ok_or_else(|| LlmError("OpenAI returned no structured decision".into()))
    } else if text.is_empty() {
        Err(LlmError("OpenAI returned no text".into()))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    fn schema() -> Value {
        json!({"oneOf":[{"type":"object","properties":{"kind":{"const":"propose"},"argv":{"type":"array","items":{"type":"string"}}},"required":["kind","argv"],"additionalProperties":false}]})
    }
    #[test]
    fn strict_function_response_and_retry_on_server_busy() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", socket.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = socket.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                let mut header = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    header.push_str(&line);
                }
                assert!(header.starts_with("POST /v1/responses "));
                assert!(header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer synthetic-fixture"));
                let len = header
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap();
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).unwrap();
                let req: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(req["reasoning"]["effort"], "none");
                assert_eq!(req["tools"][0]["strict"], true);
                assert_eq!(req["store"], false);
                assert_eq!(req["tools"][0]["parameters"]["required"], json!(["argv"]));
                let (status, b) = if index == 0 {
                    ("503 Service Unavailable", json!({}))
                } else {
                    (
                        "200 OK",
                        json!({"status":"completed","usage":{"input_tokens":100,"output_tokens":20},"output":[{"type":"function_call","name":"propose","arguments":"{\"argv\":[\"pwd\"]}"}]}),
                    )
                };
                let b = b.to_string();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{b}",b.len()).unwrap();
            }
        });
        let client =
            crate::llm::LlmClient::new(crate::llm::Backend::OpenAiResponses, ENDPOINT, MODEL);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .block_on(transport(
                &client,
                "fixture",
                Some(&schema()),
                &endpoint,
                "synthetic-fixture",
            ))
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({"kind":"propose","argv":["pwd"]})
        );
        let u = client.cloud_usage.borrow();
        assert_eq!(u.attempts, 2);
        assert_eq!(u.input, 100);
        assert!(u.estimated_usd.unwrap() > 0.0);
    }
    #[test]
    fn refuses_or_blocks_unsupported_decisions() {
        let s = schema();
        for output in [
            json!([{"type":"message","content":[{"type":"refusal"}]}]),
            json!([{"type":"function_call","name":"undeclared","arguments":"{}"}]),
            json!([{"type":"function_call","name":"propose","arguments":"invalid"}]),
        ] {
            assert!(decision(&json!({"output":output}), Some(&s)).is_err());
        }
        assert!(validate_endpoint("https://example.org/v1").is_err());
    }
}
