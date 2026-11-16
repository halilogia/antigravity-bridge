//! OpenCode Go / Zen sağlayıcısı: uç noktalar, başlıklar, oturum izleme, yeniden deneme, model keşfi.
//!
//! Credentials are resolved per request so encrypted-key updates take effect immediately.

use crate::config::{Model, Protocol, Provider, ProviderKind};
use anyhow::{Result, bail};
use serde::Serialize;

/// OpenCode çıkarım kökü (planın alt yolu bunun altına eklenir).
pub const DEFAULT_ROOT: &str = "https://opencode.ai/inference";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Go,
    Zen,
}

impl Plan {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "go" => Some(Self::Go),
            "zen" => Some(Self::Zen),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::Zen => "zen",
        }
    }

    /// Önerilen sağlayıcı adı (`opencode-go` / `opencode-zen`); şifreli depoda anahtar bu adla saklanır.
    pub fn provider_name(self) -> &'static str {
        match self {
            Self::Go => "opencode-go",
            Self::Zen => "opencode-zen",
        }
    }

    /// Sağlayıcının planı (`plan` yoksa `go`).
    pub fn of(p: &Provider) -> Self {
        p.plan.as_deref().and_then(Self::parse).unwrap_or(Self::Go)
    }
}

/// Anahtarsız hazır OpenCode sağlayıcı kaydı.
pub fn preset_provider(plan: Plan) -> Provider {
    Provider {
        base_url: DEFAULT_ROOT.into(),
        kind: ProviderKind::Opencode,
        plan: Some(plan.as_str().into()),
        ..Provider::default()
    }
}

/// OpenCode'un `GET …/v1/models` listesinden bir kayıt.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Discovered {
    pub id: String,
    pub owned_by: Option<String>,
}

/// Plan için model listesini getirir (anahtar `provider.resolve_key()`; yoksa anahtarsız denenir).
pub async fn discover_models(
    client: &reqwest::Client,
    provider: &Provider,
) -> Result<Vec<Discovered>> {
    let root = provider.base_url.trim_end_matches('/');
    let plan = if Plan::of(provider) == Plan::Go {
        "/go"
    } else {
        ""
    };
    let mut request = client
        .get(format!("{root}{plan}/v1/models"))
        .header("User-Agent", "antigravity-bridge/0.1.0");
    if let Some(key) = provider.resolve_key() {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| e.without_url())?;
    if !response.status().is_success() {
        bail!("model discovery HTTP {}", response.status().as_u16());
    }
    let bytes = crate::upstream::read_limited(response, 4 * 1024 * 1024).await?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let data = value["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid model catalog"))?;
    let mut models: Vec<_> = data
        .iter()
        .filter_map(|v| {
            Some(Discovered {
                id: v["id"].as_str()?.to_owned(),
                owned_by: v["owned_by"].as_str().map(str::to_owned),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

pub fn endpoint(provider: &Provider, protocol: Protocol, action: &str) -> String {
    let root = provider.base_url.trim_end_matches('/');
    let plan = if Plan::of(provider) == Plan::Go {
        "/go"
    } else {
        ""
    };
    let family = if protocol == Protocol::Messages {
        "anthropic"
    } else {
        "openai"
    };
    format!("{root}{plan}/{family}/v1/{action}")
}

pub fn protocol(model: &Model, plan: Plan) -> Protocol {
    if model.protocol != Protocol::Auto {
        return model.protocol;
    }
    let id = model
        .upstream_model
        .trim_start_matches("models/")
        .to_ascii_lowercase();
    let go_messages = plan == Plan::Go
        && matches!(
            id.as_str(),
            "minimax-m3" | "minimax-m2.7" | "minimax-m2.5" | "qwen3.8-max" | "qwen4-max"
        );
    if id.starts_with("claude-")
        || go_messages
        || matches!(
            id.as_str(),
            "qwen3.8-flash" | "qwen3.7-max" | "qwen3.7-plus" | "qwen3.6-plus" | "qwen3.5-plus"
        )
    {
        Protocol::Messages
    } else if ["gpt-", "grok-", "muse-spark"]
        .iter()
        .any(|p| id.starts_with(p))
    {
        Protocol::Responses
    } else {
        Protocol::Chat
    }
}

pub fn thinking_required(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("[1210]")
        || text.contains("always engages in thinking")
        || text.contains("cannot be disabled")
}

/// Keşfedilen bir kimlik için makul varsayılanlarla `Model` önerir (`id` = `<plan>-<kimlik>` sluğu, `provider` = sağlayıcı adı).
pub fn suggest_model(provider_name: &str, _plan: Plan, discovered_id: &str) -> Model {
    Model {
        id: format!("{provider_name}-{discovered_id}"),
        display_name: None,
        provider: provider_name.into(),
        upstream_model: discovered_id.into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        supports_vision: false,
        supports_thinking: false,
        reasoning_effort: None,
        protocol: Default::default(),
    }
}
