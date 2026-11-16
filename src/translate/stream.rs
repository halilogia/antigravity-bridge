//! OpenAI SSE → Gemini yanıt parçaları.
//!
//! Parçalar iç `GenerateContentResponse` nesneleridir; `v1internal` zarfı sunucu katmanında eklenir.

use crate::tools::{ToolRegistry, repair};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

// ───────────────────────── SSE satır ayrıştırıcı ─────────────────────────

/// Bayt akışından `data:` olaylarını çıkarır. UTF-8 sınırlarına duyarsızdır (satır `\n` baytından bölünür).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data).join("\n"));
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            } // yorum (`:`) ve diğer alanlar yok sayılır
        }
        events
    }

    /// Akış boş satırsız bittiğinde kalan olayı bırakır.
    pub fn finish(&mut self) -> Option<String> {
        self.feed(b"\n\n").into_iter().next()
    }
}

// ───────────────────────── <think> ayırıcı ─────────────────────────

/// `<think>…</think>` etiketlerini parça sınırlarını aşarak ayırır. `(düşünce_mi, metin)` üretir.
#[derive(Default)]
pub struct ThinkSplitter {
    in_think: bool,
    buf: String,
}

impl ThinkSplitter {
    pub fn feed(&mut self, s: &str) -> Vec<(bool, String)> {
        self.buf.push_str(s);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.buf.find(tag) {
                if pos > 0 {
                    out.push((self.in_think, self.buf[..pos].to_string()));
                }
                self.buf.drain(..pos + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // Etiketin yarım başlangıcını sonraki parçaya sakla.
            let keep = (1..tag.len())
                .rev()
                .find(|n| {
                    self.buf.len() >= *n
                        && self.buf.is_char_boundary(self.buf.len() - n)
                        && tag.starts_with(&self.buf[self.buf.len() - n..])
                })
                .unwrap_or(0);
            let emit = self.buf.len() - keep;
            if emit > 0 {
                out.push((self.in_think, self.buf[..emit].to_string()));
                self.buf.drain(..emit);
            }
            return out;
        }
    }

    pub fn flush(&mut self) -> Vec<(bool, String)> {
        if self.buf.is_empty() {
            Vec::new()
        } else {
            vec![(self.in_think, std::mem::take(&mut self.buf))]
        }
    }
}

// ───────────────────────── Çevirmen ─────────────────────────

const REASONING_FIELDS: [&str; 5] = [
    "reasoning_content",
    "reasoning",
    "reasoning_text",
    "thinking",
    "thinking_content",
];

#[derive(Default)]
struct ToolBuf {
    id: String,
    name: String,
    args: String,
}

pub struct Translator {
    registry: ToolRegistry,
    think: ThinkSplitter,
    tools: Vec<ToolBuf>,
    slots: HashMap<u64, usize>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

impl Translator {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            think: ThinkSplitter::default(),
            tools: Vec::new(),
            slots: HashMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    /// Bir OpenAI akış parçasını 0..n Gemini parçasına çevirir.
    pub fn push(&mut self, chunk: &Value) -> Result<Vec<Value>, String> {
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream akış hatası");
            return Err(msg.chars().take(300).collect());
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_metadata(u));
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(Vec::new());
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        let mut parts: Vec<Value> = Vec::new();

        let reasoning = REASONING_FIELDS.iter().find_map(|f| {
            delta
                .get(*f)
                .or_else(|| choice.get(*f))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
        if let Some(r) = reasoning {
            parts.push(json!({"text": r, "thought": true}));
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            for (thought, t) in self.think.feed(text) {
                parts.push(text_part(thought, t));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                self.accumulate(tc);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
            parts.extend(self.flush_think());
            parts.extend(self.flush_tools());
        }
        Ok(if parts.is_empty() {
            Vec::new()
        } else {
            vec![candidate(parts, None, None)]
        })
    }

    /// Akış bittiğinde kalanları boşaltır ve `finishReason` + `usageMetadata` taşıyan son parçayı üretir.
    pub fn finish(&mut self) -> Vec<Value> {
        let mut parts = self.flush_think();
        parts.extend(self.flush_tools());
        let mut out = Vec::new();
        if !parts.is_empty() {
            out.push(candidate(parts, None, None));
        }
        let reason = match self.finish_reason.as_deref() {
            Some("length") => "MAX_TOKENS",
            Some("content_filter") => "SAFETY",
            _ => "STOP",
        };
        out.push(candidate(
            vec![json!({"text": ""})],
            Some(reason),
            self.usage.take(),
        ));
        out
    }

    fn flush_think(&mut self) -> Vec<Value> {
        self.think
            .flush()
            .into_iter()
            .map(|(thought, t)| text_part(thought, t))
            .collect()
    }

    fn accumulate(&mut self, tc: &Value) {
        let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
        // Aynı index'te farklı id → yeni çağrı (index'i tekrar kullanan sağlayıcılar).
        let reuse = self
            .slots
            .get(&index)
            .filter(|i| id.is_empty() || self.tools[**i].id.is_empty() || self.tools[**i].id == id);
        let slot = match reuse {
            Some(i) => *i,
            None => {
                self.tools.push(ToolBuf::default());
                self.slots.insert(index, self.tools.len() - 1);
                self.tools.len() - 1
            }
        };
        let buf = &mut self.tools[slot];
        if !id.is_empty() {
            buf.id = id.to_string();
        }
        if let Some(name) = tc
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            && buf.name.is_empty()
        {
            buf.name = name.to_string();
        }
        if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
            buf.args.push_str(a);
        }
    }

    fn flush_tools(&mut self) -> Vec<Value> {
        self.slots.clear();
        std::mem::take(&mut self.tools)
            .into_iter()
            .filter(|t| !t.name.is_empty())
            .map(|t| {
                let args = repair::parse_args(&t.args).unwrap_or_else(|| {
                    tracing::warn!(tool = %t.name, "araç argümanı onarılamadı, boş nesne kullanıldı");
                    Value::Object(Map::new())
                });
                let n = self.registry.normalize(&t.name, args);
                for w in &n.warnings {
                    tracing::debug!(tool = %n.name, "{w}");
                }
                let mut call = json!({"name": n.name, "args": n.args});
                if !t.id.is_empty() {
                    call["id"] = t.id.into();
                }
                json!({"functionCall": call})
            })
            .collect()
    }
}

fn text_part(thought: bool, text: String) -> Value {
    if thought {
        json!({"text": text, "thought": true})
    } else {
        json!({"text": text})
    }
}

fn candidate(parts: Vec<Value>, finish: Option<&str>, usage: Option<Value>) -> Value {
    let mut c = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(f) = finish {
        c["finishReason"] = f.into();
    }
    let mut out = json!({"candidates": [c]});
    if let Some(u) = usage {
        out["usageMetadata"] = u;
    }
    out
}

fn usage_metadata(u: &Value) -> Value {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut m = json!({
        "promptTokenCount": n("prompt_tokens"),
        "candidatesTokenCount": n("completion_tokens"),
        "totalTokenCount": if n("total_tokens") > 0 { n("total_tokens") } else { n("prompt_tokens") + n("completion_tokens") },
    });
    if let Some(t) = u
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
    {
        m["thoughtsTokenCount"] = t.into();
    }
    m
}

/// `generateContent` (akışsız) için parçaları tek yanıtta birleştirir.
pub fn aggregate(chunks: Vec<Value>) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    let (mut finish, mut usage) = (None, None);
    for c in chunks {
        if let Some(u) = c.get("usageMetadata") {
            usage = Some(u.clone());
        }
        if let Some(f) = c.pointer("/candidates/0/finishReason") {
            finish = Some(f.clone());
        }
        for p in c
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let is_text = p.get("text").is_some() && p.get("functionCall").is_none();
            if is_text {
                let last_match = parts.last().is_some_and(|l| {
                    l.get("text").is_some() && l.get("thought") == p.get("thought")
                });
                if last_match {
                    let add = p["text"].as_str().unwrap_or("");
                    let l = parts.last_mut().unwrap();
                    l["text"] = format!("{}{}", l["text"].as_str().unwrap_or(""), add).into();
                    continue;
                }
            }
            parts.push(p.clone());
        }
    }
    parts.retain(|p| {
        p.get("text").and_then(Value::as_str) != Some("") || p.get("functionCall").is_some()
    });
    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }
    candidate(
        parts,
        Some(finish.as_ref().and_then(Value::as_str).unwrap_or("STOP")),
        usage,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::FunctionDecl;

    fn collect(tr: &mut Translator, chunks: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(tr.push(c).unwrap());
        }
        out.extend(tr.finish());
        out
    }

    #[test]
    fn sse_parser_handles_split_bytes_crlf_and_multiline() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: {\"a\"").is_empty());
        let ev = p.feed(b":1}\r\n\r\n: ping\n\ndata: x\ndata: y\n\n");
        assert_eq!(ev, vec![r#"{"a":1}"#.to_string(), "x\ny".to_string()]);
        // çok baytlı karakter iki parçaya bölünse de bozulmaz
        let bytes = "data: ş\n\n".as_bytes();
        assert!(p.feed(&bytes[..7]).is_empty());
        assert_eq!(p.feed(&bytes[7..]), vec!["ş".to_string()]);
        p.feed(b"data: tail");
        p.feed(b"\n");
        assert_eq!(p.finish().as_deref(), Some("tail"));
    }

    #[test]
    fn think_splitter_across_chunk_boundaries() {
        let mut t = ThinkSplitter::default();
        let mut got = Vec::new();
        for piece in ["hi <th", "ink>plan", " it</th", "ink>done"] {
            got.extend(t.feed(piece));
        }
        got.extend(t.flush());
        let join = |flag: bool| {
            got.iter()
                .filter(|(f, _)| *f == flag)
                .map(|(_, s)| s.as_str())
                .collect::<String>()
        };
        assert_eq!(join(true), "plan it");
        assert_eq!(join(false), "hi done");
        // etiket olmayan '<' kaybolmaz
        let mut t = ThinkSplitter::default();
        let mut all: String = t.feed("a < b <t").into_iter().map(|x| x.1).collect();
        all.extend(t.flush().into_iter().map(|x| x.1));
        assert_eq!(all, "a < b <t");
    }

    #[test]
    fn text_reasoning_usage_and_finish() {
        let mut tr = Translator::new(ToolRegistry::default());
        let out = collect(
            &mut tr,
            &[
                json!({"choices": [{"delta": {"reasoning_content": "düşün"}}]}),
                json!({"choices": [{"delta": {"content": "Merhaba"}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 7, "completion_tokens_details": {"reasoning_tokens": 2}}}),
            ],
        );
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"][0],
            json!({"text": "düşün", "thought": true})
        );
        assert_eq!(
            out[1]["candidates"][0]["content"]["parts"][0],
            json!({"text": "Merhaba"})
        );
        let last = out.last().unwrap();
        assert_eq!(last["candidates"][0]["finishReason"], "MAX_TOKENS");
        assert_eq!(
            last["usageMetadata"],
            json!({"promptTokenCount": 5, "candidatesTokenCount": 7, "totalTokenCount": 12, "thoughtsTokenCount": 2})
        );
    }

    #[test]
    fn streamed_tool_call_is_assembled_repaired_and_normalized() {
        let reg = ToolRegistry::from_declarations(&[FunctionDecl {
            name: "run_command".into(),
            description: None,
            parameters: Some(
                json!({"properties": {"CommandLine": {"type": "string"}, "WaitMsBeforeAsync": {"type": "number"}}, "required": ["CommandLine"]}),
            ),
            parameters_json_schema: None,
        }]);
        let mut tr = Translator::new(reg);
        let out = collect(
            &mut tr,
            &[
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "runCommand", "arguments": "{\"command\":\"di"}}]}}]}),
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "r\""}}]}}]}), // kapanmamış JSON
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ],
        );
        let call = &out[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "run_command");
        assert_eq!(
            call["args"],
            json!({"CommandLine": "dir", "WaitMsBeforeAsync": 0})
        );
        assert_eq!(call["id"], "c1");
        assert_eq!(out.last().unwrap()["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn parallel_calls_reusing_index_become_separate_calls() {
        let mut tr = Translator::new(ToolRegistry::default());
        let out = collect(
            &mut tr,
            &[
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": "{\"x\":1}"}}]}}]}),
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "b", "function": {"name": "g", "arguments": "{}"}}]}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ],
        );
        let parts = out[0]["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["functionCall"]["name"], "g");
    }

    #[test]
    fn think_tags_in_content_become_thoughts_and_error_chunk_fails() {
        let mut tr = Translator::new(ToolRegistry::default());
        let out = collect(
            &mut tr,
            &[json!({"choices": [{"delta": {"content": "<think>x</think>y"}}]})],
        );
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"],
            json!([{"text": "x", "thought": true}, {"text": "y"}])
        );
        let mut tr = Translator::new(ToolRegistry::default());
        assert!(
            tr.push(&json!({"error": {"message": "rate limit"}}))
                .is_err()
        );
    }

    #[test]
    fn aggregate_merges_text_and_keeps_calls() {
        let mut tr = Translator::new(ToolRegistry::default());
        let chunks = collect(
            &mut tr,
            &[
                json!({"choices": [{"delta": {"content": "a"}}]}),
                json!({"choices": [{"delta": {"content": "b"}}]}),
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "z", "function": {"name": "f", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}}),
            ],
        );
        let one = aggregate(chunks);
        let parts = one["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0], json!({"text": "ab"}));
        assert_eq!(parts[1]["functionCall"]["name"], "f");
        assert_eq!(one["usageMetadata"]["totalTokenCount"], 3);
    }
}
