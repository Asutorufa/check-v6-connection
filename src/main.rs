use std::{path::PathBuf, sync::Arc};

use openwrt_ipv6_watch::{
    client::{ProbeResult, SystemOpenWrtClient, WatchClient},
    config::Config,
    local_probe::LocalProbeClient,
    monitor::{Monitor, MonitorEvent},
    telegram::TelegramBot,
};
use tracing::{error, info, warn};

fn failure_detail(error: Option<&String>, probe: Option<&ProbeResult>) -> String {
    let detail = error
        .map(String::as_str)
        .or_else(|| probe.map(|probe| probe.stderr.trim()))
        .filter(|detail| !detail.is_empty())
        .unwrap_or("无详细错误");
    detail.chars().take(1000).collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/openwrt-ipv6-watch/config.toml"));
    let config = Config::from_file(path)?;
    let recovery_client = SystemOpenWrtClient::new(config.openwrt.clone())?;
    if config.openwrt.install_wrapper_on_startup {
        let install_result = recovery_client.install_wrapper().await?;
        if !install_result.success {
            return Err(format!(
                "failed to install OpenWrt wrapper remotely: exit_code={:?} stderr={}",
                install_result.exit_code, install_result.stderr
            )
            .into());
        }
    }
    let client = Arc::new(WatchClient::new(
        LocalProbeClient::new(config.probe.clone()),
        recovery_client,
    ));
    let monitor = Arc::new(Monitor::new(client, config.monitor.clone()));
    let bot = Arc::new(TelegramBot::from_env(&config.telegram)?);

    let monitor_task = {
        let monitor = Arc::clone(&monitor);
        let bot = Arc::clone(&bot);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(monitor.interval());
            let mut incident_active = false;
            let mut last_failure_notification: Option<tokio::time::Instant> = None;
            loop {
                ticker.tick().await;
                match monitor.tick().await {
                    MonitorEvent::Healthy {
                        probe,
                        consecutive_successes,
                    } => {
                        info!(
                            success = probe.success,
                            exit_code = ?probe.exit_code,
                            consecutive_successes,
                            duration_ms = probe.duration.as_millis() as u64,
                            "local IPv6 probe completed"
                        );
                        if incident_active
                            && consecutive_successes >= monitor.recovery_success_threshold()
                        {
                            let message = format!(
                                "本地 IPv6 已恢复。连续成功：{consecutive_successes}，退出码：{:?}。",
                                probe.exit_code
                            );
                            match bot.send_message(&message).await {
                                Ok(()) => {
                                    info!("sent IPv6 recovery notification to Telegram");
                                    incident_active = false;
                                    last_failure_notification = None;
                                }
                                Err(error) => {
                                    warn!(%error, "failed to send IPv6 recovery notification to Telegram")
                                }
                            }
                        }
                    }
                    MonitorEvent::Failed {
                        consecutive_failures,
                        error,
                        probe,
                    } => {
                        warn!(
                            consecutive_failures,
                            ?error,
                            ?probe,
                            "local IPv6 probe failed"
                        );
                        let reminder_due = last_failure_notification
                            .map(|sent| sent.elapsed() >= monitor.reminder_interval())
                            .unwrap_or(true);
                        if consecutive_failures >= monitor.failure_threshold()
                            && (!incident_active || reminder_due)
                        {
                            let message = format!(
                                "本地 IPv6 检测失败。连续失败：{consecutive_failures}\n错误：{}",
                                failure_detail(error.as_ref(), probe.as_ref())
                            );
                            match bot.send_message(&message).await {
                                Ok(()) => {
                                    warn!("sent IPv6 failure notification to Telegram");
                                    incident_active = true;
                                    last_failure_notification = Some(tokio::time::Instant::now());
                                }
                                Err(error) => {
                                    warn!(%error, "failed to send IPv6 failure notification to Telegram")
                                }
                            }
                        }
                    }
                }
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
            match result {
                Ok(()) => Err("monitor task stopped unexpectedly".into()),
                Err(error) => {
                    error!(%error, "monitor task stopped");
                    Err(format!("monitor task stopped: {error}").into())
                }
            }
        }
        result = telegram_task => {
            match result {
                Ok(Ok(())) => Err("Telegram task stopped unexpectedly".into()),
                Ok(Err(error)) => {
                    error!(%error, "Telegram task stopped");
                    Err(format!("Telegram task stopped: {error}").into())
                }
                Err(error) => {
                    error!(%error, "Telegram task stopped");
                    Err(format!("Telegram task stopped: {error}").into())
                }
            }
        }
    }
}
