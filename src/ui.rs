//! Gömülü yönetim arayüzü (`/ui`): tek sayfa, harici kaynak yok, sıkı CSP.
//!
//! Sayfa herkese açıktır ama hiçbir yetki taşımaz; tüm işlemler `/admin/*` uçlarına yönetim token'ıyla
//! (sayfada girilir, yalnızca `sessionStorage`'da tutulur) yapılır.

use crate::server::Shared;
use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};

const INDEX: &str = include_str!("ui/index.html");
const JS: &str = include_str!("ui/app.js");
const CSS: &str = include_str!("ui/app.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; \
                   base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, CSP),
        ],
        body,
    )
        .into_response()
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/ui",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/ui/",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/ui/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", JS) }),
        )
        .route(
            "/ui/app.css",
            get(|| async { asset("text/css; charset=utf-8", CSS) }),
        )
}
