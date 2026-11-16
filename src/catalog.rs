//! Model kataloğu: kararlı `MODEL_PLACEHOLDER_Mnnn` slotları ve Cloud Code katalog JSON'u.
//!
//! Antigravity language server yalnızca bilinen model enum değerlerini (M0..M650 = 1000..1650)
//! kabul eder. Her özel modele kalıcı bir slot ayrılır; kaldırılan modelin slotu başkasına verilmez.

use crate::config::{Config, Model};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

pub const NATIVE_CAPACITY: u32 = 651;
/// Yerel modellerin çakışmasını azaltmak için özel modeller bu aralıktan seçilir.
const POOL_START: u32 = 400;
const POOL_SIZE: u32 = 200;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Identities {
    #[serde(default)]
    pub assignments: BTreeMap<String, u32>,
    /// Upstream kataloğunda gözlenen yerel (Google) slotlar; özel modellere verilmez.
    #[serde(default)]
    pub reserved: std::collections::BTreeSet<u32>,
}

/// Upstream katalogundaki `MODEL_PLACEHOLDER_Mnnn` / `1nnn` değerlerinin indeksleri.
pub fn native_indices(upstream: &Value) -> Vec<u32> {
    let mut out = Vec::new();
    for key in ["models", "availableModels", "available_models"] {
        let entries: Vec<&Value> = match upstream.get(key) {
            Some(Value::Object(m)) => m.values().collect(),
            Some(Value::Array(a)) => a.iter().collect(),
            _ => continue,
        };
        for e in entries {
            let idx = match e.get("model") {
                Some(Value::String(s)) => placeholder_index(s),
                Some(Value::Number(n)) => {
                    n.as_u64().and_then(|n| placeholder_index(&n.to_string()))
                }
                _ => None,
            };
            out.extend(idx);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn djb2(s: &str) -> u32 {
    let mut h: i32 = 5381;
    for c in s.to_lowercase().encode_utf16() {
        h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(c as i32);
    }
    h.unsigned_abs()
}

impl Identities {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let ids: Identities = serde_json::from_str(&raw)?;
                let mut seen = HashSet::new();
                for (name, idx) in &ids.assignments {
                    if *idx >= NATIVE_CAPACITY || !seen.insert(*idx) {
                        bail!("identities.json bozuk: '{name}' → {idx}");
                    }
                }
                Ok(ids)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Gözlenen yerel slotları ayırır. Yeni bir slot eklendiyse `true` döner.
    pub fn reserve(&mut self, indices: impl IntoIterator<Item = u32>) -> bool {
        let before = self.reserved.len();
        self.reserved
            .extend(indices.into_iter().filter(|i| *i < NATIVE_CAPACITY));
        self.reserved.len() != before
    }

    /// Eksik modellere slot verir; ayrılmış (yerel) slotla çakışan atamaları düşürüp yeniden verir
    /// (yerel model kazanır). Değişiklik olduysa `true` döner.
    pub fn allocate(&mut self, ids: &[&str]) -> Result<bool> {
        let before = self.assignments.len();
        let reserved = self.reserved.clone();
        self.assignments.retain(|name, slot| {
            let keep = !reserved.contains(slot);
            if !keep {
                tracing::warn!(model = %name, slot, "slot yerel bir modelle çakıştı, yeniden atanacak");
            }
            keep
        });
        let mut changed = self.assignments.len() != before;
        for id in ids {
            if self.assignments.contains_key(*id) {
                continue;
            }
            let used: HashSet<u32> = self.assignments.values().copied().collect();
            let start = djb2(id) % POOL_SIZE;
            let slot = (0..POOL_SIZE)
                .map(|i| POOL_START + (start + i) % POOL_SIZE)
                .find(|s| !used.contains(s) && !reserved.contains(s));
            match slot {
                Some(s) => {
                    self.assignments.insert((*id).to_string(), s);
                    changed = true;
                }
                None => bail!("boş model slotu kalmadı"),
            }
        }
        Ok(changed)
    }
}

pub fn placeholder_id(index: u32) -> String {
    format!("MODEL_PLACEHOLDER_M{index}")
}

/// `MODEL_PLACEHOLDER_M452` veya sayısal `1452` → 452.
pub fn placeholder_index(selector: &str) -> Option<u32> {
    let idx = if let Some(rest) = selector.strip_prefix("MODEL_PLACEHOLDER_M") {
        rest.parse::<u32>().ok()?
    } else {
        selector.parse::<u32>().ok()?.checked_sub(1000)?
    };
    (idx < NATIVE_CAPACITY).then_some(idx)
}

pub fn slug(id: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in id.trim_start_matches("models/").chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    format!("custom-{}", out.trim_end_matches('-'))
}

#[derive(Debug)]
pub struct Catalog {
    models: Vec<Model>,
    slots: BTreeMap<String, u32>,
}

impl Catalog {
    /// Slot ataması gerekiyorsa `identities_path` üzerinde kalıcı hale getirir.
    pub fn build(cfg: &Config, identities_path: &Path) -> Result<Self> {
        let mut ids = Identities::load(identities_path)?;
        let names: Vec<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
        if ids.allocate(&names)? {
            if let Some(dir) = identities_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            ids.save(identities_path)?;
        }
        Ok(Self::from_parts(cfg.models.clone(), ids.assignments))
    }

    pub fn from_parts(models: Vec<Model>, slots: BTreeMap<String, u32>) -> Self {
        Self { models, slots }
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn slot(&self, id: &str) -> Option<u32> {
        self.slots.get(id).copied()
    }

    /// `models/x`, slug, ham id, `MODEL_PLACEHOLDER_Mnnn` veya `1nnn` → model.
    pub fn resolve(&self, selector: &str) -> Option<&Model> {
        let s = selector.trim().trim_start_matches("models/");
        if let Some(idx) = placeholder_index(s) {
            return self
                .models
                .iter()
                .find(|m| self.slots.get(&m.id) == Some(&idx));
        }
        self.models.iter().find(|m| m.id == s || slug(&m.id) == s)
    }

    /// Yerel IDE seçenekleri etikete göre çözdüğü için etiketler upstream ve kendi aralarında benzersiz olmalı.
    fn labels(&self, upstream: &Value) -> Vec<String> {
        let mut used: HashSet<String> = HashSet::new();
        for key in ["models", "availableModels", "available_models"] {
            let entries: Vec<&Value> = match upstream.get(key) {
                Some(Value::Object(m)) => m.values().collect(),
                Some(Value::Array(a)) => a.iter().collect(),
                _ => continue,
            };
            used.extend(
                entries
                    .iter()
                    .filter_map(|e| e.get("displayName").and_then(Value::as_str))
                    .map(str::to_owned),
            );
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &self.models {
            *counts.entry(m.label()).or_default() += 1;
        }
        let mut by_id: BTreeMap<&str, String> = BTreeMap::new();
        let mut sorted: Vec<&Model> = self.models.iter().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        for m in sorted {
            let base = m.label();
            let mut label = base.to_string();
            if counts[base] > 1 || used.contains(&label) {
                label = format!("{base} ({})", m.id);
            }
            let mut n = 2;
            while used.contains(&label) {
                label = format!("{base} ({} #{n})", m.id);
                n += 1;
            }
            used.insert(label.clone());
            by_id.insert(m.id.as_str(), label);
        }
        self.models
            .iter()
            .map(|m| by_id[m.id.as_str()].clone())
            .collect()
    }

    fn map_entry(&self, m: &Model, label: &str) -> Value {
        let mut mime = Map::new();
        if m.supports_vision {
            for t in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
                mime.insert(t.into(), true.into());
            }
        }
        for t in [
            "text/plain",
            "text/markdown",
            "text/html",
            "text/css",
            "application/json",
        ] {
            mime.insert(t.into(), true.into());
        }
        json!({
            "displayName": label,
            "supportsImages": m.supports_vision,
            "supportsThinking": m.supports_thinking,
            "supportsVideo": false,
            "supportedMimeTypes": mime,
            "recommended": true,
            "maxTokens": m.context_window,
            "maxOutputTokens": m.max_output_tokens,
            "tokenizerType": "LLAMA_WITH_SPECIAL",
            "model": self.slots.get(&m.id).map(|i| placeholder_id(*i)),
            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
            "modelProvider": "MODEL_PROVIDER_GOOGLE",
        })
    }

    fn list_entry(&self, m: &Model, label: &str) -> Value {
        let mut e = self.map_entry(m, label);
        let o = e.as_object_mut().unwrap();
        o.insert("name".into(), format!("models/{}", slug(&m.id)).into());
        o.insert("version".into(), "1.0".into());
        o.insert("inputTokenLimit".into(), m.context_window.into());
        o.insert("outputTokenLimit".into(), m.max_output_tokens.into());
        o.insert(
            "supportedGenerationMethods".into(),
            json!(["generateContent", "countTokens"]),
        );
        e
    }

    /// Yalnız özel modeller: `GET /v1beta/models`.
    pub fn list_json(&self) -> Value {
        json!({ "models": self.custom_list_entries(&Value::Null) })
    }

    /// `v1beta/models` biçiminde özel model girdileri (etiketler `upstream`e göre benzersiz).
    pub fn custom_list_entries(&self, upstream: &Value) -> Vec<Value> {
        let labels = self.labels(upstream);
        self.models
            .iter()
            .zip(&labels)
            .map(|(m, l)| self.list_entry(m, l))
            .collect()
    }

    /// Google'ın `GET /v1beta/models` yanıtına özel modelleri ekler (`nextPageToken` vb. korunur).
    pub fn merge_model_list(&self, upstream: Value) -> Value {
        let custom = self.custom_list_entries(
            &json!({ "models": upstream.get("models").cloned().unwrap_or(Value::Null) }),
        );
        let mut result = match upstream {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let names: HashSet<String> = custom
            .iter()
            .filter_map(|e| e["name"].as_str().map(str::to_owned))
            .collect();
        let mut models = custom;
        if let Some(Value::Array(items)) = result.remove("models") {
            models.extend(items.into_iter().filter(|i| {
                !i.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| names.contains(n))
            }));
        }
        result.insert("models".into(), Value::Array(models));
        Value::Object(result)
    }

    /// Upstream (Google) kataloğuna özel modelleri ekler; yerel modellere dokunmaz.
    /// `upstream` boşsa yalnız özel modeller döner.
    pub fn merge_fetch_available_models(&self, upstream: Value) -> Value {
        let labels = self.labels(&upstream);
        let mut result = match upstream {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let slugs: Vec<String> = self.models.iter().map(|m| slug(&m.id)).collect();
        let mut merged = false;
        for key in ["models", "availableModels", "available_models"] {
            match result.get(key).cloned() {
                Some(Value::Array(items)) => {
                    let names: HashSet<String> = self
                        .models
                        .iter()
                        .map(|m| format!("models/{}", slug(&m.id)))
                        .collect();
                    let mut out: Vec<Value> = self
                        .models
                        .iter()
                        .zip(&labels)
                        .map(|(m, l)| self.list_entry(m, l))
                        .collect();
                    out.extend(items.into_iter().filter(|i| {
                        !i.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| names.contains(n))
                    }));
                    result.insert(key.into(), Value::Array(out));
                    merged = true;
                }
                Some(Value::Object(mut map)) => {
                    for ((m, s), l) in self.models.iter().zip(&slugs).zip(&labels) {
                        map.insert(s.clone(), self.map_entry(m, l));
                    }
                    result.insert(key.into(), Value::Object(map));
                    merged = true;
                }
                _ => {}
            }
        }
        if !merged {
            let map: Map<String, Value> = self
                .models
                .iter()
                .zip(&slugs)
                .zip(&labels)
                .map(|((m, s), l)| (s.clone(), self.map_entry(m, l)))
                .collect();
            result.insert("models".into(), Value::Object(map));
        }
        if slugs.is_empty() {
            return Value::Object(result);
        }
        let mut sorts = match result.remove("agentModelSorts") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        let mut attached = false;
        for sort in &mut sorts {
            let Some(groups) = sort.get_mut("groups").and_then(Value::as_array_mut) else {
                continue;
            };
            for group in groups {
                let Some(ids) = group.get_mut("modelIds").and_then(Value::as_array_mut) else {
                    continue;
                };
                for s in &slugs {
                    if !ids.iter().any(|v| v.as_str() == Some(s)) {
                        ids.push(s.clone().into());
                    }
                }
                attached = true;
            }
        }
        if !attached {
            sorts
                .push(json!({ "displayName": "Custom models", "groups": [{ "modelIds": slugs }] }));
        }
        result.insert("agentModelSorts".into(), Value::Array(sorts));
        Value::Object(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            display_name: Some(format!("Model {id}")),
            provider: "p".into(),
            upstream_model: "up".into(),
            context_window: 1000,
            max_output_tokens: 100,
            supports_vision: true,
            supports_thinking: false,
            reasoning_effort: None,
            protocol: Default::default(),
        }
    }

    #[test]
    fn allocation_is_stable_unique_and_skips_reserved() {
        let mut ids = Identities::default();
        let first = {
            ids.allocate(&["a", "b", "c"]).unwrap();
            ids.assignments.clone()
        };
        assert_eq!(first.values().collect::<HashSet<_>>().len(), 3);
        assert!(
            first
                .values()
                .all(|v| (POOL_START..POOL_START + POOL_SIZE).contains(v))
        );
        // ikinci çağrı değişiklik yapmaz
        assert!(!ids.allocate(&["a", "b", "c"]).unwrap());
        assert_eq!(ids.assignments, first);
        // ayrılmış slot atlanır
        let mut other = Identities::default();
        let want = first["a"];
        assert!(other.reserve([want]));
        assert!(!other.reserve([want]));
        other.allocate(&["a"]).unwrap();
        assert_ne!(other.assignments["a"], want);
    }

    #[test]
    fn native_model_claiming_an_assigned_slot_wins() {
        let mut ids = Identities::default();
        ids.allocate(&["a", "b"]).unwrap();
        let (a, b) = (ids.assignments["a"], ids.assignments["b"]);
        ids.reserve([a]);
        assert!(ids.allocate(&["a", "b"]).unwrap());
        assert_ne!(ids.assignments["a"], a);
        assert_eq!(ids.assignments["b"], b, "çakışmayan model slotunu korur");
    }

    #[test]
    fn native_indices_read_every_catalog_shape() {
        let up = json!({
            "models": { "x": { "model": "MODEL_PLACEHOLDER_M18" }, "y": { "model": 1026 }, "z": { "model": "gemini" } },
            "availableModels": [{ "model": "MODEL_PLACEHOLDER_M18" }, { "model": "MODEL_PLACEHOLDER_M9999" }]
        });
        assert_eq!(native_indices(&up), vec![18, 26]);
        assert!(native_indices(&Value::Null).is_empty());
    }

    #[test]
    fn display_labels_stay_unique_against_native_and_each_other() {
        let mut a = model("a");
        let mut b = model("b");
        a.display_name = Some("Gemini 3 Pro".into()); // upstream ile çakışır
        b.display_name = Some("Gemini 3 Pro".into()); // kendi aralarında da çakışır
        let c = Catalog::from_parts(
            vec![a, b],
            BTreeMap::from([("a".to_string(), 410u32), ("b".to_string(), 411u32)]),
        );
        let up = json!({ "models": { "g": { "displayName": "Gemini 3 Pro" } } });
        let labels = c.labels(&up);
        assert_eq!(labels, vec!["Gemini 3 Pro (a)", "Gemini 3 Pro (b)"]);
        let out = c.merge_fetch_available_models(up);
        assert_eq!(out["models"]["custom-a"]["displayName"], "Gemini 3 Pro (a)");
        assert_eq!(out["models"]["g"]["displayName"], "Gemini 3 Pro");
    }

    #[test]
    fn persistence_roundtrip_and_corruption_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("identities.json");
        let mut ids = Identities::default();
        ids.allocate(&["x"]).unwrap();
        ids.save(&p).unwrap();
        assert_eq!(Identities::load(&p).unwrap(), ids);
        std::fs::write(&p, r#"{"assignments":{"a":5,"b":5}}"#).unwrap();
        assert!(Identities::load(&p).is_err());
    }

    #[test]
    fn selector_resolution() {
        let slots = BTreeMap::from([("My Model".to_string(), 452u32)]);
        let c = Catalog::from_parts(vec![model("My Model")], slots);
        assert_eq!(slug("My Model"), "custom-my-model");
        for sel in [
            "My Model",
            "models/custom-my-model",
            "custom-my-model",
            "MODEL_PLACEHOLDER_M452",
            "1452",
        ] {
            assert!(c.resolve(sel).is_some(), "{sel}");
        }
        assert!(c.resolve("MODEL_PLACEHOLDER_M453").is_none());
        assert!(c.resolve("gemini-3-pro").is_none());
    }

    #[test]
    fn merge_keeps_native_models_and_adds_custom_to_sorts() {
        let c = Catalog::from_parts(
            vec![model("m1")],
            BTreeMap::from([("m1".to_string(), 410u32)]),
        );
        let upstream = json!({
            "models": { "gemini-3": { "displayName": "Gemini 3" } },
            "agentModelSorts": [{ "groups": [{ "modelIds": ["gemini-3"] }] }]
        });
        let out = c.merge_fetch_available_models(upstream);
        assert!(out["models"]["gemini-3"].is_object());
        assert_eq!(
            out["models"]["custom-m1"]["model"],
            "MODEL_PLACEHOLDER_M410"
        );
        assert_eq!(
            out["agentModelSorts"][0]["groups"][0]["modelIds"],
            json!(["gemini-3", "custom-m1"])
        );
        // upstream yokken yalnız özel modeller
        let alone = c.merge_fetch_available_models(Value::Null);
        assert!(alone["models"]["custom-m1"].is_object());
        assert_eq!(alone["agentModelSorts"][0]["displayName"], "Custom models");
    }

    #[test]
    fn merge_handles_array_shape_without_duplicates() {
        let c = Catalog::from_parts(
            vec![model("m1")],
            BTreeMap::from([("m1".to_string(), 410u32)]),
        );
        let up = json!({ "models": [{ "name": "models/custom-m1" }, { "name": "models/gemini" }] });
        let out = c.merge_fetch_available_models(up);
        assert_eq!(out["models"].as_array().unwrap().len(), 2);
    }
}
