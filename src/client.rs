use std::{ffi::OsString, process::Stdio, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};
use tracing::{error, info, warn};

use crate::config::{OpenWrtConfig, RECOVER_REMOTE_COMMAND};
use crate::local_probe::LocalProbeClient;

const INSTALL_WRAPPER_REMOTE_COMMAND: &str =
    "mkdir -p /usr/local/sbin && cat > /usr/local/sbin/ipv6-watch-ssh-command && chmod 755 /usr/local/sbin/ipv6-watch-ssh-command";
const WRAPPER_SCRIPT: &str = include_str!("../openwrt/ipv6-watch-ssh-command");

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("SSH connection to OpenWrt failed: {0}")]
    SshConnection(String),
    #[error("SSH host key verification failed: {0}")]
    HostKeyVerification(String),
    #[error("SSH command timed out after {0} seconds")]
    CommandTimeout(u64),
    #[error("SSH password environment variable is not set: {0}")]
    PasswordEnvMissing(String),
    #[error("local DNS query failed: {0}")]
    LocalDns(String),
    #[error("failed to start local IPv6 ping: {0}")]
    LocalPingSpawn(String),
    #[error("local IPv6 ping timed out after {0} seconds")]
    LocalPingTimeout(u64),
    #[error("failed to start ssh: {0}")]
    Spawn(#[from] std::io::Error),
}

#[async_trait]
pub trait Ipv6Probe: Send + Sync {
    async fn check_ipv6(&self) -> Result<ProbeResult, ClientError>;
}

#[async_trait]
pub trait Wan6Recovery: Send + Sync {
    async fn recover_wan6(&self) -> Result<CommandResult, ClientError>;
}

pub trait OpenWrtClient: Ipv6Probe + Wan6Recovery {}

impl<T> OpenWrtClient for T where T: Ipv6Probe + Wan6Recovery {}

#[derive(Debug, Clone)]
pub struct SystemOpenWrtClient {
    config: OpenWrtConfig,
    ssh_program: OsString,
}

pub struct WatchClient {
    local_probe: LocalProbeClient,
    recovery: SystemOpenWrtClient,
}

impl WatchClient {
    pub fn new(local_probe: LocalProbeClient, recovery: SystemOpenWrtClient) -> Self {
        Self {
            local_probe,
            recovery,
        }
    }
}

#[async_trait]
impl Ipv6Probe for WatchClient {
    async fn check_ipv6(&self) -> Result<ProbeResult, ClientError> {
        self.local_probe.check_ipv6().await
    }
}

#[async_trait]
impl Wan6Recovery for WatchClient {
    async fn recover_wan6(&self) -> Result<CommandResult, ClientError> {
        self.recovery.recover_wan6().await
    }
}

impl SystemOpenWrtClient {
    pub fn new(config: OpenWrtConfig) -> Result<Self, crate::config::ConfigError> {
        Self::new_with_ssh_program(config, "ssh")
    }

    pub fn new_with_ssh_program(
        config: OpenWrtConfig,
        ssh_program: impl Into<OsString>,
    ) -> Result<Self, crate::config::ConfigError> {
        if config.recover_remote_command != RECOVER_REMOTE_COMMAND {
            return Err(crate::config::ConfigError::Invalid(
                "recover_remote_command must be exactly `recover`".into(),
            ));
        }
        Ok(Self {
            config,
            ssh_program: ssh_program.into(),
        })
    }

    fn ssh_command(&self, remote_command: &str) -> Result<Command, ClientError> {
        let target = format!("{}@{}", self.config.username, self.config.host);
        let connect_timeout = self.config.connect_timeout_seconds.to_string();
        let password_mode = self.config.password_env.is_some();
        let mut command = if let Some(password_env) = &self.config.password_env {
            let password = std::env::var(password_env)
                .map_err(|_| ClientError::PasswordEnvMissing(password_env.clone()))?;
            let mut command = Command::new("sshpass");
            command.arg("-e").arg("ssh").env("SSHPASS", password);
            command
        } else {
            Command::new(&self.ssh_program)
        };
        if !password_mode {
            if let Some(private_key) = &self.config.private_key {
                command.arg("-i").arg(private_key);
            }
        }
        command
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg("-o")
            .arg(if password_mode {
                "BatchMode=no"
            } else {
                "BatchMode=yes"
            })
            .arg("-o")
            .arg(format!("ConnectTimeout={connect_timeout}"))
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.config.known_hosts))
            .arg(target)
            .arg(remote_command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.kill_on_drop(true);
        Ok(command)
    }

    fn runtime_remote_command(&self, command: &'static str) -> String {
        if self.config.password_env.is_some() {
            format!("SSH_ORIGINAL_COMMAND={command} exec /usr/local/sbin/ipv6-watch-ssh-command")
        } else {
            command.to_owned()
        }
    }

    async fn run(&self, remote_command: &'static str) -> Result<CommandResult, ClientError> {
        let runtime_command = self.runtime_remote_command(remote_command);
        self.run_with_stdin(&runtime_command, None).await
    }

    async fn run_with_stdin(
        &self,
        remote_command: &str,
        stdin_data: Option<&[u8]>,
    ) -> Result<CommandResult, ClientError> {
        let started = tokio::time::Instant::now();
        info!(host = %self.config.host, command = remote_command, "starting remote IPv6 operation");

        let mut command = self.ssh_command(remote_command).map_err(|e| {
            error!(error = %e, "SSH authentication configuration failed");
            e
        })?;
        if stdin_data.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().map_err(|e| {
            error!(error = %e, "SSH connection to OpenWrt failed");
            ClientError::SshConnection(e.to_string())
        })?;
        let output = timeout(
            Duration::from_secs(self.config.command_timeout_seconds),
            async {
                if let Some(stdin_data) = stdin_data {
                    let mut stdin = child.stdin.take().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "ssh stdin unavailable")
                    })?;
                    stdin.write_all(stdin_data).await?;
                    drop(stdin);
                }
                child.wait_with_output().await
            },
        )
        .await
        .map_err(|_| {
            warn!(command = remote_command, "remote SSH command timed out");
            ClientError::CommandTimeout(self.config.command_timeout_seconds)
        })?
        .map_err(|e| {
            error!(error = %e, "SSH connection to OpenWrt failed");
            ClientError::SshConnection(e.to_string())
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if looks_like_host_key_error(&stderr) {
            return Err(ClientError::HostKeyVerification(stderr));
        }
        if output.status.code() == Some(255) {
            return Err(ClientError::SshConnection(stderr));
        }

        let result = CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout,
            stderr,
            duration: started.elapsed(),
        };
        info!(
            host = %self.config.host,
            command = remote_command,
            success = result.success,
            exit_code = ?result.exit_code,
            duration_ms = result.duration.as_millis() as u64,
            "remote operation completed"
        );
        Ok(result)
    }

    /// Install the fixed OpenWrt wrapper through SSH without using scp.
    pub async fn install_wrapper(&self) -> Result<CommandResult, ClientError> {
        self.run_with_stdin(
            INSTALL_WRAPPER_REMOTE_COMMAND,
            Some(WRAPPER_SCRIPT.as_bytes()),
        )
        .await
    }
}

#[async_trait]
impl Wan6Recovery for SystemOpenWrtClient {
    async fn recover_wan6(&self) -> Result<CommandResult, ClientError> {
        self.run(RECOVER_REMOTE_COMMAND).await
    }
}

fn looks_like_host_key_error(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("host key verification failed")
        || text.contains("offending") && text.contains("known_hosts")
        || text.contains("remote host identification has changed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use tempfile::TempDir;

    use crate::config::OpenWrtConfig;

    fn config(timeout_seconds: u64) -> OpenWrtConfig {
        OpenWrtConfig {
            host: "192.168.1.1".into(),
            port: 22,
            username: "root".into(),
            private_key: Some("/key".into()),
            known_hosts: "/known_hosts".into(),
            password_env: None,
            install_wrapper_on_startup: false,
            connect_timeout_seconds: 10,
            command_timeout_seconds: timeout_seconds,
            recover_remote_command: "recover".into(),
        }
    }

    fn fake_ssh(temp: &TempDir, body: &str) -> PathBuf {
        let path = temp.path().join("fake-ssh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn last_argument_script(log: &std::path::Path, result: &str) -> String {
        format!(
            "last=; for arg in \"$@\"; do last=\"$arg\"; done; printf '%s\\n' \"$last\" >> '{}'; printf '%s' '{}'; exit 0",
            log.display(), result
        )
    }

    #[test]
    fn only_host_key_errors_are_classified_as_security_errors() {
        assert!(looks_like_host_key_error("Host key verification failed."));
        assert!(!looks_like_host_key_error("ping6: bad address"));
    }

    #[test]
    fn bundled_wrapper_does_not_run_the_ipv6_probe() {
        assert!(WRAPPER_SCRIPT.contains("\"recover\""));
        assert!(!WRAPPER_SCRIPT.contains("ping6"));
    }

    #[test]
    fn password_mode_uses_only_fixed_wrapper_invocations() {
        let mut openwrt = config(10);
        openwrt.password_env = Some("OPENWRT_IPV6_WATCH_TEST_PASSWORD".into());
        let client = SystemOpenWrtClient::new_with_ssh_program(openwrt, "ssh").unwrap();

        assert_eq!(
            client.runtime_remote_command(RECOVER_REMOTE_COMMAND),
            "SSH_ORIGINAL_COMMAND=recover exec /usr/local/sbin/ipv6-watch-ssh-command"
        );
    }

    #[tokio::test]
    async fn ssh_connection_failure_and_host_key_failure_are_classified() {
        let temp = TempDir::new().unwrap();
        let ssh = fake_ssh(&temp, "printf '%s' 'Connection refused' >&2; exit 255");
        let client = SystemOpenWrtClient::new_with_ssh_program(config(10), ssh).unwrap();
        assert!(matches!(
            client.recover_wan6().await,
            Err(ClientError::SshConnection(_))
        ));

        let ssh = fake_ssh(
            &temp,
            "printf '%s' 'Host key verification failed.' >&2; exit 255",
        );
        let client = SystemOpenWrtClient::new_with_ssh_program(config(10), ssh).unwrap();
        assert!(matches!(
            client.recover_wan6().await,
            Err(ClientError::HostKeyVerification(_))
        ));
    }

    #[tokio::test]
    async fn ssh_command_timeout_is_reported() {
        let temp = TempDir::new().unwrap();
        let ssh = fake_ssh(&temp, "sleep 2");
        let client = SystemOpenWrtClient::new_with_ssh_program(config(1), ssh).unwrap();

        assert!(matches!(
            client.recover_wan6().await,
            Err(ClientError::CommandTimeout(1))
        ));
    }

    #[tokio::test]
    async fn recover_sends_only_fixed_recover_remote_command() {
        let temp = TempDir::new().unwrap();
        let log = temp.path().join("commands");
        let ssh = fake_ssh(&temp, &last_argument_script(&log, "recovered"));
        let client = SystemOpenWrtClient::new_with_ssh_program(config(10), ssh).unwrap();

        let result = client.recover_wan6().await.unwrap();

        assert!(result.success);
        assert_eq!(fs::read_to_string(log).unwrap(), "recover\n");
    }

    #[tokio::test]
    async fn password_mode_requires_the_configured_environment_variable() {
        let temp = TempDir::new().unwrap();
        let ssh = fake_ssh(&temp, "exit 0");
        let mut openwrt = config(2);
        openwrt.private_key = None;
        openwrt.password_env = Some("OPENWRT_IPV6_WATCH_TEST_MISSING".into());
        std::env::remove_var("OPENWRT_IPV6_WATCH_TEST_MISSING");
        let client = SystemOpenWrtClient::new_with_ssh_program(openwrt, ssh).unwrap();

        assert!(matches!(
            client.recover_wan6().await,
            Err(ClientError::PasswordEnvMissing(name))
                if name == "OPENWRT_IPV6_WATCH_TEST_MISSING"
        ));
    }

    #[tokio::test]
    async fn startup_wrapper_install_uses_fixed_remote_command_and_stdin_script() {
        let temp = TempDir::new().unwrap();
        let log = temp.path().join("commands");
        let script_log = temp.path().join("stdin");
        let ssh = fake_ssh(
            &temp,
            &format!(
                "cat > '{}'; last=; for arg in \"$@\"; do last=\"$arg\"; done; printf '%s\\n' \"$last\" >> '{}'",
                script_log.display(),
                log.display()
            ),
        );
        let client = SystemOpenWrtClient::new_with_ssh_program(config(10), ssh).unwrap();

        let result = client.install_wrapper().await.unwrap();

        assert!(result.success);
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            format!("{INSTALL_WRAPPER_REMOTE_COMMAND}\n")
        );
        assert_eq!(fs::read_to_string(script_log).unwrap(), WRAPPER_SCRIPT);
    }
}
