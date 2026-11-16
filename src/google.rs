//! Özel olmayan trafiğin Google'a aynen aktarılması.
//!
//! Language server tüm trafiğini köprüye gönderdiği için, özel modellere ait olmayan her istek
//! (üretim, `loadCodeAssist`, kota, onboarding …) başlıklarıyla ve yetkilendirmesiyle Google'a gitmeli.
//! Google'ın hataları (örn. süresi dolmuş token için 401) olduğu gibi döner; LS kendi oturumunu yenileyebilsin.

use crate::server::{AppState, Shared, error_response};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, Method, Uri, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

const BUFFERED_LIMIT: usize = 16 * 1024 * 1024;
const CATALOG_LIMIT: usize = 8 * 1024 * 1024;
const BUFFERED_TIMEOUT: Duration = Duration::from_secs(60);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Hop-by-hop başlıkları (RFC 9110 §7.6.1) ve köprüye ait olanlar aktarılmaz.
const STRIP: [&str; 12] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "host",
    "content-length",
    "x-admin-token",
];

fn forward_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = src.clone();
    for name in STRIP {
        out.remove(HeaderName::from_static(name));
    }
    out
}

fn is_cloud(uri: &Uri) -> bool {
    uri.path().contains("v1internal")
}

fn is_generation(uri: &Uri) -> bool {
    uri.path().to_ascii_lowercase().contains("generatecontent")
}

fn target(origin: &str, uri: &Uri) -> String {
    // Yalnızca yol+sorgu kullanılır: istek satırındaki mutlak URI ile köke sızılamaz.
    format!(
        "{}{}",
        origin.trim_end_matches('/'),
        uri.path_and_query().map_or("/", |p| p.as_str())
    )
}

/// Yanıtı gövdesini belleğe almadan istemciye boru ile iletir (SSE dahil).
fn pipe(resp: reqwest::Response) -> Response {
    let mut builder = Response::builder().status(resp.status());
    {
        let h = builder.headers_mut().unwrap();
        *h = resp.headers().clone();
        for name in STRIP.iter().filter(|n| **n != "content-length") {
            h.remove(HeaderName::from_static(name));
        }
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap()
}

async fn read_capped(resp: reqwest::Response, max: usize) -> Result<Bytes, String> {
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return Err(format!("yanıt {} MB sınırını aşıyor", max / 1024 / 1024));
    }
    let mut buf = Vec::new();
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| e.without_url().to_string())?);
        if buf.len() > max {
            return Err(format!("yanıt {} MB sınırını aşıyor", max / 1024 / 1024));
        }
    }
    Ok(buf.into())
}

fn send(
    state: &AppState,
    method: Method,
    url: String,
    headers: &HeaderMap,
    body: Bytes,
    buffered: Option<Duration>,
) -> reqwest::RequestBuilder {
    let mut rb = state
        .client
        .request(method.clone(), url)
        .headers(forward_headers(headers));
    if let Some(timeout) = buffered {
        // İnceleyeceğimiz yanıtlar sıkıştırılmamış gelmeli.
        rb = rb
            .header(header::ACCEPT_ENCODING, "identity")
            .timeout(timeout);
    }
    if !body.is_empty() || !matches!(method, Method::GET | Method::HEAD) {
        rb = rb.body(body);
    }
    rb
}

fn buffered_response(status: u16, upstream_headers: &HeaderMap, body: Vec<u8>) -> Response {
    let mut builder = Response::builder().status(status);
    {
        let h = builder.headers_mut().unwrap();
        *h = forward_headers(upstream_headers);
        h.remove(header::CONTENT_ENCODING);
    }
    builder.body(Body::from(body)).unwrap()
}

/// Google köklerini köprü adresiyle değiştirir (yanıt gövdesindeki geri dönüş URL'leri köprüden geçsin).
fn rewrite_origins(text: String, state: &AppState, request_host: Option<&str>) -> String {
    let Some(host) = request_host else {
        return text;
    };
    let g = &state.live().config.google;
    let local = format!("http://{host}");
    [&g.cloudcode_production, &g.cloudcode_daily, &g.generative]
        .into_iter()
        .fold(text, |t, origin| {
            t.replace(origin.trim_end_matches('/'), &local)
        })
}

fn host_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// Eşleşmeyen / özel olmayan her istek: Google'a aktar.
pub async fn passthrough(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let live = s.live();
    let cloud = is_cloud(&uri);
    let generation = is_generation(&uri);

    if !cloud {
        let url = target(&live.config.google.generative, &uri);
        return match send(&s, method, url, &headers, body, None).send().await {
            Ok(resp) => pipe(resp),
            Err(e) => error_response(502, format!("Google'a bağlanılamadı: {}", e.without_url())),
        };
    }

    if generation {
        let origin = match s
            .routing
            .resolve_for_generation(&s.client, &headers, &body)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("{e}");
                return error_response(
                    502,
                    format!("Cloud Code hesap yolu belirlenemedi, tekrar deneyin: {e}"),
                );
            }
        };
        return match send(&s, method, target(&origin, &uri), &headers, body, None)
            .send()
            .await
        {
            Ok(resp) => pipe(resp),
            Err(e) => error_response(502, format!("Google'a bağlanılamadı: {}", e.without_url())),
        };
    }

    // Cloud Code, üretim dışı: tamponla, `loadCodeAssist`i gözle, köklerı yeniden yaz.
    let origin = s.routing.resolve(&headers);
    let resp = match send(
        &s,
        method,
        target(&origin, &uri),
        &headers,
        body,
        Some(BUFFERED_TIMEOUT),
    )
    .send()
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(502, format!("Google'a bağlanılamadı: {}", e.without_url()));
        }
    };
    let status = resp.status().as_u16();
    let upstream_headers = resp.headers().clone();
    let bytes = match read_capped(resp, BUFFERED_LIMIT).await {
        Ok(b) => b,
        Err(e) => return error_response(502, format!("Google yanıtı okunamadı: {e}")),
    };
    if uri.path() == "/v1internal:loadCodeAssist"
        && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
    {
        s.routing.observe(&headers, status, &v);
    }
    let out = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => rewrite_origins(text, &s, host_of(&headers)).into_bytes(),
        Err(_) => bytes.to_vec(),
    };
    buffered_response(status, &upstream_headers, out)
}

/// Google'ın `fetchAvailableModels` yanıtı + özel modeller.
pub async fn fetch_available_models(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let origin = match s
        .routing
        .resolve_for_generation(&s.client, &headers, &body)
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("katalog için hesap yolu belirlenemedi, varsayılan kök: {e}");
            s.routing.resolve(&headers)
        }
    };
    let upstream = match send(
        &s,
        method,
        target(&origin, &uri),
        &headers,
        body,
        Some(CATALOG_TIMEOUT),
    )
    .send()
    .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let hdrs = resp.headers().clone();
            match read_capped(resp, CATALOG_LIMIT).await {
                // Kimlik hatasını gizlemeyiz: LS oturumunu yenileyebilmeli.
                Ok(b) if matches!(status, 401 | 403) => {
                    return buffered_response(status, &hdrs, b.to_vec());
                }
                Ok(b) if (200..300).contains(&status) => {
                    serde_json::from_slice::<Value>(&b).map_err(|_| "okunamayan yanıt".to_string())
                }
                Ok(_) => Err(format!("HTTP {status}")),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e.without_url().to_string()),
    };
    let upstream = upstream.unwrap_or_else(|why| {
        tracing::warn!("Google kataloğu alınamadı ({why}); yalnız özel modeller listeleniyor");
        Value::Null
    });
    s.observe_native(&upstream).await;
    let merged = s.live().catalog.merge_fetch_available_models(upstream);
    axum::Json(merged).into_response()
}

/// `GET /v1beta/models`: Google listesi + özel modeller.
pub async fn list_models(
    State(s): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let origin = s.live().config.google.generative.clone();
    let upstream = match send(
        &s,
        method,
        target(&origin, &uri),
        &headers,
        body,
        Some(CATALOG_TIMEOUT),
    )
    .send()
    .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let hdrs = resp.headers().clone();
            match read_capped(resp, CATALOG_LIMIT).await {
                Ok(b) if matches!(status, 401 | 403) => {
                    return buffered_response(status, &hdrs, b.to_vec());
                }
                Ok(b) if (200..300).contains(&status) => {
                    serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            }
        }
        Err(_) => Value::Null,
    };
    axum::Json(s.live().catalog.merge_model_list(upstream)).into_response()
}
