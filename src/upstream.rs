//! Transport and protocol dispatch. Never retry after streaming starts.
use crate::{
    config::{Model, Protocol, Provider, ProviderKind},
    gemini::GenerateRequest,
    opencode,
    tools::ToolRegistry,
    translate::{messages, request, responses, stream},
};
use anyhow::{Result, bail};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, Mutex},
};

pub type Sessions = Arc<Mutex<BTreeMap<String, String>>>;
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

// Buffer only the prelude. Once content/tool data is observed, retry is permanently disabled.
async fn prelude(response: reqwest::Response) -> Result<ByteStream, String> {
    let mut source = response.bytes_stream();
    let mut prefix = Vec::new();
    let mut parser = stream::SseParser::default();
    'read: while prefix.len() < 65536 {
        let Some(chunk) = source.next().await else {
            break;
        };
        let chunk = chunk.map_err(|_| "upstream stream disconnected".to_owned())?;
        prefix.extend_from_slice(&chunk);
        for event in parser.feed(&chunk) {
            let Ok(value) = serde_json::from_str::<Value>(&event) else {
                break 'read;
            };
            if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
                return Err(error.to_string());
            }
            let kind = value["type"].as_str().unwrap_or("");
            let neutral = matches!(
                kind,
                "message_start" | "ping" | "response.created" | "response.in_progress"
            ) || (kind.is_empty()
                && value
                    .pointer("/choices/0/delta")
                    .is_some_and(|d| d.as_object().is_some_and(|o| o.keys().all(|k| k == "role"))))
                || (kind.is_empty()
                    && value.get("choices").is_none()
                    && value.get("session_id").is_some());
            if !neutral {
                break 'read;
            }
        }
    }
    Ok(Box::pin(
        futures_util::stream::once(async move { Ok(bytes::Bytes::from(prefix)) }).chain(source),
    ))
}

fn enable_retry_thinking(body: &mut Value, protocol: Protocol) {
    match protocol {
        Protocol::Responses => body["reasoning"]["effort"] = "low".into(),
        Protocol::Messages => {
            body["thinking"] = serde_json::json!({"type":"enabled", "budget_tokens":1024});
        }
        _ => body["reasoning_effort"] = "low".into(),
    }
}
pub struct Session {
    cache: Sessions,
    key: String,
}
impl Session {
    pub fn capture(&self, event: &Value) {
        let id = event
            .get("session_id")
            .or_else(|| event.pointer("/response/session_id"))
            .or_else(|| event.pointer("/metadata/session_id"));
        if let Some(id) = id.and_then(Value::as_str).filter(|s| {
            !s.is_empty() && s.len() <= 256 && s.is_ascii() && !s.chars().any(char::is_control)
        }) {
            let mut cache = self.cache.lock().unwrap();
            if cache.len() >= 256 && !cache.contains_key(&self.key) {
                cache.pop_first();
            }
            cache.insert(self.key.clone(), id.to_owned());
        }
    }
}

pub enum Translator {
    Chat(stream::Translator),
    Messages(messages::Translator),
    Responses(responses::Translator),
}
impl Translator {
    pub fn push(&mut self, event: &Value) -> Result<Vec<Value>, String> {
        match self {
            Self::Chat(t) => t.push(event),
            Self::Messages(t) => t.push(event),
            Self::Responses(t) => t.push(event),
        }
    }
    pub fn finish(&mut self) -> Vec<Value> {
        match self {
            Self::Chat(t) => t.finish(),
            Self::Messages(t) => t.finish(),
            Self::Responses(t) => t.finish(),
        }
    }
}

pub async fn read_limited(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.without_url())?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            bail!("upstream response exceeds limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn session_key(provider: &str, req: &GenerateRequest) -> Result<String> {
    // Identical prompts can belong to different conversations; never use prompt text as identity.
    if let Some(id) = req
        .session_id
        .as_deref()
        .or(req.conversation_id.as_deref())
        .filter(|s| !s.is_empty())
    {
        Ok(Sha256::digest(format!("{provider}\0{id}"))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    } else {
        crate::config::generate_token()
    }
}

pub async fn send(
    client: &reqwest::Client,
    provider: &Provider,
    model: &Model,
    req: &GenerateRequest,
    registry: ToolRegistry,
    sessions: &Sessions,
) -> Result<(ByteStream, Translator, Option<Session>), crate::server::ApiError> {
    use crate::server::ApiError;
    let is_oc = provider.kind == ProviderKind::Opencode;
    if is_oc
        && model.protocol == Protocol::Auto
        && model
            .upstream_model
            .trim_start_matches("models/")
            .starts_with("gemini-")
    {
        return Err(ApiError(400, "OpenCode Google-native models are not supported; use a chat, responses or messages model".into()));
    }
    let protocol = if is_oc {
        opencode::protocol(model, opencode::Plan::of(provider))
    } else {
        Protocol::Chat
    };
    let session = session_key(&model.provider, req)
        .map_err(|_| ApiError(500, "session generation failed".into()))?;
    let key_hash: String = Sha256::digest(provider.resolve_key().unwrap_or_default())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let cache_key = format!(
        "{}:{}:{}:{}:{}",
        provider.base_url,
        provider.plan.as_deref().unwrap_or(""),
        model.upstream_model,
        key_hash,
        session
    );
    let cached = sessions
        .lock()
        .unwrap()
        .get(&cache_key)
        .cloned()
        .unwrap_or_else(|| session.clone());
    let effort = model
        .reasoning_effort
        .as_deref()
        .or(if is_oc { Some("max") } else { None })
        .filter(|e| *e != "default");
    let mut body = match protocol {
        Protocol::Messages => messages::to_messages(req, model, effort, Some(&session)),
        Protocol::Responses => responses::to_responses(req, model, effort, Some(&session)),
        _ => request::to_openai(req, model),
    };
    if is_oc {
        body["session_id"] = cached.into();
        if protocol == Protocol::Chat
            && let Some(effort) = effort
        {
            body["reasoning_effort"] = effort.into();
        }
    }
    let action = match protocol {
        Protocol::Messages => "messages",
        Protocol::Responses => "responses",
        _ => "chat/completions",
    };
    let url = if is_oc {
        opencode::endpoint(provider, protocol, action)
    } else {
        provider.chat_url()
    };
    for attempt in 0..2 {
        let mut request = client.post(&url).json(&body);
        for (key, value) in &provider.headers {
            request = request.header(key, value);
        }
        if let Some(key) = provider.resolve_key() {
            request = request.bearer_auth(key);
        }
        if is_oc {
            request = request
                .header("x-opencode-session", &session)
                .header("User-Agent", "antigravity-bridge/0.1.0");
        }
        if protocol == Protocol::Messages {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = request
            .send()
            .await
            .map_err(|_| ApiError(502, "upstream connection failed".into()))?;
        let status = response.status();
        if status.is_success() {
            let bytes: ByteStream = if is_oc {
                match prelude(response).await {
                    Ok(bytes) => bytes,
                    Err(error) if attempt == 0 && opencode::thinking_required(&error) => {
                        enable_retry_thinking(&mut body, protocol);
                        continue;
                    }
                    Err(_) => return Err(ApiError(502, "upstream stream rejected".into())),
                }
            } else {
                Box::pin(response.bytes_stream())
            };
            let translator = match protocol {
                Protocol::Messages => Translator::Messages(messages::Translator::new(registry)),
                Protocol::Responses => Translator::Responses(responses::Translator::new(registry)),
                _ => Translator::Chat(stream::Translator::new(registry)),
            };
            return Ok((
                bytes,
                translator,
                is_oc.then(|| Session {
                    cache: sessions.clone(),
                    key: cache_key,
                }),
            ));
        }
        let error = read_limited(response, 8192).await.unwrap_or_default();
        if is_oc
            && attempt == 0
            && status.as_u16() == 400
            && opencode::thinking_required(&String::from_utf8_lossy(&error))
        {
            enable_retry_thinking(&mut body, protocol);
            continue;
        }
        // Error bodies can echo credentials and prompts; only expose the status.
        return Err(ApiError(
            if matches!(status.as_u16(), 400 | 429) {
                status.as_u16()
            } else {
                502
            },
            format!("upstream HTTP {}", status.as_u16()),
        ));
    }
    unreachable!()
}
