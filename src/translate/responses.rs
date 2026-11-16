//! Responses API translation through the shared tool/stream normalizer.
use crate::{
    config::Model,
    gemini::GenerateRequest,
    tools::ToolRegistry,
    translate::{request, stream},
};
use serde_json::{Value, json};
use std::collections::HashSet;

pub fn to_responses(
    req: &GenerateRequest,
    model: &Model,
    effort: Option<&str>,
    session: Option<&str>,
) -> Value {
    let chat = request::to_openai(req, model);
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for m in chat["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "system" {
            instructions.push(m["content"].as_str().unwrap_or("").to_owned());
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":m["tool_call_id"],"output":m["content"]}));
            continue;
        }
        if !m["content"].is_null() {
            let content = if let Some(parts) = m["content"].as_array() {
                Value::Array(parts.iter().map(|p| if p["type"] == "image_url" {
                    json!({"type":"input_image","image_url":p["image_url"]["url"]})
                } else { json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":p["text"]}) }).collect())
            } else {
                m["content"].clone()
            };
            input.push(json!({"role":role,"content":content}));
        }
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            input.push(json!({"type":"function_call","call_id":tc["id"],"name":tc["function"]["name"],"arguments":tc["function"]["arguments"]}));
        }
    }
    let mut body = json!({"model":model.upstream_model,"stream":true,"input":input,"max_output_tokens":chat["max_tokens"].as_u64().unwrap_or(model.max_output_tokens as u64)});
    if !instructions.is_empty() {
        body["instructions"] = instructions.join("\n").into();
    }
    if let Some(tools) = chat["tools"].as_array() {
        body["tools"] = tools.iter().map(|t| json!({"type":"function","name":t["function"]["name"],"description":t["function"]["description"],"parameters":t["function"]["parameters"]})).collect();
    }
    if let Some(e) = effort {
        body["reasoning"] = json!({"effort":if e == "max" {"high"} else {e}});
    }
    if let Some(s) = session {
        body["prompt_cache_key"] = s.into();
    }
    if let Some(v) = chat.get("tool_choice") {
        body["tool_choice"] = v.clone();
    }
    if effort.is_none() {
        for key in ["temperature", "top_p"] {
            if let Some(v) = chat.get(key) {
                body[key] = v.clone();
            }
        }
    }
    body
}

pub struct Translator {
    inner: stream::Translator,
    arguments: HashSet<u64>,
}
impl Translator {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            inner: stream::Translator::new(registry),
            arguments: HashSet::new(),
        }
    }
    pub fn push(&mut self, e: &Value) -> Result<Vec<Value>, String> {
        let mut delta = json!({});
        let index = e["output_index"].as_u64().unwrap_or(0);
        match e["type"].as_str().unwrap_or("") {
            "error" | "response.failed" => return Err("Responses upstream stream error".into()),
            "response.output_text.delta" => delta["content"] = e["delta"].clone(),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                delta["reasoning_content"] = e["delta"].clone()
            }
            "response.output_item.added" if e["item"]["type"] == "function_call" => {
                delta["tool_calls"] = json!([{"index":index,"id":e["item"]["call_id"],"function":{"name":e["item"]["name"]}}]);
            }
            "response.function_call_arguments.delta" => {
                self.arguments.insert(index);
                delta["tool_calls"] = json!([{"index":index,"function":{"arguments":e["delta"]}}]);
            }
            "response.output_item.done"
                if e["item"]["type"] == "function_call" && !self.arguments.contains(&index) =>
            {
                delta["tool_calls"] = json!([{"index":index,"id":e["item"]["call_id"],"function":{"name":e["item"]["name"],"arguments":e["item"]["arguments"]}}]);
            }
            "response.completed" | "response.incomplete" => {
                let u = &e["response"]["usage"];
                return self.inner.push(&json!({"choices":[{"delta":{},"finish_reason":if e["type"] == "response.incomplete" {"length"} else {"stop"}}],"usage":{"prompt_tokens":u["input_tokens"],"completion_tokens":u["output_tokens"],"total_tokens":u["total_tokens"]}}));
            }
            _ => {}
        }
        self.inner.push(&json!({"choices":[{"delta":delta}]}))
    }
    pub fn finish(&mut self) -> Vec<Value> {
        self.inner.finish()
    }
}
