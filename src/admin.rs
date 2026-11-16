//! Yönetim API'si (`/admin/*`): sağlayıcı/model ekleme-silme, anahtar güncelleme, durum.
//!
//! Güvenlik katmanları (hepsi gerekli):
//!
//! 1. `admin_token` tanımlı olmalı; yoksa API kapalıdır (403).
//! 2. `Host` loopback olmalı (DNS rebinding savunması).
//! 3. `Origin` varsa köprünün kendi kaynağı olmalı (yalnızca gömülü `/ui` sayfası çağırabilir; başka sayfalar çağıramaz).
//! 4. Token sabit zamanlı karşılaştırılır (`Authorization: Bearer` veya `x-admin-token`).
//!
//! Yanıtlar hiçbir zaman anahtar/token değeri içermez; yalnız var/yok bilgisi döner.

use crate::{
    config::{Model, Provider},
    guard::{host_is_loopback, origin_ok},
    opencode::{self, Plan},
    server::{ApiError, Shared, constant_time_eq, error_response},
};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::header,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn router(state: Shared) -> Router<Shared> {
    Router::new()
        .route("/status", get(status))
        .route("/config", get(config_view))
        .route("/reload", post(reload))
        .route(
            "/providers/{name}",
            put(put_provider).delete(delete_provider),
        )
        .route("/providers/{name}/key", put(put_key))
        .route("/models", post(post_model))
        .route("/models/{id}", delete(delete_model))
        .route("/opencode/setup", post(opencode_setup))
        .route(
            "/opencode/models",
            get(opencode_models).post(opencode_add_models),
        )
        .layer(middleware::from_fn_with_state(state, guard))
}

async fn guard(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let live = s.live();
    let Some(token) = &live.config.admin_token else {
        return error_response(
            403,
            "yönetim API'si kapalı: config.json'a admin_token ekleyin (`admin-token` komutu)",
        );
    };
    let headers = req.headers();
    if !host_is_loopback(headers) {
        return error_response(
            403,
            "yönetim API'si yalnızca loopback Host ile erişilebilir",
        );
    }
    if !origin_ok(headers) {
        return error_response(403, "tarayıcı kaynaklı (Origin) istekler reddedilir");
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let custom = headers.get("x-admin-token").and_then(|v| v.to_str().ok());
    let ok = [bearer, custom]
        .into_iter()
        .flatten()
        .any(|c| constant_time_eq(c.as_bytes(), token.as_bytes()));
    if !ok {
        return error_response(401, "geçersiz yönetim token'ı");
    }
    next.run(req).await
}

async fn status(State(s): State<Shared>) -> Response {
    let live = s.live();
    let providers: Vec<Value> = live
        .config
        .providers
        .iter()
        .map(|(name, p)| json!({"name": name, "kind": p.kind, "plan": p.plan, "baseUrl": p.base_url, "hasKey": p.resolve_key().is_some(), "keySource": p.key_source()}))
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSecs": s.started.elapsed().as_secs(),
        "listen": live.config.listen,
        "models": live.catalog.models().len(),
        "providers": providers,
        "routing": {"cachedAccounts": s.routing.cached_accounts()},
        "gatewayTokenSet": live.config.gateway_token.is_some(),
    }))
    .into_response()
}

/// Gizli değerler çıkarılmış yapılandırma görünümü.
async fn config_view(State(s): State<Shared>) -> Response {
    let live = s.live();
    let providers: BTreeMap<&String, Value> = live
        .config
        .providers
        .iter()
        .map(|(n, p)| {
            let headers: Vec<&String> = p.headers.keys().collect();
            (n, json!({"kind": p.kind, "plan": p.plan, "baseUrl": p.base_url, "apiKeyEnv": p.api_key_env, "hasKey": p.resolve_key().is_some(), "keySource": p.key_source(), "headerNames": headers}))
        })
        .collect();
    let models: Vec<Value> = live
        .catalog
        .models()
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
            v["slot"] = live.catalog.slot(&m.id).into();
            v["placeholder"] = live
                .catalog
                .slot(&m.id)
                .map(crate::catalog::placeholder_id)
                .into();
            v
        })
        .collect();
    Json(json!({"listen": live.config.listen, "providers": providers, "models": models}))
        .into_response()
}

async fn reload(State(s): State<Shared>) -> Response {
    match s.reload().await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderInput {
    base_url: String,
    /// Verilmezse mevcut değer korunur; boş string temizler.
    api_key_env: Option<String>,
    api_key: Option<String>,
    headers: Option<BTreeMap<String, String>>,
}

fn non_empty(v: Option<String>) -> Option<Option<String>> {
    v.map(|s| (!s.trim().is_empty()).then_some(s))
}

async fn put_provider(
    State(s): State<Shared>,
    Path(name): Path<String>,
    Json(input): Json<ProviderInput>,
) -> Response {
    let ProviderInput {
        base_url,
        api_key_env,
        api_key,
        headers,
    } = input;
    // Anahtar config.json'a değil, şifreli depoya yazılır; boş değer anahtarı temizler.
    let key = api_key.as_deref().map(|k| (name.as_str(), Some(k)));
    let result = s
        .mutate_keyed(key, |cfg| {
            let mut p = cfg.providers.get(&name).cloned().unwrap_or_default();
            p.base_url = base_url;
            if let Some(v) = non_empty(api_key_env) {
                p.api_key_env = v;
            }
            if let Some(h) = headers {
                p.headers = h;
            }
            cfg.providers.insert(name.clone(), p);
            Ok(())
        })
        .await;
    match result {
        Ok(()) => Json(json!({"ok": true, "provider": name})).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyInput {
    /// Şifreli depoya yazılır. Boş / null → anahtarı (depo + düz metin) siler.
    api_key: Option<String>,
}

async fn put_key(
    State(s): State<Shared>,
    Path(name): Path<String>,
    Json(input): Json<KeyInput>,
) -> Response {
    match s.set_key(&name, input.api_key.as_deref()).await {
        Ok(()) => {
            let live = s.live();
            let p = live.config.providers.get(&name);
            Json(json!({"ok": true, "hasKey": p.is_some_and(|p| p.resolve_key().is_some()), "keySource": p.map(Provider::key_source)})).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn delete_provider(State(s): State<Shared>, Path(name): Path<String>) -> Response {
    let result = s
        .mutate(|cfg| {
            if let Some(m) = cfg.models.iter().find(|m| m.provider == name) {
                return Err(ApiError(
                    409,
                    format!("'{name}' hâlâ '{}' modeli tarafından kullanılıyor", m.id),
                ));
            }
            cfg.providers
                .remove(&name)
                .map(|_| ())
                .ok_or_else(|| ApiError(404, format!("sağlayıcı yok: {name}")))
        })
        .await;
    match result {
        Ok(()) => {
            if let Err(e) = s.store.secrets().remove(&name) {
                tracing::warn!("'{name}' anahtarı depodan silinemedi: {e:#}");
            }
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn post_model(State(s): State<Shared>, Json(model): Json<Model>) -> Response {
    let id = model.id.clone();
    let result = s
        .mutate(|cfg| {
            if cfg.model(&model.id).is_some() {
                return Err(ApiError(409, format!("model zaten var: {}", model.id)));
            }
            cfg.models.push(model);
            Ok(())
        })
        .await;
    match result {
        Ok(()) => {
            let live = s.live();
            let slot = live.catalog.slot(&id);
            Json(json!({"ok": true, "id": id, "slot": slot, "placeholder": slot.map(crate::catalog::placeholder_id)})).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn delete_model(State(s): State<Shared>, Path(id): Path<String>) -> Response {
    let result = s
        .mutate(|cfg| {
            let before = cfg.models.len();
            cfg.models.retain(|m| m.id != id);
            if cfg.models.len() == before {
                Err(ApiError(404, format!("model yok: {id}")))
            } else {
                Ok(())
            }
        })
        .await;
    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => e.into_response(),
    }
}

// ───────────────────────── OpenCode ─────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpencodeSetup {
    /// `go` (varsayılan) veya `zen`.
    plan: Option<String>,
    /// Verilirse şifreli depoya yazılır.
    api_key: Option<String>,
}

/// OpenCode sağlayıcısını (`opencode-go` / `opencode-zen`) hazır ayarlarla oluşturur; anahtar isteğe bağlı.
async fn opencode_setup(State(s): State<Shared>, Json(input): Json<OpencodeSetup>) -> Response {
    let Some(plan) = Plan::parse(input.plan.as_deref().unwrap_or("go")) else {
        return error_response(400, "plan go|zen olmalı");
    };
    let name = plan.provider_name();
    let key = input
        .api_key
        .as_deref()
        .filter(|k| !k.trim().is_empty())
        .map(|k| (name, Some(k)));
    let result = s
        .mutate_keyed(key, |cfg| {
            let mut preset = opencode::preset_provider(plan);
            if let Some(old) = cfg.providers.get(name) {
                preset.headers = old.headers.clone();
                preset.api_key_env = old.api_key_env.clone();
                preset.base_url = old.base_url.clone();
                preset.secret = old.secret.clone();
            }
            cfg.providers.insert(name.to_string(), preset);
            Ok(())
        })
        .await;
    match result {
        Ok(()) => {
            let live = s.live();
            let has = live
                .config
                .providers
                .get(name)
                .is_some_and(|p| p.resolve_key().is_some());
            Json(json!({"ok": true, "provider": name, "hasKey": has})).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct ProviderQuery {
    provider: Option<String>,
}

fn opencode_provider(s: &Shared, name: Option<&str>) -> Result<(String, Provider), ApiError> {
    let live = s.live();
    let name = name.unwrap_or(Plan::Go.provider_name());
    match live.config.providers.get(name) {
        Some(p) if p.is_opencode() => Ok((name.to_string(), p.clone())),
        Some(_) => Err(ApiError(
            400,
            format!("'{name}' bir OpenCode sağlayıcısı değil"),
        )),
        None => Err(ApiError(
            404,
            format!("sağlayıcı yok: {name} (önce /admin/opencode/setup)"),
        )),
    }
}

/// OpenCode'un sunduğu modelleri getirir; hangilerinin zaten ekli olduğunu işaretler.
async fn opencode_models(State(s): State<Shared>, Query(q): Query<ProviderQuery>) -> Response {
    let (name, provider) = match opencode_provider(&s, q.provider.as_deref()) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match opencode::discover_models(&s.client, &provider).await {
        Ok(found) => {
            let live = s.live();
            let models: Vec<Value> = found
                .iter()
                .map(|d| {
                    let suggested = opencode::suggest_model(&name, Plan::of(&provider), &d.id);
                    let added = live.config.models.iter().any(|m| m.provider == name && m.upstream_model == d.id);
                    json!({"id": d.id, "ownedBy": d.owned_by, "suggestedId": suggested.id, "added": added})
                })
                .collect();
            Json(json!({"provider": name, "models": models})).into_response()
        }
        Err(e) => error_response(502, format!("OpenCode model listesi alınamadı: {e:#}")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddModels {
    provider: Option<String>,
    ids: Vec<String>,
}

/// Seçilen OpenCode model kimliklerini varsayılan ayarlarla ekler (var olanlar atlanır).
async fn opencode_add_models(State(s): State<Shared>, Json(input): Json<AddModels>) -> Response {
    let (name, provider) = match opencode_provider(&s, input.provider.as_deref()) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let plan = Plan::of(&provider);
    let mut added = Vec::new();
    let result = s
        .mutate(|cfg| {
            for id in &input.ids {
                if cfg
                    .models
                    .iter()
                    .any(|m| m.provider == name && &m.upstream_model == id)
                {
                    continue;
                }
                let model = opencode::suggest_model(&name, plan, id);
                if cfg.model(&model.id).is_some() {
                    continue;
                }
                added.push(model.id.clone());
                cfg.models.push(model);
            }
            Ok(())
        })
        .await;
    match result {
        Ok(()) => Json(json!({"ok": true, "added": added})).into_response(),
        Err(e) => e.into_response(),
    }
}
