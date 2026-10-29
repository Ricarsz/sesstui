//! CLI 子命令。无子命令时由 `main` 启动 TUI。

use std::path::Path;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use unicode_width::UnicodeWidthStr;

use crate::config::Config;
use crate::delete;
use crate::model::{Client, Session};
use crate::scan;
use crate::util;

#[derive(Parser)]
#[command(
    name = "sesstui",
    version,
    about = "浏览、筛选与删除本机 AI 编码客户端的会话",
    long_about = "扫描 DeepSeek Harness、Codex、Opencode 的会话存储，列出会话并在终端里\
                  多选删除。无子命令时启动 TUI。"
)]
pub struct Cli {
    /// 以 JSON 输出
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// 列出会话
    List {
        /// 只看某个客户端：dsh / codex / opencode
        #[arg(long)]
        client: Option<String>,
    },
    /// 删除会话，参数形如 `dsh:session-<uuid>`、`codex:<uuid>`、`opencode:ses_<id>`
    Delete {
        /// 会话标识；省略 `client:` 前缀时按 ID 全局匹配
        ids: Vec<String>,
        /// 只打印将要删除的内容
        #[arg(long)]
        dry_run: bool,
        /// 只删除某个客户端的会话
        #[arg(long)]
        client: Option<String>,
    },
    /// 检查扫描到的存储位置、数据库与依赖
    Doctor,
    /// 打印扫描路径
    Paths,
    /// 显示配置路径与当前值
    Config {
        /// 写入默认配置文件
        #[arg(long)]
        init: bool,
        /// 覆盖已有配置文件（配合 --init）
        #[arg(long)]
        force: bool,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => unreachable!("无子命令时由 main 启动 TUI"),
        Some(command) => dispatch(command, cli.json),
    }
}

fn dispatch(command: Cmd, json: bool) -> Result<()> {
    match command {
        Cmd::List { client } => list(client.as_deref(), json),
        Cmd::Delete {
            ids,
            dry_run,
            client,
        } => delete_cmd(&ids, dry_run, client.as_deref(), json),
        Cmd::Doctor => doctor(),
        Cmd::Paths => paths(json),
        Cmd::Config { init, force } => config_cmd(init, force, json),
    }
}

fn parse_client(name: &str) -> Result<Client> {
    Client::from_key(name)
        .ok_or_else(|| anyhow::anyhow!("未知客户端：{name}（可用 dsh / codex / opencode）"))
}

fn sessions_of(client: Option<Client>) -> Vec<Session> {
    scan::scan()
        .sessions
        .into_iter()
        .filter(|session| client.is_none_or(|wanted| session.client == wanted))
        .collect()
}

fn list(client: Option<&str>, json: bool) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    let sessions = sessions_of(client);
    if json {
        let items: Vec<_> = sessions
            .iter()
            .map(|session| {
                json!({
                    "client": session.client.key(),
                    "id": session.id,
                    "title": session.title,
                    "path": session.path.display().to_string(),
                    "cwd": session.cwd,
                    "bytes": session.bytes,
                    "messages": session.messages,
                    "locked": session.locked,
                    "tokens_input": session.tokens.map(|tokens| tokens.0),
                    "tokens_output": session.tokens.map(|tokens| tokens.1),
                    "cost": session.cost,
                    "created": session.created.map(util::local_datetime),
                    "updated": session.updated.map(util::local_datetime),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if sessions.is_empty() {
        println!("没有找到会话。用 `sesstui doctor` 检查扫描路径。");
        return Ok(());
    }

    let widths = TableWidths::new(&sessions);
    println!("{}", header_row(&widths));
    for session in &sessions {
        println!("{}", session_row(session, &widths));
    }
    let bytes: u64 = sessions.iter().map(|session| session.bytes).sum();
    println!(
        "\n共 {} 个会话 · {}\n客户端：DSH {} · Codex {} · Opencode {}",
        sessions.len(),
        util::human_bytes(bytes),
        count_client(&sessions, Client::Dsh),
        count_client(&sessions, Client::Codex),
        count_client(&sessions, Client::Opencode),
    );
    Ok(())
}

fn count_client(sessions: &[Session], client: Client) -> usize {
    sessions
        .iter()
        .filter(|session| session.client == client)
        .count()
}

struct TableWidths {
    client: usize,
    updated: usize,
    size: usize,
    messages: usize,
}

impl TableWidths {
    fn new(sessions: &[Session]) -> Self {
        Self {
            client: sessions
                .iter()
                .map(|session| session.client.label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            updated: 16,
            size: sessions
                .iter()
                .map(|session| session.size_label().len())
                .max()
                .unwrap_or(6)
                .max(6),
            messages: 5,
        }
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(text);
    if current >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - current))
}

fn header_row(widths: &TableWidths) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}",
        pad_right("客户端", widths.client),
        pad_right("更新时间", widths.updated),
        pad_right("大小", widths.size),
        pad_right("消息", widths.messages),
        pad_right("状态", 6),
        "标题"
    )
}

fn session_row(session: &Session, widths: &TableWidths) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}",
        pad_right(session.client.label(), widths.client),
        pad_right(
            &session
                .updated
                .map(util::local_datetime)
                .unwrap_or_else(|| "--".to_string()),
            widths.updated
        ),
        pad_right(&session.size_label(), widths.size),
        pad_right(&session.messages.to_string(), widths.messages),
        pad_right(
            if session.locked {
                "使用中"
            } else {
                "空闲"
            },
            6
        ),
        session.title
    )
}

/// 支持 `client:id` 前缀，也接受裸 ID。
fn resolve(sessions: &[Session], spec: &str) -> Result<Session> {
    let (client, id) = match spec.split_once(':') {
        Some((prefix, rest)) if Client::from_key(prefix).is_some() => {
            (Client::from_key(prefix), rest)
        }
        _ => (None, spec),
    };
    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id == id && client.is_none_or(|wanted| session.client == wanted))
        .collect();
    match matches.len() {
        0 => bail!("没有找到会话：{spec}"),
        1 => Ok(matches[0].clone()),
        _ => bail!(
            "{spec} 匹配到多个会话，请加客户端前缀（{}）",
            matches
                .iter()
                .map(|session| format!("{}:{}", session.client.key(), session.id))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
    }
}

fn delete_cmd(ids: &[String], dry_run: bool, client: Option<&str>, json: bool) -> Result<()> {
    let client = client.map(parse_client).transpose()?;
    if ids.is_empty() {
        bail!("请给出至少一个会话 ID（或先在 TUI 里用 `S` 复制）");
    }
    let all = sessions_of(client);
    let mut targets = Vec::new();
    for spec in ids {
        targets.push(resolve(&all, spec)?);
    }
    let report = delete::delete_many(&targets, dry_run);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": dry_run,
                "deleted": report.deleted,
                "bytes": report.bytes,
                "errors": report.errors,
            }))?
        );
    } else {
        for session in &targets {
            let status = if dry_run {
                "将删除"
            } else if report.failed.contains(&session.key()) {
                "跳过"
            } else {
                "已删除"
            };
            println!(
                "{status} {} · {}（{}）",
                session.client.label(),
                session.title,
                util::human_bytes(session.bytes)
            );
        }
        println!(
            "\n{} 个 · 释放 {}{}",
            report.deleted,
            util::human_bytes(report.bytes),
            if dry_run { "（演练）" } else { "" }
        );
        for error in &report.errors {
            println!("错误：{error}");
        }
    }

    if report.failed() {
        std::process::exit(1);
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let snapshot = scan::scan();
    println!("扫描结果\n");
    if snapshot.roots.is_empty() {
        println!("  没有找到任何根目录。");
    }
    for root in &snapshot.roots {
        println!(
            "  [{}] {}\n        会话 {} 个{}",
            root.client.key(),
            root.path.display(),
            root.sessions,
            root.error
                .as_ref()
                .map(|error| format!("\n        错误：{error}"))
                .unwrap_or_default()
        );
    }
    println!(
        "\n合计 {} 个会话 · {}",
        snapshot.sessions.len(),
        util::human_bytes(snapshot.bytes(None))
    );

    let zstd = which("zstd");
    println!(
        "\n依赖\n  zstd 二进制：{}（仅在内置解码失败时兜底）",
        zstd.map(|path| path.display().to_string())
            .unwrap_or_else(|| "未找到".to_string())
    );
    for root in &snapshot.roots {
        if !root.path.exists() {
            println!("  {} 不存在", root.path.display());
            continue;
        }
        // Opencode 的“根”是数据库文件，写探测落在父目录。
        let target = if root.path.is_dir() {
            root.path.clone()
        } else {
            root.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        let writable = probe_writable(&target);
        println!(
            "  {} 可写：{}",
            root.path.display(),
            if writable { "是" } else { "否" }
        );
    }
    for warning in &snapshot.warnings {
        println!("  警告：{warning}");
    }
    println!("\n配置：{}", Config::path().display());
    Ok(())
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn probe_writable(path: &Path) -> bool {
    let probe = path.join(".sesstui-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn paths(json: bool) -> Result<()> {
    let snapshot = scan::scan();
    if json {
        let items: Vec<_> = snapshot
            .roots
            .iter()
            .map(|root| {
                json!({
                    "client": root.client.key(),
                    "root": root.path.display().to_string(),
                    "sessions": root.sessions,
                    "error": root.error,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for root in &snapshot.roots {
            println!(
                "{:<10} {}  ({} 个)",
                root.client.key(),
                root.path.display(),
                root.sessions
            );
        }
    }
    Ok(())
}

fn config_cmd(init: bool, force: bool, json: bool) -> Result<()> {
    if init {
        let path = Config::init(force)?;
        println!("已写入 {}", path.display());
        return Ok(());
    }
    let path = Config::path();
    let config = Config::load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }
    println!(
        "配置：{}（{}）",
        path.display(),
        if path.exists() { "存在" } else { "默认值" }
    );
    println!("  theme            = {}", config.theme);
    println!("  mouse            = {}", config.mouse);
    println!("  rescan_secs      = {}", config.rescan_secs);
    println!("  confirm_delete   = {}", config.confirm_delete);
    Ok(())
}
