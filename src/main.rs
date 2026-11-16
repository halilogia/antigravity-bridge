use antigravity_bridge::{
    catalog::Catalog,
    config::{ConfigStore, Model, Provider},
    server::{self, AppState},
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    name = "antigravity-bridge",
    version,
    about = "Antigravity için yerel model köprüsü"
)]
struct Cli {
    /// config.json yolu (varsayılan: ~/.gemini/antigravity/antigravity-bridge/config.json)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Örnek yapılandırma yazar (var olanı ezmez)
    Init,
    /// Yapılandırmayı doğrular
    Check,
    /// Tanımlı modelleri ve slotlarını listeler
    Models,
    /// Ağ geçidini başlatır
    Serve,
    /// Yönetim token'ını yazdırır; yoksa üretip config.json'a kaydeder
    AdminToken,
    /// Patch the installed Desktop archive; --check only inspects compatibility.
    Patch {
        #[arg(long)]
        install_dir: Option<PathBuf>,
        #[arg(long)]
        check: bool,
    },
    /// Restore a hash-verified original archive.
    Restore {
        #[arg(long)]
        install_dir: Option<PathBuf>,
    },
    /// Save a provider key using Windows DPAPI (hidden console input).
    Key { name: String },
    /// Create a Go or Zen provider without making an API request.
    Opencode {
        #[arg(value_parser = ["go", "zen"])]
        plan: String,
    },
    #[command(subcommand)]
    Provider(ProviderCmd),
    #[command(subcommand)]
    Model(ModelCmd),
}

#[derive(Subcommand)]
enum ProviderCmd {
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        /// Anahtarın okunacağı ortam değişkeni (önerilen)
        #[arg(long)]
        api_key_env: Option<String>,
        /// Düz metin anahtar (config.json içinde saklanır)
        #[arg(long)]
        api_key: Option<String>,
    },
    Rm {
        name: String,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    Add {
        id: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        upstream_model: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long, default_value_t = 128_000)]
        context_window: u32,
        #[arg(long, default_value_t = 8_192)]
        max_output_tokens: u32,
        #[arg(long)]
        vision: bool,
        #[arg(long)]
        thinking: bool,
    },
    Rm {
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "antigravity_bridge=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let store = ConfigStore::new(cli.config)?;

    match cli.cmd {
        Cmd::Patch { install_dir, check } => {
            let cfg = store.load()?;
            let addr = cfg.listen_addr()?;
            let heartbeat =
                std::path::absolute(antigravity_bridge::heartbeat::default_path(store.dir()))?;
            let install = install_dir
                .map(Ok)
                .unwrap_or_else(antigravity_bridge::patch::default_install)?;
            println!(
                "{}",
                antigravity_bridge::patch::patch(
                    &install,
                    &heartbeat,
                    &format!("http://{addr}"),
                    check
                )?
            );
        }
        Cmd::Restore { install_dir } => {
            let install = install_dir
                .map(Ok)
                .unwrap_or_else(antigravity_bridge::patch::default_install)?;
            println!("{}", antigravity_bridge::patch::restore(&install)?);
        }
        Cmd::Key { name } => {
            let mut cfg = store.load()?;
            let provider = cfg.providers.get_mut(&name).context("provider not found")?;
            let key = antigravity_bridge::secrets::read_secret_line("API key: ")?;
            store.secrets().set(&name, &key)?;
            provider.api_key = None;
            store.save(&cfg)?;
            println!("Key saved with Windows DPAPI: {name}");
        }
        Cmd::Opencode { plan } => {
            let plan = antigravity_bridge::opencode::Plan::parse(&plan).context("invalid plan")?;
            let mut cfg = store.load()?;
            cfg.providers
                .entry(plan.provider_name().into())
                .or_insert_with(|| antigravity_bridge::opencode::preset_provider(plan));
            store.save(&cfg)?;
            println!("Provider ready: {}", plan.provider_name());
        }
        Cmd::Init => {
            if store.init()? {
                println!("yazıldı: {}", store.path.display());
            } else {
                println!("zaten var, dokunulmadı: {}", store.path.display());
            }
        }
        Cmd::Check => {
            let cfg = store.load()?;
            println!(
                "tamam: {} sağlayıcı, {} model",
                cfg.providers.len(),
                cfg.models.len()
            );
            for (name, p) in &cfg.providers {
                let has_key = p.resolve_key().is_some();
                println!(
                    "  {name}: {} (anahtar: {})",
                    p.base_url,
                    if has_key { "var" } else { "yok" }
                );
            }
        }
        Cmd::Models => {
            let cfg = store.load()?;
            let catalog = Catalog::build(&cfg, &store.dir().join("identities.json"))?;
            for m in catalog.models() {
                println!(
                    "MODEL_PLACEHOLDER_M{:<4} {} → {}/{}",
                    catalog.slot(&m.id).unwrap_or(0),
                    m.label(),
                    m.provider,
                    m.upstream_model
                );
            }
        }
        Cmd::AdminToken => {
            let mut cfg = store.load()?;
            if cfg.admin_token.is_none() {
                cfg.admin_token = Some(antigravity_bridge::config::generate_token()?);
                store.save(&cfg)?;
            }
            println!("{}", cfg.admin_token.unwrap_or_default());
        }
        Cmd::Serve => {
            let cfg = store.load()?;
            let addr = cfg.listen_addr()?;
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("{addr} dinlenemedi"))?;
            let _heartbeat = antigravity_bridge::heartbeat::Heartbeat::start(
                antigravity_bridge::heartbeat::default_path(store.dir()),
                format!("http://{}", listener.local_addr()?),
            )?;
            let state = Arc::new(AppState::new(store, cfg)?);
            let live = state.live();
            tracing::info!(
                "dinleniyor: http://{addr} ({} model, yönetim API'si: {})",
                live.catalog.models().len(),
                if live.config.admin_token.is_some() {
                    "açık"
                } else {
                    "kapalı"
                }
            );
            server::serve(state, listener).await?;
        }
        Cmd::Provider(ProviderCmd::Add {
            name,
            base_url,
            api_key_env,
            api_key,
        }) => {
            let mut cfg = store.load()?;
            if api_key.is_some() {
                eprintln!("uyarı: --api-key düz metin saklanır; --api-key-env tercih edin");
            }
            cfg.providers.insert(
                name.clone(),
                Provider {
                    base_url,
                    api_key_env,
                    api_key,
                    ..Provider::default()
                },
            );
            store.save(&cfg)?;
            println!("sağlayıcı kaydedildi: {name}");
        }
        Cmd::Provider(ProviderCmd::Rm { name }) => {
            let mut cfg = store.load()?;
            if let Some(m) = cfg.models.iter().find(|m| m.provider == name) {
                bail!("'{name}' hâlâ '{}' modeli tarafından kullanılıyor", m.id);
            }
            if cfg.providers.remove(&name).is_none() {
                bail!("sağlayıcı yok: {name}");
            }
            store.save(&cfg)?;
        }
        Cmd::Model(ModelCmd::Add {
            id,
            provider,
            upstream_model,
            display_name,
            context_window,
            max_output_tokens,
            vision,
            thinking,
        }) => {
            let mut cfg = store.load()?;
            if cfg.model(&id).is_some() {
                bail!("model zaten var: {id}");
            }
            cfg.models.push(Model {
                id: id.clone(),
                display_name,
                provider,
                upstream_model,
                context_window,
                max_output_tokens,
                supports_vision: vision,
                supports_thinking: thinking,
                reasoning_effort: None,
                protocol: Default::default(),
            });
            store.save(&cfg)?;
            let catalog = Catalog::build(&cfg, &store.dir().join("identities.json"))?;
            println!(
                "model eklendi: {id} (MODEL_PLACEHOLDER_M{})",
                catalog.slot(&id).unwrap_or(0)
            );
        }
        Cmd::Model(ModelCmd::Rm { id }) => {
            let mut cfg = store.load()?;
            let before = cfg.models.len();
            cfg.models.retain(|m| m.id != id);
            if cfg.models.len() == before {
                bail!("model yok: {id}");
            }
            store.save(&cfg)?; // slot identities.json'da korunur
            println!("model kaldırıldı: {id} (slotu başkasına verilmez)");
        }
    }
    Ok(())
}
