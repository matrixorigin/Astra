use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "astra-gateway", about = "Chat platform gateway for Astra")]
struct Cli {
    #[arg(long, default_value = "gateway.yaml")]
    config: PathBuf,
    /// Override database URL (also: GATEWAY_DATABASE_URL env var)
    #[arg(long)]
    database_url: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// QR code login for WeChat (iLink Bot API)
    #[command(name = "login-weixin")]
    LoginWeixin,
}

#[tokio::main]
async fn main() {
    // Load .env file if present (before logging init so RUST_LOG works)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,astra_gateway=debug".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    let pid_file = std::path::PathBuf::from("/tmp/astra-gateway.pid");
    if pid_file.exists()
        && let Ok(old_pid) = std::fs::read_to_string(&pid_file)
    {
        let old_pid = old_pid.trim();
        let cmdline_path = format!("/proc/{old_pid}/cmdline");
        if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path)
            && cmdline.contains("astra-gateway")
        {
            tracing::warn!(pid = old_pid, "killing stale gateway process");
            let _ = std::process::Command::new("kill").arg(old_pid).status();
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    std::fs::write(&pid_file, std::process::id().to_string()).ok();

    if let Some(Command::LoginWeixin) = cli.command {
        match astra_gateway::platforms::weixin::qr_login().await {
            Ok((token, account_id)) => {
                // Save to store if config is loadable
                let db_saved = if cli.config.exists() {
                    if let Ok(cfg) = astra_gateway::config::GatewayConfig::load(&cli.config) {
                        let storage_config = cfg.resolve_storage();
                        match astra_gateway::store::open_store(&storage_config).await {
                            Ok(Some(store)) => {
                                let creds = serde_json::json!({
                                    "token": token,
                                    "account_id": account_id,
                                });
                                match store
                                    .save_credential("weixin", "default", "bot_token", &creds, None)
                                    .await
                                {
                                    Ok(()) => {
                                        println!("✅ 凭证已保存到存储 (换机器无需重新扫码)");
                                        true
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "store save failed, falling back to config file");
                                        false
                                    }
                                }
                            }
                            _ => false,
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !db_saved {
                    // Fallback: write to yaml
                    if cli.config.exists() {
                        if let Ok(content) = std::fs::read_to_string(&cli.config) {
                            use regex::Regex;
                            let token_re = Regex::new(r#"(?m)(    token: )"[^"]*""#).unwrap();
                            let account_re =
                                Regex::new(r#"(?m)(    account_id: )"[^"]*""#).unwrap();
                            let patched = token_re
                                .replace(&content, &format!("${{1}}\"{token}\""))
                                .to_string();
                            let patched = account_re
                                .replace(&patched, &format!("${{1}}\"{account_id}\""))
                                .to_string();
                            if patched != content {
                                std::fs::write(&cli.config, &patched).ok();
                                println!("✅ 已自动写入 {}", cli.config.display());
                            }
                        }
                    } else {
                        println!("将以下内容写入 gateway.yaml:");
                        println!();
                        println!("platforms:");
                        println!("  weixin:");
                        println!("    enabled: true");
                        println!("    token: \"{token}\"");
                        println!("    account_id: \"{account_id}\"");
                    }
                }
                println!();
                println!("现在可以运行: make gateway");
            }
            Err(e) => {
                tracing::error!(error = %e, "WeChat login failed");
                std::process::exit(1);
            }
        }
        return;
    }

    let mut config = match astra_gateway::config::GatewayConfig::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %cli.config.display(), error = %e, "config load failed");
            std::process::exit(1);
        }
    };

    // CLI flag overrides config file
    if let Some(ref db_url) = cli.database_url {
        config.storage = astra_gateway::store::StorageConfig::Mysql {
            url: db_url.clone(),
        };
    }

    let mut runner = match astra_gateway::runner::GatewayRunner::new(config.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "runner init failed");
            std::process::exit(1);
        }
    };
    let scheduler_config = config.clone();

    let mut adapters: Vec<Box<dyn astra_gateway::platforms::PlatformAdapter>> = Vec::new();

    if let Some(wecom_cfg) = config.platforms.wecom
        && wecom_cfg.enabled
    {
        adapters.push(Box::new(
            astra_gateway::platforms::wecom::WeComAdapter::new(wecom_cfg),
        ));
    }

    if let Some(weixin_cfg) = config.platforms.weixin
        && weixin_cfg.enabled
    {
        let mut adapter = astra_gateway::platforms::weixin::WeixinAdapter::new(weixin_cfg);
        if let Some(store) = runner.store() {
            adapter = adapter.with_store(store);
        }
        adapters.push(Box::new(adapter));
    }

    if adapters.is_empty() {
        tracing::error!("no platforms enabled");
        std::process::exit(1);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let (cron_tx, cron_rx) = tokio::sync::mpsc::channel(64);
    runner.set_outbound_tx(cron_tx.clone());

    // Start cron scheduler (only if store + trace_repo available)
    if let (Some(store), Some(trace_repo)) = (runner.store(), runner.trace_repo()) {
        let scheduler = astra_gateway::scheduler::CronScheduler::new(
            store,
            scheduler_config,
            trace_repo,
            cron_tx,
        );
        let _scheduler_handle = scheduler.spawn(shutdown_tx.subscribe());
    } else {
        tracing::info!("cron scheduler disabled (no store or trace_repo)");
    }

    // Ctrl+C
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("shutting down");
        let _ = shutdown_tx_clone.send(());
    });

    let runner = std::sync::Arc::new(runner);
    runner.run(adapters, cron_rx, shutdown_rx).await;
}
