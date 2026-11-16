//! Tüm yollar için ortak ağ koruması (web tabanlı saldırılara karşı).
//!
//! * `Host` loopback olmalı → DNS rebinding ile `evil.example` adından köprüye ulaşılamaz.
//! * `Origin` varsa köprünün kendi kaynağıyla (`http://<Host>`) birebir aynı olmalı → tarayıcıdaki başka bir
//!   sayfa köprüyü çağıramaz (tarayıcı `Origin`'i sahteletmeye izin vermez). Language server (Go istemcisi) `Origin` göndermez.
//!
//! Yönetim token'ı bu korumanın yerine geçmez; ikisi birlikte uygulanır.

use crate::server::error_response;
use axum::{
    extract::Request,
    http::{HeaderMap, header},
    middleware::Next,
    response::Response,
};

pub fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// `Origin` yoksa serbest; varsa köprünün kendi kaynağı olmalı.
pub fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(host)) = (
        origin.to_str(),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
}

pub async fn local_only(req: Request, next: Next) -> Response {
    let headers = req.headers();
    if !host_is_loopback(headers) {
        return error_response(403, "yalnızca loopback Host ile erişilebilir");
    }
    if !origin_ok(headers) {
        return error_response(403, "tarayıcı kaynaklı (Origin) istekler reddedilir");
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut m = HeaderMap::new();
        if let Some(v) = host {
            m.insert(header::HOST, HeaderValue::from_str(v).unwrap());
        }
        if let Some(v) = origin {
            m.insert(header::ORIGIN, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn host_must_be_loopback() {
        for ok in [
            "127.0.0.1:4000",
            "localhost:4000",
            "[::1]:4000",
            "LOCALHOST",
        ] {
            assert!(host_is_loopback(&h(Some(ok), None)), "{ok}");
        }
        for bad in [
            "evil.example:4000",
            "127.0.0.1.evil.example",
            "10.0.0.5:4000",
        ] {
            assert!(!host_is_loopback(&h(Some(bad), None)), "{bad}");
        }
        assert!(!host_is_loopback(&h(None, None)));
    }

    #[test]
    fn origin_must_match_own_host() {
        assert!(origin_ok(&h(Some("127.0.0.1:4000"), None)));
        assert!(origin_ok(&h(
            Some("127.0.0.1:4000"),
            Some("http://127.0.0.1:4000")
        )));
        assert!(!origin_ok(&h(
            Some("127.0.0.1:4000"),
            Some("http://127.0.0.1:5000")
        )));
        assert!(!origin_ok(&h(
            Some("127.0.0.1:4000"),
            Some("https://evil.example")
        )));
        assert!(!origin_ok(&h(Some("127.0.0.1:4000"), Some("null"))));
        assert!(!origin_ok(&h(None, Some("http://127.0.0.1:4000"))));
    }
}
