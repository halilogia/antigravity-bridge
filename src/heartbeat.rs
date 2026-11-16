//! Nabız dosyası (`bridge.json`): köprü çalışırken her 5 sn yenilenir, çıkışta silinir.
//!
//! Antigravity'ye uygulanan yama (`patch`) language server adreslerini yalnızca bu dosya
//! taze ve süreç canlıysa köprüye çevirir; aksi halde Google adreslerine geri döner.
//! Böylece köprü kapalıyken Antigravity bozulmaz.
//!
//! Biçim: `{"pid":123,"url":"http://127.0.0.1:4000","ts":<epoch ms>,"version":"0.1.0"}`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const FILE_NAME: &str = "bridge.json";
pub const INTERVAL: Duration = Duration::from_secs(5);
/// Yama tarafındaki tazelik eşiği ile aynı olmalı.
pub const FRESH_MS: u64 = 15_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub pid: u32,
    pub url: String,
    pub ts: u64,
    pub version: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn default_path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

fn write(path: &Path, url: &str) -> Result<()> {
    let info = Info {
        pid: std::process::id(),
        url: url.to_string(),
        ts: now_ms(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&info)?)?;
    if std::fs::rename(&tmp, path).is_err() {
        // Okuyucu dosyayı tutuyorsa yeniden adlandırma başarısız olabilir; doğrudan yaz.
        std::fs::write(path, serde_json::to_vec(&info)?)?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

/// Dosyayı okur; yoksa/bozuksa `None`.
pub fn read(path: &Path) -> Option<Info> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn is_fresh(info: &Info) -> bool {
    now_ms().saturating_sub(info.ts) < FRESH_MS
}

/// Çalışan nabız: düşürüldüğünde / `stop` çağrıldığında dosya (kendi pid'imizse) silinir.
pub struct Heartbeat {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Heartbeat {
    /// Hemen yazar ve arka planda yenilemeyi başlatır. Tokio çalışma zamanı içinde çağrılmalı.
    pub fn start(path: PathBuf, url: String) -> Result<Self> {
        write(&path, &url)?;
        let (p, u) = (path.clone(), url);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = write(&p, &u) {
                    tracing::debug!("nabız yazılamadı: {e}");
                }
            }
        });
        Ok(Self { path, task })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.task.abort();
        if read(&self.path).is_some_and(|i| i.pid == std::process::id()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_fresh_file_and_removes_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let hb = Heartbeat::start(path.clone(), "http://127.0.0.1:4000".into()).unwrap();
        let info = read(&path).unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.url, "http://127.0.0.1:4000");
        assert!(is_fresh(&info));
        drop(hb);
        assert!(!path.exists());
    }

    #[test]
    fn stale_and_foreign_files() {
        let old = Info {
            pid: 1,
            url: "u".into(),
            ts: now_ms() - FRESH_MS - 1,
            version: "x".into(),
        };
        assert!(!is_fresh(&old));
        assert!(read(Path::new("/yok/bridge.json")).is_none());
    }
}
