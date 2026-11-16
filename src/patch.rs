//! Conservative ASAR patching with immutable backups and hash-checked recovery.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MARKER: &str = "ANTIGRAVITY_RUST_BRIDGE_V1";
const MEMBER: &str = "/files/dist/files/languageServer.js";
const STATE_DIR: &str = ".antigravity-rust-bridge";

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn safe_path(path: &Path) -> Result<()> {
    for part in path.ancestors() {
        if part.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(part) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "symbolic links are not supported: {}",
                    part.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse points are not supported: {}",
                        part.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn default_install() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA missing; use --install-dir")?,
    )
    .join("Programs/Antigravity"))
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    original: String,
    patched: String,
    language_server: String,
}

struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn lock(dir: &Path) -> Result<Lock> {
    let path = dir.join("operation.lock");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .context("another operation or interrupted lock exists")?;
    Ok(Lock(path))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("asar.bridge-tmp");
    safe_path(&temp)?;
    write_new(&temp, bytes)?;
    let result = fs::rename(&temp, path);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.context("cannot replace archive; close Antigravity and its language server")
}

#[cfg(all(windows, not(test)))]
fn require_closed() -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    // SAFETY: initialized ToolHelp structure and snapshot handle; handle closed on every path.
    unsafe {
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        ensure!(
            handle != INVALID_HANDLE_VALUE,
            "cannot inspect running processes"
        );
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(handle, &mut entry);
        let mut found = false;
        while ok != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
            if name == "antigravity.exe" || name == "language_server.exe" {
                found = true;
                break;
            }
            ok = Process32NextW(handle, &mut entry);
        }
        CloseHandle(handle);
        ensure!(
            !found,
            "close Antigravity and language_server.exe before patch/restore (no processes were stopped)"
        );
    }
    Ok(())
}
#[cfg(all(not(windows), not(test)))]
fn require_closed() -> Result<()> {
    anyhow::bail!("installation changes are only supported on Windows")
}
#[cfg(test)]
fn require_closed() -> Result<()> {
    Ok(())
}

/// Read Electron's two Pickle headers without extracting any archive paths.
fn archive(bytes: &[u8]) -> Result<(Value, usize)> {
    let word = |offset: usize| -> Result<usize> {
        Ok(u32::from_le_bytes(
            bytes
                .get(offset..offset + 4)
                .context("truncated ASAR header")?
                .try_into()?,
        ) as usize)
    };
    ensure!(word(0)? == 4, "invalid ASAR size pickle");
    let size = word(4)?;
    let data_start = size.checked_add(8).context("ASAR overflow")?;
    ensure!(
        (8..=32 * 1024 * 1024).contains(&size) && data_start <= bytes.len(),
        "invalid ASAR header size"
    );
    ensure!(word(8)? + 4 == size, "invalid ASAR header pickle");
    let len = word(12)?;
    ensure!(len <= size - 8, "invalid ASAR JSON length");
    let header: Value = serde_json::from_slice(&bytes[16..16 + len])?;
    Ok((header, data_start))
}

fn member<'a>(header: &Value, bytes: &'a [u8], start: usize, pointer: &str) -> Result<&'a [u8]> {
    let entry = header
        .pointer(pointer)
        .context("unsupported archive layout")?;
    ensure!(
        entry.get("link").is_none() && entry.get("unpacked").is_none(),
        "linked/unpacked patch target refused"
    );
    let offset: usize = entry["offset"]
        .as_str()
        .context("invalid ASAR offset")?
        .parse()?;
    let size: usize = entry["size"]
        .as_u64()
        .context("invalid ASAR size")?
        .try_into()?;
    let begin = start.checked_add(offset).context("ASAR overflow")?;
    let end = begin.checked_add(size).context("ASAR overflow")?;
    bytes.get(begin..end).context("ASAR member out of bounds")
}

fn patch_source(source: &str, heartbeat: &Path, url: &str) -> Result<String> {
    ensure!(
        !source.contains(MARKER)
            && !source.contains("modelPatch")
            && !source.contains("__modelPatch"),
        "already patched or foreign patch detected; restore it with its original installer first"
    );
    // Exact known startup shape. Changes in the vendor layout must be reviewed, never guessed.
    let signature = "async function startLanguageServer(port, csrf, options = {}) {";
    let anchor = "        let spawnArgs = args;";
    ensure!(
        source.matches(signature).count() == 1
            && source.matches(anchor).count() == 1
            && source.matches("const args = [").count() == 1
            && source.contains("(0, child_process_1.spawn)(spawnCmd, spawnArgs,")
            && source
                .contains("const { headless, hostBridgeUrl, hostBridgeToken, wsl } = options;"),
        "unsupported language-server startup layout"
    );
    let heartbeat = serde_json::to_string(&heartbeat.to_string_lossy())?;
    let url = serde_json::to_string(url)?;
    let hook = format!(
        r#"
        // {MARKER}
        if (!wsl) {{
            try {{
                const bridge = JSON.parse(require('fs').readFileSync({heartbeat}, 'utf8'));
                const age = Date.now() - bridge.ts;
                if (Number.isInteger(bridge.pid) && bridge.pid > 0 && age >= 0 && age < 15000 && bridge.url === {url}) {{
                    process.kill(bridge.pid, 0);
                    for (const flag of ['--api_server_url', '--cloud_code_endpoint', '--inference_api_server_url']) {{
                        const indices = args.map((v, i) => v === flag ? i : -1).filter(i => i >= 0);
                        if (indices.length > 1 || (indices.length && indices[0] + 1 >= args.length)) throw new Error('ambiguous bridge flag');
                    }}
                    for (const flag of ['--api_server_url', '--cloud_code_endpoint', '--inference_api_server_url']) {{
                        const i = args.indexOf(flag);
                        if (i >= 0) args[i + 1] = bridge.url;
                        else args.push(flag, bridge.url);
                    }}
                }}
            }} catch (_) {{ /* Keep vendor defaults when bridge is absent. */ }}
        }}
"#
    );
    Ok(source.replacen(anchor, &format!("{hook}{anchor}"), 1))
}

pub fn plan(bytes: &[u8], heartbeat: &Path, url: &str) -> Result<Vec<u8>> {
    let parsed = url::Url::parse(url)?;
    ensure!(
        parsed.scheme() == "http"
            && parsed.host_str() == Some("127.0.0.1")
            && parsed.port().is_some_and(|port| port != 0)
            && parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.username().is_empty(),
        "patch URL must be http://127.0.0.1:<port>"
    );
    let (mut header, start) = archive(bytes)?;
    let source = std::str::from_utf8(member(&header, bytes, start, MEMBER)?)?;
    if let Some(integrity) = header.pointer(MEMBER).and_then(|v| v.get("integrity")) {
        ensure!(
            integrity["algorithm"] == "SHA256" && integrity["hash"] == digest(source.as_bytes()),
            "original member integrity mismatch"
        );
    }
    let defaults = std::str::from_utf8(member(
        &header,
        bytes,
        start,
        "/files/dist/files/lsDefaults.js",
    )?)?;
    ensure!(
        defaults.matches("'--api_server_url'").count() == 1
            && defaults.matches("'--cloud_code_endpoint'").count() == 1
            && defaults.contains("https://generativelanguage.googleapis.com")
            && defaults.contains("https://daily-cloudcode-pa.googleapis.com"),
        "unsupported vendor endpoint defaults"
    );
    let replacement = patch_source(source, heartbeat, url)?;
    let entry = header.pointer_mut(MEMBER).context("missing patch member")?;
    entry["offset"] = (bytes.len() - start).to_string().into();
    entry["size"] = replacement.len().into();
    let block_size = entry
        .pointer("/integrity/blockSize")
        .and_then(Value::as_u64)
        .unwrap_or(4 * 1024 * 1024);
    ensure!(
        (1..=16 * 1024 * 1024).contains(&block_size),
        "invalid ASAR integrity block size"
    );
    entry["integrity"] = json!({"algorithm":"SHA256","hash":digest(replacement.as_bytes()),"blockSize":block_size,
        "blocks":replacement.as_bytes().chunks(block_size as usize).map(digest).collect::<Vec<_>>()});
    let json = serde_json::to_vec(&header)?;
    let payload_size = (4 + json.len() + 3) & !3;
    let header_size = payload_size + 4;
    let mut output = Vec::new();
    for n in [
        4u32,
        header_size.try_into()?,
        payload_size.try_into()?,
        json.len().try_into()?,
    ] {
        output.extend_from_slice(&n.to_le_bytes());
    }
    output.extend_from_slice(&json);
    output.resize(8 + header_size, 0);
    output.extend_from_slice(&bytes[start..]);
    output.extend_from_slice(replacement.as_bytes());
    let (check, offset) = archive(&output)?;
    ensure!(
        member(&check, &output, offset, MEMBER)? == replacement.as_bytes(),
        "ASAR verification failed"
    );
    Ok(output)
}

pub fn patch(install: &Path, heartbeat: &Path, url: &str, check: bool) -> Result<String> {
    safe_path(install)?;
    let install = install.canonicalize()?;
    let asar = install.join("resources/app.asar");
    let ls = install.join("resources/bin/language_server.exe");
    let dir = install.join(STATE_DIR);
    for path in [&asar, &ls, &dir, &dir.join("state.json")] {
        safe_path(path)?;
    }
    ensure!(
        !dir.join("state.json").exists(),
        "patch state already exists; restore before patching again"
    );
    let original = fs::read(&asar)?;
    let patched = plan(&original, heartbeat, url)?;
    let state = State {
        version: 1,
        original: digest(&original),
        patched: digest(&patched),
        language_server: file_hash(&ls)?,
    };
    if check {
        return Ok(format!(
            "compatible; app.asar SHA-256 {}; language_server.exe SHA-256 {}",
            state.original, state.language_server
        ));
    }
    require_closed()?;
    fs::create_dir_all(&dir)?;
    let _lock = lock(&dir)?;
    // A previous partial backup is never overwritten.
    write_new(&dir.join("app.asar.original"), &original)?;
    let backup_ls = dir.join("language_server.exe.original");
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_ls)?;
    std::io::copy(&mut fs::File::open(&ls)?, &mut backup)?;
    backup.sync_all()?;
    ensure!(
        file_hash(&backup_ls)? == state.language_server
            && file_hash(&dir.join("app.asar.original"))? == state.original,
        "backup verification failed"
    );
    write_new(&dir.join("state.json"), &serde_json::to_vec_pretty(&state)?)?;
    ensure!(
        file_hash(&asar)? == state.original && file_hash(&ls)? == state.language_server,
        "installation changed during backup; restore required"
    );
    replace(&asar, &patched)?;
    ensure!(
        file_hash(&asar)? == state.patched,
        "post-write verification failed; restore required"
    );
    Ok(format!("patched; backup: {}", dir.display()))
}

pub fn restore(install: &Path) -> Result<String> {
    safe_path(install)?;
    let install = install.canonicalize()?;
    let dir = install.join(STATE_DIR);
    let asar = install.join("resources/app.asar");
    let ls = install.join("resources/bin/language_server.exe");
    for path in [
        &asar,
        &ls,
        &dir,
        &dir.join("state.json"),
        &dir.join("app.asar.original"),
        &dir.join("language_server.exe.original"),
    ] {
        safe_path(path)?;
    }
    require_closed()?;
    let _lock = lock(&dir)?;
    let state: State = serde_json::from_slice(&fs::read(dir.join("state.json"))?)?;
    ensure!(state.version == 1, "unsupported backup version");
    let current = file_hash(&asar)?;
    ensure!(
        current == state.patched || current == state.original,
        "archive changed since patch; refusing to overwrite an update"
    );
    ensure!(
        file_hash(&ls)? == state.language_server,
        "language server changed since patch; refusing to overwrite an update"
    );
    let original = fs::read(dir.join("app.asar.original"))?;
    ensure!(
        digest(&original) == state.original
            && file_hash(&dir.join("language_server.exe.original"))? == state.language_server,
        "backup hash mismatch"
    );
    if current != state.original {
        replace(&asar, &original)?;
    }
    ensure!(
        file_hash(&asar)? == state.original,
        "restored archive verification failed"
    );
    // Preserve evidence/backups, but move the completed transaction out of the active path.
    drop(_lock);
    let archived = install.join(format!(
        "{STATE_DIR}.restored-{}",
        crate::config::generate_token()?
    ));
    fs::rename(&dir, &archived)?;
    Ok(format!(
        "restored; preserved backup: {}",
        archived.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let source = "async function startLanguageServer(port, csrf, options = {}) {\nconst { headless, hostBridgeUrl, hostBridgeToken, wsl } = options;\nconst args = [];\n        let spawnArgs = args;\n(0, child_process_1.spawn)(spawnCmd, spawnArgs, {});\n}";
        let defaults = "'--api_server_url' '--cloud_code_endpoint' https://generativelanguage.googleapis.com https://daily-cloudcode-pa.googleapis.com";
        let header = json!({"files":{"dist":{"files":{
            "languageServer.js":{"offset":"0","size":source.len()},
            "lsDefaults.js":{"offset":source.len().to_string(),"size":defaults.len()}
        }}}});
        let json = serde_json::to_vec(&header).unwrap();
        let payload = (4 + json.len() + 3) & !3;
        let mut out = Vec::new();
        for n in [4, (payload + 4) as u32, payload as u32, json.len() as u32] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&json);
        out.resize(payload + 12, 0);
        out.extend_from_slice(source.as_bytes());
        out.extend_from_slice(defaults.as_bytes());
        out
    }
    fn installation() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("resources/bin")).unwrap();
        fs::write(dir.path().join("resources/app.asar"), fixture()).unwrap();
        fs::write(
            dir.path().join("resources/bin/language_server.exe"),
            b"fixture binary",
        )
        .unwrap();
        dir
    }
    #[test]
    fn archive_preserves_other_files_and_refuses_repatch() {
        let original = fixture();
        let output = plan(
            &original,
            Path::new("C:/bridge/bridge.json"),
            "http://127.0.0.1:4000",
        )
        .unwrap();
        let (h, s) = archive(&output).unwrap();
        let (old, os) = archive(&original).unwrap();
        assert_eq!(
            member(&h, &output, s, "/files/dist/files/lsDefaults.js").unwrap(),
            member(&old, &original, os, "/files/dist/files/lsDefaults.js").unwrap()
        );
        assert!(plan(&output, Path::new("x"), "http://127.0.0.1:4000").is_err());
        assert!(plan(&original, Path::new("x"), "http://evil.example:4000").is_err());
        assert!(plan(&original[..20], Path::new("x"), "http://127.0.0.1:4000").is_err());
    }
    #[test]
    fn patch_restore_roundtrip_and_check_is_read_only() {
        let dir = installation();
        let heartbeat = dir.path().join("bridge.json");
        patch(dir.path(), &heartbeat, "http://127.0.0.1:4000", true).unwrap();
        assert!(!dir.path().join(STATE_DIR).exists());
        patch(dir.path(), &heartbeat, "http://127.0.0.1:4000", false).unwrap();
        assert_ne!(
            fs::read(dir.path().join("resources/app.asar")).unwrap(),
            fixture()
        );
        assert!(patch(dir.path(), &heartbeat, "http://127.0.0.1:4000", false).is_err());
        restore(dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("resources/app.asar")).unwrap(),
            fixture()
        );
        assert!(!dir.path().join(STATE_DIR).exists());
    }
    #[test]
    fn restore_refuses_updates_and_corrupted_backup() {
        let dir = installation();
        patch(
            dir.path(),
            &dir.path().join("bridge.json"),
            "http://127.0.0.1:4000",
            false,
        )
        .unwrap();
        let target = dir.path().join("resources/app.asar");
        let patched = fs::read(&target).unwrap();
        fs::write(&target, b"vendor update").unwrap();
        assert!(restore(dir.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"vendor update");
        fs::write(&target, &patched).unwrap();
        fs::write(
            dir.path().join(STATE_DIR).join("app.asar.original"),
            b"bad backup",
        )
        .unwrap();
        assert!(restore(dir.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), patched);
    }
    #[test]
    fn restore_recovers_interruption_before_archive_replace() {
        let dir = installation();
        patch(
            dir.path(),
            &dir.path().join("bridge.json"),
            "http://127.0.0.1:4000",
            false,
        )
        .unwrap();
        fs::write(dir.path().join("resources/app.asar"), fixture()).unwrap();
        restore(dir.path()).unwrap();
    }

    #[test]
    #[ignore = "read-only check of the locally installed Desktop archive; requires Node"]
    fn installed_archive_javascript_syntax() {
        let input = fs::read(default_install().unwrap().join("resources/app.asar")).unwrap();
        let output = plan(
            &input,
            Path::new("C:/bridge/bridge.json"),
            "http://127.0.0.1:4000",
        )
        .unwrap();
        let (header, start) = archive(&output).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("languageServer.js");
        fs::write(&script, member(&header, &output, start, MEMBER).unwrap()).unwrap();
        let result = std::process::Command::new("node")
            .arg("--check")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    #[ignore = "requires Node to execute the injected JavaScript in an isolated harness"]
    fn injected_hook_redirects_and_falls_back() {
        let source = r#"async function startLanguageServer(port, csrf, options = {}) {
const { headless, hostBridgeUrl, hostBridgeToken, wsl } = options;
const args = ['--api_server_url','https://original-api','--cloud_code_endpoint','https://original-cloud'];
const spawnCmd = 'fixture';
        let spawnArgs = args;
return (0, child_process_1.spawn)(spawnCmd, spawnArgs, {});
}"#;
        let patched = patch_source(
            source,
            Path::new("C:/fixture/bridge.json"),
            "http://127.0.0.1:4000",
        )
        .unwrap();
        let harness = r#"
const assert = require('assert');
const vm = require('vm');
const source = require('fs').readFileSync(process.argv[2], 'utf8');
async function run(info, wsl, alive=true) {
  const context = {require:()=>({readFileSync:()=>JSON.stringify(info)}), process:{kill:()=>{if(!alive)throw Error('dead');}}, child_process_1:{spawn:(_cmd,args)=>args}, Date};
  vm.createContext(context);vm.runInContext(source, context);
  return Array.from(await context.startLanguageServer(1,'csrf',{wsl}));
}
(async()=>{
  const fresh={pid:123,url:'http://127.0.0.1:4000',ts:Date.now()};
  const redirected=await run(fresh,false);
  for(const flag of ['--api_server_url','--cloud_code_endpoint','--inference_api_server_url'])assert.equal(redirected[redirected.indexOf(flag)+1],fresh.url);
  for(const [info,wsl,alive] of [[{...fresh,ts:0},false,true],[fresh,true,true],[fresh,false,false],[{...fresh,url:'http://evil.example'},false,true]]) {
    const args=await run(info,wsl,alive);assert.equal(args[1],'https://original-api');assert.equal(args.length,4);
  }
})().catch(e=>{console.error(e);process.exitCode=1;});
"#;
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("patched.js");
        let test = dir.path().join("harness.cjs");
        fs::write(&js, patched).unwrap();
        fs::write(&test, harness).unwrap();
        let result = std::process::Command::new("node")
            .arg(test)
            .arg(js)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
