//! Cloud Code hesap yönlendirmesi: production mu daily mi?
//!
//! Antigravity IDE önce production Cloud Code'u kullanır, `loadCodeAssist` hesap katmanını gösterince
//! GCP-ToS dışı katmanları daily köke taşır. LS'yi yerel adrese yönlendirmek bu geçişi gizlediği için burada
//! yeniden yapılır. Önbellek yalnızca Bearer token'ın SHA-256 özetini tutar (token saklanmaz).

use anyhow::{Result, anyhow, bail};
use axum::http::{HeaderMap, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_CALLERS: usize = 128;
const TIER_TTL: Duration = Duration::from_secs(3600);
const MAX_TIER_BYTES: usize = 512 * 1024;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

type Key = [u8; 32];

struct Route {
    origin: String,
    expires: Instant,
}

#[derive(Default)]
struct Callers {
    map: HashMap<Key, Route>,
    order: VecDeque<Key>,
}

pub struct CloudRouting {
    production: String,
    daily: String,
    callers: Mutex<Callers>,
    /// Aynı anda tek `loadCodeAssist` araması; bekleyenler sonucu önbellekten okur.
    gate: tokio::sync::Mutex<()>,
}

fn caller_key(headers: &HeaderMap) -> Option<Key> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if value.len() > 16 * 1024 {
        return None;
    }
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.contains(char::is_whitespace)
    {
        return None;
    }
    Some(Sha256::digest(token.as_bytes()).into())
}

impl CloudRouting {
    pub fn new(production: &str, daily: &str) -> Self {
        Self {
            production: production.trim_end_matches('/').to_string(),
            daily: daily.trim_end_matches('/').to_string(),
            callers: Mutex::default(),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn production(&self) -> &str {
        &self.production
    }

    pub fn cached_accounts(&self) -> usize {
        self.callers.lock().unwrap().map.len()
    }

    pub fn clear(&self) {
        *self.callers.lock().unwrap() = Callers::default();
    }

    fn expire(c: &mut Callers, now: Instant) {
        c.map.retain(|_, r| r.expires > now);
        let map = &c.map;
        c.order.retain(|k| map.contains_key(k));
    }

    /// Bilinmeyen çağıran → production (IDE'nin varsayılanı).
    pub fn resolve(&self, headers: &HeaderMap) -> String {
        self.resolve_at(headers, Instant::now())
    }

    fn resolve_at(&self, headers: &HeaderMap, now: Instant) -> String {
        let Some(key) = caller_key(headers) else {
            return self.production.clone();
        };
        let mut c = self.callers.lock().unwrap();
        Self::expire(&mut c, now);
        match c.map.get(&key) {
            Some(r) => {
                let origin = r.origin.clone();
                // Yakınlık sırasını tazele; TTL uzatılmaz.
                c.order.retain(|k| *k != key);
                c.order.push_back(key);
                origin
            }
            None => self.production.clone(),
        }
    }

    /// `loadCodeAssist` yanıtından kök seçimini öğrenir. Geçersiz yanıt yönlendirmeyi değiştirmez.
    pub fn observe(&self, headers: &HeaderMap, status: u16, response: &Value) {
        self.observe_at(headers, status, response, Instant::now());
    }

    fn observe_at(&self, headers: &HeaderMap, status: u16, response: &Value, now: Instant) {
        if !(200..300).contains(&status) || !response.is_object() {
            return;
        }
        let Some(key) = caller_key(headers) else {
            return;
        };
        let tier = response
            .get("paidTier")
            .or_else(|| response.get("paid_tier"))
            .filter(|t| !t.is_null());
        if tier.is_some_and(|t| !t.is_object()) {
            return;
        }
        let uses_gcp_tos = tier
            .and_then(|t| t.get("usesGcpTos").or_else(|| t.get("uses_gcp_tos")))
            .filter(|v| !v.is_null());
        if uses_gcp_tos.is_some_and(|v| !v.is_boolean()) {
            return;
        }
        let origin = if uses_gcp_tos == Some(&Value::Bool(true)) {
            &self.production
        } else {
            &self.daily
        };
        let mut c = self.callers.lock().unwrap();
        Self::expire(&mut c, now);
        c.order.retain(|k| *k != key);
        c.order.push_back(key);
        c.map.insert(
            key,
            Route {
                origin: origin.clone(),
                expires: now + TIER_TTL,
            },
        );
        while c.order.len() > MAX_CALLERS {
            if let Some(old) = c.order.pop_front() {
                c.map.remove(&old);
            }
        }
    }

    /// Üretim isteği öncesi: önbellekte yoksa `loadCodeAssist` ile hesap katmanını öğrenir.
    /// Başarısız olursa hata döner; üretim asla yanlış köke tekrar gönderilmez.
    pub async fn resolve_for_generation(
        &self,
        client: &reqwest::Client,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<String> {
        let Some(key) = caller_key(headers) else {
            return Ok(self.production.clone());
        };
        if self.is_known(&key) {
            return Ok(self.resolve(headers));
        }
        let _gate = self.gate.lock().await;
        if self.is_known(&key) {
            return Ok(self.resolve(headers));
        }
        let (status, response) = self.lookup(client, headers, body).await?;
        self.observe(headers, status, &response);
        if !self.is_known(&key) {
            bail!("hesap katmanı öğrenilemedi (loadCodeAssist HTTP {status})");
        }
        Ok(self.resolve(headers))
    }

    fn is_known(&self, key: &Key) -> bool {
        let mut c = self.callers.lock().unwrap();
        Self::expire(&mut c, Instant::now());
        c.map.contains_key(key)
    }

    async fn lookup(
        &self,
        client: &reqwest::Client,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<(u16, Value)> {
        let project = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
            v.get("project")
                .and_then(Value::as_str)
                .filter(|p| p.len() <= 1024)
                .map(str::to_owned)
        });
        let version = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .and_then(|ua| {
                ua.to_ascii_lowercase()
                    .find("antigravity/")
                    .map(|i| (ua.to_string(), i + "antigravity/".len()))
            })
            .map(|(ua, i)| {
                ua[i..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty());
        let mut metadata = json!({"ideName": "antigravity", "ideType": "ANTIGRAVITY"});
        if let Some(v) = version {
            metadata["ideVersion"] = v.into();
        }
        let mut payload = json!({ "metadata": metadata });
        if let Some(p) = project {
            payload["cloudaicompanionProject"] = p.into();
        }
        let mut req = client
            .post(format!("{}/v1internal:loadCodeAssist", self.production))
            .timeout(LOOKUP_TIMEOUT)
            .header(header::ACCEPT_ENCODING, "identity")
            .json(&payload);
        for name in [
            "authorization",
            "user-agent",
            "x-goog-user-project",
            "x-goog-api-client",
        ] {
            if let Some(v) = headers.get(name) {
                req = req.header(name, v);
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("katman araması başarısız: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        if resp
            .content_length()
            .is_some_and(|n| n as usize > MAX_TIER_BYTES)
        {
            bail!("katman yanıtı çok büyük");
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("katman yanıtı okunamadı: {}", e.without_url()))?;
        if bytes.len() > MAX_TIER_BYTES {
            bail!("katman yanıtı çok büyük");
        }
        let value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("katman yanıtı okunamadı"))?;
        Ok((status, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    fn routing() -> CloudRouting {
        CloudRouting::new("https://prod.test", "https://daily.test/")
    }

    #[test]
    fn unknown_caller_defaults_to_production_and_gcp_tos_stays_production() {
        let r = routing();
        assert_eq!(r.resolve(&headers("t1")), "https://prod.test");
        assert_eq!(r.resolve(&HeaderMap::new()), "https://prod.test");
        r.observe(
            &headers("t1"),
            200,
            &json!({"paidTier": {"usesGcpTos": true}}),
        );
        assert_eq!(r.resolve(&headers("t1")), "https://prod.test");
        assert_eq!(r.cached_accounts(), 1);
    }

    #[test]
    fn non_gcp_tiers_move_to_daily_and_accounts_are_isolated() {
        let r = routing();
        r.observe(
            &headers("free"),
            200,
            &json!({"paidTier": {"usesGcpTos": false}}),
        );
        r.observe(&headers("noTier"), 200, &json!({"currentTier": {}}));
        assert_eq!(r.resolve(&headers("free")), "https://daily.test");
        assert_eq!(r.resolve(&headers("noTier")), "https://daily.test");
        assert_eq!(r.resolve(&headers("other")), "https://prod.test");
    }

    #[test]
    fn invalid_observations_change_nothing() {
        let r = routing();
        let h = headers("t");
        r.observe(&h, 500, &json!({"paidTier": {"usesGcpTos": false}}));
        r.observe(&h, 200, &json!([1]));
        r.observe(&h, 200, &json!({"paidTier": "x"}));
        r.observe(&h, 200, &json!({"paidTier": {"usesGcpTos": "yes"}}));
        r.observe(&HeaderMap::new(), 200, &json!({}));
        assert_eq!(r.cached_accounts(), 0);
    }

    #[test]
    fn entries_expire_and_cache_is_bounded() {
        let r = routing();
        let t0 = Instant::now();
        r.observe_at(&headers("a"), 200, &json!({}), t0);
        assert_eq!(
            r.resolve_at(&headers("a"), t0 + Duration::from_secs(3599)),
            "https://daily.test"
        );
        assert_eq!(
            r.resolve_at(&headers("a"), t0 + Duration::from_secs(3601)),
            "https://prod.test"
        );
        for i in 0..MAX_CALLERS + 10 {
            r.observe_at(&headers(&format!("tok{i}")), 200, &json!({}), t0);
        }
        assert_eq!(r.cached_accounts(), MAX_CALLERS);
        assert_eq!(
            r.resolve_at(&headers("tok0"), t0),
            "https://prod.test",
            "en eski çıkarıldı"
        );
        assert_eq!(
            r.resolve_at(&headers(&format!("tok{}", MAX_CALLERS + 9)), t0),
            "https://daily.test"
        );
    }

    #[test]
    fn bearer_parsing_is_strict() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(caller_key(&h).is_none());
        h.insert(header::AUTHORIZATION, "bearer a b".parse().unwrap());
        assert!(caller_key(&h).is_none());
        h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(caller_key(&h), caller_key(&headers("abc")));
    }
}
