//! Model-agnostic local inference clients. Supports Ollama (`/api/chat`)
//! and llama.cpp server / any OpenAI-compatible endpoint
//! (`/v1/chat/completions`). An explicit model takes precedence over local Ollama discovery.

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
    pub context_size: u32,
    pub debug: bool,
    pub usage: std::cell::RefCell<Option<(usize, usize, String)>>,
    pub prompt_chars: std::cell::Cell<usize>,
    pub idle_seconds: u32,
    discovered: std::cell::OnceCell<String>,
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
    done_reason: Option<String>,
    prompt_eval_count: Option<usize>,
    eval_count: Option<usize>,
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
    finish_reason: Option<String>,
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
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build HTTP client");
        Self {
            backend,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            temperature: 0.1,
            max_tokens: 256,
            context_size: 4096,
            debug: false,
            usage: Default::default(),
            prompt_chars: Default::default(),
            idle_seconds: 300,
            discovered: Default::default(),
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
        self.runtime.block_on(self.complete_async(prompt, None))
    }

    pub fn complete_cancellable(
        &self,
        prompt: &str,
        cancel: &crate::harness::CancelToken,
    ) -> Result<String, LlmError> {
        self.request(prompt, cancel, None, std::time::Duration::from_secs(60))
    }

    pub fn request(
        &self,
        prompt: &str,
        cancel: &crate::harness::CancelToken,
        format: Option<&serde_json::Value>,
        deadline: std::time::Duration,
    ) -> Result<String, LlmError> {
        self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = async {
                    while !cancel.is_cancelled() {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                } => Err(LlmError("inference cancelled".into())),
                result = self.complete_async(prompt,format) => result,
                () = tokio::time::sleep(deadline) => Err(LlmError("inference deadline exceeded".into())),
            }
        })
    }

    async fn complete_async(
        &self,
        prompt: &str,
        format: Option<&serde_json::Value>,
    ) -> Result<String, LlmError> {
        match self.backend {
            Backend::Ollama => self.complete_ollama(prompt, format).await,
            Backend::OpenAiCompatible => self.complete_oai(prompt, format).await,
        }
    }

    fn ollama_body(&self, model: &str) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model":model, "messages":[], "stream":false, "keep_alive":format!("{}s",self.idle_seconds),
            "options":{"num_ctx":self.context_size, "num_predict":self.max_tokens, "temperature":self.temperature}
        });
        if model.starts_with("qwen3.5:") {
            body["think"] = false.into();
        }
        body
    }

    pub fn details(&self) -> Result<serde_json::Value, LlmError> {
        self.runtime.block_on(async {
            let mut data=serde_json::json!({});
            for (name,req) in [
                ("version", self.http.get(format!("{}/api/version",self.endpoint))),
                ("show", self.http.post(format!("{}/api/show",self.endpoint)).json(&serde_json::json!({"model":self.model}))),
                ("tags", self.http.get(format!("{}/api/tags",self.endpoint)))
            ] {
                let resp=req.timeout(std::time::Duration::from_secs(3)).send().await.and_then(reqwest::Response::error_for_status)
                    .map_err(|e|LlmError(format!("Ollama {name}: {e}")))?;
                data[name]=serde_json::from_slice(&response_body(resp,1048576).await?).map_err(|e|LlmError(e.to_string()))?;
            }
            let info=data["tags"]["models"].as_array().and_then(|tags|tags.iter().find(|m|m["name"].as_str()==Some(&self.model)));
            Ok(serde_json::json!({"version":data["version"]["version"],"capabilities":data["show"]["capabilities"],"quantization":data["show"]["details"]["quantization_level"],"digest":info.map(|m|&m["digest"])}))
        })
    }

    pub fn resident(&self) -> Result<bool, LlmError> {
        Ok(self.resident_context()? == Some(self.context_size as u64))
    }

    pub fn resident_context(&self) -> Result<Option<u64>, LlmError> {
        if self.backend != Backend::Ollama {
            return Err(LlmError("residency inspection requires Ollama".into()));
        }
        self.runtime.block_on(async {
            let resp = self
                .http
                .get(format!("{}/api/ps", self.endpoint))
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| LlmError(format!("Ollama service unavailable: {e}")))?;
            let json: serde_json::Value =
                serde_json::from_slice(&response_body(resp, 262144).await?)
                    .map_err(|e| LlmError(format!("bad residency response: {e}")))?;
            let models = json["models"]
                .as_array()
                .ok_or_else(|| LlmError("Ollama returned an invalid residency response".into()))?;
            Ok(models
                .iter()
                .find(|m| m["name"].as_str() == Some(&self.model))
                .and_then(|m| m["context_length"].as_u64()))
        })
    }

    pub fn preload(&self) -> Result<(), LlmError> {
        if self.backend != Backend::Ollama {
            return Err(LlmError("native preload requires Ollama".into()));
        }
        self.runtime.block_on(async {
            let resp = self
                .http
                .post(format!("{}/api/chat", self.endpoint))
                .json(&self.ollama_body(&self.model))
                .send()
                .await
                .map_err(|e| LlmError(format!("Ollama preload failed: {e}")))?;
            let status = resp.status();
            let bytes = response_body(resp, 65536).await?;
            if !status.is_success() {
                return Err(LlmError(format!(
                    "preload {status}: {}",
                    short(&String::from_utf8_lossy(&bytes), 300)
                )));
            }
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| LlmError(e.to_string()))?;
            if value["done"] != true {
                return Err(LlmError("Ollama did not complete preload".into()));
            }
            Ok(())
        })
    }

    async fn complete_ollama(
        &self,
        prompt: &str,
        format: Option<&serde_json::Value>,
    ) -> Result<String, LlmError> {
        const PREFERRED: &str = "qwen2.5-coder:0.5b";
        let model = if !self.model.is_empty() {
            &self.model
        } else if let Some(model) = self.discovered.get() {
            model
        } else {
            let models = self.list_models_async().await?;
            let selected = models.into_iter().find(|name| name == PREFERRED).ok_or_else(|| {
                LlmError(format!("{PREFERRED} is unavailable on this Ollama server. Run `ollama pull {PREFERRED}` or select an installed model with --model."))
            })?;
            self.discovered.get_or_init(|| selected)
        };
        let url = format!("{}/api/chat", self.endpoint);
        let mut body = self.ollama_body(model);
        if let Some(schema) = format {
            body["format"] = schema.clone();
        }
        body["messages"] = serde_json::json!([{"role": "user", "content": prompt}]);
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
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(LlmError(format!(
                    "model {model} unavailable; inspect gico models and gico status"
                )));
            }
            return Err(LlmError(format!(
                "ollama HTTP {status}: {}",
                short(&String::from_utf8_lossy(&t), 300)
            )));
        }
        let parsed: OllamaChatResponse =
            serde_json::from_slice(&response_body(resp, 1_048_576).await?)
                .map_err(|e| LlmError(format!("ollama bad response: {e}")))?;
        self.usage.replace(parsed.prompt_eval_count.map(|n| {
            (
                n,
                parsed.eval_count.unwrap_or(0),
                parsed.done_reason.clone().unwrap_or_default(),
            )
        }));
        if parsed.done_reason.as_deref() == Some("length") {
            return Err(LlmError(
                "response truncated at configured output limit".into(),
            ));
        }
        Ok(parsed.message.content)
    }

    async fn complete_oai(
        &self,
        prompt: &str,
        format: Option<&serde_json::Value>,
    ) -> Result<String, LlmError> {
        let url = format!("{}/v1/chat/completions", self.endpoint);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
        });
        if let Some(schema) = format {
            body["response_format"] = serde_json::json!({"type":"json_schema","json_schema":{"name":"gico","strict":true,"schema":schema}});
        }
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
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError("llama.cpp returned no choices".into()))?;
        if choice.finish_reason.as_deref() == Some("length") {
            return Err(LlmError(
                "response truncated at configured output limit".into(),
            ));
        }
        Ok(choice.message.content)
    }

    /// List available models on the backend.
    pub fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.runtime.block_on(self.list_models_async())
    }

    async fn list_models_async(&self) -> Result<Vec<String>, LlmError> {
        let path = match self.backend {
            Backend::Ollama => "/api/tags",
            Backend::OpenAiCompatible => "/v1/models",
        };
        let response = self
            .http
            .get(format!("{}{path}", self.endpoint))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| LlmError(format!("could not list models: {error}")))?;
        let body = response_body(response, 1_048_576).await?;
        match self.backend {
            Backend::Ollama => serde_json::from_slice::<OllamaTags>(&body)
                .map(|tags| tags.models.into_iter().map(|m| m.name).collect()),
            Backend::OpenAiCompatible => serde_json::from_slice::<OaiModels>(&body)
                .map(|models| models.data.into_iter().map(|m| m.id).collect()),
        }
        .map_err(|error| LlmError(format!("invalid model list: {error}")))
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
