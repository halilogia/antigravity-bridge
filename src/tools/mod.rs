//! Araç çağrısı normalizasyonu: isim eşleme, parametre takma adları, tip zorlama, varsayılanlar.
//!
//! İstekte bildirilen araçlar (`functionDeclarations`) yetkili kaynaktır; `well_known.json`
//! yalnızca aynı isimli araçlara takma ad ve varsayılan değer ekler.

pub mod repair;

use crate::gemini::FunctionDecl;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Object,
}

impl ParamType {
    fn from_schema(v: Option<&Value>) -> Self {
        let name = match v {
            Some(Value::String(s)) => Some(s.as_str()),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .find(|s| !s.eq_ignore_ascii_case("null")),
            _ => None,
        };
        match name.map(str::to_ascii_lowercase).as_deref() {
            Some("number") => Self::Number,
            Some("integer") => Self::Integer,
            Some("boolean") => Self::Boolean,
            Some("array") => Self::Array,
            Some("object") => Self::Object,
            _ => Self::String,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParamDef {
    #[serde(rename = "type")]
    pub ty: ParamType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub aliases: Vec<String>,
    pub params: Vec<(String, ParamDef)>,
    /// MCP/istek kaynaklı araç: yalnızca birebir/büyük-küçük harf farksız eşleşme.
    pub dynamic: bool,
    /// Şemada `additionalProperties: true` veya hiç özellik yok → bilinmeyen parametreyi atma.
    pub open: bool,
}

// well_known.json'da params nesne olduğu için ayrı ham tip.
#[derive(Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    params: indexmap_free::OrderedParams,
}

mod indexmap_free {
    use super::ParamDef;
    use serde::{Deserialize, Deserializer};
    /// Sırayı koruyarak `{ "A": {...}, "B": {...} }` okur (ek bağımlılık gerekmez).
    pub struct OrderedParams(pub Vec<(String, ParamDef)>);
    impl<'de> Deserialize<'de> for OrderedParams {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = OrderedParams;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("params nesnesi")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut m: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut v = Vec::new();
                    while let Some((k, d)) = m.next_entry::<String, ParamDef>()? {
                        v.push((k, d));
                    }
                    Ok(OrderedParams(v))
                }
            }
            d.deserialize_map(V)
        }
    }
}

const WELL_KNOWN: &str = include_str!("well_known.json");

#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub name: String,
    pub args: Value,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<ToolSchema>,
    by_exact: HashMap<String, usize>,
    by_squash: HashMap<String, usize>,
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn well_known() -> Vec<ToolSchema> {
    let raw: Vec<RawTool> =
        serde_json::from_str(WELL_KNOWN).expect("well_known.json geçerli olmalı");
    raw.into_iter()
        .map(|t| ToolSchema {
            name: t.name,
            aliases: t.aliases,
            params: t.params.0,
            dynamic: false,
            open: false,
        })
        .collect()
}

impl ToolRegistry {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// İstekte bildirilen araçlardan kayıt defteri kurar.
    pub fn from_declarations(decls: &[FunctionDecl]) -> Self {
        let known = well_known();
        let mut reg = Self::default();
        for d in decls {
            let schema = d.parameters_json_schema.as_ref().or(d.parameters.as_ref());
            let props = schema
                .and_then(|s| s.get("properties"))
                .and_then(Value::as_object);
            let required: Vec<&str> = schema
                .and_then(|s| s.get("required"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let wk = known.iter().find(|k| k.name == d.name);
            let mut params = Vec::new();
            for (pname, pschema) in props.into_iter().flatten() {
                let hint = wk
                    .and_then(|k| k.params.iter().find(|(n, _)| n == pname))
                    .map(|(_, p)| p);
                params.push((
                    pname.clone(),
                    ParamDef {
                        ty: ParamType::from_schema(pschema.get("type")),
                        required: required.contains(&pname.as_str()),
                        default: pschema
                            .get("default")
                            .cloned()
                            .or_else(|| hint.and_then(|h| h.default.clone())),
                        aliases: hint.map(|h| h.aliases.clone()).unwrap_or_default(),
                    },
                ));
            }
            let open = params.is_empty()
                || schema
                    .and_then(|s| s.get("additionalProperties"))
                    .and_then(Value::as_bool)
                    == Some(true);
            reg.insert(ToolSchema {
                name: d.name.clone(),
                aliases: wk.map(|k| k.aliases.clone()).unwrap_or_default(),
                params,
                dynamic: wk.is_none(),
                open,
            });
        }
        reg
    }

    fn insert(&mut self, tool: ToolSchema) {
        let idx = self.tools.len();
        self.by_exact.insert(tool.name.clone(), idx);
        self.by_squash.entry(squash(&tool.name)).or_insert(idx);
        for a in &tool.aliases {
            self.by_squash.entry(squash(a)).or_insert(idx);
        }
        self.tools.push(tool);
    }

    fn resolve_tool(&self, name: &str) -> Option<&ToolSchema> {
        self.by_exact
            .get(name)
            .or_else(|| self.by_squash.get(&squash(name)))
            .map(|i| &self.tools[*i])
    }

    pub fn normalize(&self, name: &str, args: Value) -> Normalized {
        let mut warnings = Vec::new();
        let args = match args {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let Some(tool) = self.resolve_tool(name) else {
            return Normalized {
                name: name.to_string(),
                args: Value::Object(args),
                warnings,
            };
        };
        if tool.name != name {
            warnings.push(format!("araç adı \"{name}\" → \"{}\"", tool.name));
        }
        let mut out = Map::new();
        for (raw_key, raw_val) in args {
            let Some((canon, def)) = resolve_param(tool, &raw_key) else {
                if tool.open {
                    out.insert(raw_key, raw_val);
                } else {
                    warnings.push(format!("bilinmeyen parametre \"{raw_key}\" atıldı"));
                }
                continue;
            };
            if canon != raw_key {
                warnings.push(format!("parametre \"{raw_key}\" → \"{canon}\""));
            }
            let coerced = coerce(raw_val.clone(), def.ty);
            if coerced != raw_val {
                warnings.push(format!("\"{canon}\" tipi {:?} olarak düzeltildi", def.ty));
            }
            out.insert(canon.to_string(), coerced);
        }
        for (pname, def) in &tool.params {
            if out.contains_key(pname) {
                continue;
            }
            if let Some(d) = &def.default {
                out.insert(pname.clone(), d.clone());
                if def.required {
                    warnings.push(format!("zorunlu \"{pname}\" varsayılanla dolduruldu"));
                }
            } else if def.required {
                warnings.push(format!("zorunlu \"{pname}\" eksik, varsayılan yok"));
            }
        }
        Normalized {
            name: tool.name.clone(),
            args: Value::Object(out),
            warnings,
        }
    }
}

fn resolve_param<'a>(tool: &'a ToolSchema, key: &str) -> Option<(&'a str, &'a ParamDef)> {
    let entry = |n: &'a (String, ParamDef)| (n.0.as_str(), &n.1);
    if let Some(p) = tool.params.iter().find(|(n, _)| n == key) {
        return Some(entry(p));
    }
    let sq = squash(key);
    if let Some(p) = tool.params.iter().find(|(n, _)| squash(n) == sq) {
        return Some(entry(p));
    }
    if tool.dynamic {
        return None;
    }
    if let Some(p) = tool
        .params
        .iter()
        .find(|(_, d)| d.aliases.iter().any(|a| squash(a) == sq))
    {
        return Some(entry(p));
    }
    // Alt dize eşleşmesi (en az 3 karakter): `file_path_abs` → `file_path` benzeri durumlar.
    if sq.len() >= 3
        && let Some(p) = tool.params.iter().find(|(n, _)| {
            let c = squash(n);
            c.len() >= 3 && (sq.contains(&c) || c.contains(&sq))
        })
    {
        return Some(entry(p));
    }
    if sq.len() >= 4 {
        return tool
            .params
            .iter()
            .filter(|(n, _)| n.len() >= 4)
            .map(|p| (levenshtein(&sq, &squash(&p.0)), p))
            .filter(|(d, _)| *d <= 2)
            .min_by_key(|(d, _)| *d)
            .map(|(_, p)| entry(p));
    }
    None
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn coerce(v: Value, ty: ParamType) -> Value {
    if v.is_null() {
        return v;
    }
    match ty {
        ParamType::Boolean => match &v {
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Value::Bool(true),
                "false" | "0" | "no" | "off" => Value::Bool(false),
                _ => v,
            },
            Value::Number(n) => Value::Bool(n.as_f64().is_some_and(|f| f != 0.0)),
            _ => v,
        },
        ParamType::Number | ParamType::Integer => {
            let n = match &v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
                _ => None,
            };
            match n {
                Some(f) if ty == ParamType::Integer || (f.fract() == 0.0 && f.abs() < 9e15) => {
                    Value::from(f.floor() as i64)
                }
                Some(f) => Value::from(f),
                None => v,
            }
        }
        ParamType::String => match v {
            Value::Number(_) | Value::Bool(_) => Value::String(v.to_string()),
            other => other,
        },
        ParamType::Array => match v {
            Value::Array(_) => v,
            Value::String(ref s) => {
                if let Ok(a @ Value::Array(_)) = serde_json::from_str::<Value>(s) {
                    a
                } else if s.contains(',') {
                    Value::Array(
                        s.split(',')
                            .map(|p| p.trim())
                            .filter(|p| !p.is_empty())
                            .map(Value::from)
                            .collect(),
                    )
                } else {
                    Value::Array(vec![v.clone()])
                }
            }
            Value::Object(_) => Value::Array(vec![v]),
            other => other,
        },
        ParamType::Object => match v {
            Value::String(ref s) => serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_object)
                .unwrap_or(v),
            other => other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decl(name: &str, schema: Value) -> FunctionDecl {
        FunctionDecl {
            name: name.into(),
            description: None,
            parameters: Some(schema),
            parameters_json_schema: None,
        }
    }

    fn run_command_registry() -> ToolRegistry {
        ToolRegistry::from_declarations(&[decl(
            "run_command",
            json!({"type":"object","properties":{
                "CommandLine":{"type":"string"},"Cwd":{"type":"string"},"WaitMsBeforeAsync":{"type":"number"}
            },"required":["CommandLine"]}),
        )])
    }

    #[test]
    fn well_known_json_parses() {
        assert_eq!(well_known().len(), 7);
    }

    #[test]
    fn name_and_param_aliases_resolve() {
        let r = run_command_registry();
        let n = r.normalize(
            "runCommand",
            json!({"command":"dir","working_dir":"C:\\","bogus":1}),
        );
        assert_eq!(n.name, "run_command");
        assert_eq!(
            n.args,
            json!({"CommandLine":"dir","Cwd":"C:\\","WaitMsBeforeAsync":0})
        );
        assert!(n.warnings.iter().any(|w| w.contains("bogus")));
    }

    #[test]
    fn types_are_coerced() {
        let r = ToolRegistry::from_declarations(&[decl(
            "write_to_file",
            json!({"properties":{"TargetFile":{"type":"string"},"Overwrite":{"type":"boolean"},"N":{"type":"integer"},"Tags":{"type":"array"}},"required":["Overwrite"]}),
        )]);
        let n = r.normalize(
            "write_to_file",
            json!({"TargetFile":42,"Overwrite":"yes","N":"7.9","Tags":"a, b"}),
        );
        assert_eq!(
            n.args,
            json!({"TargetFile":"42","Overwrite":true,"N":7,"Tags":["a","b"]})
        );
    }

    #[test]
    fn dynamic_tools_do_not_fuzzy_match_and_strip_nothing_when_open() {
        let r = ToolRegistry::from_declarations(&[decl(
            "mcp_x",
            json!({"properties":{"query":{"type":"string"}}}),
        )]);
        let n = r.normalize("mcp_x", json!({"Query":"a","q":"b"}));
        assert_eq!(n.args, json!({"query":"a"})); // "q" bilinmeyen → atıldı (kapalı şema)
        let open = ToolRegistry::from_declarations(&[decl("free", json!({"type":"object"}))]);
        assert_eq!(
            open.normalize("free", json!({"anything":1})).args,
            json!({"anything":1})
        );
    }

    #[test]
    fn unknown_tool_passes_through_and_single_object_wrapped() {
        let r = ToolRegistry::from_declarations(&[decl(
            "invoke",
            json!({"properties":{"Subagents":{"type":"array"}}}),
        )]);
        assert_eq!(r.normalize("zzz", json!({"a":1})).args, json!({"a":1}));
        assert_eq!(
            r.normalize("invoke", json!({"Subagents":{"x":1}})).args,
            json!({"Subagents":[{"x":1}]})
        );
    }

    #[test]
    fn non_object_args_become_empty_and_levenshtein_fixes_typos() {
        let r = run_command_registry();
        let n = r.normalize("run_command", json!({"CommandLin":"x"}));
        assert_eq!(n.args["CommandLine"], "x");
        assert_eq!(
            r.normalize("run_command", json!("garbage")).args["WaitMsBeforeAsync"],
            0
        );
    }
}
