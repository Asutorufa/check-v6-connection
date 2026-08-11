use std::time::Duration;
use std::{future::Future, sync::Arc};

use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::client::{ClientError, OpenWrtClient, ProbeResult};
use crate::config::MonitorConfig;

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Healthy {
        probe: ProbeResult,
        consecutive_successes: u32,
    },
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

#[derive(Debug, Clone)]
pub enum AutoRecoveryOutcome {
    NotDue,
    InProgress,
    Attempted {
        attempt: u32,
        outcome: RecoveryOutcome,
    },
    Suppressed {
        attempts: u32,
    },
}

#[derive(Debug, Default)]
struct State {
    consecutive_failures: u32,
    consecutive_recovery_successes: u32,
    incident_active: bool,
    auto_recovery_attempts: u32,
    next_auto_recovery_at: Option<Instant>,
    auto_recovery_suppressed: bool,
    auto_recovery_suppression_reported: bool,
}

pub struct Monitor<C> {
    client: Arc<C>,
    config: MonitorConfig,
    state: Mutex<State>,
    recovery_lock: Mutex<()>,
}

impl<C: OpenWrtClient> Monitor<C> {
    pub fn new(client: Arc<C>, config: MonitorConfig) -> Self {
        Self {
            client,
            config,
            state: Mutex::new(State::default()),
            recovery_lock: Mutex::new(()),
        }
    }

    pub fn client(&self) -> &Arc<C> {
        &self.client
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.interval_seconds)
    }

    pub async fn probe_once(&self) -> Result<ProbeResult, ClientError> {
        self.client.check_ipv6().await
    }

    pub async fn tick(&self) -> MonitorEvent {
        self.record_probe(self.probe_once().await).await
    }

    async fn record_probe(&self, result: Result<ProbeResult, ClientError>) -> MonitorEvent {
        let mut state = self.state.lock().await;
        match result {
            Ok(probe) if probe.success => {
                state.consecutive_failures = 0;
                state.consecutive_recovery_successes =
                    state.consecutive_recovery_successes.saturating_add(1);
                if state.incident_active
                    && state.consecutive_recovery_successes
                        >= self.config.recovery_success_threshold
                {
                    info!("IPv6 incident closed after consecutive healthy probes");
                    Self::clear_incident(&mut state);
                }
                info!("local IPv6 probe is healthy");
                MonitorEvent::Healthy {
                    probe,
                    consecutive_successes: state.consecutive_recovery_successes,
                }
            }
            Ok(probe) => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_recovery_successes = 0;
                Self::open_incident_if_needed(&mut state, &self.config);
                warn!(
                    consecutive_failures = state.consecutive_failures,
                    "local IPv6 probe failed"
                );
                MonitorEvent::Failed {
                    consecutive_failures: state.consecutive_failures,
                    error: None,
                    probe: Some(probe),
                }
            }
            Err(error) => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_recovery_successes = 0;
                Self::open_incident_if_needed(&mut state, &self.config);
                warn!(
                    consecutive_failures = state.consecutive_failures,
                    error = %error,
                    "local IPv6 probe could not be completed"
                );
                MonitorEvent::Failed {
                    consecutive_failures: state.consecutive_failures,
                    error: Some(error.to_string()),
                    probe: None,
                }
            }
        }
    }

    pub async fn auto_recover_if_due(&self) -> AutoRecoveryOutcome {
        self.auto_recover_if_due_with_hook(|_| async {}).await
    }

    pub async fn auto_recover_if_due_with_hook<F, Fut>(
        &self,
        before_recovery: F,
    ) -> AutoRecoveryOutcome
    where
        F: FnOnce(u32) -> Fut,
        Fut: Future<Output = ()>,
    {
        if !self.config.auto_recovery_enabled {
            return AutoRecoveryOutcome::NotDue;
        }

        let Ok(_recovery_guard) = self.recovery_lock.try_lock() else {
            return AutoRecoveryOutcome::InProgress;
        };

        let attempt = {
            let mut state = self.state.lock().await;
            if !state.incident_active {
                return AutoRecoveryOutcome::NotDue;
            }
            if state.auto_recovery_suppressed {
                if state.auto_recovery_suppression_reported {
                    return AutoRecoveryOutcome::NotDue;
                }
                state.auto_recovery_suppression_reported = true;
                return AutoRecoveryOutcome::Suppressed {
                    attempts: state.auto_recovery_attempts,
                };
            }
            if state.auto_recovery_attempts >= self.config.auto_recovery_max_attempts {
                state.auto_recovery_suppressed = true;
                state.auto_recovery_suppression_reported = true;
                return AutoRecoveryOutcome::Suppressed {
                    attempts: state.auto_recovery_attempts,
                };
            }
            if state
                .next_auto_recovery_at
                .is_some_and(|next| Instant::now() < next)
            {
                return AutoRecoveryOutcome::NotDue;
            }

            state.auto_recovery_attempts = state.auto_recovery_attempts.saturating_add(1);
            state.next_auto_recovery_at = None;
            state.auto_recovery_attempts
        };

        before_recovery(attempt).await;
        info!(attempt, "starting automatic WAN6 recovery");
        let outcome = self.recover_and_probe_locked().await;
        let mut state = self.state.lock().await;
        self.apply_recovery_probe(&mut state, &outcome);
        if state.incident_active {
            if attempt >= self.config.auto_recovery_max_attempts {
                state.auto_recovery_suppressed = true;
            } else {
                state.next_auto_recovery_at =
                    Some(Instant::now() + self.auto_recovery_backoff(attempt));
            }
        }
        drop(state);
        info!(attempt, "automatic WAN6 recovery completed");

        AutoRecoveryOutcome::Attempted { attempt, outcome }
    }

    fn auto_recovery_backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(63);
        let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
        let seconds = self
            .config
            .auto_recovery_initial_backoff_seconds
            .saturating_mul(multiplier)
            .min(self.config.auto_recovery_max_backoff_seconds);
        Duration::from_secs(seconds)
    }

    pub async fn recover_and_probe(&self) -> RecoveryOutcome {
        let _recovery_guard = self.recovery_lock.lock().await;
        let outcome = self.recover_and_probe_locked().await;
        self.record_manual_recovery(&outcome).await;
        outcome
    }

    pub async fn try_recover_and_probe(&self) -> Option<RecoveryOutcome> {
        let Ok(_recovery_guard) = self.recovery_lock.try_lock() else {
            return None;
        };
        let outcome = self.recover_and_probe_locked().await;
        self.record_manual_recovery(&outcome).await;
        Some(outcome)
    }

    async fn record_manual_recovery(&self, outcome: &RecoveryOutcome) {
        let mut state = self.state.lock().await;
        self.apply_recovery_probe(&mut state, outcome);
        if state.incident_active && !state.auto_recovery_suppressed {
            state.next_auto_recovery_at = Some(
                Instant::now()
                    + Duration::from_secs(self.config.auto_recovery_initial_backoff_seconds),
            );
        }
    }

    fn apply_recovery_probe(&self, state: &mut State, outcome: &RecoveryOutcome) {
        match outcome {
            RecoveryOutcome::Recovered { .. } => {
                state.consecutive_failures = 0;
                state.consecutive_recovery_successes =
                    state.consecutive_recovery_successes.saturating_add(1);
                if state.incident_active
                    && state.consecutive_recovery_successes
                        >= self.config.recovery_success_threshold
                {
                    Self::clear_incident(state);
                }
            }
            RecoveryOutcome::ProbeFailed { .. } => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_recovery_successes = 0;
            }
            RecoveryOutcome::RecoverCommandFailed { .. } => {}
        }
    }

    fn open_incident_if_needed(state: &mut State, config: &MonitorConfig) {
        if !state.incident_active && state.consecutive_failures >= config.failure_threshold {
            state.incident_active = true;
            state.auto_recovery_attempts = 0;
            state.next_auto_recovery_at = None;
            state.auto_recovery_suppressed = false;
            state.auto_recovery_suppression_reported = false;
            info!("IPv6 incident opened");
        }
    }

    fn clear_incident(state: &mut State) {
        state.incident_active = false;
        state.auto_recovery_attempts = 0;
        state.next_auto_recovery_at = None;
        state.auto_recovery_suppressed = false;
        state.auto_recovery_suppression_reported = false;
    }

    async fn recover_and_probe_locked(&self) -> RecoveryOutcome {
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
            "remote WAN6 recovery completed; waiting before local probe"
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

    pub fn reminder_interval(&self) -> Duration {
        Duration::from_secs(self.config.reminder_interval_seconds)
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
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use crate::client::{CommandResult, Ipv6Probe, Wan6Recovery};

    struct ScriptedClient {
        probes: StdMutex<VecDeque<Result<ProbeResult, ClientError>>>,
        recoveries: StdMutex<VecDeque<Result<CommandResult, ClientError>>>,
        calls: StdMutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl Ipv6Probe for ScriptedClient {
        async fn check_ipv6(&self) -> Result<ProbeResult, ClientError> {
            self.calls.lock().unwrap().push("probe");
            self.probes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(successful_probe()))
        }
    }

    #[async_trait]
    impl Wan6Recovery for ScriptedClient {
        async fn recover_wan6(&self) -> Result<CommandResult, ClientError> {
            self.calls.lock().unwrap().push("recover");
            self.recoveries
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(successful_command()))
        }
    }

    fn successful_probe() -> ProbeResult {
        ProbeResult {
            success: true,
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            duration: Duration::from_millis(1),
        }
    }

    fn failed_probe() -> ProbeResult {
        ProbeResult {
            success: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "unreachable".into(),
            duration: Duration::from_millis(1),
        }
    }

    fn successful_command() -> CommandResult {
        CommandResult {
            success: true,
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            duration: Duration::from_millis(1),
        }
    }

    fn failed_command() -> CommandResult {
        CommandResult {
            success: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "ifup failed".into(),
            duration: Duration::from_millis(1),
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
            auto_recovery_enabled: true,
            auto_recovery_max_attempts: 3,
            auto_recovery_initial_backoff_seconds: 0,
            auto_recovery_max_backoff_seconds: 0,
        }
    }

    fn client(
        probes: Vec<Result<ProbeResult, ClientError>>,
        recoveries: Vec<Result<CommandResult, ClientError>>,
    ) -> Arc<ScriptedClient> {
        Arc::new(ScriptedClient {
            probes: StdMutex::new(probes.into()),
            recoveries: StdMutex::new(recoveries.into()),
            calls: StdMutex::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn status_probe_does_not_change_monitor_counters() {
        let client = client(vec![Ok(failed_probe()), Ok(failed_probe())], vec![]);
        let monitor = Monitor::new(Arc::clone(&client), config());

        assert!(!monitor.probe_once().await.unwrap().success);
        let event = monitor.tick().await;
        assert!(matches!(
            event,
            MonitorEvent::Failed {
                consecutive_failures: 1,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn automatic_recovery_is_capped_per_incident() {
        let client = client(
            vec![Ok(failed_probe()), Ok(failed_probe()), Ok(failed_probe())],
            vec![
                Ok(failed_command()),
                Ok(failed_command()),
                Ok(failed_command()),
            ],
        );
        let monitor = Monitor::new(Arc::clone(&client), config());

        for _ in 0..3 {
            monitor.tick().await;
        }
        for _ in 0..3 {
            assert!(matches!(
                monitor.auto_recover_if_due().await,
                AutoRecoveryOutcome::Attempted { .. }
            ));
        }
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::Suppressed { attempts: 3 }
        ));
        assert_eq!(
            client
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == "recover")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn automatic_recovery_waits_for_backoff_before_retrying() {
        let client = client(
            vec![Ok(failed_probe()), Ok(failed_probe()), Ok(failed_probe())],
            vec![Ok(failed_command()), Ok(failed_command())],
        );
        let mut recovery_config = config();
        recovery_config.auto_recovery_initial_backoff_seconds = 60;
        recovery_config.auto_recovery_max_backoff_seconds = 60;
        let monitor = Monitor::new(Arc::clone(&client), recovery_config);

        for _ in 0..3 {
            monitor.tick().await;
        }
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::Attempted { attempt: 1, .. }
        ));
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::NotDue
        ));
        assert_eq!(
            client
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == "recover")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn automatic_recovery_invokes_start_hook_for_each_attempt() {
        let client = client(
            vec![Ok(failed_probe()), Ok(failed_probe()), Ok(failed_probe())],
            vec![Ok(failed_command())],
        );
        let monitor = Monitor::new(Arc::clone(&client), config());
        let notifications = Arc::new(StdMutex::new(Vec::new()));
        let hook_notifications = Arc::clone(&notifications);

        for _ in 0..3 {
            monitor.tick().await;
        }
        assert!(matches!(
            monitor
                .auto_recover_if_due_with_hook(move |attempt| {
                    hook_notifications.lock().unwrap().push(attempt);
                    async {}
                })
                .await,
            AutoRecoveryOutcome::Attempted { attempt: 1, .. }
        ));
        assert_eq!(*notifications.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn recovery_requires_consecutive_successes_before_closing_incident() {
        let client = client(
            vec![
                Ok(failed_probe()),
                Ok(failed_probe()),
                Ok(failed_probe()),
                Ok(successful_probe()),
            ],
            vec![Ok(successful_command())],
        );
        let monitor = Monitor::new(Arc::clone(&client), config());

        for _ in 0..3 {
            monitor.tick().await;
        }
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::Attempted {
                outcome: RecoveryOutcome::Recovered { .. },
                ..
            }
        ));
        assert!(matches!(
            monitor.tick().await,
            MonitorEvent::Healthy {
                consecutive_successes: 2,
                ..
            }
        ));
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::NotDue
        ));
    }

    #[tokio::test]
    async fn recovery_is_single_flight_for_manual_and_automatic_paths() {
        let client = client(
            vec![Ok(failed_probe()), Ok(failed_probe()), Ok(failed_probe())],
            vec![],
        );
        let monitor = Monitor::new(Arc::clone(&client), config());

        for _ in 0..3 {
            monitor.tick().await;
        }
        let _guard = monitor.recovery_lock.lock().await;
        assert!(matches!(
            monitor.auto_recover_if_due().await,
            AutoRecoveryOutcome::InProgress
        ));
        assert!(monitor.try_recover_and_probe().await.is_none());
    }

    #[tokio::test]
    async fn recovery_is_followed_by_a_local_probe() {
        let client = client(vec![Ok(successful_probe())], vec![Ok(successful_command())]);
        let monitor = Monitor::new(Arc::clone(&client), config());

        assert!(matches!(
            monitor.recover_and_probe().await,
            RecoveryOutcome::Recovered { .. }
        ));
        assert_eq!(*client.calls.lock().unwrap(), vec!["recover", "probe"]);
    }
}
