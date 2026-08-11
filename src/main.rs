use std::{path::PathBuf, sync::Arc};

use openwrt_ipv6_watch::{
    client::{ProbeResult, SystemOpenWrtClient, WatchClient},
    config::Config,
    local_probe::LocalProbeClient,
    monitor::{AutoRecoveryOutcome, Monitor, MonitorEvent, RecoveryOutcome},
    telegram::TelegramBot,
};
use tracing::{error, info, warn};

fn failure_detail(error: Option<&String>, probe: Option<&ProbeResult>) -> String {
    let detail = error
        .map(String::as_str)
        .or_else(|| probe.map(|probe| probe.stderr.trim()))
        .filter(|detail| !detail.is_empty())
        .unwrap_or("no detailed error");
    detail.chars().take(1000).collect()
}

fn recovery_outcome_detail(outcome: &RecoveryOutcome) -> String {
    let detail = match outcome {
        RecoveryOutcome::Recovered { probe } => format!(
            "WAN6 recovery command succeeded and the local IPv6 probe succeeded (exit code {:?}). The monitor will continue checking for stable recovery.",
            probe.exit_code
        ),
        RecoveryOutcome::RecoverCommandFailed { error, stderr } => format!(
            "WAN6 recovery command failed. {} {}",
            error.as_deref().unwrap_or("no command error"),
            stderr.trim()
        ),
        RecoveryOutcome::ProbeFailed { error, probe } => format!(
            "WAN6 recovery command completed, but the local IPv6 probe still failed. {}",
            error
                .as_deref()
                .or_else(|| probe.as_ref().map(|probe| probe.stderr.trim()))
                .unwrap_or("no probe error")
        ),
    };
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
                                "Local IPv6 connectivity recovered. Consecutive successes: {consecutive_successes}, exit code: {:?}.",
                                probe.exit_code
                            );
                            match bot.send_message(&message).await {
                                Ok(()) => {
                                    info!("sent IPv6 recovery notification to Telegram");
                                }
                                Err(error) => {
                                    warn!(%error, "failed to send IPv6 recovery notification to Telegram")
                                }
                            }
                            incident_active = false;
                            last_failure_notification = None;
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
                        let incident_started =
                            consecutive_failures >= monitor.failure_threshold() && !incident_active;
                        if incident_started {
                            incident_active = true;
                            last_failure_notification = None;
                        }
                        if consecutive_failures >= monitor.failure_threshold()
                            && (incident_started || reminder_due)
                        {
                            let message = format!(
                                "Local IPv6 connectivity check failed. Consecutive failures: {consecutive_failures}\nError: {}",
                                failure_detail(error.as_ref(), probe.as_ref())
                            );
                            match bot.send_message(&message).await {
                                Ok(()) => {
                                    warn!("sent IPv6 failure notification to Telegram");
                                }
                                Err(error) => {
                                    warn!(%error, "failed to send IPv6 failure notification to Telegram")
                                }
                            }
                            last_failure_notification = Some(tokio::time::Instant::now());
                        }

                        if incident_active {
                            let start_bot = Arc::clone(&bot);
                            match monitor
                                .auto_recover_if_due_with_hook(move |attempt| {
                                    let bot = Arc::clone(&start_bot);
                                    async move {
                                        let message = format!(
                                            "Starting automatic IPv6 recovery attempt {attempt}."
                                        );
                                        if let Err(error) = bot.send_message(&message).await {
                                            warn!(%error, attempt, "failed to send automatic recovery start notification");
                                        }
                                    }
                                })
                                .await
                            {
                                AutoRecoveryOutcome::Attempted { attempt, outcome } => {
                                    let message = format!(
                                        "Automatic IPv6 recovery attempt {attempt} completed.\n{}",
                                        recovery_outcome_detail(&outcome)
                                    );
                                    if let Err(error) = bot.send_message(&message).await {
                                        warn!(%error, attempt, "failed to send automatic recovery result notification");
                                    }
                                }
                                AutoRecoveryOutcome::Suppressed { attempts } => {
                                    let message = format!(
                                        "Automatic IPv6 recovery is now paused after {attempts} failed attempts. The watcher will continue passive checks and reminders; manual recovery is required if the ISP remains unavailable."
                                    );
                                    if let Err(error) = bot.send_message(&message).await {
                                        warn!(%error, attempts, "failed to send automatic recovery suppression notification");
                                    }
                                }
                                AutoRecoveryOutcome::NotDue | AutoRecoveryOutcome::InProgress => {}
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
