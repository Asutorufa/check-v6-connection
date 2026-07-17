use std::{fs, path::Path};

use serde::Deserialize;
use thiserror::Error;

pub const RECOVER_REMOTE_COMMAND: &str = "recover";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub monitor: MonitorConfig,
    pub telegram: TelegramConfig,
    pub probe: ProbeConfig,
    pub openwrt: OpenWrtConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeConfig {
    pub interface: String,
    #[serde(default = "default_dns_server")]
    pub dns_server: String,
    #[serde(default = "default_probe_timeout_seconds")]
    pub dns_timeout_seconds: u64,
    #[serde(default = "default_probe_timeout_seconds")]
    pub ping_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MonitorConfig {
    pub interval_seconds: u64,
    pub failure_threshold: u32,
    pub recovery_success_threshold: u32,
    pub recovery_wait_seconds: u64,
    pub reminder_interval_seconds: u64,
    pub incident_expire_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub bot_token_env: String,
    pub chat_id: i64,
    pub allowed_user_ids: Vec<i64>,
    #[serde(default = "default_telegram_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_telegram_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    #[serde(default = "default_telegram_poll_timeout_seconds")]
    pub poll_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenWrtConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub private_key: Option<String>,
    pub known_hosts: String,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default = "default_install_wrapper_on_startup")]
    pub install_wrapper_on_startup: bool,
    pub connect_timeout_seconds: u64,
    pub command_timeout_seconds: u64,
    #[serde(default = "default_recover_command")]
    pub recover_remote_command: String,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_recover_command() -> String {
    RECOVER_REMOTE_COMMAND.to_owned()
}

fn default_install_wrapper_on_startup() -> bool {
    true
}

fn default_dns_server() -> String {
    "1.1.1.1:53".to_owned()
}

fn default_probe_timeout_seconds() -> u64 {
    3
}

fn default_telegram_connect_timeout_seconds() -> u64 {
    10
}

fn default_telegram_request_timeout_seconds() -> u64 {
    35
}

fn default_telegram_poll_timeout_seconds() -> u64 {
    20
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let o = &self.openwrt;
        if o.host.trim().is_empty() || o.username.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "OpenWrt host and username are required".into(),
            ));
        }
        if o.known_hosts.trim().is_empty() {
            return Err(ConfigError::Invalid("known_hosts path is required".into()));
        }
        if o.private_key
            .as_deref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(ConfigError::Invalid("private_key cannot be empty".into()));
        }
        if o.password_env
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(ConfigError::Invalid("password_env cannot be empty".into()));
        }
        if o.private_key.is_none() && o.password_env.is_none() {
            return Err(ConfigError::Invalid(
                "configure either private_key or password_env".into(),
            ));
        }
        if o.connect_timeout_seconds == 0 || o.command_timeout_seconds == 0 {
            return Err(ConfigError::Invalid(
                "SSH timeouts must be greater than zero".into(),
            ));
        }
        if o.recover_remote_command != RECOVER_REMOTE_COMMAND {
            return Err(ConfigError::Invalid(
                "recover_remote_command must be exactly `recover`".into(),
            ));
        }
        if self.monitor.interval_seconds == 0
            || self.monitor.failure_threshold == 0
            || self.monitor.recovery_success_threshold == 0
        {
            return Err(ConfigError::Invalid(
                "monitor interval and thresholds must be greater than zero".into(),
            ));
        }
        if self.telegram.bot_token_env.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "telegram bot_token_env is required".into(),
            ));
        }
        if self.telegram.connect_timeout_seconds == 0
            || self.telegram.request_timeout_seconds == 0
            || self.telegram.poll_timeout_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "Telegram timeouts must be greater than zero".into(),
            ));
        }
        if self.telegram.request_timeout_seconds <= self.telegram.poll_timeout_seconds {
            return Err(ConfigError::Invalid(
                "Telegram request timeout must be longer than the long-poll timeout".into(),
            ));
        }
        if self.probe.interface.trim().is_empty() {
            return Err(ConfigError::Invalid("probe interface is required".into()));
        }
        if self
            .probe
            .dns_server
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            return Err(ConfigError::Invalid(
                "probe dns_server must be an IP address with port, for example 1.1.1.1:53".into(),
            ));
        }
        if self.probe.dns_timeout_seconds == 0 || self.probe.ping_timeout_seconds == 0 {
            return Err(ConfigError::Invalid(
                "probe timeouts must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
            [monitor]
            interval_seconds = 30
            failure_threshold = 3
            recovery_success_threshold = 2
            recovery_wait_seconds = 10
            reminder_interval_seconds = 1800
            incident_expire_seconds = 3600

            [telegram]
            bot_token_env = "TELEGRAM_BOT_TOKEN"
            chat_id = 1
            allowed_user_ids = [2]

            [probe]
            interface = "eth0"
            dns_server = "1.1.1.1:53"
            dns_timeout_seconds = 3
            ping_timeout_seconds = 3

            [openwrt]
            host = "192.168.1.1"
            username = "root"
            private_key = "/key"
            known_hosts = "/known_hosts"
            connect_timeout_seconds = 10
            command_timeout_seconds = 10
            recover_remote_command = "recover"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_modified_recover_command() {
        let mut c = config();
        c.openwrt.recover_remote_command = "recover; id".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn telegram_timeout_defaults_leave_room_for_long_polling() {
        let c = config();
        assert_eq!(c.telegram.connect_timeout_seconds, 10);
        assert_eq!(c.telegram.poll_timeout_seconds, 20);
        assert_eq!(c.telegram.request_timeout_seconds, 35);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_telegram_request_timeout_not_longer_than_poll_timeout() {
        let mut c = config();
        c.telegram.request_timeout_seconds = c.telegram.poll_timeout_seconds;
        assert!(c.validate().is_err());
    }
}
