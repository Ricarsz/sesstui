//! 会话模型：三个客户端共用的统一视图。

use std::path::PathBuf;
use std::time::SystemTime;

use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Client {
    Dsh,
    Codex,
    Opencode,
}

impl Client {
    pub const ALL: [Client; 3] = [Client::Dsh, Client::Codex, Client::Opencode];

    pub fn key(self) -> &'static str {
        match self {
            Client::Dsh => "dsh",
            Client::Codex => "codex",
            Client::Opencode => "opencode",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Client::Dsh => "DeepSeek Harness",
            Client::Codex => "Codex",
            Client::Opencode => "Opencode",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Client::Dsh => "DSH",
            Client::Codex => "CDX",
            Client::Opencode => "OC",
        }
    }

    pub fn from_key(name: &str) -> Option<Client> {
        match name.trim().to_ascii_lowercase().as_str() {
            "dsh" | "deepseek" | "harness" => Some(Client::Dsh),
            "codex" | "cdx" => Some(Client::Codex),
            "opencode" | "oc" => Some(Client::Opencode),
            _ => None,
        }
    }
}

/// 一个可删除的会话。`path` 的含义随客户端而定：
/// DSH 是会话目录，Codex 是 rollout 文件，Opencode 是数据库文件。
#[derive(Debug, Clone)]
pub struct Session {
    pub client: Client,
    pub id: String,
    pub title: String,
    pub preview: String,
    pub path: PathBuf,
    pub root: PathBuf,
    pub cwd: Option<String>,
    pub created: Option<SystemTime>,
    pub updated: Option<SystemTime>,
    pub bytes: u64,
    pub messages: u32,
    pub tokens: Option<(u64, u64)>,
    pub cost: Option<f64>,
    pub locked: bool,
}

impl Session {
    /// 稳定标识：`<client>:<id>`，用于多选集合。
    pub fn key(&self) -> String {
        format!("{}:{}", self.client.key(), self.id)
    }

    pub fn updated_or_epoch(&self) -> SystemTime {
        self.updated
            .or(self.created)
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// 列表行的时间列：`MM-DD HH:MM`。
    pub fn time_label(&self) -> String {
        match self.updated.or(self.created) {
            Some(time) => util::local_short(time),
            None => "--".to_string(),
        }
    }

    pub fn age_label(&self) -> String {
        match self.updated.or(self.created) {
            Some(time) => util::human_age(time),
            None => "未知".to_string(),
        }
    }

    pub fn size_label(&self) -> String {
        util::human_bytes(self.bytes)
    }
}
