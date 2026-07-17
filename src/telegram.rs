use std::{sync::Arc, time::Duration};

use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::{
    client::OpenWrtClient,
    monitor::{Monitor, MonitorEvent, RecoveryOutcome},
};

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("Telegram bot token environment variable is unavailable: {0}")]
    TokenEnvironment(#[from] std::env::VarError),
    #[error("failed to build Telegram HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
    #[error("Telegram request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Telegram API returned an error: {0}")]
    Api(String),
}

#[derive(Debug, Clone)]
pub struct TelegramBot {
    http: Client,
    token: String,
    chat_id: i64,
    allowed_user_ids: Arc<[i64]>,
    poll_timeout_seconds: u64,
    command_slots: Arc<Semaphore>,
}

impl TelegramBot {
    pub fn from_env(config: &crate::config::TelegramConfig) -> Result<Self, TelegramError> {
        let token = std::env::var(&config.bot_token_env)?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_seconds))
            // Telegram's long-poll timeout is 20 seconds by default. Keep a
            // margin here so a normal empty poll can complete before the
            // client gives up, while still bounding half-open connections.
            .timeout(Duration::from_secs(config.request_timeout_seconds))
            .build()
            .map_err(TelegramError::HttpClient)?;
        Ok(Self {
            http,
            token,
            chat_id: config.chat_id,
            allowed_user_ids: config.allowed_user_ids.clone().into(),
            poll_timeout_seconds: config.poll_timeout_seconds,
            // Keep polling responsive even while SSH/recovery work is in
            // progress. The limit bounds the number of command handlers that
            // can actively use the monitor and OpenWrt connection at once.
            command_slots: Arc::new(Semaphore::new(8)),
        })
    }

    pub fn is_allowed(&self, user_id: i64) -> bool {
        self.allowed_user_ids.contains(&user_id)
    }

    pub async fn send_message(&self, text: &str) -> Result<(), TelegramError> {
        #[derive(Serialize)]
        struct Request<'a> {
            chat_id: i64,
            text: &'a str,
        }
        self.call::<serde_json::Value, _>(
            "sendMessage",
            &Request {
                chat_id: self.chat_id,
                text,
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn run<C>(&self, monitor: Arc<Monitor<C>>) -> Result<(), TelegramError>
    where
        C: OpenWrtClient + 'static,
    {
        let mut offset: i64 = 0;
        let mut commands_registered = false;
        let mut retry_delay = Duration::from_secs(1);
        loop {
            if !commands_registered {
                match self.register_commands().await {
                    Ok(()) => {
                        commands_registered = true;
                        retry_delay = Duration::from_secs(1);
                        info!("Telegram bot commands registered");
                    }
                    Err(error) => {
                        warn!(%error, ?retry_delay, "Telegram command registration failed; retrying");
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = next_retry_delay(retry_delay);
                        continue;
                    }
                }
            }

            let updates: Vec<Update> = match self
                .call(
                    "getUpdates",
                    &GetUpdates {
                        offset,
                        timeout: self.poll_timeout_seconds,
                        allowed_updates: ["message", "callback_query"],
                    },
                )
                .await
            {
                Ok(updates) => {
                    retry_delay = Duration::from_secs(1);
                    updates
                }
                Err(error) => {
                    commands_registered = false;
                    warn!(%error, ?retry_delay, "Telegram polling failed; reconnecting");
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = next_retry_delay(retry_delay);
                    continue;
                }
            };
            for update in updates {
                offset = update.update_id + 1;
                let bot = self.clone();
                let monitor = Arc::clone(&monitor);
                let command_slots = Arc::clone(&self.command_slots);
                tokio::spawn(async move {
                    let Ok(_permit) = command_slots.acquire_owned().await else {
                        return;
                    };
                    let update_id = update.update_id;
                    if let Err(error) = bot.handle_update(&monitor, &update).await {
                        warn!(update_id, %error, "Telegram update handling failed; continuing polling");
                    }
                });
            }
        }
    }

    async fn handle_update<C>(
        &self,
        monitor: &Arc<Monitor<C>>,
        update: &Update,
    ) -> Result<(), TelegramError>
    where
        C: OpenWrtClient + 'static,
    {
        let (user_id, action, chat_allowed) = if let Some(message) = &update.message {
            (
                message.from.as_ref().map(|u| u.id),
                message.text.as_deref().and_then(parse_command),
                message.chat.id == self.chat_id,
            )
        } else if let Some(callback) = &update.callback_query {
            (
                Some(callback.from.id),
                callback
                    .data
                    .as_deref()
                    .filter(|data| is_recover_confirmation(data))
                    .map(|_| BotAction::Recover),
                callback
                    .message
                    .as_ref()
                    .is_some_and(|message| message.chat.id == self.chat_id),
            )
        } else {
            return Ok(());
        };

        if !chat_allowed || action.is_none() || !user_id.is_some_and(|id| self.is_allowed(id)) {
            warn!(
                ?user_id,
                "ignored unauthorized or unsupported Telegram input"
            );
            return Ok(());
        }

        match action.expect("checked above") {
            BotAction::Status => {
                info!("authorized Telegram status request accepted");
                self.send_status(monitor).await?;
            }
            BotAction::Recover => {
                info!("authorized Telegram recovery request accepted");
                self.send_recovery_result(monitor).await?;
            }
        }
        Ok(())
    }

    async fn register_commands(&self) -> Result<(), TelegramError> {
        #[derive(Serialize)]
        struct BotCommand<'a> {
            command: &'a str,
            description: &'a str,
        }
        #[derive(Serialize)]
        struct Request<'a> {
            commands: [BotCommand<'a>; 2],
        }
        self.call::<serde_json::Value, _>(
            "setMyCommands",
            &Request {
                commands: [
                    BotCommand {
                        command: "status",
                        description: "获取 OpenWrt IPv6 当前状态",
                    },
                    BotCommand {
                        command: "recover",
                        description: "恢复 OpenWrt WAN6 并重新检测",
                    },
                ],
            },
        )
        .await
        .map(|_| ())
    }

    async fn send_status<C>(&self, monitor: &Arc<Monitor<C>>) -> Result<(), TelegramError>
    where
        C: OpenWrtClient + 'static,
    {
        let text = match monitor.tick().await {
            MonitorEvent::Healthy { probe, .. } => format!(
                "本地 IPv6 状态：正常\n检测位置：Linux 指定网口\n命令：本地 DNS + ping6（不存在时使用 ping -6）\n退出码：{:?}\n耗时：{} ms",
                probe.exit_code,
                probe.duration.as_millis()
            ),
            MonitorEvent::Failed {
                consecutive_failures,
                error,
                probe,
            } => {
                let (exit_code, duration, stderr) = probe
                    .map(|probe| {
                        (
                            format!("{:?}", probe.exit_code),
                            format!("{} ms", probe.duration.as_millis()),
                            probe.stderr,
                        )
                    })
                    .unwrap_or_else(|| ("N/A".into(), "N/A".into(), String::new()));
                let detail = error.unwrap_or(stderr);
                format!(
                    "本地 IPv6 状态：失败\n检测位置：Linux 指定网口\n命令：本地 DNS + ping6（不存在时使用 ping -6）\n退出码：{exit_code}\n耗时：{duration}\n连续失败：{consecutive_failures}\n错误：{detail}"
                )
            }
        };
        self.send_message(&text).await
    }

    async fn send_recovery_result<C>(&self, monitor: &Arc<Monitor<C>>) -> Result<(), TelegramError>
    where
        C: OpenWrtClient + 'static,
    {
        match monitor.recover_and_probe().await {
            RecoveryOutcome::Recovered { probe } => {
                self.send_message(&format!(
                    "OpenWrt WAN6 已恢复，本地 IPv6 检测成功，退出码 {:?}。",
                    probe.exit_code
                ))
                .await?;
            }
            RecoveryOutcome::RecoverCommandFailed { error, stderr } => {
                self.send_message(&format!(
                    "OpenWrt WAN6 恢复失败：{} {}",
                    error.unwrap_or_default(),
                    stderr
                ))
                .await?;
            }
            RecoveryOutcome::ProbeFailed { error, probe } => {
                let detail = error
                    .or_else(|| probe.map(|p| p.stderr))
                    .unwrap_or_default();
                self.send_message(&format!("恢复后本地 IPv6 检测仍失败：{detail}"))
                    .await?;
            }
        }
        Ok(())
    }

    async fn call<T: DeserializeOwned, B: Serialize>(
        &self,
        method: &str,
        body: &B,
    ) -> Result<T, TelegramError> {
        let url = format!("https://api.telegram.org/bot{}/{}", self.token, method);
        let response: ApiResponse<T> = self.http.post(url).json(body).send().await?.json().await?;
        if response.ok {
            response
                .result
                .ok_or_else(|| TelegramError::Api("missing result".into()))
        } else {
            Err(TelegramError::Api(
                response
                    .description
                    .unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }
}

fn next_retry_delay(current: Duration) -> Duration {
    std::cmp::min(current + current, Duration::from_secs(60))
}

pub fn is_recover_command(text: &str) -> bool {
    is_command(text, "recover")
}

pub fn is_status_command(text: &str) -> bool {
    is_command(text, "status")
}

fn is_command(text: &str, expected: &str) -> bool {
    let mut parts = text.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    let Some(command) = command.strip_prefix('/') else {
        return false;
    };
    match command.split_once('@') {
        Some((name, username)) => name == expected && !username.is_empty(),
        None => command == expected,
    }
}

#[derive(Debug, Clone, Copy)]
enum BotAction {
    Status,
    Recover,
}

fn parse_command(text: &str) -> Option<BotAction> {
    if is_status_command(text) {
        Some(BotAction::Status)
    } else if is_recover_command(text) {
        Some(BotAction::Recover)
    } else {
        None
    }
}

pub fn is_recover_confirmation(data: &str) -> bool {
    data == "recover_confirm"
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct GetUpdates {
    offset: i64,
    timeout: u64,
    allowed_updates: [&'static str; 2],
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct Message {
    from: Option<User>,
    #[allow(dead_code)]
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    from: User,
    data: Option<String>,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct Chat {
    #[allow(dead_code)]
    id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_command_accepts_telegram_bot_mentions_but_not_arguments() {
        assert!(is_recover_command("/recover"));
        assert!(is_recover_command("  /recover  "));
        assert!(is_recover_command("/recover@watch_bot"));
        assert!(!is_recover_command("/recover example.com"));
        assert!(!is_recover_command("/recover@"));
        assert!(!is_recover_command("/recover; ifup wan6"));
        assert!(!is_recover_command("/bin/sh"));
    }

    #[test]
    fn status_command_accepts_telegram_bot_mentions_but_not_arguments() {
        assert!(is_status_command("/status"));
        assert!(is_status_command("  /status  "));
        assert!(is_status_command("/status@watch_bot"));
        assert!(!is_status_command("/status example.com"));
        assert!(!is_status_command("/status@"));
        assert!(!is_status_command("/status; id"));
        assert!(!is_status_command("status"));
    }

    #[test]
    fn only_fixed_callback_is_accepted() {
        assert!(is_recover_confirmation("recover_confirm"));
        assert!(!is_recover_confirmation("recover; id"));
    }
}
