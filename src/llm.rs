//! Model-agnostic local inference clients. Supports Ollama (`/api/chat`)
//! and llama.cpp server / any OpenAI-compatible endpoint
//! (`/v1/chat/completions`). The model is always supplied by the caller —
//! nothing is hard-coded.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Ollama,
    /// llama.cpp `llama-server` or any OpenAI-compatible local server.
    OpenAiCompatible,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Backend> {
        match s.to_ascii_lowercase().as_str() {
            "ollama" => Some(Backend::Ollama),
            "llamacpp" | "llama.cpp" | "llama-cpp" | "openai" | "oai" => {
                Some(Backend::OpenAiCompatible)
            }
            _ => None,
        }
    }
}

pub struct LlmClient {
    pub backend: Backend,
    pub endpoint: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

#[derive(Debug)]
pub struct LlmError(pub String);

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for LlmError {}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
}
#[derive(Deserialize)]
struct OllamaMessage {
    content: String,
}

#[derive(Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
}
#[derive(Deserialize)]
struct OaiChoice {
    message: OaiMessage,
}
#[derive(Deserialize)]
struct OaiMessage {
    content: String,
}

#[derive(Deserialize)]
struct OaiModels {
    data: Vec<OaiModel>,
}
#[derive(Deserialize)]
struct OaiModel {
    id: String,
}

#[derive(Deserialize)]
struct OllamaTags {
    models: Vec<OllamaTag>,
}
#[derive(Deserialize)]
struct OllamaTag {
    name: String,
}

impl LlmClient {
    pub fn new(backend: Backend, endpoint: &str, model: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client");
        Self {
            backend,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            temperature: 0.1,
            max_tokens: 256,
            http,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build HTTP runtime"),
        }
    }

    /// Generate a completion for a single user prompt (system prompt is
    /// folded into the user turn to stay compatible with tiny models).
    pub fn complete(&self, prompt: &str) -> Result<String, LlmError> {
        self.runtime.block_on(self.complete_async(prompt))
    }

    pub fn complete_cancellable(
        &self,
        prompt: &str,
        cancel: &crate::harness::CancelToken,
    ) -> Result<String, LlmError> {
        self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = async {
                    while !cancel.is_cancelled() {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                } => Err(LlmError("inference cancelled".into())),
                result = self.complete_async(prompt) => result,
            }
        })
    }

    async fn complete_async(&self, prompt: &str) -> Result<String, LlmError> {
        match self.backend {
            Backend::Ollama => self.complete_ollama(prompt).await,
            Backend::OpenAiCompatible => self.complete_oai(prompt).await,
        }
    }

    async fn complete_ollama(&self, prompt: &str) -> Result<String, LlmError> {
        let url = format!("{}/api/chat", self.endpoint);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
            "options": {
                "temperature": self.temperature,
                "num_predict": self.max_tokens,
            }
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError(format!("ollama request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let t = response_body(resp, 4_096).await.unwrap_or_default();
            return Err(LlmError(format!(
                "ollama HTTP {status}: {}",
                short(&String::from_utf8_lossy(&t), 300)
            )));
        }
        let parsed: OllamaChatResponse =
            serde_json::from_slice(&response_body(resp, 1_048_576).await?)
                .map_err(|e| LlmError(format!("ollama bad response: {e}")))?;
        Ok(parsed.message.content)
    }

    async fn complete_oai(&self, prompt: &str) -> Result<String, LlmError> {
        let url = format!("{}/v1/chat/completions", self.endpoint);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError(format!("llama.cpp request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let t = response_body(resp, 4_096).await.unwrap_or_default();
            return Err(LlmError(format!(
                "llama.cpp HTTP {status}: {}",
                short(&String::from_utf8_lossy(&t), 300)
            )));
        }
        let parsed: OaiResponse = serde_json::from_slice(&response_body(resp, 1_048_576).await?)
            .map_err(|e| LlmError(format!("llama.cpp bad response: {e}")))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError("llama.cpp returned no choices".to_string()))
    }

    /// List available models on the backend.
    pub fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.runtime.block_on(self.list_models_async())
    }

    async fn list_models_async(&self) -> Result<Vec<String>, LlmError> {
        match self.backend {
            Backend::Ollama => {
                let resp = self
                    .http
                    .get(format!("{}/api/tags", self.endpoint))
                    .send()
                    .await
                    .map_err(|e| LlmError(format!("ollama request failed: {e}")))?;
                let parsed: OllamaTags = resp
                    .json()
                    .await
                    .map_err(|e| LlmError(format!("bad response: {e}")))?;
                Ok(parsed.models.into_iter().map(|m| m.name).collect())
            }
            Backend::OpenAiCompatible => {
                let resp = self
                    .http
                    .get(format!("{}/v1/models", self.endpoint))
                    .send()
                    .await
                    .map_err(|e| LlmError(format!("request failed: {e}")))?;
                let parsed: OaiModels = resp
                    .json()
                    .await
                    .map_err(|e| LlmError(format!("bad response: {e}")))?;
                Ok(parsed.data.into_iter().map(|m| m.id).collect())
            }
        }
    }
}

async fn response_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| LlmError(error.to_string()))?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(LlmError("model response exceeds the size limit".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Extract a single shell command from raw model output: prefer the first
/// fenced code block, strip a leading prompt character, drop chatter.
pub fn extract_command(raw: &str) -> String {
    let text = raw.trim();
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        // Skip an optional language tag on the opening fence line (```sh).
        let after = match after.find('\n') {
            Some(nl)
                if !after[..nl].trim().is_empty()
                    && after[..nl]
                        .trim()
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '+' | '#' | '-')) =>
            {
                &after[nl + 1..]
            }
            _ => after,
        };
        if let Some(end) = after.find("```") {
            let cmd = after[..end].trim();
            if !cmd.is_empty() {
                return first_line(cmd);
            }
        }
    }
    first_line(text)
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line.strip_prefix('$').map(str::trim_start).unwrap_or(line);
    // Strip inline code markers some small models wrap around the command.
    let stripped = line.trim_matches('`');
    stripped.to_string()
}

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.len() > n {
        format!("{t}...")
    } else {
        t
    }
}
