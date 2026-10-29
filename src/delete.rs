//! 删除：按客户端各自的存储方式清理，并做路径与锁的安全检查。

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::{Client, Session};
use crate::util;

pub struct Report {
    pub deleted: usize,
    pub bytes: u64,
    /// 失败会话的 `client:id`，供界面精确清理勾选状态。
    pub failed: Vec<String>,
    pub errors: Vec<String>,
}

impl Report {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// 删除若干个已选会话；`dry_run` 只统计不落盘。
pub fn delete_many(sessions: &[Session], dry_run: bool) -> Report {
    let mut report = Report {
        deleted: 0,
        bytes: 0,
        failed: Vec::new(),
        errors: Vec::new(),
    };
    for session in sessions {
        let outcome = if dry_run {
            Ok(session.bytes)
        } else {
            delete(session)
        };
        match outcome {
            Ok(bytes) => {
                report.deleted += 1;
                report.bytes += bytes;
            }
            Err(error) => {
                report.failed.push(session.key());
                report.errors.push(format!(
                    "{} · {}：{error}",
                    session.client.label(),
                    util::truncate(&session.title, 40)
                ));
            }
        }
    }
    report
}

pub fn delete(session: &Session) -> Result<u64> {
    match session.client {
        Client::Dsh => delete_dsh(session),
        Client::Codex => delete_codex(session),
        Client::Opencode => delete_opencode(session),
    }
}

fn delete_dsh(session: &Session) -> Result<u64> {
    let dir = &session.path;
    let allowed = session.root.join("sessions");
    ensure_inside(dir, &allowed)?;
    if !dir
        .file_name()
        .map(|name| name.to_string_lossy().starts_with("session-"))
        .unwrap_or(false)
    {
        bail!("不是会话目录：{}", dir.display());
    }
    if util::lock_held(&dir.join("session.lock")) {
        bail!("会话正在使用中（session.lock 被持有），已跳过");
    }
    let bytes = util::dir_size(dir);
    fs::remove_dir_all(dir).with_context(|| format!("删除 {}", dir.display()))?;
    // 工作区目录空了就一并收掉，避免残留空壳。
    if let Some(parent) = dir.parent()
        && let Ok(mut entries) = fs::read_dir(parent)
        && entries.next().is_none()
    {
        let _ = fs::remove_dir(parent);
    }
    Ok(bytes)
}

fn delete_codex(session: &Session) -> Result<u64> {
    let file = &session.path;
    let allowed = session.root.join("sessions");
    ensure_inside(file, &allowed)?;
    if !file
        .file_name()
        .map(|name| {
            let name = name.to_string_lossy();
            name.starts_with("rollout-") && name.ends_with(".jsonl")
        })
        .unwrap_or(false)
    {
        bail!("不是 rollout 文件：{}", file.display());
    }
    let bytes = fs::metadata(file).map(|meta| meta.len()).unwrap_or(0);
    fs::remove_file(file).with_context(|| format!("删除 {}", file.display()))?;
    drop_codex_index(&session.root.join("session_index.jsonl"), &session.id)?;
    Ok(bytes)
}

/// 从 `session_index.jsonl` 里摘掉对应条目，写回时用临时文件替换。
fn drop_codex_index(index: &Path, id: &str) -> Result<()> {
    let Ok(text) = fs::read_to_string(index) else {
        return Ok(());
    };
    let mut kept = String::with_capacity(text.len());
    for line in text.lines() {
        let matches = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| value == id)
            })
            .unwrap_or(false);
        if !matches {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if kept.len() == text.len() {
        return Ok(());
    }
    let tmp = index.with_extension("jsonl.tmp");
    {
        let mut file = fs::File::create(&tmp).with_context(|| format!("写入 {}", tmp.display()))?;
        file.write_all(kept.as_bytes())?;
        file.sync_all().ok();
    }
    fs::rename(&tmp, index).with_context(|| format!("替换 {}", index.display()))?;
    Ok(())
}

fn delete_opencode(session: &Session) -> Result<u64> {
    let db = &session.path;
    if db
        .file_name()
        .map(|name| name != "opencode.db")
        .unwrap_or(true)
    {
        bail!("不是 opencode 数据库：{}", db.display());
    }
    let conn = rusqlite::Connection::open(db).with_context(|| format!("打开 {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let tx = conn.unchecked_transaction()?;
    let removed = tx.execute("DELETE FROM session_v2 WHERE id = ?1", [&session.id])?;
    tx.commit()?;
    if removed == 0 {
        bail!("数据库里没有该会话，可能已被删除");
    }
    Ok(session.bytes)
}

/// 规范化后确认 `child` 位于 `parent` 内，防止脏数据导致误删。
fn ensure_inside(child: &Path, parent: &Path) -> Result<()> {
    let child = child
        .canonicalize()
        .with_context(|| format!("解析 {}", child.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("解析 {}", parent.display()))?;
    if !child.starts_with(&parent) {
        bail!(
            "拒绝删除 {}：不在 {} 之内",
            child.display(),
            parent.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_outside_root() {
        let error = ensure_inside(Path::new("/etc"), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("拒绝删除"));
    }
}
