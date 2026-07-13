use std::sync::Arc;

use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::{
    client::OpenWrtClient,
    monitor::{Monitor, RecoveryOutcome},
};

#[derive(Debug, Error)]
pub enum TelegramError {
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
}

impl TelegramBot {
    pub fn from_env(config: &crate::config::TelegramConfig) -> Result<Self, std::env::VarError> {
        let token = std::env::var(&config.bot_token_env)?;
        Ok(Self {
            http: Client::new(),
            token,
            chat_id: config.chat_id,
            allowed_user_ids: config.allowed_user_ids.clone().into(),
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
        loop {
            let updates: Vec<Update> = self
                .call(
                    "getUpdates",
                    &GetUpdates {
                        offset,
                        timeout: 20,
                        allowed_updates: ["message", "callback_query"],
                    },
                )
                .await?;
            for update in updates {
                offset = update.update_id + 1;
                self.handle_update(&monitor, &update).await?;
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
        let (user_id, recover_requested, chat_allowed) = if let Some(message) = &update.message {
            (
                message.from.as_ref().map(|u| u.id),
                message
                    .text
                    .as_deref()
                    .map(is_recover_command)
                    .unwrap_or(false),
                message.chat.id == self.chat_id,
            )
        } else if let Some(callback) = &update.callback_query {
            (
                Some(callback.from.id),
                callback
                    .data
                    .as_deref()
                    .map(is_recover_confirmation)
                    .unwrap_or(false),
                callback
                    .message
                    .as_ref()
                    .is_some_and(|message| message.chat.id == self.chat_id),
            )
        } else {
            return Ok(());
        };

        if !chat_allowed || !recover_requested || !user_id.is_some_and(|id| self.is_allowed(id)) {
            warn!(
                ?user_id,
                "ignored unauthorized or unsupported Telegram input"
            );
            return Ok(());
        }

        info!("authorized Telegram recovery request accepted");
        match monitor.recover_and_probe().await {
            RecoveryOutcome::Recovered { probe } => {
                self.send_message(&format!(
                    "OpenWrt IPv6 已恢复，远端 probe 退出码 {:?}。",
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
                self.send_message(&format!("恢复后远端 IPv6 检测仍失败：{detail}"))
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

pub fn is_recover_command(text: &str) -> bool {
    text.trim() == "/recover"
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
    fn only_exact_recover_command_is_accepted() {
        assert!(is_recover_command("/recover"));
        assert!(is_recover_command("  /recover  "));
        assert!(!is_recover_command("/recover example.com"));
        assert!(!is_recover_command("/recover; ifup wan6"));
        assert!(!is_recover_command("/bin/sh"));
    }

    #[test]
    fn only_fixed_callback_is_accepted() {
        assert!(is_recover_confirmation("recover_confirm"));
        assert!(!is_recover_confirmation("recover; id"));
    }
}
