//! 扫描本机的会话存储：
//!
//! - DeepSeek Harness：`~/.dsh*/sessions/<slug>/session-<uuid>/session*.jsonl.zstd`
//! - Codex：`~/.codex*/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! - Opencode：`~/.local/share/opencode/opencode.db`（SQLite）
//!
//! 目录位置可由 `SESSTUI_DSH_ROOT` / `SESSTUI_CODEX_HOME` / `SESSTUI_OPENCODE_DIR`
//! 覆盖，多个路径用 `:` 分隔，便于扫描代理版等独立实例。

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{Client, Session};
use crate::util;

#[derive(Debug, Clone)]
pub struct Root {
    pub client: Client,
    pub path: PathBuf,
    pub sessions: usize,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
}

impl Snapshot {
    pub fn bytes(&self, client: Option<Client>) -> u64 {
        self.sessions
            .iter()
            .filter(|session| client.is_none_or(|c| session.client == c))
            .map(|session| session.bytes)
            .sum()
    }
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_roots(name: &str) -> Vec<PathBuf> {
    env::var_os(name)
        .map(|raw| {
            env::split_paths(&raw)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `$HOME` 下所有名字形如 `<prefix>*` 且包含 `sessions/` 的目录。
fn sibling_roots(prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home()) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("sessions").is_dir())
        .collect();
    found.sort();
    found
}

pub fn scan() -> Snapshot {
    let mut snapshot = Snapshot::default();
    scan_dsh(&mut snapshot);
    scan_codex(&mut snapshot);
    scan_opencode(&mut snapshot);
    snapshot.sessions.sort_by(|a, b| {
        b.updated_or_epoch()
            .cmp(&a.updated_or_epoch())
            .then_with(|| a.client.cmp(&b.client))
    });
    dedupe_storage(&mut snapshot);
    snapshot
}

/// 同一份存储在 `$HOME` 下可能有多个入口（例如代理实例把 `sessions`
/// 符号链接到主目录）。这里按规范化路径合并，避免同一个会话重复列出。
fn dedupe_storage(snapshot: &mut Snapshot) {
    let mut root_map: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut kept_roots = Vec::with_capacity(snapshot.roots.len());
    let mut aliases = Vec::new();
    for root in std::mem::take(&mut snapshot.roots) {
        let key = storage_key(root.client, &root.path);
        match root_map.get(&key) {
            Some(first) => aliases.push(format!(
                "{} 与 {} 指向同一存储，已合并",
                root.path.display(),
                first.display()
            )),
            None => {
                root_map.insert(key, root.path.clone());
                kept_roots.push(root);
            }
        }
    }
    snapshot.roots = kept_roots;
    snapshot.warnings.extend(aliases);

    // 会话重复：codex 的别名实例会扫到同一批复刻文件；opencode 的 db 是同一个文件，按 ID 区分。
    let mut seen = HashSet::new();
    snapshot.sessions.retain(|session| match session.client {
        Client::Opencode => true,
        _ => seen.insert(canonical(&session.path)),
    });
    for session in &mut snapshot.sessions {
        if let Some(root) = root_map.get(&storage_key(session.client, &session.root)) {
            session.root = root.clone();
        }
    }
}

/// 判断两个根目录是否指向同一份存储：DSH/Codex 看 `sessions/`，Opencode 看数据库文件。
fn storage_key(client: Client, root: &Path) -> PathBuf {
    match client {
        Client::Opencode => canonical(root),
        Client::Dsh | Client::Codex => canonical(&root.join("sessions")),
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------- DSH

fn scan_dsh(snapshot: &mut Snapshot) {
    let mut roots = env_roots("SESSTUI_DSH_ROOT");
    if roots.is_empty() {
        roots = sibling_roots(".dsh");
    }
    for root in roots {
        let base = root.join("sessions");
        let mut root_info = Root {
            client: Client::Dsh,
            path: root.clone(),
            sessions: 0,
            error: None,
        };
        let mut entries: Vec<PathBuf> = fs::read_dir(&base)
            .map(|reads| {
                reads
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        for bucket in entries {
            let Ok(sessions) = fs::read_dir(&bucket) else {
                continue;
            };
            for session_dir in sessions.flatten() {
                let dir = session_dir.path();
                if !dir.is_dir() {
                    continue;
                }
                match load_dsh_session(&dir, &root) {
                    Ok(Some(session)) => {
                        root_info.sessions += 1;
                        snapshot.sessions.push(session);
                    }
                    Ok(None) => {}
                    Err(error) => snapshot
                        .warnings
                        .push(format!("{}：{error}", dir.display())),
                }
            }
        }
        snapshot.roots.push(root_info);
    }
}

fn load_dsh_session(dir: &Path, root: &Path) -> Result<Option<Session>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("读取 {}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    let payload = files
        .iter()
        .find(|path| path.to_string_lossy().ends_with(".jsonl.zstd"))
        .or_else(|| {
            files
                .iter()
                .find(|path| path.to_string_lossy().ends_with(".jsonl"))
        });

    let mut id = String::new();
    let mut cwd = None;
    let mut created = None;
    let mut title = String::new();
    let mut preview = String::new();
    let mut messages = 0u32;

    if let Some(payload) = payload {
        let mut reader = open_maybe_zstd(payload)?;
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            let trimmed = line.trim_end();
            if !trimmed.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(trimmed)
            {
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "session" => {
                        if id.is_empty() {
                            id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                        }
                        if created.is_none() {
                            created = value
                                .get("createdAt")
                                .and_then(Value::as_i64)
                                .and_then(util::system_time_from_millis);
                        }
                        if cwd.is_none() {
                            cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
                        }
                    }
                    "session/title" => {
                        if let Some(text) = value
                            .pointer("/data/title")
                            .and_then(Value::as_str)
                            .filter(|text| !text.trim().is_empty())
                        {
                            title = util::one_line(text);
                        }
                    }
                    "user/message" => {
                        messages += 1;
                        if preview.is_empty()
                            && let Some(text) = dsh_user_text(&value)
                        {
                            preview = text;
                        }
                    }
                    "assistant/message" => messages += 1,
                    _ => {}
                }
            }
            line.clear();
        }
    }

    if id.is_empty() {
        // 目录名形如 session-<uuid>。
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if let Some(rest) = name.strip_prefix("session-") {
            id = format!("session-{rest}");
        } else {
            id = name.to_string();
        }
    }

    let mut updated = None;
    let mut bytes = 0;
    for file in &files {
        if let Ok(meta) = file.metadata() {
            bytes += meta.len();
            if let Ok(modified) = meta.modified() {
                updated = Some(updated.map_or(modified, |prev: SystemTime| prev.max(modified)));
            }
        }
    }

    let locked = util::lock_held(&dir.join("session.lock"));
    let title = if title.is_empty() {
        if preview.is_empty() {
            "(无标题)".to_string()
        } else {
            util::truncate(&preview, 60)
        }
    } else {
        title
    };

    Ok(Some(Session {
        client: Client::Dsh,
        id,
        title,
        preview,
        path: dir.to_path_buf(),
        root: root.to_path_buf(),
        cwd,
        created,
        updated,
        bytes,
        messages,
        tokens: None,
        cost: None,
        locked,
    }))
}

/// 取一条真实用户消息：跳过 runtime context、system-reminder 之类的系统注入。
fn dsh_user_text(record: &Value) -> Option<String> {
    let source = record
        .pointer("/data/source/kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source != "user" {
        return None;
    }
    let parts = record.pointer("/data/content").and_then(Value::as_array)?;
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = util::one_line(text);
        if text.is_empty() || looks_like_injection(&text) {
            continue;
        }
        return Some(util::truncate(&text, 160));
    }
    None
}

fn looks_like_injection(text: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "Current runtime context",
        "<system-reminder>",
        "<environment_context>",
        "<user_instructions>",
        "Time sampled while preparing",
        "Approval prompts are disabled",
        "Current DSH file policy",
        "Time sampled",
    ];
    let head = text.chars().take(80).collect::<String>();
    MARKERS.iter().any(|marker| head.starts_with(marker))
}

fn open_maybe_zstd(path: &Path) -> Result<Box<dyn BufRead>> {
    let file = fs::File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    if path.to_string_lossy().ends_with(".zstd") {
        return Ok(Box::new(BufReader::new(ZstdReader::new(
            path.to_path_buf(),
            BufReader::new(file),
        ))));
    }
    Ok(Box::new(BufReader::new(file)))
}

/// DSH 把 JSONL 分块追加成 zstd 流（一个文件里几十上百帧），
/// 而 `StreamingDecoder` 只解一帧；这里在一帧结束后重建解码器接着解下一帧。
/// 首帧就失败时退回系统 `zstd -dc`。
struct ZstdReader {
    path: PathBuf,
    decoder: Option<
        ruzstd::decoding::StreamingDecoder<BufReader<fs::File>, ruzstd::decoding::FrameDecoder>,
    >,
    child: Option<BufReader<std::process::ChildStdout>>,
    started: bool,
}

impl ZstdReader {
    fn new(path: PathBuf, file: BufReader<fs::File>) -> Self {
        let decoder = ruzstd::decoding::StreamingDecoder::new(file).ok();
        Self {
            path,
            decoder,
            child: None,
            started: false,
        }
    }

    fn fallback(&mut self, buf: &mut [u8], error: std::io::Error) -> std::io::Result<usize> {
        let Ok(mut child) = Command::new("zstd")
            .args(["-dc"])
            .arg(&self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return Err(error);
        };
        let Some(stdout) = child.stdout.take() else {
            return Err(error);
        };
        let mut reader = BufReader::new(stdout);
        let read = reader.read(buf)?;
        self.child = Some(reader);
        self.decoder = None;
        self.started = true;
        Ok(read)
    }

    /// 当前帧已解完，换到下一帧；返回 false 表示流结束。
    fn next_frame(&mut self) -> bool {
        let Some(decoder) = self.decoder.take() else {
            return false;
        };
        let (reader, _) = decoder.into_parts();
        match ruzstd::decoding::StreamingDecoder::new(reader) {
            Ok(decoder) => {
                self.decoder = Some(decoder);
                true
            }
            Err(_) => false,
        }
    }
}

impl Read for ZstdReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(child) = self.child.as_mut() {
            return child.read(buf);
        }
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(0);
            };
            match decoder.read(buf) {
                Ok(0) => {
                    if !self.next_frame() {
                        return Ok(0);
                    }
                }
                Ok(read) => {
                    self.started = true;
                    return Ok(read);
                }
                Err(error) if self.started => return Err(error),
                Err(error) => return self.fallback(buf, error),
            }
        }
    }
}

// ---------------------------------------------------------------- Codex

fn scan_codex(snapshot: &mut Snapshot) {
    let mut roots = env_roots("SESSTUI_CODEX_HOME");
    if roots.is_empty() {
        roots = sibling_roots(".codex");
    }
    for root in roots {
        let mut root_info = Root {
            client: Client::Codex,
            path: root.clone(),
            sessions: 0,
            error: None,
        };
        let titles = codex_titles(&root.join("session_index.jsonl"));
        let mut files = Vec::new();
        collect_rollouts(&root.join("sessions"), 0, &mut files);
        files.sort();
        for file in files {
            match load_codex_session(&file, &root, &titles) {
                Ok(session) => {
                    root_info.sessions += 1;
                    snapshot.sessions.push(session);
                }
                Err(error) => snapshot
                    .warnings
                    .push(format!("{}：{error}", file.display())),
            }
        }
        snapshot.roots.push(root_info);
    }
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, depth + 1, out);
        } else if path
            .file_name()
            .map(|name| {
                let name = name.to_string_lossy();
                name.starts_with("rollout-") && name.ends_with(".jsonl")
            })
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn codex_titles(index: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(index) else {
        return map;
    };
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let (Some(id), Some(title)) = (
                value.get("id").and_then(Value::as_str),
                value.get("thread_name").and_then(Value::as_str),
            ) else {
                continue;
            };
            if !title.trim().is_empty() {
                map.insert(id.to_string(), util::one_line(title));
            }
        }
    }
    map
}

fn load_codex_session(
    file: &Path,
    root: &Path,
    titles: &HashMap<String, String>,
) -> Result<Session> {
    let mut id = String::new();
    let mut cwd = None;
    let mut created = None;
    let mut preview = String::new();
    let mut messages = 0u32;

    let handle = fs::File::open(file).with_context(|| format!("打开 {}", file.display()))?;
    for line in BufReader::new(handle).lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        match kind {
            "session_meta" => {
                if id.is_empty() {
                    id = payload
                        .get("session_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                if cwd.is_none() {
                    cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                if created.is_none() {
                    created = payload
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339);
                }
            }
            "response_item" if payload.get("type").and_then(Value::as_str) == Some("message") => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                if role == "user" || role == "assistant" {
                    messages += 1;
                }
                if role == "user"
                    && preview.is_empty()
                    && let Some(text) = codex_message_text(&payload)
                    && !looks_like_injection(&text)
                {
                    preview = util::truncate(&text, 160);
                }
            }
            _ => {}
        }
    }

    if id.is_empty() {
        id = codex_id_from_name(file);
    }

    let meta = fs::metadata(file)?;
    let updated = meta.modified().ok();
    let title = titles
        .get(&id)
        .cloned()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            if preview.is_empty() {
                "(无标题)".to_string()
            } else {
                util::truncate(&preview, 60)
            }
        });

    Ok(Session {
        client: Client::Codex,
        id,
        title,
        preview,
        path: file.to_path_buf(),
        root: root.to_path_buf(),
        cwd,
        created,
        updated,
        bytes: meta.len(),
        messages,
        tokens: None,
        cost: None,
        locked: false,
    })
}

fn codex_message_text(payload: &Value) -> Option<String> {
    let parts = payload.get("content")?.as_array()?;
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "input_text" | "text" | "output_text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = util::one_line(text);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn codex_id_from_name(file: &Path) -> String {
    let name = file.file_stem().unwrap_or_default().to_string_lossy();
    // rollout-2026-01-01T00-00-00-<uuid>
    if name.len() >= 36 {
        let tail = &name[name.len() - 36..];
        if tail.chars().filter(|c| *c == '-').count() == 4 {
            return tail.to_string();
        }
    }
    name.to_string()
}

/// 极简 RFC3339 解析：`2026-10-29T10:31:12.701Z` / `+08:00`。
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    use time_like::days_from_civil;
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    let mut epoch = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    // 时区偏移
    let rest = &text[19..];
    if let Some(offset) = rest.strip_suffix('Z').map(|_| 0) {
        epoch -= offset;
    } else if rest.len() >= 6 {
        let sign = rest.as_bytes()[0];
        if sign == b'+' || sign == b'-' {
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = rest.get(4..6)?.parse().ok()?;
            let offset = hours * 3600 + minutes * 60;
            epoch -= if sign == b'+' { offset } else { -offset };
        }
    }
    if epoch < 0 {
        return None;
    }
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(epoch as u64))
}

mod time_like {
    /// Howard Hinnant 的 civil_from_days 逆运算。
    pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let yoe = year - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}

// ---------------------------------------------------------------- Opencode

fn scan_opencode(snapshot: &mut Snapshot) {
    let mut roots = env_roots("SESSTUI_OPENCODE_DIR");
    if roots.is_empty() {
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| Some(home().join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from(".local/share"));
        roots = vec![data.join("opencode")];
    }
    for root in roots {
        let db = root.join("opencode.db");
        let mut root_info = Root {
            client: Client::Opencode,
            path: db.clone(),
            sessions: 0,
            error: None,
        };
        if !db.is_file() {
            root_info.error = Some("未找到 opencode.db".to_string());
            snapshot.roots.push(root_info);
            continue;
        }
        match load_opencode_sessions(&db, &root) {
            Ok(sessions) => {
                root_info.sessions = sessions.len();
                snapshot.sessions.extend(sessions);
            }
            Err(error) => root_info.error = Some(format!("{error:#}")),
        }
        snapshot.roots.push(root_info);
    }
}

fn load_opencode_sessions(db: &Path, root: &Path) -> Result<Vec<Session>> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {}", db.display()))?;
    let mut stmt = conn.prepare(
        "SELECT s.id, COALESCE(s.title, ''), s.directory, s.time_created, s.time_updated,
                s.tokens_input, s.tokens_output, s.cost,
                (SELECT COUNT(*) FROM session_message m WHERE m.session_id = s.id),
                (SELECT COALESCE(SUM(LENGTH(m.data)), 0) FROM session_message m WHERE m.session_id = s.id)
         FROM session_v2 s
         ORDER BY s.time_updated DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        let (id, title, directory, created, updated, tokens_in, tokens_out, cost, messages, bytes) =
            row?;
        let title = if title.trim().is_empty() {
            "(无标题)".to_string()
        } else {
            util::one_line(&title)
        };
        sessions.push(Session {
            client: Client::Opencode,
            id,
            title,
            preview: String::new(),
            path: db.to_path_buf(),
            root: root.to_path_buf(),
            cwd: Some(directory),
            created: util::system_time_from_millis(created),
            updated: util::system_time_from_millis(updated),
            bytes: bytes.max(0) as u64,
            messages: messages.max(0) as u32,
            tokens: Some((tokens_in.max(0) as u64, tokens_out.max(0) as u64)),
            cost: Some(cost),
            locked: false,
        });
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_with_zulu() {
        let time = parse_rfc3339("2026-10-29T10:31:12.701Z").unwrap();
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_793_269_872);
    }

    #[test]
    fn codex_id_falls_back_to_stem() {
        let path = Path::new(
            "/tmp/rollout-2026-10-29T18-30-58-01a18736-81c4-7400-837e-fa5b3fe08842.jsonl",
        );
        assert_eq!(
            codex_id_from_name(path),
            "01a18736-81c4-7400-837e-fa5b3fe08842"
        );
    }

    #[test]
    fn skips_injected_user_messages() {
        let record = serde_json::json!({
            "type": "user/message",
            "data": {
                "source": {"kind": "user"},
                "content": [{"type": "text", "text": "<system-reminder>hi</system-reminder>"}]
            }
        });
        assert!(dsh_user_text(&record).is_none());
    }
}
