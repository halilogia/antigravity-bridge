//! API anahtarı deposu: Windows DPAPI ile (geçerli kullanıcı kapsamı) şifrelenmiş `secrets.json`.
//!
//! * Dosya biçimi: `{"version":1,"scheme":"dpapi","entries":{"<sağlayıcı>":"<base64 blob>"}}`.
//! * Her kayıt, sağlayıcı adından türetilen entropy ile şifrelenir; bir kaydı başka bir ada kopyalamak çözülemez.
//! * Blob yalnızca bu Windows kullanıcısı + bu makinede çözülür; dosyayı kopyalamak anahtarları sızdırmaz.
//! * Windows dışında depo kullanılamaz; `api_key_env` kullanın.
//!
//! Düz metin anahtar hiçbir zaman diske yazılmaz, günlüklenmez ve yanıtlarda dönmez.

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::BufRead,
    path::{Path, PathBuf},
};

pub const FILE_NAME: &str = "secrets.json";
const SCHEME: &str = "dpapi";
const VERSION: u32 = 1;
const MAX_NAME: usize = 128;
const MAX_VALUE: usize = 16 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretFile {
    version: u32,
    scheme: String,
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bu platformda şifreli depo kullanılabilir mi?
    pub fn available() -> bool {
        cfg!(windows)
    }

    fn read(&self) -> Result<SecretFile> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SecretFile {
                    version: VERSION,
                    scheme: SCHEME.into(),
                    entries: BTreeMap::new(),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("{} okunamadı", self.path.display())),
        };
        let file: SecretFile = serde_json::from_str(&raw)
            .with_context(|| format!("{} geçerli JSON değil", self.path.display()))?;
        if file.version != VERSION || file.scheme != SCHEME {
            bail!(
                "{}: desteklenmeyen biçim (sürüm {}, şema '{}')",
                self.path.display(),
                file.version,
                file.scheme
            );
        }
        Ok(file)
    }

    fn write(&self, file: &SecretFile) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(file)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Kayıtlı ad listesi (değerler çözülmez).
    pub fn names(&self) -> Result<Vec<String>> {
        Ok(self.read()?.entries.into_keys().collect())
    }

    pub fn contains(&self, name: &str) -> Result<bool> {
        Ok(self.read()?.entries.contains_key(name))
    }

    pub fn get(&self, name: &str) -> Result<Option<String>> {
        match self.read()?.entries.get(name) {
            None => Ok(None),
            Some(b64) => decode_entry(name, b64).map(Some),
        }
    }

    /// Çözülebilen tüm kayıtlar. Çözülemeyenler (başka makine/kullanıcı, bozuk) atlanır ve uyarı verilir.
    pub fn get_all(&self) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for (name, b64) in self.read()?.entries {
            match decode_entry(&name, &b64) {
                Ok(v) => {
                    out.insert(name, v);
                }
                Err(e) => tracing::warn!("'{name}' anahtarı çözülemedi (atlandı): {e:#}"),
            }
        }
        Ok(out)
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        check_name(name)?;
        let value = value.trim();
        if value.is_empty() {
            bail!("anahtar boş olamaz");
        }
        if value.len() > MAX_VALUE {
            bail!("anahtar {MAX_VALUE} bayttan uzun olamaz");
        }
        let blob = protect(value.as_bytes(), &entropy(name))?;
        let mut file = self.read()?;
        file.entries.insert(name.to_string(), STANDARD.encode(blob));
        self.write(&file)
    }

    /// Kayıt varsa siler ve `true` döner.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut file = self.read()?;
        let existed = file.entries.remove(name).is_some();
        if existed {
            self.write(&file)?;
        }
        Ok(existed)
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        bail!("geçersiz anahtar adı");
    }
    Ok(())
}

fn entropy(name: &str) -> Vec<u8> {
    format!("antigravity-bridge:v1:{name}").into_bytes()
}

fn decode_entry(name: &str, b64: &str) -> Result<String> {
    let blob = STANDARD.decode(b64.trim()).context("kayıt base64 değil")?;
    let plain = unprotect(&blob, &entropy(name))
        .context("DPAPI çözme başarısız (başka kullanıcı/makine veya bozuk kayıt)")?;
    String::from_utf8(plain).context("anahtar UTF-8 değil")
}

// ───────────────────────── DPAPI ─────────────────────────

#[cfg(windows)]
fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr().cast_mut(),
    };
    let ent = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: girdi/entropy işaretçileri çağrı süresince geçerli; çıktı belleği LocalFree ile bırakılır.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &ent,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        bail!(
            "CryptProtectData başarısız: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: başarılı çağrı `out`u LocalAlloc ile doldurdu.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData.cast()) };
    Ok(bytes)
}

#[cfg(windows)]
fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr().cast_mut(),
    };
    let ent = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: protect() ile aynı sözleşme; açıklama dizgesi istenmediği için null verilir.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &ent,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        bail!(
            "CryptUnprotectData başarısız: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: başarılı çağrı `out`u LocalAlloc ile doldurdu; kopyadan sonra düz metin sıfırlanıp serbest bırakılır.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe {
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn protect(_: &[u8], _: &[u8]) -> Result<Vec<u8>> {
    bail!("şifreli anahtar deposu yalnızca Windows'ta (DPAPI) desteklenir; `api_key_env` kullanın")
}

#[cfg(not(windows))]
fn unprotect(_: &[u8], _: &[u8]) -> Result<Vec<u8>> {
    bail!("şifreli anahtar deposu yalnızca Windows'ta (DPAPI) desteklenir")
}

// ───────────────────────── gizli giriş ─────────────────────────

/// Terminalden yankısız satır okur (konsol değilse stdin'den düz satır). Sondaki satır sonu atılır.
pub fn read_secret_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let _echo = EchoOff::new();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("giriş okunamadı")?;
    if _echo.active() {
        eprintln!();
    }
    Ok(line.trim().to_string())
}

#[cfg(windows)]
struct EchoOff {
    handle: windows_sys::Win32::Foundation::HANDLE,
    original: Option<u32>,
}

#[cfg(windows)]
impl EchoOff {
    fn new() -> Self {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };
        // SAFETY: standart Win32 konsol çağrıları; geçersiz tutamak/konsol dışı durumda `original` None kalır.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) != 0
                && SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) != 0
            {
                return Self {
                    handle,
                    original: Some(mode),
                };
            }
            Self {
                handle,
                original: None,
            }
        }
    }
    fn active(&self) -> bool {
        self.original.is_some()
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        if let Some(mode) = self.original {
            // SAFETY: yukarıda alınan geçerli konsol tutamağı.
            unsafe { windows_sys::Win32::System::Console::SetConsoleMode(self.handle, mode) };
        }
    }
}

#[cfg(not(windows))]
struct EchoOff;

#[cfg(not(windows))]
impl EchoOff {
    fn new() -> Self {
        Self
    }
    fn active(&self) -> bool {
        false
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_encrypted_at_rest_and_bound_to_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::in_dir(dir.path());
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "  sk-secret-value \n").unwrap();
        store.set("b", "other").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("sk-secret-value"));
        assert_eq!(store.names().unwrap(), vec!["a", "b"]);

        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("sk-secret-value") && raw.contains("\"scheme\": \"dpapi\""));

        // Blobu başka ada taşımak çözülemez (ad entropy'ye dahil).
        let mut file: SecretFile = serde_json::from_str(&raw).unwrap();
        let a = file.entries["a"].clone();
        file.entries.insert("c".into(), a);
        std::fs::write(store.path(), serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(store.get("c").is_err());
        let all = store.get_all().unwrap();
        assert_eq!(all.len(), 2, "çözülemeyen kayıt atlanmalı: {all:?}");

        assert!(store.remove("a").unwrap());
        assert!(!store.remove("a").unwrap());
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn rejects_bad_input_and_unknown_format() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::in_dir(dir.path());
        assert!(store.set("", "x").is_err());
        assert!(store.set("a", "   ").is_err());
        std::fs::write(
            store.path(),
            r#"{"version":9,"scheme":"dpapi","entries":{}}"#,
        )
        .unwrap();
        assert!(
            store
                .names()
                .unwrap_err()
                .to_string()
                .contains("desteklenmeyen")
        );
    }
}
