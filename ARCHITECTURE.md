# Antigravity Bridge (Rust) — Mimari

Antigravity'nin model seçicisine ve Settings → Models paneline kendi API anahtarlı modellerimizi ekleyen,
OpenAI uyumlu sağlayıcıları (Ollama, OpenRouter, DeepSeek, Groq …) Antigravity'nin kendi protokolüyle konuşturan yerel köprü.

Kaynak analiz: `../_ref` (vahapogut/antigravity-add-model türevi, Apache-2.0). Bu proje onun TypeScript gateway'ini
(~2500 satır) ve Electron yamasını Rust ile yeniden kurar; kod kopyalanmadı, davranış taşındı.

## 1. `_ref` bulguları

### 1.1 Antigravity modelleri nasıl görüyor
| Gözlem | Kanıt (`_ref`) | Sonuç |
|---|---|---|
| Language server (LS) Google'a Cloud Code ile konuşur: `POST /v1internal:streamGenerateContent`, gövde `{project, model, request:{contents, tools, …}}`, yanıt `{response:{candidates…}, traceId, metadata}` SSE | `proxy.ts:1405`, `customRequest.ts:82` | Köprü hem düz Gemini hem bu zarfı konuşmalı |
| Model listesi `POST /v1internal:fetchAvailableModels` ile gelir: `models{slug→girdi}` + `agentModelSorts[].groups[].modelIds` | `modelCatalog.ts:70` | **Sohbet açılır menüsü yalnızca bu kataloğa bakar**; UI yaması gerekmez |
| LS yalnızca bilinen enum değerlerini kabul eder: `MODEL_PLACEHOLDER_M0..M650` (= 1000..1650) | `modelIdentity.ts:7` | Her özel modele kalıcı slot; kaldırılan slot yeniden verilmez |
| LS'yi yönlendirmek için `--api_server_url`, `--cloud_code_endpoint`, `--inference_api_server_url` argümanları değiştirilir | `runtime-patch.mjs:195` | Tek zorunlu kanca: LS'nin başlangıç argümanları |
| Hesap katmanına göre production / daily Cloud Code kökü seçilir (`loadCodeAssist`) | `cloudCodeRouting.ts` | Yönlendirme sonrası gerçek Google trafiği aynı kökten geçirilmeli |

### 1.2 Yama mekanizması (`deploy.mjs`, `runtime-patch.mjs`)
`resources/app.asar` içinde `dist/main.js`, `dist/languageServer.js`, `dist/preload.js` üç dosyası TypeScript AST ile
düzenlenir (her kanca için "tam olarak bir eşleşme" şartı; yoksa hiçbir şey değişmez):
1. `main.js` → `app.whenReady()` öncesine JS proxy `require` edilir.
2. `languageServer.js` → `startLanguageServer` async yapılır, LS URL argümanları `http://localhost:<proxy>` ile değiştirilir.
3. `preload.js` → sona özel picker kodu eklenir (Settings → Models formu, sandbox'ta yalnız `electron` require edilebilir).

Güvenlik/geri alma: orijinal `app.asar` + `.unpacked` + `language_server.exe` SHA-256 ile yedeklenir
(`.antigravity-model-patch/state.json`), güncelleme sonrası uyuşmazlıkta geri yükleme reddedilir, symlink'ler reddedilir.
**Zayıflık:** Antigravity her güncellendiğinde AST kancaları kırılabilir; yama sürüme bağlıdır.

### 1.3 Gateway'in çözdükleri → Rust karşılıkları
| Sorun (TS) | Rust modülü |
|---|---|
| Gemini `contents/functionCall/functionResponse` → OpenAI mesajları, çağrı kimliği eşleme (`mapper.ts`) | `translate/request.rs` |
| OpenAI SSE → Gemini SSE, `<think>` ayrıştırma, reasoning alanları, parça parça `tool_calls` (`adapters/openai.ts`) | `translate/stream.rs` |
| Araç adı/parametre takma adı, tip zorlama, varsayılan, tekil nesne → dizi (`tool-normalizer.ts`, `tool-capabilities.ts`) | `tools/mod.rs`, `tools/well_known.json` |
| Bozuk/kesik araç argümanı JSON'u (`parse-tool-args.ts` yalnızca `{}` döndürüyordu) | `tools/repair.rs` |
| Katalog birleştirme + slot ataması (`modelCatalog.ts`, `modelIdentity.ts`) | `catalog.rs` |

## 2. Rust yapısı
```
antigravity-bridge/
├─ src/
│  ├─ config.rs        config.json oku/doğrula/atomik yaz; loopback + düz-http-anahtar kuralları
│  ├─ catalog.rs       slot ataması (identities.json), seçici çözümleme, fetchAvailableModels birleştirme
│  ├─ gemini.rs        Gemini/Cloud Code istek tipleri + zarf
│  ├─ translate/       request.rs (Gemini→OpenAI), stream.rs (SSE→Gemini, aggregate)
│  ├─ tools/           registry+normalizer, repair, well_known.json
│  ├─ server.rs        axum yönlendirici, AppState (sıcak yeniden yüklenen Live{config,catalog}), özel model üretimi, SSE pompası
│  ├─ google.rs        özel olmayan trafiğin Google'a aktarımı, fetchAvailableModels / v1beta/models birleştirme
│  ├─ routing.rs       hesap katmanı (loadCodeAssist) → production/daily kök seçimi, token-özeti önbelleği
│  ├─ admin.rs         /admin/* yönetim API'si + koruma katmanı
│  └─ main.rs          CLI: init | check | models | serve | admin-token | provider add/rm | model add/rm
└─ tests/e2e.rs        sahte OpenAI sağlayıcı + sahte Google (production/daily) + gerçek HTTP/SSE
```
Veri dizini: `~/.gemini/antigravity/antigravity-bridge/` (`config.json`, `identities.json`; `--config` / `AGY_BRIDGE_CONFIG` ile değişir).

### Akış (şu an çalışan kısım)
```
LS ──POST /v1internal:streamGenerateContent {model:"MODEL_PLACEHOLDER_M424", request:{…}}──▶ köprü
      catalog.resolve("MODEL_PLACEHOLDER_M424") → deepseek-chat @ provider "deepseek"
      to_openai(): contents→messages, functionDeclarations→tools (tip adları küçük harf)
köprü ──POST {base_url}/chat/completions {stream:true}──▶ sağlayıcı
sağlayıcı ──SSE──▶ SseParser → Translator.push() → {"response":{"candidates":[…]}} SSE ──▶ LS
```
Örnek (e2e testinde doğrulandı): model `runCommand` + `{"command":"dir"` (kapanmamış JSON) üretir →
LS'ye `functionCall{name:"run_command", args:{"CommandLine":"dir"}, id:"c1"}` gider.

### Google geçişi (Faz 2)
`catalog.resolve(model)` boşsa (Google'ın kendi modeli) istek `google::passthrough`'a düşer:
```
LS ──POST /v1internal:streamGenerateContent {model:"gemini-pro"} Authorization: Bearer T──▶ köprü
      routing: T'nin SHA-256'sı önbellekte yok → köprü ──POST {production}/v1internal:loadCodeAssist──▶ Google
               paidTier.usesGcpTos=false → daily, true → production (1 saat TTL, en fazla 128 hesap)
köprü ──aynı gövde bayt bayt + aynı başlıklar (Host/hop-by-hop/x-admin-token hariç)──▶ {daily|production}/v1internal:…
Google ──SSE──▶ gövde tamponlanmadan boru ile LS'ye
```
- Üretim dışı Cloud Code çağrıları (`loadCodeAssist`, kota, onboarding…) tamponlanır (`accept-encoding: identity`, 16 MB sınır);
  `loadCodeAssist` yanıtı katmanı öğretir, gövdedeki Google kökleri köprü adresiyle değiştirilir. Cloud Code dışı yollar `generative` köküne gider.
- Hesap katmanı öğrenilemezse üretim **502** döner; istek yanlış köke gönderilmez. Token önbellekte düz saklanmaz, yalnız SHA-256 özeti.
- Google'ın hataları (401/403 dahil) olduğu gibi döner; LS oturumunu yenileyebilir. Yalnız *özel sağlayıcı* 401/403'ü 502'ye çevrilir.
- **Katalog:** `fetchAvailableModels` Google'dan alınır (hesabın köküne), `models{slug→bilgi}` / dizi biçimine ve `agentModelSorts[].groups[].modelIds`'e özel modeller eklenir.
  Google erişilemezse yalnız özel modellerle yanıt verilir. Etiketler Google'daki ve birbirleri arasındaki etiketlerden benzersizleştirilir
  (`DeepSeek` zaten varsa `DeepSeek (deepseek)`). Google'ın gözlenen `MODEL_PLACEHOLDER_Mn` slotları `identities.json`'a `reserved` yazılır;
  çakışan özel modele yeni slot verilir, Google'ınki dokunulmaz.
- Hot reload: `AppState::mutate` (seri, doğrula → katalog kur → atomik yaz → `Arc` değiştir) ve `/admin/reload`. Google kökleri değişirse yeniden başlatma gerekir.

### Yönetim API'si (`/admin`)
`GET /status`, `GET /config` (gizli değersiz), `POST /reload`, `PUT|DELETE /providers/{ad}`, `PUT /providers/{ad}/key` (`{"api_key":null}` siler),
`POST /models`, `DELETE /models/{id}`. Sağlayıcı güncellemede verilmeyen anahtar korunur; kullanımdaki sağlayıcı silinemez (409); aynı model yeniden eklenince aynı slotu alır.

## 3. Kararlar
- **Çevirmen durum makinesi, kanal pompası:** `Translator` saf (I/O yok) → birim testlenebilir; `pump` bayt akışını okuyup `mpsc` ile
  gövdeye verir. İstemci kopunca `send` hata verir ve upstream bağlantısı bırakılır. Akışsız istek aynı yolu kullanır, `aggregate` birleştirir.
- **Upstream her zaman akışlı** (`stream_options.include_usage`): tek kod yolu, `usageMetadata` son parçada gelir.
- **Araç çağrıları `finish_reason`'da toplu yayınlanır:** argüman parçaları birleşip onarılmadan normalize edilemez.
- **Yetkili şema = istekteki `functionDeclarations`;** `well_known.json` yalnızca takma ad/varsayılan ekler. İstekte bildirilmeyen araç dokunulmadan geçer.
- **Sağlayıcı 401/403 → 502:** 401 olarak dönerse Antigravity Google oturumunu yenilemeye çalışabilir.
- **UI için en temiz yol:** sohbet menüsü = katalog (yama yok). Settings → Models formu = preload'a gömülü küçük JS, Electron IPC yerine
  köprünün loopback `/admin` API'sini çağırır (Rust binary içine `include_str!`). Böylece 126 KB'lık `customPreload.ts` ve Node IPC katmanı gerekmez.

## 4. Güvenlik
- `listen` yalnızca loopback; doğrulamada reddedilir.
- Anahtarlar: `api_key_env` (önerilen) veya düz metin `api_key`; loglanmaz, hata mesajlarına girmez. Loopback dışına düz `http` + anahtar reddedilir.
- `/admin/*` dört katmanlı: (1) `admin_token` yoksa API kapalı (403); (2) `Host` loopback olmalı (DNS rebinding); (3) `Origin` başlığı olan istek reddedilir (tarayıcı/CSRF);
  (4) `Authorization: Bearer` veya `x-admin-token`, sabit zamanlı karşılaştırma. `gateway_token` yönetim yetkisi vermez. Yanıtlar anahtar/token içermez (`hasKey`, `keySource`).
  Token `antigravity-bridge admin-token` ile üretilir/okunur; `init` de üretir. Preload JS'i token'ı dosyadan okuyup yalnız loopback'e göndermeli.
- `/v1beta/*` için isteğe bağlı `gateway_token` (yalnız özel modelleri korur; Google'a aktarılan istekler kendi kimlik bilgisini taşır). **`/v1internal:*` token istemez** (LS Google bearer'ı gönderir, kendi tokenımızı bilmez):
  yerelde çalışan başka bir süreç bu uçtan model anahtarlarını kullanabilir. Faz 4'te yama LS argümanına gizli bir yol öneki ekleyerek kapatılacak.
- Köprü, Google'a gelen `Authorization`'ı olduğu gibi iletir ve saklamaz; yalnız SHA-256 özeti yönlendirme önbelleğinde tutulur. İsteğin mutlak URI'si hedefi belirleyemez (yalnız yol+sorgu kullanılır).
- İstek/yanıt gövdeleri loglanmaz; upstream hata gövdesi 2 KB ile sınırlı; istek gövdesi 64 MB üst sınır.

## 5. Yol haritası
- [x] **Faz 0** — `_ref` analizi, bu belge
- [x] **Faz 1** — config + katalog + çeviri + normalizer + gateway + CLI (37 test: 34 birim, 3 e2e)
- [x] **Faz 2** — Google geçişi (başlık/yetki aktarımı, SSE borusu, `loadCodeAssist` + production/daily seçimi), katalog birleştirme (slot çakışma çözümü dahil),
      `/admin` API (CRUD, anahtar, durum, reload; 4 katmanlı koruma). 63 test (43 birim, 20 e2e), clippy temiz.
- [ ] **Faz 3 — Yönetim genişletmeleri:** `GET {base}/models` ile canlı katalog keşfi, anahtarı Windows DPAPI ile saklama, Google köklerinin sıcak yeniden yüklenmesi, `countTokens`
- [ ] **Faz 4 — Yama aracı (Rust, `patch` alt komutu):** `app.asar` okuma/yazma, `languageServer.js` argüman kancası, preload enjeksiyonu, SHA-256 yedek/geri alma, `--check`
      (ref'teki `deploy.mjs` davranışı; Antigravity kapalıyken). Sürüm başına kanca eşleşme testi.
- [ ] **Faz 5 — Sağlayıcı genişletmeleri:** Anthropic Messages, Responses API, `reasoning_effort` eşlemesi, token tahmini/`countTokens`, rate-limit + failover

## 6. Test stratejisi ve doğrulama durumu
- Birim: çeviri, SSE ayrıştırma (bölünmüş bayt/CRLF/çok satır), `<think>` sınırları, JSON onarımı, normalizer, slot kalıcılığı, katalog birleştirme.
- Birim (Faz 2): hesap yönlendirme (önbellek, TTL, sınır, katı Bearer ayrıştırma), slot ayırma/çakışma.
- e2e (`tests/e2e.rs`): sahte OpenAI upstream + sahte Google (production ve daily ayrı sunucu). Özel model: Cloud Code zarfı, takma ad + onarım, kimlik doğrulama,
  401→502, akışsız birleştirme, Google'a hiç gidilmemesi. Passthrough: bayt bayt gövde + başlıklar, `x-admin-token` sızmaması, free→daily / GCP-ToS→production,
  önbellek ve hesap izolasyonu, katman araması başarısızsa 502 (yanlış köke gönderim yok), `loadCodeAssist` kök yeniden yazımı, artımlı SSE (kapılı sahte akış),
  Google 401'in aynen dönmesi, Google erişilemezken yalnız özel katalog. Katalog: birleştirme, etiket benzersizliği, slot çakışması, `/v1beta/models`.
  Admin: kapalı/yanlış token/Origin/Host, gizli değer sızıntısı yok, CRUD + sıcak yükleme + kalıcılık + slot kararlılığı, doğrulama hatasında durum değişmez, reload.
- **Doğrulanmadı:** gerçek bir sağlayıcıyla canlı üretim; Antigravity'nin gerçek `v1internal` yükü (örn. `thoughtSignature`, görsel/dosya `fileData`, `countTokens`);
  gerçek Google ile geçiş (testler yerel sahte sunucularla; gerçek `loadCodeAssist` alan adları `_ref` kaynaklarından alındı); gzip/br yanıtlar boru ile sıkıştırılmış
  hâlde aynen iletilir ama bu yol gerçek Google'a karşı denenmedi; Antigravity'ye yama uygulanmadı (Faz 4).

## 7. Rust takeover delivery (2026-11-16)

This section supersedes the earlier implementation-status checklist; the discovery notes above are preserved.

- OpenCode Go/Zen roots and catalog paths are separate. Request-time DPAPI key resolution feeds chat, Responses and Messages transports. Automatic protocol selection is plan-aware and can be overridden in model configuration.
- Chat defaults to maximum reasoning; Responses maps maximum to high; Messages uses a bounded thinking budget. HTTP 400 and pre-content SSE thinking-required errors allow one retry across both paths. No retry occurs after content/tool output starts.
- Stable conversation IDs feed the OpenCode session header. Returned session IDs are captured in a bounded in-memory cache scoped by upstream root, model, credential digest and conversation. Missing conversation IDs are never inferred from prompt text.
- `patch --check`, `patch`, and `restore` support the inspected Desktop startup layout using `lsDefaults.js` and `spawnArgs`. The archive is rebuilt in Rust without extracting arbitrary paths. The original and patched SHA-256 values, original LS hash and immutable backups form a recoverable transaction. Updates, foreign patches, reparse points and unrecognized startup layouts are refused.
- The LS executable is not binary-patched: its three upstream arguments are changed in its JavaScript launcher. Both original files are backed up. The fresh heartbeat condition preserves native endpoints when starting without the bridge; WSL is excluded.
- `/ui` provides local OpenCode key storage, catalog selection, model addition/removal. It is a separate local web UI, not an injected native Settings panel. `BASLAT.cmd`, `KUR.cmd`, `GERI_AL.cmd` and `KULLANIM.md` provide the Windows workflow.
- The unused `replay.rs` reservation remains outside this delivery; provider-specific reasoning-signature replay and Google-native OpenCode inference are not implemented. Native Google passthrough remains separate. Live provider generation and native picker acceptance require separate verification.

See `VALIDATION.md` for exact verification commands and boundaries. No provider credentials were copied from the reference project and no paid inference calls were made.
