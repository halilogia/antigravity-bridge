# Antigravity Model Bridge — Kullanım

Windows üzerinde yerel Rust köprüsü. Google modellerinin trafiğini aktarır; eklediğiniz modelleri Antigravity kataloğuna ayrı seçenekler olarak ekler. OpenCode Go ve Zen anahtarları birbirinden bağımsız kaydedilir.

## Hızlı kurulum

1. Antigravity'deki çalışmanızı kaydedin; Antigravity ve `language_server.exe` süreçlerini kapatın. Kurucu süreçleri zorla kapatmaz.
2. Bu klasörde `KUR.cmd` dosyasını çalıştırın. Uyum kontrolü, yedekleme ve yama otomatik yapılır.
3. `BASLAT.cmd` dosyasını çalıştırın ve pencereyi açık tutun. Varsayılan port `4000`'dir.
4. `http://127.0.0.1:4000/ui` adresini açın. Konsolda gösterilen yönetim token'ını girip **Bağlan** düğmesine basın.
5. Go veya Zen planınızı seçin, API anahtarını girip **Bağlantıyı kaydet** düğmesine basın. Anahtar bu adımda DPAPI ile saklanır; model eklemek gerekmez.
6. **Model listesini getir** ile sağlayıcı kataloğunu alın; modelleri seçip **Seçilen modelleri ekle** düğmesine basın. Katalog alma gerçek bir ağ isteğidir; kurulum kendiliğinden çıkarım isteği göndermez.
7. Antigravity'yi başlatın. Model seçicide eklenen modelleri seçin. Açık Antigravity yeni kataloğu almıyorsa yeniden başlatın.

Yönetim ekranı köprünün yerel web sayfasıdır; Antigravity Settings içine gömülü bir form değildir. Gerçek sağlayıcıyla ilk mesaj hesabınızın kullanım hakkını tüketebilir.

Varsayılan kurulum hedefi `%LOCALAPPDATA%\Programs\Antigravity` içindeki Desktop uygulamasıdır. `Antigravity IDE` farklı bir üründür; bu yama onun için hazırlanmadı. Farklı Desktop konumu:

```powershell
.\KUR.cmd --install-dir "D:\Apps\Antigravity"
```

## Geri alma

Antigravity ve dil sunucusunu kapatıp `GERI_AL.cmd` çalıştırın. Alternatif:

```powershell
.\target\release\antigravity-bridge.exe restore
```

Yedekler kurulum kökündeki `.antigravity-rust-bridge` klasöründedir. Geri yükleme sonrasında `.antigravity-rust-bridge.restored-<kimlik>` olarak korunur. Kullanıcı yapılandırmanız ve API anahtarlarınız silinmez.

Yama `app.asar` içindeki `dist/languageServer.js` dosyasına bir başlangıç kancası ekler. `--api_server_url`, `--cloud_code_endpoint`, `--inference_api_server_url` argümanları köprüye yönlendirilir. `language_server.exe` ikilisi değiştirilmez; yedeklenir ve SHA-256 ile takip edilir. ASAR'daki diğer dosyalar ve `.unpacked` içeriği değiştirilmez.

Arşiv, dil sunucusu veya yedek dosya hash'i beklenenden farklıysa geri yükleme reddedilir; yeni uygulama sürümü eski dosyalarla ezilmez. Antigravity güncellemesinden **önce** geri alın, güncel sürümde tekrar `patch --check` çalıştırın. Eski TypeScript yaması algılanırsa önce kendi kurucusuyla geri alın. Kesintiden kalan `operation.lock` varsa, çalışan bir patch/restore olmadığını doğrulamadan dosyayı kaldırmayın. Başarısız kısmi yedekler otomatik ezilmez.

Köprü kapanmışsa veya nabız dosyası 15 saniyeden eskiyse, **yeni başlayan** Antigravity dil sunucusu özgün Google adreslerini kullanır. Çalışan bir dil sunucusunun argümanları sonradan değişmez: köprüyü o oturum boyunca açık tutun; köprüyü kapattıysanız Antigravity'yi yeniden başlatın. WSL dil sunucuları yönlendirilmez.

## Komut satırı

```powershell
$bridge = '.\target\release\antigravity-bridge.exe'
& $bridge init
& $bridge opencode go
& $bridge key opencode-go
& $bridge model add go-kimi --provider opencode-go --upstream-model kimi-k3
& $bridge check
& $bridge models
& $bridge patch --check
& $bridge serve
```

`key` anahtarı konsolda gizleyerek sorar; komut satırına anahtar yazmayın. DPAPI Windows kullanıcı hesabına bağlıdır; TPM/donanım koruması garantisi değildir. Farklı kullanıcı/makineye taşımada anahtarı yeniden girmeniz gerekebilir. `--api-key` eski düz metin seçeneğidir; önerilmez. Ortam değişkeni tanımlandıysa önceliği şifreli anahtardan yüksektir.

Veriler: `%USERPROFILE%\.gemini\antigravity\antigravity-bridge\`. `--config "C:\...\config.json"` veya `AGY_BRIDGE_CONFIG` ile değiştirilebilir. Patch ve serve aynı yapılandırmayı kullanmalıdır. Portu `config.json` içindeki `listen` alanından değiştirdikten sonra geri alıp yeniden yama uygulayın. Yama yalnızca `127.0.0.1` ve sabit, sıfırdan farklı port destekler.

Sağlayıcı kökü `https://opencode.ai/inference`, türü `opencode`, planı `go` veya `zen` olmalıdır. Kökün sonuna `/openai/v1` eklemeyin. Model kaydındaki `protocol` alanı `auto`, `chat`, `responses`, `messages` olabilir. Otomatik eşleme planı dikkate alır; sağlayıcı yeni bir protokol kullandığında açık seçim yapabilirsiniz.

Varsayılan reasoning chat için `max`, Responses için API'ye uygun `high`, Messages için çıktı sınırından küçük düşünme bütçesidir. `[1210]` düşünme zorunluluğu hatasında HTTP 400 veya SSE başlangıcında en fazla bir kez düşük düşünme ayarıyla tekrar denenir. İçerik başladıktan sonra tekrar gönderim yapılmaz.

`sessionId`/`conversationId` alanları (`snake_case` karşılıkları da kabul edilir) mevcutsa aynı konuşmada kararlı `x-opencode-session` kullanılır. Sağlayıcının döndürdüğü `session_id` en fazla 256 kayıtlık bellek önbelleğinde takip edilir; anahtar, sağlayıcı ve model arasında ayrılır. Konuşma kimliği olmayan istekler bağımsız oturum alır; bu durumda konuşmalar arası önbellek indirimi garanti edilmez.

OpenCode'un Google-native Gemini uçları bu sürümün özel sağlayıcı adaptöründe desteklenmez; `auto` seçiminde açık hata verilir. Antigravity'nin kendi Google modelleri mevcut passthrough akışını kullanır.

## Derleme ve doğrulama

Rust ve Windows MSVC derleme araçları kurulu olmalıdır. Hazır release ikilisi varsa çalıştırmak için Rust veya Node gerekmez.

```powershell
cargo test --all
cargo clippy --all-targets
cargo build --release
```

Çıktı: `target\release\antigravity-bridge.exe`. Node varsa ek JavaScript kontrolleri:

```powershell
cargo test --lib patch::tests -- --include-ignored
node --check src/ui/app.js
```

Ek testlerden biri mevcut Desktop arşivini salt okunur açar, yamayı geçici kopyada üretir ve JavaScript sözdizimini kontrol eder. Diğeri sahte süreç/nabız bilgisiyle üç bayrağın yönlendirilmesini, eski nabız, ölü süreç ve WSL durumunda özgün adreslerin korunmasını çalıştırarak sınar.

Testler yerel sahte HTTP/SSE sunucuları kullanır. Test başarısı canlı OpenCode üretimi, gerçek Google hesabıyla uçtan uca geçiş veya Antigravity model seçicisinin görsel kabulü anlamına gelmez. Bu sürümün teslim doğrulaması `VALIDATION.md` dosyasındadır.

## Kaynaklar

Davranış referansı: `../_ref` içindeki vahapogut/antigravity-add-model türevi. Rust taşıması, depo içindeki mimari keşifleri ve mevcut çeviri motorunu temel alır; referans kaynakların lisansları kendilerinde korunur.

Uç noktalar: [OpenCode Go](https://opencode.ai/docs/console/go/) ve [OpenCode model uçları](https://opencode.ai/docs/zen/). Model/protokol listeleri sağlayıcı tarafından değiştirilebilir.
