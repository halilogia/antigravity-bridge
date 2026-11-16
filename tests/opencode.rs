//! Real HTTP against local mocks only: no credentials or paid provider requests.
use antigravity_bridge::{
    config::{Config, ConfigStore, Protocol},
    opencode::{self, Plan},
    server::{self, AppState},
};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Mock {
    seen: Arc<Mutex<Vec<Value>>>,
    mode: u8,
}
async fn handler(
    axum::extract::State(mock): axum::extract::State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if uri.path().ends_with("/models") {
        return Json(json!({"data":[{"id":"kimi-test"},{"id":"kimi-test"},{"id":"glm-test"}]}))
            .into_response();
    }
    let mut v: Value = serde_json::from_slice(&body).unwrap();
    v["_path"] = uri.path().into();
    v["_session"] = headers["x-opencode-session"].to_str().unwrap().into();
    v["_auth"] = headers["authorization"].to_str().unwrap().into();
    v["_ua"] = headers["user-agent"].to_str().unwrap().into();
    let count = {
        let mut seen = mock.seen.lock().unwrap();
        seen.push(v.clone());
        seen.len()
    };
    if mock.mode == 2 || (mock.mode == 1 && count == 1) {
        return (
            StatusCode::BAD_REQUEST,
            "[1210] thinking required secret-that-must-not-leak",
        )
            .into_response();
    }
    if mock.mode == 5 || (mock.mode == 4 && count == 1) {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"error\":{\"message\":\"[1210] thinking required\"}}\n\n",
        )
            .into_response();
    }
    if mock.mode == 6 {
        return (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"choices\":[{\"delta\":{\"content\":\"unfinished\"}}]}\n\n",
        )
            .into_response();
    }
    let events = if uri.path().ends_with("/messages") {
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"c1","name":"runCommand","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"dir\""}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]
    } else if uri.path().ends_with("/responses") {
        vec![
            json!({"type":"response.output_text.delta","delta":"hello"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"runCommand"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\":\"dir\""}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"runCommand","arguments":"{\"command\":\"dir\"}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
        ]
    } else {
        vec![
            json!({"session_id":"upstream-cache-42","choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}),
        ]
    };
    let mut sse = events
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    if mock.mode == 3 {
        sse.push_str("data: {\"error\":{\"message\":\"[1210] thinking required\"}}\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
}
struct Harness {
    url: String,
    mock: Mock,
    _dir: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn harness(protocol: Protocol, plan: Plan, mode: u8) -> Harness {
    let mock = Mock {
        mode,
        ..Mock::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(handler).with_state(mock.clone());
    let a = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(Some(dir.path().join("config.json"))).unwrap();
    let mut provider = opencode::preset_provider(plan);
    provider.base_url = base;
    provider.secret = Some("test-key".into());
    let mut model = opencode::suggest_model("oc", plan, "kimi-test");
    model.id = "test".into();
    model.protocol = protocol;
    let mut cfg = Config::default();
    cfg.providers.insert("oc".into(), provider);
    cfg.models.push(model);
    #[cfg(windows)]
    {
        store.secrets().set("oc", "test-key").unwrap();
        store.save(&cfg).unwrap();
        cfg = store.load().unwrap();
        assert!(cfg.providers["oc"].api_key.is_none());
    }
    let state = Arc::new(AppState::new(store, cfg).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let b = tokio::spawn(async move { server::serve(state, listener).await.unwrap() });
    Harness {
        url,
        mock,
        _dir: dir,
        tasks: vec![a, b],
    }
}
fn request(session: &str) -> Value {
    json!({"model":"test","request":{"sessionId":session,"contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"run_command","parameters":{"type":"OBJECT","properties":{"CommandLine":{"type":"STRING"}}}}]}]}})
}
async fn generate(h: &Harness, session: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1internal:generateContent", h.url))
        .json(&request(session))
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn retry_once_max_effort_session_cache_and_isolation() {
    let h = harness(Protocol::Chat, Plan::Go, 1).await;
    assert!(generate(&h, "one").await.status().is_success());
    assert!(generate(&h, "one").await.status().is_success());
    assert!(generate(&h, "two").await.status().is_success());
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0]["reasoning_effort"], "max");
    assert_eq!(seen[1]["reasoning_effort"], "low");
    assert_eq!(seen[0]["_session"], seen[1]["_session"]);
    assert_eq!(seen[1]["_session"], seen[2]["_session"]);
    assert_ne!(seen[2]["_session"], seen[3]["_session"]);
    assert_eq!(seen[2]["session_id"], "upstream-cache-42");
    assert_ne!(seen[3]["session_id"], "upstream-cache-42");
    assert_eq!(seen[0]["_auth"], "Bearer test-key");
    assert_eq!(seen[0]["_path"], "/go/openai/v1/chat/completions");
    assert_eq!(seen[0]["_ua"], "antigravity-bridge/0.1.0");
}
#[tokio::test]
async fn retry_is_bounded_and_errors_do_not_expose_provider_body() {
    let h = harness(Protocol::Chat, Plan::Go, 2).await;
    let response = generate(&h, "one").await;
    assert_eq!(response.status(), 400);
    assert!(!response.text().await.unwrap().contains("secret-that"));
    assert_eq!(h.mock.seen.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn never_retry_after_content_started() {
    let h = harness(Protocol::Chat, Plan::Go, 3).await;
    assert_eq!(generate(&h, "one").await.status(), 502);
    assert_eq!(h.mock.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn messages_and_responses_preserve_text_tools_usage_and_correct_endpoints() {
    for (protocol, path) in [
        (Protocol::Messages, "/anthropic/v1/messages"),
        (Protocol::Responses, "/openai/v1/responses"),
    ] {
        let h = harness(protocol, Plan::Zen, 0).await;
        let response = generate(&h, "one").await;
        assert!(response.status().is_success());
        let result: Value = response.json().await.unwrap();
        let parts = result["response"]["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        assert!(parts.iter().any(|p| p["text"] == "hello"));
        let calls: Vec<_> = parts.iter().filter_map(|p| p.get("functionCall")).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "run_command");
        assert_eq!(calls[0]["args"]["CommandLine"], "dir");
        assert_eq!(result["response"]["usageMetadata"]["totalTokenCount"], 15);
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen[0]["_path"], path);
        if protocol == Protocol::Messages {
            assert_eq!(seen[0]["max_tokens"], 8192);
            assert_eq!(seen[0]["thinking"]["budget_tokens"], 8191);
        } else {
            assert_eq!(seen[0]["max_output_tokens"], 8192);
            assert_eq!(seen[0]["reasoning"]["effort"], "high");
        }
    }
}
#[tokio::test]
async fn discover_uses_plan_catalog_endpoint_and_deduplicates() {
    let mock = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/go/v1/models", axum::routing::get(handler))
        .with_state(mock);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut p = opencode::preset_provider(Plan::Go);
    p.base_url = base;
    let models = opencode::discover_models(&reqwest::Client::new(), &p)
        .await
        .unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "glm-test");
    task.abort();
}
#[test]
fn protocol_mapping_is_plan_specific_and_explicit_override_wins() {
    let mut model = opencode::suggest_model("oc", Plan::Go, "minimax-m3");
    assert_eq!(opencode::protocol(&model, Plan::Go), Protocol::Messages);
    assert_eq!(opencode::protocol(&model, Plan::Zen), Protocol::Chat);
    model.upstream_model = "minimax-m3.1-flash".into();
    assert_eq!(opencode::protocol(&model, Plan::Go), Protocol::Chat);
    model.protocol = Protocol::Responses;
    assert_eq!(opencode::protocol(&model, Plan::Go), Protocol::Responses);
}

#[tokio::test]
async fn thinking_error_in_sse_prelude_retries_only_once() {
    let h = harness(Protocol::Chat, Plan::Go, 4).await;
    assert!(generate(&h, "one").await.status().is_success());
    assert_eq!(h.mock.seen.lock().unwrap().len(), 2);
    let h = harness(Protocol::Chat, Plan::Go, 5).await;
    assert_eq!(generate(&h, "one").await.status(), 502);
    assert_eq!(h.mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn truncated_stream_is_not_reported_as_success() {
    let h = harness(Protocol::Chat, Plan::Go, 6).await;
    assert_eq!(generate(&h, "one").await.status(), 502);
    assert_eq!(h.mock.seen.lock().unwrap().len(), 1);
}

#[test]
fn protocol_requests_preserve_system_images_and_tool_history() {
    use antigravity_bridge::{
        gemini::GenerateRequest,
        translate::{messages, responses},
    };
    let req:GenerateRequest = serde_json::from_value(json!({
        "systemInstruction":{"parts":[{"text":"system rules"}]},
        "contents":[
            {"role":"user","parts":[{"text":"inspect"},{"inlineData":{"mimeType":"image/png","data":"YWJj"}}]},
            {"role":"model","parts":[{"functionCall":{"id":"c7","name":"inspect","args":{"x":1}}}]},
            {"role":"user","parts":[{"functionResponse":{"id":"c7","name":"inspect","response":{"output":"done"}}}]}
        ]
    })).unwrap();
    let mut model = opencode::suggest_model("oc", Plan::Go, "test");
    model.supports_vision = true;
    let m = messages::to_messages(&req, &model, Some("max"), None);
    assert_eq!(m["system"], "system rules");
    assert_eq!(m["messages"][0]["content"][1]["source"]["data"], "YWJj");
    assert_eq!(m["messages"][1]["content"][0]["id"], "c7");
    assert_eq!(m["messages"][2]["content"][0]["tool_use_id"], "c7");
    let r = responses::to_responses(&req, &model, Some("max"), None);
    assert_eq!(r["instructions"], "system rules");
    assert_eq!(
        r["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,YWJj"
    );
    let items = r["input"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|v| v["type"] == "function_call" && v["call_id"] == "c7")
    );
    assert!(items.iter().any(|v| v["type"] == "function_call_output"
        && v["call_id"] == "c7"
        && v["output"] == "done"));
}
