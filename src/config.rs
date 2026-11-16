//! Yapılandırma: `config.json` okuma, doğrulama ve atomik yazma.

use crate::secrets::SecretStore;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use url::Url;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:4000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// `/v1beta/*` ve `/admin/*` için isteğe bağlı Bearer / x-goog-api-key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_token: Option<String>,
    /// `/admin/*` için zorunlu token. Yoksa yönetim API'si kapalıdır.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<String>,
    /// Özel olmayan trafiğin aktarıldığı Google kökleri (testte sahte sunucuya çevrilir).
    #[serde(default, skip_serializing_if = "GoogleOrigins::is_default")]
    pub google: GoogleOrigins,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoogleOrigins {
    #[serde(default = "prod_origin")]
    pub cloudcode_production: String,
    #[serde(default = "daily_origin")]
    pub cloudcode_daily: String,
    #[serde(default = "generative_origin")]
    pub generative: String,
}

fn prod_origin() -> String {
    "https://cloudcode-pa.googleapis.com".into()
}
fn daily_origin() -> String {
    "https://daily-cloudcode-pa.googleapis.com".into()
}
fn generative_origin() -> String {
    "https://generativelanguage.googleapis.com".into()
}

impl Default for GoogleOrigins {
    fn default() -> Self {
        Self {
            cloudcode_production: prod_origin(),
            cloudcode_daily: daily_origin(),
            generative: generative_origin(),
        }
    }
}

impl GoogleOrigins {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_local_host(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "[::1]" | "::1"))
}

/// Kriptografik rastgele 256-bit token (hex).
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("rastgele sayı üretilemedi: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sağlayıcı türü: `openai` genel OpenAI uyumlu uçlar, `opencode` OpenCode Go / Zen ağ geçidi.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Openai,
    Opencode,
}

impl ProviderKind {
    fn is_default(&self) -> bool {
        *self == Self::Openai
    }
}

/// Modelin konuştuğu upstream API biçimi. `auto`: sağlayıcı türüne / model adına göre seçilir.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Auto,
    Chat,
    Responses,
    Messages,
}

impl Protocol {
    fn is_default(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    /// OpenAI uyumlu kök, örn. `https://openrouter.ai/api/v1` (sonuna `/chat/completions` eklenir).
    /// `kind = "opencode"` için çıkarım kökü: `https://opencode.ai/inference`.
    pub base_url: String,
    #[serde(default, skip_serializing_if = "ProviderKind::is_default")]
    pub kind: ProviderKind,
    /// OpenCode planı: `go` (varsayılan) veya `zen`. Yalnızca `kind = "opencode"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Ortam değişkeni adı (önerilen). Doluysa depodaki / düz metin anahtara göre önceliklidir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Düz metin anahtar (önerilmez; `key migrate` ile şifreli depoya taşınır).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Şifreli depodan (DPAPI) yüklenen anahtar. Hiçbir zaman serileştirilmez.
    #[serde(skip)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    /// Antigravity tarafında görünen kararlı kimlik (slug'a çevrilir).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub provider: String,
    /// Sağlayıcıya giden gerçek model adı.
    pub upstream_model: String,
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "default_max_output")]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_thinking: bool,
    /// Varsa upstream isteğe `reasoning_effort` olarak eklenir (low|medium|high|max).
    /// `default`: hiçbir şey gönderilmez. OpenCode düşünen modellerinde ayarsızsa `max` kullanılır.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Upstream API biçimi; `auto` dışındakiler yalnızca `kind = "opencode"` sağlayıcılarda geçerlidir
    /// (`chat` her sağlayıcıda geçerli).
    #[serde(default, skip_serializing_if = "Protocol::is_default")]
    pub protocol: Protocol,
}

fn default_listen() -> String {
    DEFAULT_LISTEN.into()
}
fn default_context() -> u32 {
    128_000
}
fn default_max_output() -> u32 {
    8_192
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            gateway_token: None,
            admin_token: None,
            google: GoogleOrigins::default(),
            providers: BTreeMap::new(),
            models: Vec::new(),
        }
    }
}

impl Provider {
    /// Anahtarı çözer: ortam değişkeni → şifreli depo → düz metin `api_key`. Hiçbir zaman loglanmaz.
    pub fn resolve_key(&self) -> Option<String> {
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.secret.clone().filter(|v| !v.trim().is_empty()))
            .or_else(|| self.api_key.clone().filter(|v| !v.trim().is_empty()))
    }

    /// Anahtarın kaynağı (değer değil): `env` | `secret` | `file` | `none`.
    pub fn key_source(&self) -> &'static str {
        let filled = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
        if self
            .api_key_env
            .as_deref()
            .is_some_and(|n| std::env::var(n).is_ok_and(|v| !v.trim().is_empty()))
        {
            "env"
        } else if filled(&self.secret) {
            "secret"
        } else if filled(&self.api_key) {
            "file"
        } else {
            "none"
        }
    }

    pub fn is_opencode(&self) -> bool {
        self.kind == ProviderKind::Opencode
    }

    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

impl Model {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

impl Config {
    pub fn listen_addr(&self) -> Result<SocketAddr> {
        let addr: SocketAddr = self
            .listen
            .parse()
            .with_context(|| format!("geçersiz listen adresi: {}", self.listen))?;
        if !addr.ip().is_loopback() {
            bail!("listen yalnızca loopback olabilir (127.0.0.1 / ::1), verilen: {addr}");
        }
        Ok(addr)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Şifreli depodaki anahtarları sağlayıcılara bağlar (depo yoksa / okunamazsa uyarıyla atlanır).
    pub fn attach_secrets(&mut self, store: &SecretStore) {
        if !store.path().exists() {
            return;
        }
        match store.get_all() {
            Ok(all) => {
                for (name, p) in &mut self.providers {
                    p.secret = all.get(name).cloned();
                }
            }
            Err(e) => tracing::warn!("anahtar deposu okunamadı: {e:#}"),
        }
    }

    /// Tüm sorunları tek seferde döndürür.
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        if let Err(e) = self.listen_addr() {
            problems.push(e.to_string());
        }
        for (label, token) in [
            ("gateway_token", &self.gateway_token),
            ("admin_token", &self.admin_token),
        ] {
            if token.as_ref().is_some_and(|t| t.len() < 16) {
                problems.push(format!("{label} en az 16 karakter olmalı"));
            }
        }
        let g = &self.google;
        for (label, origin) in [
            ("google.cloudcode_production", &g.cloudcode_production),
            ("google.cloudcode_daily", &g.cloudcode_daily),
            ("google.generative", &g.generative),
        ] {
            match Url::parse(origin) {
                Ok(u) if u.path() == "/" && (u.scheme() == "https" || (u.scheme() == "http" && is_local_host(u.host_str()))) => {}
                _ => problems.push(format!("{label}: kök URL https olmalı (yalnız loopback için http), yol içermemeli: '{origin}'")),
            }
        }
        for (name, p) in &self.providers {
            if name.trim().is_empty() {
                problems.push("boş sağlayıcı adı".into());
            }
            match Url::parse(&p.base_url) {
                Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
                    let local = is_local_host(u.host_str());
                    let has_key =
                        p.api_key.is_some() || p.api_key_env.is_some() || p.secret.is_some();
                    if u.scheme() == "http" && !local && has_key {
                        problems.push(format!("sağlayıcı '{name}': API anahtarı loopback dışına düz http ile gönderilmez"));
                    }
                }
                _ => problems.push(format!(
                    "sağlayıcı '{name}': geçersiz base_url '{}'",
                    p.base_url
                )),
            }
            match (&p.plan, p.kind) {
                (Some(plan), ProviderKind::Opencode) if !matches!(plan.as_str(), "go" | "zen") => {
                    problems.push(format!("sağlayıcı '{name}': plan go|zen olmalı"));
                }
                (Some(_), ProviderKind::Openai) => problems.push(format!(
                    "sağlayıcı '{name}': plan yalnızca kind=opencode ile kullanılır"
                )),
                _ => {}
            }
        }
        let mut seen = std::collections::HashSet::new();
        for m in &self.models {
            if m.id.trim().is_empty() || m.upstream_model.trim().is_empty() {
                problems.push(format!("model '{}': id ve upstream_model boş olamaz", m.id));
            }
            if !seen.insert(m.id.as_str()) {
                problems.push(format!("model id tekrarlı: '{}'", m.id));
            }
            match self.providers.get(&m.provider) {
                None => problems.push(format!(
                    "model '{}': bilinmeyen sağlayıcı '{}'",
                    m.id, m.provider
                )),
                Some(p)
                    if !matches!(m.protocol, Protocol::Auto | Protocol::Chat)
                        && !p.is_opencode() =>
                {
                    problems.push(format!("model '{}': protocol '{:?}' yalnızca kind=opencode sağlayıcılarda desteklenir", m.id, m.protocol));
                }
                Some(_) => {}
            }
            if let Some(e) = &m.reasoning_effort
                && !matches!(e.as_str(), "low" | "medium" | "high" | "max" | "default")
            {
                problems.push(format!(
                    "model '{}': reasoning_effort low|medium|high|max|default olmalı",
                    m.id
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            bail!("yapılandırma geçersiz:\n  - {}", problems.join("\n  - "))
        }
    }
}

/// Dosya tabanlı yapılandırma deposu.
pub struct ConfigStore {
    pub path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: Option<PathBuf>) -> Result<Self> {
        let path = match path.or_else(|| std::env::var_os("AGY_BRIDGE_CONFIG").map(PathBuf::from)) {
            Some(p) => p,
            None => default_dir()?.join("config.json"),
        };
        Ok(Self { path })
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Bu yapılandırmanın yanındaki şifreli anahtar deposu (`secrets.json`).
    pub fn secrets(&self) -> SecretStore {
        SecretStore::in_dir(self.dir())
    }

    pub fn load(&self) -> Result<Config> {
        let raw = std::fs::read_to_string(&self.path).with_context(|| {
            format!("{} okunamadı (önce `init` çalıştırın)", self.path.display())
        })?;
        let mut cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("{} geçerli JSON değil", self.path.display()))?;
        cfg.attach_secrets(&self.secrets());
        cfg.validate()?;
        Ok(cfg)
    }

    /// Doğrulayıp atomik yazar (geçici dosya + rename).
    pub fn save(&self, cfg: &Config) -> Result<()> {
        cfg.validate()?;
        std::fs::create_dir_all(self.dir())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Yoksa örnek yapılandırma yazar; var olanı asla ezmez.
    pub fn init(&self) -> Result<bool> {
        if self.path.exists() {
            return Ok(false);
        }
        let mut cfg = Config {
            admin_token: Some(generate_token()?),
            ..Config::default()
        };
        cfg.providers.insert(
            "ollama".into(),
            Provider {
                base_url: "http://127.0.0.1:11434/v1".into(),
                ..Provider::default()
            },
        );
        cfg.providers.insert(
            "openrouter".into(),
            Provider {
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key_env: Some("OPENROUTER_API_KEY".into()),
                ..Provider::default()
            },
        );
        self.save(&cfg)?;
        Ok(true)
    }
}

pub fn default_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("kullanıcı dizini bulunamadı")?;
    Ok(home
        .join(".gemini")
        .join("antigravity")
        .join("antigravity-bridge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let mut c = Config::default();
        c.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://x.test/v1/".into(),
                api_key: Some("k".into()),
                ..Provider::default()
            },
        );
        c.models.push(Model {
            id: "m1".into(),
            display_name: None,
            provider: "p".into(),
            upstream_model: "up".into(),
            context_window: 1000,
            max_output_tokens: 100,
            supports_vision: false,
            supports_thinking: false,
            reasoning_effort: None,
            protocol: Protocol::Auto,
        });
        c
    }

    #[test]
    fn valid_config_passes_and_url_joins() {
        let c = sample();
        c.validate().unwrap();
        assert_eq!(
            c.providers["p"].chat_url(),
            "https://x.test/v1/chat/completions"
        );
    }

    #[test]
    fn rejects_non_loopback_listen() {
        let mut c = sample();
        c.listen = "0.0.0.0:4000".into();
        assert!(c.validate().unwrap_err().to_string().contains("loopback"));
    }

    #[test]
    fn rejects_unknown_provider_duplicate_and_plain_http_key() {
        let mut c = sample();
        c.models.push(Model {
            provider: "yok".into(),
            ..c.models[0].clone()
        });
        c.providers.get_mut("p").unwrap().base_url = "http://uzak.example/v1".into();
        let msg = c.validate().unwrap_err().to_string();
        assert!(
            msg.contains("tekrarlı")
                && msg.contains("bilinmeyen sağlayıcı")
                && msg.contains("düz http"),
            "{msg}"
        );
    }

    #[test]
    fn store_roundtrip_and_init_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(Some(dir.path().join("sub").join("config.json"))).unwrap();
        assert!(store.init().unwrap());
        let mut cfg = store.load().unwrap();
        assert_eq!(cfg.admin_token.as_ref().map(String::len), Some(64));
        cfg.gateway_token = Some("t".repeat(16));
        store.save(&cfg).unwrap();
        assert!(!store.init().unwrap());
        assert_eq!(store.load().unwrap().gateway_token, Some("t".repeat(16)));
        cfg.admin_token = Some("kisa".into());
        assert!(store.save(&cfg).is_err());
    }

    #[test]
    fn google_origins_must_be_https_or_loopback_roots() {
        let mut c = sample();
        c.google.generative = "http://evil.example".into();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("google.generative")
        );
        c.google.generative = "http://127.0.0.1:9999".into();
        c.validate().unwrap();
        c.google.generative = "https://x.test/v1beta".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn env_key_takes_priority() {
        let p = Provider {
            base_url: "https://x".into(),
            api_key_env: Some("AGY_TEST_KEY_XYZ".into()),
            api_key: Some("file".into()),
            ..Provider::default()
        };
        assert_eq!(p.resolve_key().as_deref(), Some("file"));
    }
}
