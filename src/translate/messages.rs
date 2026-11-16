//! Messages API translation through the shared tool/stream normalizer.
use crate::{
    config::Model,
    gemini::GenerateRequest,
    tools::ToolRegistry,
    translate::{request, stream},
};
use serde_json::{Value, json};

pub fn to_messages(
    req: &GenerateRequest,
    model: &Model,
    effort: Option<&str>,
    session: Option<&str>,
) -> Value {
    let chat = request::to_openai(req, model);
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for m in chat["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "system" {
            system.push(m["content"].as_str().unwrap_or("").to_owned());
            continue;
        }
        let mut blocks = Vec::new();
        if role == "tool" {
            blocks.push(json!({"type":"tool_result", "tool_use_id":m["tool_call_id"], "content":m["content"]}));
        } else {
            if let Some(text) = m["content"].as_str().filter(|t| !t.is_empty()) {
                blocks.push(json!({"type":"text","text":text}));
            }
            for part in m["content"].as_array().into_iter().flatten() {
                if part["type"] == "text" {
                    blocks.push(part.clone());
                }
                if let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str)
                    && let Some((mime, data)) = url
                        .strip_prefix("data:")
                        .and_then(|s| s.split_once(";base64,"))
                {
                    blocks.push(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}));
                }
            }
            for tc in m["tool_calls"].as_array().into_iter().flatten() {
                let input: Value =
                    serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}"))
                        .unwrap_or(json!({}));
                blocks.push(json!({"type":"tool_use","id":tc["id"],"name":tc["function"]["name"],"input":input}));
            }
        }
        messages.push(json!({"role":if role == "tool" {"user"} else {role},"content":blocks}));
    }
    let max = chat["max_tokens"]
        .as_u64()
        .unwrap_or(model.max_output_tokens as u64);
    let mut body =
        json!({"model":model.upstream_model,"stream":true,"max_tokens":max,"messages":messages});
    if !system.is_empty() {
        body["system"] = system.join("\n").into();
    }
    if let Some(tools) = chat["tools"].as_array() {
        body["tools"] = tools.iter().map(|t| json!({"name":t["function"]["name"],"description":t["function"]["description"],"input_schema":t["function"]["parameters"]})).collect();
    }
    match chat["tool_choice"].as_str() {
        Some("none") => {
            body.as_object_mut().unwrap().remove("tools");
        }
        Some("required") => {
            body["tool_choice"] = json!({"type":"any"});
        }
        _ => {}
    }
    if let Some(e) = effort {
        let max = body["max_tokens"].as_u64().unwrap_or(8192);
        if max > 1024 {
            let budget = match e {
                "low" => 1024,
                "medium" => 4096,
                _ => 16384,
            }
            .min(max - 1);
            body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
        }
    } else {
        for key in ["temperature", "top_p"] {
            if let Some(v) = chat.get(key) {
                body[key] = v.clone();
            }
        }
    }
    if let Some(v) = chat.get("stop") {
        body["stop_sequences"] = v.clone();
    }
    if let Some(s) = session {
        body["metadata"] = json!({"user_id":s});
    }
    body
}

pub struct Translator {
    inner: stream::Translator,
    input: u64,
    output: u64,
}
impl Translator {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            inner: stream::Translator::new(registry),
            input: 0,
            output: 0,
        }
    }
    pub fn push(&mut self, e: &Value) -> Result<Vec<Value>, String> {
        let mut delta = json!({});
        let mut reason = Value::Null;
        match e["type"].as_str().unwrap_or("") {
            "error" => return Err("Messages upstream stream error".into()),
            "message_start" => {
                let u = &e["message"]["usage"];
                self.input = u["input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let b = &e["content_block"];
                match b["type"].as_str().unwrap_or("") {
                    "tool_use" => {
                        let args = if b["input"].as_object().is_some_and(|o| !o.is_empty()) {
                            b["input"].to_string()
                        } else {
                            String::new()
                        };
                        delta["tool_calls"] = json!([{"index":e["index"],"id":b["id"],"function":{"name":b["name"],"arguments":args}}]);
                    }
                    "text" => delta["content"] = b["text"].clone(),
                    "thinking" => delta["reasoning_content"] = b["thinking"].clone(),
                    _ => {}
                }
            }
            "content_block_delta" => match e["delta"]["type"].as_str().unwrap_or("") {
                "text_delta" => delta["content"] = e["delta"]["text"].clone(),
                "thinking_delta" => delta["reasoning_content"] = e["delta"]["thinking"].clone(),
                "input_json_delta" => {
                    delta["tool_calls"] = json!([{"index":e["index"],"function":{"arguments":e["delta"]["partial_json"]}}])
                }
                _ => {}
            },
            "message_delta" => {
                self.output = e["usage"]["output_tokens"].as_u64().unwrap_or(self.output);
                reason = if e["delta"]["stop_reason"] == "max_tokens" {
                    "length"
                } else {
                    "stop"
                }
                .into();
            }
            _ => {}
        }
        self.inner.push(&json!({"choices":[{"delta":delta,"finish_reason":reason}],"usage":{"prompt_tokens":self.input,"completion_tokens":self.output,"total_tokens":self.input+self.output}}))
    }
    pub fn finish(&mut self) -> Vec<Value> {
        self.inner.finish()
    }
}
