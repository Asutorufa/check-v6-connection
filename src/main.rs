use std::{path::PathBuf, sync::Arc};

use openwrt_ipv6_watch::{
    client::SystemOpenWrtClient,
    config::Config,
    monitor::{Monitor, MonitorEvent},
    telegram::TelegramBot,
};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/openwrt-ipv6-watch/config.toml"));
    let config = Config::from_file(path)?;
    let client = Arc::new(SystemOpenWrtClient::new(config.openwrt.clone())?);
    if config.openwrt.install_wrapper_on_startup {
        let install_result = client.install_wrapper().await?;
        if !install_result.success {
            return Err(format!(
                "failed to install OpenWrt wrapper remotely: exit_code={:?} stderr={}",
                install_result.exit_code, install_result.stderr
            )
            .into());
        }
    }
    let monitor = Arc::new(Monitor::new(client, config.monitor.clone()));
    let bot = Arc::new(TelegramBot::from_env(&config.telegram)?);

    let monitor_task = {
        let monitor = Arc::clone(&monitor);
        let bot = Arc::clone(&bot);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(monitor.interval());
            loop {
                ticker.tick().await;
                match monitor.tick().await {
                    MonitorEvent::Healthy(probe) => info!(
                        success = probe.success,
                        exit_code = ?probe.exit_code,
                        duration_ms = probe.duration.as_millis() as u64,
                        "remote probe completed"
                    ),
                    MonitorEvent::Failed {
                        consecutive_failures,
                        error,
                        probe,
                    } => {
                        warn!(
                            consecutive_failures,
                            ?error,
                            ?probe,
                            "remote IPv6 probe failed"
                        )
                    }
                }
                // Telegram notifications can be added here without ever changing the
                // fixed SSH command path. Keep the bot alive independently below.
                let _ = &bot;
            }
        })
    };
    let telegram_task = {
        let monitor = Arc::clone(&monitor);
        let bot = Arc::clone(&bot);
        tokio::spawn(async move { bot.run(monitor).await })
    };

    tokio::select! {
        result = monitor_task => {
            if let Err(error) = result { error!(%error, "monitor task stopped"); }
        }
        result = telegram_task => {
            if let Err(error) = result { error!(%error, "Telegram task stopped"); }
        }
    }
    Ok(())
}
