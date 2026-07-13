use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::client::{ClientError, OpenWrtClient, ProbeResult};
use crate::config::MonitorConfig;

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Healthy(ProbeResult),
    Failed {
        consecutive_failures: u32,
        error: Option<String>,
        probe: Option<ProbeResult>,
    },
}

#[derive(Debug, Clone)]
pub enum RecoveryOutcome {
    Recovered {
        probe: ProbeResult,
    },
    RecoverCommandFailed {
        error: Option<String>,
        stderr: String,
    },
    ProbeFailed {
        error: Option<String>,
        probe: Option<ProbeResult>,
    },
}

#[derive(Debug, Default)]
struct State {
    consecutive_failures: u32,
    consecutive_recovery_successes: u32,
}

pub struct Monitor<C> {
    client: Arc<C>,
    config: MonitorConfig,
    state: Mutex<State>,
}

impl<C: OpenWrtClient> Monitor<C> {
    pub fn new(client: Arc<C>, config: MonitorConfig) -> Self {
        Self {
            client,
            config,
            state: Mutex::new(State::default()),
        }
    }

    pub fn client(&self) -> &Arc<C> {
        &self.client
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.interval_seconds)
    }

    pub async fn tick(&self) -> MonitorEvent {
        match self.client.check_ipv6().await {
            Ok(probe) if probe.success => {
                let mut state = self.state.lock().await;
                state.consecutive_failures = 0;
                state.consecutive_recovery_successes =
                    state.consecutive_recovery_successes.saturating_add(1);
                info!("OpenWrt remote IPv6 probe is healthy");
                MonitorEvent::Healthy(probe)
            }
            Ok(probe) => {
                let mut state = self.state.lock().await;
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_recovery_successes = 0;
                warn!(
                    consecutive_failures = state.consecutive_failures,
                    "OpenWrt remote IPv6 probe failed"
                );
                MonitorEvent::Failed {
                    consecutive_failures: state.consecutive_failures,
                    error: None,
                    probe: Some(probe),
                }
            }
            Err(error) => {
                let mut state = self.state.lock().await;
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_recovery_successes = 0;
                warn!(
                    consecutive_failures = state.consecutive_failures,
                    error = %error,
                    "OpenWrt remote IPv6 probe could not be completed"
                );
                MonitorEvent::Failed {
                    consecutive_failures: state.consecutive_failures,
                    error: Some(error.to_string()),
                    probe: None,
                }
            }
        }
    }

    pub async fn recover_and_probe(&self) -> RecoveryOutcome {
        let recovery = match self.client.recover_wan6().await {
            Ok(result) if result.success => result,
            Ok(result) => {
                return RecoveryOutcome::RecoverCommandFailed {
                    error: None,
                    stderr: result.stderr,
                };
            }
            Err(error) => {
                return RecoveryOutcome::RecoverCommandFailed {
                    error: Some(error.to_string()),
                    stderr: String::new(),
                };
            }
        };
        info!(
            exit_code = ?recovery.exit_code,
            "remote WAN6 recovery completed; waiting before remote probe"
        );
        tokio::time::sleep(Duration::from_secs(self.config.recovery_wait_seconds)).await;

        match self.client.check_ipv6().await {
            Ok(probe) if probe.success => RecoveryOutcome::Recovered { probe },
            Ok(probe) => RecoveryOutcome::ProbeFailed {
                error: None,
                probe: Some(probe),
            },
            Err(error) => RecoveryOutcome::ProbeFailed {
                error: Some(error.to_string()),
                probe: None,
            },
        }
    }

    pub fn failure_threshold(&self) -> u32 {
        self.config.failure_threshold
    }

    pub fn recovery_success_threshold(&self) -> u32 {
        self.config.recovery_success_threshold
    }
}

impl From<ClientError> for MonitorEvent {
    fn from(error: ClientError) -> Self {
        MonitorEvent::Failed {
            consecutive_failures: 1,
            error: Some(error.to_string()),
            probe: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use crate::client::CommandResult;

    struct RecordingClient {
        calls: StdMutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl OpenWrtClient for RecordingClient {
        async fn check_ipv6(&self) -> Result<ProbeResult, ClientError> {
            self.calls.lock().unwrap().push("probe");
            Ok(ProbeResult {
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration: Duration::from_millis(1),
            })
        }

        async fn recover_wan6(&self) -> Result<CommandResult, ClientError> {
            self.calls.lock().unwrap().push("recover");
            Ok(CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration: Duration::from_millis(1),
            })
        }
    }

    fn config() -> MonitorConfig {
        MonitorConfig {
            interval_seconds: 1,
            failure_threshold: 3,
            recovery_success_threshold: 2,
            recovery_wait_seconds: 0,
            reminder_interval_seconds: 60,
            incident_expire_seconds: 60,
        }
    }

    #[tokio::test]
    async fn recovery_is_followed_by_a_remote_probe() {
        let client = Arc::new(RecordingClient {
            calls: StdMutex::new(Vec::new()),
        });
        let monitor = Monitor::new(Arc::clone(&client), config());

        assert!(matches!(
            monitor.recover_and_probe().await,
            RecoveryOutcome::Recovered { .. }
        ));
        assert_eq!(*client.calls.lock().unwrap(), vec!["recover", "probe"]);
    }
}
