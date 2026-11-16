//! Yerel HTTP/SSE ağ geçidi.
//!
//! * Özel modeller (`catalog`'da çözülenler): Gemini / Cloud Code → OpenAI uyumlu sağlayıcı.
//! * Diğer her şey: Google'a olduğu gibi aktarılır (`google.rs`).
//! * `/admin/*`: yönetim API'si (`admin.rs`).

use crate::{
    admin,
    catalog::{Catalog, Identities, native_indices},
    config::{Config, ConfigStore},
    gemini::{self, wrap},
    google, guard,
    routing::CloudRouting,
    tools::ToolRegistry,
    translate::stream,
    ui,
};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_BODY: usize = 64 * 1024 * 1024;

/// Atomik olarak değiştirilen çalışma anı görüntüsü.
pub struct Live {
    pub config: Config,
    pub catalog: Catalog,
}

#[derive(Debug)]
pub struct ApiError(pub u16, pub String);

pub struct AppState {
    live: RwLock<Arc<Live>>,
    pub store: ConfigStore,
    pub client: reqwest::Client,
    pub routing: CloudRouting,
    admin_lock: tokio::sync::Mutex<()>,
    pub started: Instant,
    pub sessions: crate::upstream::Sessions,
}

impl AppState {
    pub fn new(store: ConfigStore, config: Config) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let catalog = Catalog::build(&config, &store.dir().join("identities.json"))?;
        let routing = CloudRouting::new(
            &config.google.cloudcode_production,
            &config.google.cloudcode_daily,
        );
        Ok(Self {
            live: RwLock::new(Arc::new(Live { config, catalog })),
            store,
            client,
            routing,
            admin_lock: tokio::sync::Mutex::new(()),
            started: Instant::now(),
            sessions: Default::default(),
        })
    }

    pub fn live(&self) -> Arc<Live> {
        self.live.read().unwrap().clone()
    }

    fn identities_path(&self) -> PathBuf {
        self.store.dir().join("identities.json")
    }

    /// Yapılandırmayı doğrular, diske yazar, kataloğu yeniden kurar ve atomik olarak yayınlar.
    /// Eşzamanlı yönetim istekleri sıraya girer (kayıp güncelleme olmaz).
    pub async fn mutate(
        &self,
        change: impl FnOnce(&mut Config) -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        self.mutate_keyed(None, change).await
    }

    /// `mutate` + isteğe bağlı anahtar işlemi: `key = (sağlayıcı, Some(anahtar) | None → sil)`.
    /// Anahtar, doğrulama geçtikten sonra şifreli depoya yazılır (geçersiz istek hiçbir şeyi değiştirmez);
    /// düz metin `api_key` her durumda temizlenir ve `config.json`'a anahtar yazılmaz.
    pub async fn mutate_keyed(
        &self,
        key: Option<(&str, Option<&str>)>,
        change: impl FnOnce(&mut Config) -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        let _guard = self.admin_lock.lock().await;
        let mut cfg = self.live().config.clone();
        change(&mut cfg)?;
        if let Some((name, value)) = key {
            let p = cfg
                .providers
                .get_mut(name)
                .ok_or_else(|| ApiError(404, format!("sağlayıcı yok: {name}")))?;
            p.secret = value
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string);
            p.api_key = None;
        }
        cfg.validate().map_err(|e| ApiError(400, e.to_string()))?;
        let catalog = Catalog::build(&cfg, &self.identities_path())
            .map_err(|e| ApiError(500, e.to_string()))?;
        if let Some((name, _)) = key {
            let secrets = self.store.secrets();
            let stored = match cfg.providers[name].secret.as_deref() {
                Some(k) => secrets.set(name, k),
                None => secrets.remove(name).map(|_| ()),
            };
            stored.map_err(|e| ApiError(500, format!("{e:#}")))?;
        }
        self.store
            .save(&cfg)
            .map_err(|e| ApiError(500, e.to_string()))?;
        *self.live.write().unwrap() = Arc::new(Live {
            config: cfg,
            catalog,
        });
        Ok(())
    }

    /// Sağlayıcı anahtarını şifreli depoya yazar (`None`/boş: siler).
    pub async fn set_key(&self, name: &str, key: Option<&str>) -> Result<(), ApiError> {
        self.mutate_keyed(Some((name, key)), |_| Ok(())).await
    }

    /// `config.json`'u diskten yeniden okur (elle düzenleme sonrası). Google kökleri yeniden başlatmayı gerektirir.
    pub async fn reload(&self) -> Result<(), ApiError> {
        let _guard = self.admin_lock.lock().await;
        let cfg = self
            .store
            .load()
            .map_err(|e| ApiError(400, e.to_string()))?;
        let catalog = Catalog::build(&cfg, &self.identities_path())
            .map_err(|e| ApiError(500, e.to_string()))?;
        *self.live.write().unwrap() = Arc::new(Live {
            config: cfg,
            catalog,
        });
        Ok(())
    }

    /// Google kataloğunda gözlenen yerel slotları ayırır; çakışan özel modeller yeni slot alır.
    pub async fn observe_native(&self, upstream: &Value) {
        let indices = native_indices(upstream);
        if indices.is_empty() {
            return;
        }
        let _guard = self.admin_lock.lock().await;
        let path = self.identities_path();
        let result = (|| -> anyhow::Result<()> {
            let mut ids = Identities::load(&path)?;
            if !ids.reserve(indices) {
                return Ok(());
            }
            ids.save(&path)?;
            let cfg = self.live().config.clone();
            let catalog = Catalog::build(&cfg, &path)?;
            *self.live.write().unwrap() = Arc::new(Live {
                config: cfg,
                catalog,
            });
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("yerel slot kaydı başarısız: {e}");
        }
    }
}

pub type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/v1beta/models",
            get(google::list_models).fallback(google::passthrough),
        )
        .route(
            "/v1/models",
            get(google::list_models).fallback(google::passthrough),
        )
        .route(
            "/v1beta/models/{spec}",
            post(public_generate).fallback(google::passthrough),
        )
        .route(
            "/v1/models/{spec}",
            post(public_generate).fallback(google::passthrough),
        )
        .route(
            "/v1internal:streamGenerateContent",
            post(cloud_stream).fallback(google::passthrough),
        )
        .route(
            "/v1internal:generateContent",
            post(cloud_generate).fallback(google::passthrough),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(google::fetch_available_models).fallback(google::passthrough),
        )
        .nest("/admin", admin::router(state.clone()))
        .merge(ui::router())
        .fallback(google::passthrough)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(guard::local_only))
        .with_state(state)
}

pub async fn serve(state: Shared, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

// ───────────────────────── hata yardımcıları ─────────────────────────

pub(crate) fn error_response(code: u16, message: impl Into<String>) -> Response {
    let status = match code {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        409 => "ALREADY_EXISTS",
        429 => "RESOURCE_EXHAUSTED",
        502 | 503 => "UNAVAILABLE",
        _ => "INTERNAL",
    };
    let body = json!({"error": {"code": code, "message": message.into(), "status": status}});
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error_response(self.0, self.1)
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(live: &Live, headers: &HeaderMap) -> bool {
    let Some(token) = &live.config.gateway_token else {
        return true;
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let goog = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok());
    [bearer, goog]
        .into_iter()
        .flatten()
        .any(|candidate| constant_time_eq(candidate.as_bytes(), token.as_bytes()))
}

async fn health(State(s): State<Shared>) -> Response {
    axum::Json(json!({"status": "ok", "models": s.live().catalog.models().len()})).into_response()
}

// ───────────────────────── üretim uçları ─────────────────────────

/// `POST /v1beta/models/{model}:{eylem}` — özel modelse çevir, değilse Google'a aktar.
async fn public_generate(
    State(s): State<Shared>,
    Path(spec): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let live = s.live();
    let Some((model, action)) = spec.rsplit_once(':') else {
        return google::passthrough(State(s), method, uri, headers, body).await;
    };
    let stream_mode = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        _ => return google::passthrough(State(s), method, uri, headers, body).await,
    };
    if live.catalog.resolve(model).is_none() {
        return google::passthrough(State(s), method, uri, headers, body).await;
    }
    if !authorized(&live, &headers) {
        return error_response(401, "geçersiz ağ geçidi anahtarı");
    }
    generate(&s, live, Some(model.to_string()), stream_mode, body).await
}

async fn cloud_stream(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    cloud_generation(s, true, method, uri, headers, body).await
}

async fn cloud_generate(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    cloud_generation(s, false, method, uri, headers, body).await
}

/// Zarftaki `model` özel bir modelse köprü işler; aksi halde (veya gövde okunamazsa) gövde bozulmadan Google'a gider.
async fn cloud_generation(
    s: Shared,
    stream_mode: bool,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let live = s.live();
    let custom = gemini::parse_body(&body)
        .ok()
        .and_then(|p| p.model)
        .is_some_and(|m| live.catalog.resolve(&m).is_some());
    if custom {
        generate(&s, live, None, stream_mode, body).await
    } else {
        google::passthrough(State(s), method, uri, headers, body).await
    }
}

type Item = Result<Vec<Value>, String>;

async fn generate(
    state: &AppState,
    live: Arc<Live>,
    selector: Option<String>,
    stream_mode: bool,
    bytes: Bytes,
) -> Response {
    let parsed = match gemini::parse_body(&bytes) {
        Ok(p) => p,
        Err(e) => return error_response(400, e.to_string()),
    };
    let selector = selector
        .or_else(|| parsed.model.clone())
        .unwrap_or_default();
    let Some(model) = live.catalog.resolve(&selector) else {
        return error_response(404, format!("model bulunamadı: {selector}"));
    };
    let Some(provider) = live.config.providers.get(&model.provider) else {
        return error_response(500, "model sağlayıcısı tanımsız");
    };
    let decls: Vec<_> = parsed
        .request
        .tools
        .iter()
        .flat_map(|t| t.function_declarations.iter().cloned())
        .collect();
    let registry = ToolRegistry::from_declarations(&decls);
    tracing::info!(model = %model.id, provider = %model.provider, stream = stream_mode, "özel model isteği");
    let (resp, translator, session) = match crate::upstream::send(
        &state.client,
        provider,
        model,
        &parsed.request,
        registry,
        &state.sessions,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => return e.into_response(),
    };

    let (tx, rx) = mpsc::channel::<Item>(32);
    tokio::spawn(pump(resp, translator, session, tx));
    let cloud = parsed.cloud;

    if stream_mode {
        let events = futures_util::stream::unfold(rx, move |mut rx| async move {
            let item = rx.recv().await?;
            let text = match item {
                Ok(chunks) => chunks
                    .into_iter()
                    .map(|c| sse_event(&wrap(c, cloud)))
                    .collect::<String>(),
                Err(msg) => sse_event(
                    &json!({"error": {"code": 502, "message": msg, "status": "UNAVAILABLE"}}),
                ),
            };
            Some((Ok::<_, Infallible>(Bytes::from(text)), rx))
        });
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(events))
            .unwrap();
    }

    let mut rx = rx;
    let mut all = Vec::new();
    while let Some(item) = rx.recv().await {
        match item {
            Ok(mut c) => all.append(&mut c),
            Err(msg) => return error_response(502, msg),
        }
    }
    axum::Json(wrap(stream::aggregate(all), cloud)).into_response()
}

fn sse_event(v: &Value) -> String {
    format!("data: {v}\n\n")
}

/// Upstream SSE akışını okur, çevirir ve kanala yazar. İstemci kopunca (`send` hatası) upstream bırakılır.
#[allow(unused_assignments)]
async fn pump(
    mut bytes: crate::upstream::ByteStream,
    mut tr: crate::upstream::Translator,
    session: Option<crate::upstream::Session>,
    tx: mpsc::Sender<Item>,
) {
    let mut parser = stream::SseParser::default();
    let mut done = false;
    let mut terminal = false;

    macro_rules! handle {
        ($ev:expr) => {{
            let ev: String = $ev;
            let ev = ev.trim();
            if ev == "[DONE]" {
                done = true;
                terminal = true;
            } else if let Ok(chunk) = serde_json::from_str::<Value>(ev) {
                terminal |= chunk
                    .pointer("/choices/0/finish_reason")
                    .is_some_and(Value::is_string)
                    || matches!(
                        chunk["type"].as_str(),
                        Some("message_stop" | "response.completed" | "response.incomplete")
                    );
                if let Some(session) = &session {
                    session.capture(&chunk);
                }
                match tr.push(&chunk) {
                    Ok(out) if out.is_empty() => {}
                    Ok(out) => {
                        if tx.send(Ok(out)).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Err("upstream stream error".into())).await;
                        return;
                    }
                }
            } else {
                let _ = tx.send(Err("invalid upstream SSE JSON".into())).await;
                return;
            }
        }};
    }

    while !done {
        let next = tokio::select! {
            _ = tx.closed() => return,
            next = bytes.next() => next,
        };
        match next {
            Some(Ok(b)) => {
                for ev in parser.feed(&b) {
                    handle!(ev);
                    if done {
                        break;
                    }
                }
            }
            Some(Err(e)) => {
                let _ = tx
                    .send(Err(format!("upstream akışı koptu: {}", e.without_url())))
                    .await;
                return;
            }
            None => break,
        }
    }
    if !done && let Some(ev) = parser.finish() {
        handle!(ev);
    }
    if !terminal {
        let _ = tx
            .send(Err("upstream stream ended before completion".into()))
            .await;
        return;
    }
    let _ = tx.send(Ok(tr.finish())).await;
}
