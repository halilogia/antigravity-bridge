//! Antigravity'nin konuştuğu Gemini / Cloud Code (`v1internal`) istek tipleri.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GenerateRequest {
    pub contents: Vec<Content>,
    pub system_instruction: Option<Content>,
    pub tools: Vec<Tool>,
    pub tool_config: Option<Value>,
    pub generation_config: Option<GenerationConfig>,
    /// Antigravity'nin konuşma oturumu kimliği (`request.sessionId`); önbellek/oturum eşlemesi için.
    #[serde(alias = "session_id")]
    pub session_id: Option<String>,
    #[serde(alias = "conversation_id")]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Content {
    pub role: Option<String>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Part {
    pub text: Option<String>,
    pub thought: Option<bool>,
    pub function_call: Option<FunctionCall>,
    pub function_response: Option<FunctionResponse>,
    pub inline_data: Option<InlineData>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FunctionCall {
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FunctionResponse {
    pub id: Option<String>,
    pub name: String,
    pub response: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InlineData {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Tool {
    pub function_declarations: Vec<FunctionDecl>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FunctionDecl {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub parameters_json_schema: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GenerationConfig {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop_sequences: Vec<String>,
}

/// Çözümlenmiş gelen istek.
#[derive(Debug)]
pub struct Parsed {
    pub request: GenerateRequest,
    /// `v1internal` zarfı: yanıtlar `{"response": …}` içinde dönmeli.
    pub cloud: bool,
    /// Zarftaki `model` (varsa).
    pub model: Option<String>,
    /// Zarftaki `requestId` (varsa).
    pub request_id: Option<String>,
}

pub fn parse_body(bytes: &[u8]) -> Result<Parsed> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("geçersiz JSON gövdesi: {e}"))?;
    let Value::Object(mut obj) = root else {
        bail!("gövde bir JSON nesnesi olmalı")
    };
    let model = obj.get("model").and_then(Value::as_str).map(str::to_owned);
    let request_id = obj
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let session_id = obj
        .get("sessionId")
        .or_else(|| obj.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let conversation_id = obj
        .get("conversationId")
        .or_else(|| obj.get("conversation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (inner, cloud) = match obj.remove("request") {
        Some(inner @ Value::Object(_)) => (inner, true),
        _ => (Value::Object(obj), false),
    };
    let mut request: GenerateRequest = serde_json::from_value(inner)
        .map_err(|e| anyhow::anyhow!("geçersiz Gemini isteği: {e}"))?;
    request.session_id = request.session_id.or(session_id);
    request.conversation_id = request.conversation_id.or(conversation_id);
    Ok(Parsed {
        request,
        cloud,
        model,
        request_id,
    })
}

pub fn wrap(value: Value, cloud: bool) -> Value {
    if cloud {
        serde_json::json!({ "response": value, "traceId": "", "metadata": {} })
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cloud_envelope_and_plain() {
        let cloud = br#"{"project":"p","model":"MODEL_PLACEHOLDER_M410","requestId":"r1","request":{"sessionId":"s1","contents":[{"role":"user","parts":[{"text":"hi"}]}]}}"#;
        let p = parse_body(cloud).unwrap();
        assert!(p.cloud);
        assert_eq!(p.model.as_deref(), Some("MODEL_PLACEHOLDER_M410"));
        assert_eq!(p.request_id.as_deref(), Some("r1"));
        assert_eq!(p.request.session_id.as_deref(), Some("s1"));
        assert_eq!(p.request.contents[0].parts[0].text.as_deref(), Some("hi"));
        let plain = parse_body(br#"{"contents":[],"unknownField":1}"#).unwrap();
        assert!(!plain.cloud);
        assert!(parse_body(b"[1]").is_err());
    }
}
