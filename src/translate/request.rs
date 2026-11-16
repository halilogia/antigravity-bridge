//! Gemini `contents` → OpenAI `chat/completions` istek gövdesi.

use crate::config::Model;
use crate::gemini::GenerateRequest;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};

pub fn to_openai(req: &GenerateRequest, model: &Model) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(sys) = &req.system_instruction {
        let text = join_text(
            sys.parts
                .iter()
                .filter(|p| p.thought != Some(true))
                .filter_map(|p| p.text.as_deref()),
        );
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let mut pending: HashMap<String, VecDeque<String>> = HashMap::new();
    let mut counter = 0usize;
    let mut next_id = |name: &str| {
        counter += 1;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(48)
            .collect();
        format!("call_{safe}_{counter}")
    };

    for content in &req.contents {
        if content.role.as_deref() == Some("model") {
            let text = join_text(
                content
                    .parts
                    .iter()
                    .filter(|p| p.thought != Some(true))
                    .filter_map(|p| p.text.as_deref()),
            );
            let thought = join_text(
                content
                    .parts
                    .iter()
                    .filter(|p| p.thought == Some(true))
                    .filter_map(|p| p.text.as_deref()),
            );
            let mut calls = Vec::new();
            for fc in content
                .parts
                .iter()
                .filter_map(|p| p.function_call.as_ref())
            {
                let id = fc
                    .id
                    .clone()
                    .filter(|i| !i.is_empty())
                    .unwrap_or_else(|| next_id(&fc.name));
                pending
                    .entry(fc.name.clone())
                    .or_default()
                    .push_back(id.clone());
                let args = if fc.args.is_object() {
                    fc.args.to_string()
                } else {
                    "{}".to_string()
                };
                calls.push(json!({"id": id, "type": "function", "function": {"name": fc.name, "arguments": args}}));
            }
            if text.is_empty() && thought.is_empty() && calls.is_empty() {
                continue;
            }
            let mut msg = Map::new();
            msg.insert("role".into(), "assistant".into());
            msg.insert("content".into(), text.into());
            if !thought.is_empty() {
                msg.insert("reasoning_content".into(), thought.into());
            }
            if !calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(calls));
            }
            messages.push(Value::Object(msg));
            continue;
        }

        // Kullanıcı / function rolü: önce araç sonuçları, sonra serbest içerik.
        for fr in content
            .parts
            .iter()
            .filter_map(|p| p.function_response.as_ref())
        {
            let queue = pending.entry(fr.name.clone()).or_default();
            let id = match fr.id.clone().filter(|i| !i.is_empty()) {
                Some(id) => {
                    queue.retain(|q| *q != id);
                    id
                }
                None => queue.pop_front().unwrap_or_else(|| next_id(&fr.name)),
            };
            messages.push(
                json!({"role": "tool", "tool_call_id": id, "content": tool_content(&fr.response)}),
            );
        }
        let text = join_text(
            content
                .parts
                .iter()
                .filter(|p| p.thought != Some(true))
                .filter_map(|p| p.text.as_deref()),
        );
        let mut images = Vec::new();
        let mut omitted = Vec::new();
        for d in content.parts.iter().filter_map(|p| p.inline_data.as_ref()) {
            if d.mime_type.starts_with("image/") && model.supports_vision {
                images.push(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", d.mime_type, d.data)}}));
            } else {
                omitted.push(d.mime_type.clone());
            }
        }
        let mut text = text;
        for mime in omitted {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[ek desteklenmiyor: {mime}]"));
        }
        if images.is_empty() {
            if !text.is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            }
        } else {
            let mut parts = vec![json!({"type": "text", "text": text})];
            parts.extend(images);
            messages.push(json!({"role": "user", "content": parts}));
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), model.upstream_model.clone().into());
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), true.into());
    body.insert("stream_options".into(), json!({"include_usage": true}));

    let tools: Vec<Value> = req
        .tools
        .iter()
        .flat_map(|t| &t.function_declarations)
        .map(|d| {
            let schema = d.parameters_json_schema.as_ref().or(d.parameters.as_ref());
            let mut params = schema.map(|s| convert_schema(s, false)).unwrap_or_else(|| json!({}));
            if let Some(o) = params.as_object_mut() {
                o.entry("type").or_insert("object".into());
                if o.get("type") == Some(&Value::String("object".into())) {
                    o.entry("properties").or_insert_with(|| json!({}));
                }
            }
            json!({"type": "function", "function": {"name": d.name, "description": d.description.clone().unwrap_or_default(), "parameters": params}})
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        let mode = req
            .tool_config
            .as_ref()
            .and_then(|c| c.pointer("/functionCallingConfig/mode"))
            .and_then(Value::as_str);
        match mode {
            Some("NONE") => {
                body.insert("tool_choice".into(), "none".into());
            }
            Some("ANY") => {
                body.insert("tool_choice".into(), "required".into());
            }
            _ => {}
        }
    }

    if let Some(g) = &req.generation_config {
        if let Some(max) = g.max_output_tokens {
            body.insert("max_tokens".into(), max.min(model.max_output_tokens).into());
        }
        if let Some(t) = g.temperature {
            body.insert("temperature".into(), t.into());
        }
        if let Some(p) = g.top_p {
            body.insert("top_p".into(), p.into());
        }
        if !g.stop_sequences.is_empty() {
            body.insert("stop".into(), g.stop_sequences.clone().into());
        }
    }
    if let Some(e) = &model.reasoning_effort {
        body.insert("reasoning_effort".into(), e.clone().into());
    }
    Value::Object(body)
}

fn join_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts.collect::<Vec<_>>().join("")
}

/// `{"output":"…"}` gibi tek alanlı sarmalayıcıları düz metne indirir.
fn tool_content(resp: &Value) -> String {
    if let Value::Object(o) = resp
        && o.len() == 1
        && let Some(Value::String(s)) = ["output", "result", "content", "text", "error"]
            .iter()
            .find_map(|k| o.get(*k))
    {
        return s.clone();
    }
    match resp {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Gemini şemasını JSON Schema'ya çevirir: `type` küçük harf; `nullable`/`$schema` atılır.
/// `properties` altındaki anahtarlar özellik adıdır, şema anahtarı değildir.
fn convert_schema(v: &Value, props_map: bool) -> Value {
    match v {
        Value::Object(o) if props_map => Value::Object(
            o.iter()
                .map(|(k, s)| (k.clone(), convert_schema(s, false)))
                .collect(),
        ),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, val) in o {
                match k.as_str() {
                    "nullable" | "$schema" => {}
                    "type" => {
                        let lowered = match val {
                            Value::String(s) => Value::String(s.to_lowercase()),
                            Value::Array(a) => Value::Array(
                                a.iter()
                                    .map(|x| {
                                        x.as_str().map_or(x.clone(), |s| s.to_lowercase().into())
                                    })
                                    .collect(),
                            ),
                            other => other.clone(),
                        };
                        out.insert(k.clone(), lowered);
                    }
                    "properties" if val.is_object() => {
                        out.insert(k.clone(), convert_schema(val, true));
                    }
                    _ => {
                        out.insert(k.clone(), convert_schema(val, false));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| convert_schema(x, false)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::parse_body;

    fn model(vision: bool) -> Model {
        Model {
            id: "m".into(),
            display_name: None,
            provider: "p".into(),
            upstream_model: "deepseek-chat".into(),
            context_window: 1000,
            max_output_tokens: 4096,
            supports_vision: vision,
            supports_thinking: false,
            reasoning_effort: Some("high".into()),
            protocol: Default::default(),
        }
    }

    fn convert(body: Value, vision: bool) -> Value {
        let parsed = parse_body(body.to_string().as_bytes()).unwrap();
        to_openai(&parsed.request, &model(vision))
    }

    #[test]
    fn tool_round_trip_matches_call_ids_in_order() {
        let out = convert(
            json!({
                "systemInstruction": {"parts": [{"text": "sys"}]},
                "contents": [
                    {"role": "user", "parts": [{"text": "run"}]},
                    {"role": "model", "parts": [
                        {"thought": true, "text": "hmm"},
                        {"functionCall": {"name": "run_command", "args": {"CommandLine": "ls"}}},
                        {"functionCall": {"name": "run_command", "args": {"CommandLine": "pwd"}}}]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "run_command", "response": {"output": "a"}}},
                        {"functionResponse": {"name": "run_command", "response": {"output": "b", "exit": 0}}}]}
                ]
            }),
            false,
        );
        let m = out["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        let calls = m[2]["tool_calls"].as_array().unwrap();
        assert_eq!(m[2]["reasoning_content"], "hmm");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], calls[0]["id"]);
        assert_eq!(m[3]["content"], "a");
        assert_eq!(m[4]["tool_call_id"], calls[1]["id"]);
        assert_eq!(m[4]["content"], r#"{"exit":0,"output":"b"}"#);
        assert_eq!(out["model"], "deepseek-chat");
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn explicit_ids_are_preserved() {
        let out = convert(
            json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"id": "abc", "name": "t", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "abc", "name": "t", "response": {"result": "ok"}}}]}]}),
            false,
        );
        assert_eq!(out["messages"][0]["tool_calls"][0]["id"], "abc");
        assert_eq!(out["messages"][1]["tool_call_id"], "abc");
        assert_eq!(out["messages"][1]["content"], "ok");
    }

    #[test]
    fn images_follow_model_capability() {
        let body = json!({"contents": [{"role": "user", "parts": [{"text": "see"}, {"inlineData": {"mimeType": "image/png", "data": "AAA"}}]}]});
        let with = convert(body.clone(), true);
        assert_eq!(
            with["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAA"
        );
        let without = convert(body, false);
        assert!(
            without["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("[ek desteklenmiyor: image/png]")
        );
    }

    #[test]
    fn schema_types_lowercased_but_property_named_type_survives() {
        let out = convert(
            json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}],
                   "tools": [{"functionDeclarations": [{"name": "f", "parameters": {
                       "type": "OBJECT", "properties": {"type": {"type": "STRING", "nullable": true}, "n": {"type": "ARRAY", "items": {"type": "INTEGER"}}}}}]}],
                   "toolConfig": {"functionCallingConfig": {"mode": "ANY"}},
                   "generationConfig": {"maxOutputTokens": 99999, "temperature": 0.2, "stopSequences": ["x"]}}),
            false,
        );
        let p = &out["tools"][0]["function"]["parameters"];
        assert_eq!(p["type"], "object");
        assert_eq!(p["properties"]["type"], json!({"type": "string"}));
        assert_eq!(p["properties"]["n"]["items"]["type"], "integer");
        assert_eq!(out["tool_choice"], "required");
        assert_eq!(out["max_tokens"], 4096);
        assert_eq!(out["stop"], json!(["x"]));
    }

    #[test]
    fn tool_without_parameters_gets_empty_object_schema() {
        let out = convert(
            json!({"contents": [], "tools": [{"functionDeclarations": [{"name": "noargs"}]}]}),
            false,
        );
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }
}
