//! Uçtan uca: sahte OpenAI-uyumlu sağlayıcı + sahte Google (production/daily) + gerçek köprü, gerçek HTTP/SSE üzerinden.

use antigravity_bridge::{
    config::{Config, ConfigStore, GoogleOrigins, Model, Provider},
    server::{self, AppState},
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

const GATEWAY: &str = "gateway-secret-0123456789";
const ADMIN: &str = "admin-secret-0123456789";

// ───────────────────────── sahte OpenAI-uyumlu sağlayıcı ─────────────────────────

async fn spawn_upstream(sse: &'static str, seen: Arc<Mutex<Vec<Value>>>) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let mut stored = body;
                stored["__auth"] = auth.into();
                seen.lock().unwrap().push(stored);
                ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/v1")
}

// ───────────────────────── sahte Google ─────────────────────────

#[derive(Clone, Debug)]
struct Rec {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Mock {
    label: &'static str,
    origin: String,
    seen: Arc<Mutex<Vec<Rec>>>,
    catalog: Arc<Mutex<Value>>,
    gate: Arc<Notify>,
}

impl Mock {
    fn calls(&self, path: &str) -> Vec<Rec> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }
    fn total(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn default_native_catalog() -> Value {
    json!({
        "models": {
            "gemini-pro": {"displayName": "DeepSeek", "model": "MODEL_PLACEHOLDER_M8"},
            "gemini-flash": {"displayName": "Gemini Flash", "model": "MODEL_PLACEHOLDER_M18"}
        },
        "agentModelSorts": [{"displayName": "Recommended", "groups": [{"modelIds": ["gemini-pro", "gemini-flash"]}]}],
        "extra": "korunmalı"
    })
}

fn sse_chunk(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}]}, "traceId": "", "metadata": {}})
    )
}

async fn mock_handler(
    State(m): State<Mock>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    m.seen.lock().unwrap().push(Rec {
        method: method.to_string(),
        path: uri.path().to_string(),
        headers: headers.clone(),
        body: body.to_vec(),
    });
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match uri.path() {
        "/v1internal:loadCodeAssist" => match auth.as_str() {
            "Bearer broken" => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "boom"}}))).into_response(),
            "Bearer paid" => Json(json!({"paidTier": {"usesGcpTos": true}, "endpoint": format!("http://{host}/v1internal")})).into_response(),
            _ => Json(json!({"paidTier": {"usesGcpTos": false}, "endpoint": format!("http://{host}/v1internal")})).into_response(),
        },
        "/v1internal:streamGenerateContent" if headers.contains_key("x-test-hold") => {
            let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
            let gate = m.gate.clone();
            tokio::spawn(async move {
                let _ = tx.send(Bytes::from(sse_chunk("birinci"))).await;
                gate.notified().await;
                let _ = tx.send(Bytes::from(sse_chunk("ikinci"))).await;
            });
            let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (Ok::<_, std::convert::Infallible>(v), rx)) });
            Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from_stream(stream)).unwrap()
        }
        "/v1internal:streamGenerateContent" => {
            ([(header::CONTENT_TYPE, "text/event-stream")], format!("{}{}", sse_chunk(&format!("from-{}", m.label)), sse_chunk("sonu"))).into_response()
        }
        "/v1internal:fetchAvailableModels" if auth == "Bearer expired" => {
            (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": 401, "message": "token süresi doldu", "status": "UNAUTHENTICATED"}}))).into_response()
        }
        "/v1internal:fetchAvailableModels" => Json(m.catalog.lock().unwrap().clone()).into_response(),
        "/v1beta/models" => Json(json!({"models": [{"name": "models/gemini-native", "displayName": "Gemini Native"}]})).into_response(),
        path => Json(json!({"echo": path, "label": m.label, "method": method.to_string()})).into_response(),
    }
}

async fn spawn_mock(label: &'static str) -> Mock {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        label,
        origin: format!("http://{}", l.local_addr().unwrap()),
        seen: Arc::default(),
        catalog: Arc::new(Mutex::new(default_native_catalog())),
        gate: Arc::new(Notify::new()),
    };
    let app = Router::new()
        .fallback(mock_handler)
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    mock
}

async fn dead_origin() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", l.local_addr().unwrap())
}

// ───────────────────────── köprü ortamı ─────────────────────────

struct Env {
    bridge: String,
    prod: Mock,
    daily: Mock,
    dir: PathBuf,
    client: reqwest::Client,
}

struct Opts {
    gateway_token: Option<&'static str>,
    admin_token: Option<&'static str>,
    google_dead: bool,
    upstream: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            gateway_token: None,
            admin_token: Some(ADMIN),
            google_dead: false,
            upstream: None,
        }
    }
}

async fn spawn_env(opts: Opts) -> Env {
    let upstream = match opts.upstream {
        Some(u) => u,
        None => spawn_upstream(SSE, Arc::default()).await,
    };
    let prod = spawn_mock("prod").await;
    let daily = spawn_mock("daily").await;
    let google = if opts.google_dead {
        GoogleOrigins {
            cloudcode_production: dead_origin().await,
            cloudcode_daily: dead_origin().await,
            generative: dead_origin().await,
        }
    } else {
        GoogleOrigins {
            cloudcode_production: prod.origin.clone(),
            cloudcode_daily: daily.origin.clone(),
            generative: prod.origin.clone(),
        }
    };
    let mut cfg = Config {
        gateway_token: opts.gateway_token.map(Into::into),
        admin_token: opts.admin_token.map(Into::into),
        google,
        ..Config::default()
    };
    cfg.providers.insert(
        "fake".into(),
        Provider {
            base_url: upstream,
            api_key: Some("sk-test".into()),
            ..Provider::default()
        },
    );
    cfg.models.push(Model {
        id: "deepseek".into(),
        display_name: Some("DeepSeek".into()),
        provider: "fake".into(),
        upstream_model: "deepseek-chat".into(),
        context_window: 64_000,
        max_output_tokens: 4096,
        supports_vision: false,
        supports_thinking: true,
        reasoning_effort: None,
        protocol: Default::default(),
    });
    let dir = tempfile::tempdir().unwrap().keep();
    let store = ConfigStore::new(Some(dir.join("config.json"))).unwrap();
    let state = Arc::new(AppState::new(store, cfg).unwrap());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, server::router(state)).await.unwrap() });
    Env {
        bridge: format!("http://{addr}"),
        prod,
        daily,
        dir,
        client: reqwest::Client::new(),
    }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.bridge)
    }

    async fn catalog(&self, auth: Option<&str>) -> Value {
        let mut rb = self
            .client
            .post(self.url("/v1internal:fetchAvailableModels"))
            .json(&json!({}));
        if let Some(a) = auth {
            rb = rb.header("authorization", format!("Bearer {a}"));
        }
        rb.send().await.unwrap().json().await.unwrap()
    }

    fn admin(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(&format!("/admin{path}")))
            .bearer_auth(ADMIN)
    }
}

const SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"Mer\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"haba\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"runCommand\",\"arguments\":\"{\\\"command\\\":\\\"dir\\\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
    "data: [DONE]\n\n",
);

fn cloud_request() -> Value {
    json!({
        "project": "p",
        "model": "MODEL_PLACEHOLDER_M0", // aşağıda gerçek slotla değiştirilir
        "request": {
            "contents": [{"role": "user", "parts": [{"text": "selam"}]}],
            "tools": [{"functionDeclarations": [{"name": "run_command", "parameters": {
                "type": "OBJECT", "properties": {"CommandLine": {"type": "STRING"}}, "required": ["CommandLine"]}}]}]
        }
    })
}

fn events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|e| e.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

/// Google'a gitmesi gereken, özel olmayan model isteği; biçimi bilerek düzensiz (bayt bayt aktarım testi).
const NATIVE_BODY: &str = "{\"project\":\"p-1\",  \"model\":\"gemini-pro\",\n \"request\":{\"contents\":[{\"role\":\"user\",\"parts\":[{\"text\":\"merhaba  dünya\"}]}]}}";

async fn native_generate(env: &Env, token: &str) -> reqwest::Response {
    env.client
        .post(env.url("/v1internal:streamGenerateContent?alt=sse"))
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", "antigravity/1.2.3 windows/amd64")
        .header("x-goog-api-client", "gl-go/1.0")
        .header("content-type", "application/json")
        .body(NATIVE_BODY)
        .send()
        .await
        .unwrap()
}

// ───────────────────────── özel model (Faz 1 davranışı) ─────────────────────────

#[tokio::test]
async fn cloud_code_stream_end_to_end_without_touching_google_generation() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream(SSE, seen.clone()).await;
    let env = spawn_env(Opts {
        upstream: Some(upstream),
        ..Opts::default()
    })
    .await;

    let catalog = env.catalog(Some("free")).await;
    let placeholder = catalog["models"]["custom-deepseek"]["model"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(placeholder.starts_with("MODEL_PLACEHOLDER_M"));
    assert!(
        catalog["agentModelSorts"][0]["groups"][0]["modelIds"]
            .as_array()
            .unwrap()
            .contains(&json!("custom-deepseek"))
    );
    let google_calls_before = env.prod.total() + env.daily.total();

    let mut req = cloud_request();
    req["model"] = placeholder.into();
    let resp = env
        .client
        .post(env.url("/v1internal:streamGenerateContent?alt=sse"))
        .bearer_auth("free")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let evs = events(&resp.text().await.unwrap());

    assert!(evs.iter().all(|e| e.get("response").is_some()), "{evs:?}");
    let parts: Vec<&Value> = evs
        .iter()
        .flat_map(|e| {
            e["response"]["candidates"][0]["content"]["parts"]
                .as_array()
                .unwrap()
        })
        .collect();
    assert_eq!(parts[0], &json!({"text": "plan", "thought": true}));
    let text: String = parts
        .iter()
        .filter(|p| p.get("thought").is_none())
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert_eq!(text, "Merhaba");
    let call = parts.iter().find_map(|p| p.get("functionCall")).unwrap();
    assert_eq!(call["name"], "run_command");
    assert_eq!(call["args"], json!({"CommandLine": "dir"}));
    let last = &evs.last().unwrap()["response"];
    assert_eq!(last["candidates"][0]["finishReason"], "STOP");
    assert_eq!(last["usageMetadata"]["totalTokenCount"], 7);

    let sent = seen.lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "deepseek-chat");
    assert_eq!(sent["__auth"], "Bearer sk-test");
    assert_eq!(
        sent["messages"][0],
        json!({"role": "user", "content": "selam"})
    );
    assert_eq!(sent["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(
        sent["tools"][0]["function"]["parameters"]["properties"]["CommandLine"]["type"],
        "string"
    );

    // Özel model isteği Google'a hiç gitmemeli (ne üretim ne hesap katmanı araması).
    assert_eq!(env.prod.total() + env.daily.total(), google_calls_before);
}

#[tokio::test]
async fn public_non_stream_aggregates_and_auth_is_enforced() {
    let env = spawn_env(Opts {
        gateway_token: Some(GATEWAY),
        ..Opts::default()
    })
    .await;
    let url = env.url("/v1beta/models/custom-deepseek:generateContent");
    let body = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});

    assert_eq!(
        env.client
            .post(&url)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let ok: Value = env
        .client
        .post(&url)
        .bearer_auth(GATEWAY)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let parts = ok["candidates"][0]["content"]["parts"].as_array().unwrap();
    assert_eq!(parts[0], json!({"text": "plan", "thought": true}));
    assert_eq!(parts[1], json!({"text": "Merhaba"}));
    assert_eq!(parts[2]["functionCall"]["name"], "runCommand");
    assert_eq!(
        env.client
            .get(env.url("/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn upstream_auth_failure_is_not_forwarded_as_401() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::UNAUTHORIZED, "bad key sk-test") }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let env = spawn_env(Opts {
        upstream: Some(upstream),
        ..Opts::default()
    })
    .await;
    let r = env
        .client
        .post(env.url("/v1beta/models/custom-deepseek:streamGenerateContent"))
        .json(&json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502); // 401 olsaydı Antigravity Google oturumunu yenilemeye çalışabilirdi
}

// ───────────────────────── Google passthrough ve yönlendirme ─────────────────────────

#[tokio::test]
async fn native_generation_passes_through_unchanged_to_daily_for_non_gcp_account() {
    let env = spawn_env(Opts::default()).await;
    let resp = env
        .client
        .post(env.url("/v1internal:streamGenerateContent?alt=sse"))
        .header("authorization", "Bearer free")
        .header("user-agent", "antigravity/1.2.3 windows/amd64")
        .header("x-goog-api-client", "gl-go/1.0")
        .header("x-admin-token", "sızmamalı")
        .header("content-type", "application/json")
        .body(NATIVE_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let evs = events(&resp.text().await.unwrap());
    assert_eq!(
        evs[0]["response"]["candidates"][0]["content"]["parts"][0]["text"],
        "from-daily"
    );

    // Hesap katmanı production'dan öğrenildi; üretim daily'ye gitti, production'a değil.
    let lookup = env.prod.calls("/v1internal:loadCodeAssist");
    assert_eq!(lookup.len(), 1);
    assert_eq!(lookup[0].headers["authorization"], "Bearer free");
    let lookup_body: Value = serde_json::from_slice(&lookup[0].body).unwrap();
    assert_eq!(lookup_body["metadata"]["ideVersion"], "1.2.3");
    assert_eq!(lookup_body["cloudaicompanionProject"], "p-1");
    assert!(
        env.prod
            .calls("/v1internal:streamGenerateContent")
            .is_empty()
    );

    let gen_calls = env.daily.calls("/v1internal:streamGenerateContent");
    assert_eq!(gen_calls.len(), 1);
    let rec = &gen_calls[0];
    assert_eq!(rec.method, "POST");
    assert_eq!(
        rec.body,
        NATIVE_BODY.as_bytes(),
        "gövde bayt bayt aynı olmalı"
    );
    assert_eq!(rec.headers["authorization"], "Bearer free");
    assert_eq!(rec.headers["user-agent"], "antigravity/1.2.3 windows/amd64");
    assert_eq!(rec.headers["x-goog-api-client"], "gl-go/1.0");
    assert!(
        !rec.headers.contains_key("x-admin-token"),
        "köprüye ait başlık Google'a sızmamalı"
    );
    assert_ne!(
        rec.headers["host"].to_str().unwrap(),
        env.bridge.trim_start_matches("http://"),
        "Host köprüyü göstermemeli"
    );
}

#[tokio::test]
async fn gcp_tos_account_stays_on_production() {
    let env = spawn_env(Opts::default()).await;
    let resp = native_generate(&env, "paid").await;
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("from-prod"));
    assert_eq!(env.prod.calls("/v1internal:streamGenerateContent").len(), 1);
    assert_eq!(env.daily.total(), 0);
}

#[tokio::test]
async fn account_route_is_cached_and_accounts_are_isolated() {
    let env = spawn_env(Opts::default()).await;
    for _ in 0..3 {
        assert!(
            native_generate(&env, "free")
                .await
                .text()
                .await
                .unwrap()
                .contains("from-daily")
        );
    }
    assert!(
        native_generate(&env, "paid")
            .await
            .text()
            .await
            .unwrap()
            .contains("from-prod")
    );
    // free: 1 arama, paid: 1 arama; tekrarlar önbellekten.
    assert_eq!(env.prod.calls("/v1internal:loadCodeAssist").len(), 2);
    assert_eq!(
        env.daily.calls("/v1internal:streamGenerateContent").len(),
        3
    );
    assert_eq!(env.prod.calls("/v1internal:streamGenerateContent").len(), 1);
}

#[tokio::test]
async fn tier_lookup_failure_never_sends_generation_to_wrong_origin() {
    let env = spawn_env(Opts::default()).await;
    let resp = native_generate(&env, "broken").await;
    assert_eq!(resp.status(), 502);
    assert!(
        env.prod
            .calls("/v1internal:streamGenerateContent")
            .is_empty()
    );
    assert_eq!(env.daily.total(), 0);
}

#[tokio::test]
async fn load_code_assist_passthrough_rewrites_origins_and_primes_routing() {
    let env = spawn_env(Opts::default()).await;
    let resp = env
        .client
        .post(env.url("/v1internal:loadCodeAssist"))
        .header("authorization", "Bearer free")
        .json(&json!({"metadata": {"ideType": "ANTIGRAVITY"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // Google yanıtındaki kendi kökü köprü adresine çevrilir (geri dönüş URL'leri köprüden geçsin).
    assert_eq!(body["endpoint"], format!("{}/v1internal", env.bridge));
    assert_eq!(env.prod.calls("/v1internal:loadCodeAssist").len(), 1);

    // Katman öğrenildi: sonraki üretim için ek arama yok, daily'ye gider.
    assert!(
        native_generate(&env, "free")
            .await
            .text()
            .await
            .unwrap()
            .contains("from-daily")
    );
    assert_eq!(env.prod.calls("/v1internal:loadCodeAssist").len(), 1);
}

#[tokio::test]
async fn other_cloud_code_and_non_cloud_paths_are_forwarded() {
    let env = spawn_env(Opts::default()).await;
    let quota: Value = env
        .client
        .post(env.url("/v1internal:retrieveUserQuotaSummary"))
        .header("authorization", "Bearer paid")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(quota["echo"], "/v1internal:retrieveUserQuotaSummary");
    assert_eq!(quota["label"], "prod");

    let other: Value = env
        .client
        .get(env.url("/v1beta/something"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other["echo"], "/v1beta/something");
    assert_eq!(other["method"], "GET");
}

#[tokio::test]
async fn sse_passthrough_streams_incrementally() {
    let env = spawn_env(Opts::default()).await;
    let mut resp = env
        .client
        .post(env.url("/v1internal:streamGenerateContent?alt=sse"))
        .header("authorization", "Bearer paid")
        .header("x-test-hold", "1")
        .body(NATIVE_BODY)
        .send()
        .await
        .unwrap();
    // İkinci parça kapı açılana kadar gelmez; ilk parça tamponlanmadan ulaşmalı.
    let first = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
        .await
        .expect("ilk parça tamponlandı")
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("birinci"));
    env.prod.gate.notify_one();
    let second = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
        .await
        .expect("ikinci parça gelmedi")
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&second).contains("ikinci"));
}

// ───────────────────────── katalog birleştirme ─────────────────────────

#[tokio::test]
async fn fetch_available_models_merges_google_and_custom_models() {
    let env = spawn_env(Opts::default()).await;
    let cat = env.catalog(Some("free")).await;

    let models = cat["models"].as_object().unwrap();
    assert!(
        models.contains_key("gemini-pro") && models.contains_key("gemini-flash"),
        "Google modelleri korunmalı"
    );
    assert!(
        models.contains_key("custom-deepseek"),
        "özel model eklenmeli"
    );
    assert_eq!(cat["extra"], "korunmalı");
    assert_eq!(
        models["gemini-flash"]["displayName"], "Gemini Flash",
        "Google girdisi değiştirilmemeli"
    );

    // Seçici etikete göre çözdüğü için özel modelin etiketi Google'dakiyle çakışmamalı ("DeepSeek" zaten Google'da var).
    let custom_label = models["custom-deepseek"]["displayName"].as_str().unwrap();
    let labels: Vec<&str> = models
        .values()
        .filter_map(|m| m["displayName"].as_str())
        .collect();
    assert_eq!(labels.iter().filter(|l| **l == custom_label).count(), 1);
    assert_ne!(custom_label, "DeepSeek");

    let ids: Vec<&str> = cat["agentModelSorts"][0]["groups"][0]["modelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(ids, ["gemini-pro", "gemini-flash", "custom-deepseek"]);
    assert_eq!(cat["agentModelSorts"][0]["displayName"], "Recommended");

    // Katalog sorgusu da hesap katmanına göre (free → daily) yönlendirildi.
    assert_eq!(env.daily.calls("/v1internal:fetchAvailableModels").len(), 1);
    assert!(
        env.prod
            .calls("/v1internal:fetchAvailableModels")
            .is_empty()
    );
}

#[tokio::test]
async fn native_slot_collision_moves_the_custom_model() {
    let env = spawn_env(Opts::default()).await;
    let first = env.catalog(Some("paid")).await;
    let slot = first["models"]["custom-deepseek"]["model"]
        .as_str()
        .unwrap()
        .to_string();

    // Google sonradan aynı yer tutucuyu kendi modeline verirse yerel model taşınır, Google'ınki dokunulmaz.
    *env.prod.catalog.lock().unwrap() =
        json!({"models": {"gemini-new": {"displayName": "Gemini New", "model": slot}}});
    let second = env.catalog(Some("paid")).await;
    assert_eq!(second["models"]["gemini-new"]["model"], slot.as_str());
    let moved = second["models"]["custom-deepseek"]["model"]
        .as_str()
        .unwrap();
    assert_ne!(moved, slot);

    // Yeni slot kalıcı: bir sonraki sorguda değişmez.
    let third = env.catalog(Some("paid")).await;
    assert_eq!(third["models"]["custom-deepseek"]["model"], moved);
}

#[tokio::test]
async fn google_auth_errors_are_forwarded_not_masked() {
    let env = spawn_env(Opts::default()).await;
    let resp = env
        .client
        .post(env.url("/v1internal:fetchAvailableModels"))
        .header("authorization", "Bearer expired")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], "token süresi doldu");
}

#[tokio::test]
async fn google_unreachable_falls_back_to_custom_only_catalog() {
    let env = spawn_env(Opts {
        google_dead: true,
        ..Opts::default()
    })
    .await;
    for auth in [None, Some("any")] {
        let cat = env.catalog(auth).await;
        let models = cat["models"].as_object().unwrap();
        assert_eq!(models.keys().collect::<Vec<_>>(), ["custom-deepseek"]);
        assert_eq!(
            cat["agentModelSorts"][0]["groups"][0]["modelIds"][0],
            "custom-deepseek"
        );
    }
    // Özel model Google erişilemese de çalışır.
    let r = env
        .client
        .post(env.url("/v1beta/models/custom-deepseek:generateContent"))
        .json(&json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // Google modeline gidiş ise temiz bir 502 verir.
    let g = native_generate(&env, "any").await;
    assert_eq!(g.status(), 502);
}

#[tokio::test]
async fn public_model_list_merges_google_and_custom() {
    let env = spawn_env(Opts::default()).await;
    let list: Value = env
        .client
        .get(env.url("/v1beta/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(names.contains(&"models/gemini-native"), "{names:?}");
    assert!(names.contains(&"models/custom-deepseek"), "{names:?}");
}

// ───────────────────────── yönetim API'si ─────────────────────────

#[tokio::test]
async fn admin_is_disabled_without_token_and_rejects_bad_callers() {
    let off = spawn_env(Opts {
        admin_token: None,
        ..Opts::default()
    })
    .await;
    assert_eq!(
        off.client
            .get(off.url("/admin/status"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );

    let env = spawn_env(Opts::default()).await;
    let url = env.url("/admin/status");
    assert_eq!(env.client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        env.client
            .get(&url)
            .bearer_auth("yanlış-token-0123456789")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        env.client
            .get(&url)
            .header("x-admin-token", ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        env.client
            .get(&url)
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // Tarayıcı kaynaklı istek (CSRF) ve loopback olmayan Host (DNS rebinding) token doğru olsa da reddedilir.
    assert_eq!(
        env.client
            .get(&url)
            .bearer_auth(ADMIN)
            .header("origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        env.client
            .get(&url)
            .bearer_auth(ADMIN)
            .header("host", "evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    // Gateway token'ı yönetim yetkisi vermez.
    let gw = spawn_env(Opts {
        gateway_token: Some(GATEWAY),
        ..Opts::default()
    })
    .await;
    assert_eq!(
        gw.client
            .get(gw.url("/admin/status"))
            .bearer_auth(GATEWAY)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn admin_status_and_config_never_leak_secrets() {
    let env = spawn_env(Opts {
        gateway_token: Some(GATEWAY),
        ..Opts::default()
    })
    .await;
    let status = env
        .admin(Method::GET, "/status")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let config = env
        .admin(Method::GET, "/config")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for body in [&status, &config] {
        for secret in ["sk-test", ADMIN, GATEWAY] {
            assert!(!body.contains(secret), "{secret} sızdı: {body}");
        }
    }
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["models"], 1);
    assert_eq!(status["providers"][0]["name"], "fake");
    assert_eq!(status["providers"][0]["hasKey"], true);
    assert_eq!(status["providers"][0]["keySource"], "file");
    assert_eq!(status["gatewayTokenSet"], true);
    let config: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(config["models"][0]["id"], "deepseek");
    assert!(
        config["models"][0]["placeholder"]
            .as_str()
            .unwrap()
            .starts_with("MODEL_PLACEHOLDER_M")
    );
}

#[tokio::test]
async fn admin_crud_hot_reloads_catalog_and_persists_to_disk() {
    let env = spawn_env(Opts::default()).await;

    let r = env
        .admin(Method::PUT, "/providers/second")
        .json(&json!({"base_url": "http://127.0.0.1:1/v1", "api_key": "sk-secret-xyz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let model = json!({"id": "m2", "provider": "second", "upstream_model": "x-large", "display_name": "İkinci"});
    let added: Value = env
        .admin(Method::POST, "/models")
        .json(&model)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slot = added["placeholder"].as_str().unwrap().to_string();
    assert_eq!(
        env.admin(Method::POST, "/models")
            .json(&model)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );

    // Anında menüde: yeniden başlatma yok.
    let cat = env.catalog(None).await;
    assert_eq!(cat["models"]["custom-m2"]["displayName"], "İkinci");
    assert_eq!(cat["models"]["custom-m2"]["model"], slot.as_str());

    // Kalıcılık: config.json'a yazıldı; anahtar düz metin olarak hiçbir dosyada yok (DPAPI deposunda şifreli).
    let disk = std::fs::read_to_string(env.dir.join("config.json")).unwrap();
    assert!(disk.contains("\"m2\"") && !disk.contains("sk-secret-xyz"));
    let vault = std::fs::read_to_string(env.dir.join("secrets.json")).unwrap();
    assert!(vault.contains("\"second\"") && !vault.contains("sk-secret-xyz"));
    let cfg_view = env
        .admin(Method::GET, "/config")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!cfg_view.contains("sk-secret-xyz"));

    // Kullanımdaki sağlayıcı silinemez; model silinince menüden kalkar.
    assert_eq!(
        env.admin(Method::DELETE, "/providers/second")
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        env.admin(Method::DELETE, "/models/m2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        env.admin(Method::DELETE, "/models/m2")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(env.catalog(None).await["models"].get("custom-m2").is_none());

    // Aynı model yeniden eklenince aynı slotu alır (kalıcı kimlik).
    let again: Value = env
        .admin(Method::POST, "/models")
        .json(&model)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["placeholder"], slot.as_str());

    // Anahtar güncelleme / silme.
    let r: Value = env
        .admin(Method::PUT, "/providers/second/key")
        .json(&json!({"api_key": null}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["hasKey"], false);
    let r: Value = env
        .admin(Method::PUT, "/providers/second/key")
        .json(&json!({"api_key": "sk-yeni-anahtar"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["hasKey"], true);
    assert_eq!(
        env.admin(Method::PUT, "/providers/yok/key")
            .json(&json!({"api_key": "k"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Sağlayıcı güncellemede verilmeyen anahtar korunur.
    env.admin(Method::PUT, "/providers/second")
        .json(&json!({"base_url": "http://127.0.0.1:2/v1"}))
        .send()
        .await
        .unwrap();
    let status: Value = env
        .admin(Method::GET, "/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second = status["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "second")
        .unwrap();
    assert_eq!(second["baseUrl"], "http://127.0.0.1:2/v1");
    assert_eq!(second["hasKey"], true);
}

#[tokio::test]
async fn admin_validation_errors_do_not_change_state() {
    let env = spawn_env(Opts::default()).await;
    // Bilinmeyen sağlayıcıya bağlı model, geçersiz URL, düz http + anahtar (loopback olmayan) reddedilir.
    let bad_model = json!({"id": "x", "provider": "yok", "upstream_model": "m"});
    assert_eq!(
        env.admin(Method::POST, "/models")
            .json(&bad_model)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let bad_url = json!({"base_url": "not a url"});
    assert_eq!(
        env.admin(Method::PUT, "/providers/p")
            .json(&bad_url)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let insecure = json!({"base_url": "http://example.com/v1", "api_key": "sk-abc"});
    assert_eq!(
        env.admin(Method::PUT, "/providers/p")
            .json(&insecure)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let status: Value = env
        .admin(Method::GET, "/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["providers"].as_array().unwrap().len(), 1);
    assert_eq!(status["models"], 1);
    assert!(
        !env.dir.join("secrets.json").exists(),
        "reddedilen istek anahtar deposuna yazmamalı"
    );
}

#[tokio::test]
async fn admin_reload_picks_up_manual_config_edits() {
    let env = spawn_env(Opts::default()).await;
    // Önce bir mutasyon: config.json oluşsun.
    env.admin(Method::PUT, "/providers/second")
        .json(&json!({"base_url": "http://127.0.0.1:1/v1"}))
        .send()
        .await
        .unwrap();
    let path = env.dir.join("config.json");
    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    cfg["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "elle", "provider": "second", "upstream_model": "m"}));
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    assert!(
        env.catalog(None).await["models"]
            .get("custom-elle")
            .is_none()
    );
    assert_eq!(
        env.admin(Method::POST, "/reload")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        env.catalog(None).await["models"]
            .get("custom-elle")
            .is_some()
    );

    // Bozuk dosya yüklenmez, çalışan durum korunur.
    std::fs::write(&path, "{ bozuk").unwrap();
    assert_eq!(
        env.admin(Method::POST, "/reload")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert!(
        env.catalog(None).await["models"]
            .get("custom-elle")
            .is_some()
    );
}
