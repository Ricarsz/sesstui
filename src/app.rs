//! 交互状态：列表、多选、过滤、删除确认与周期性重扫。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::config::Config;
use crate::delete;
use crate::model::{self, Client, Session, Workspace, WorkspaceGroup};
use crate::scan::{self, Root};
use crate::theme::Palette;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
}

impl Default for Notice {
    fn default() -> Self {
        Self {
            text: "就绪 · 空格多选 · d 删除".to_string(),
            kind: NoticeKind::Info,
        }
    }
}

/// 主视图：会话列表，或按工作区聚合的列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Sessions,
    Workspaces,
}

impl View {
    pub fn is_workspaces(self) -> bool {
        self == View::Workspaces
    }
}

/// 删除确认框的目标：若干会话，或一整个工作区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmTarget {
    Sessions(Vec<usize>),
    Workspace(String),
}

#[derive(Debug, Clone)]
pub struct Confirm {
    pub target: ConfirmTarget,
    pub dry_run: bool,
}

pub struct App {
    pub sessions: Vec<Session>,
    /// 扫描到的工作区登记项，包含还没有会话的。
    pub known_workspaces: Vec<Workspace>,
    /// 按当前视图与过滤条件聚合出的工作区行。
    pub workspaces: Vec<WorkspaceGroup>,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
    /// 过滤后可见的会话下标，顺序即列表顺序。
    pub rows: Vec<usize>,
    pub selected: usize,
    pub workspace_selected: usize,
    pub marked: BTreeSet<String>,
    pub filter: String,
    pub filter_editing: bool,
    /// 只显示某个工作区下的会话；由工作区视图按 Enter 设置。
    pub workspace_filter: Option<String>,
    pub client_filter: Option<Client>,
    pub view: View,
    pub modal: Option<Confirm>,
    pub show_help: bool,
    pub help_scroll: u16,
    pub detail_scroll: u16,
    pub detail_max: u16,
    pub notice: Notice,
    pub palette: Palette,
    pub config: Config,
    pub dirty: bool,
    pub quitting: bool,
    pub list_area: Rect,
    pub list_offset: usize,
    pub workspace_list_offset: usize,
    last_scan: Instant,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self::from_snapshot(scan::scan(), config)
    }

    pub fn from_snapshot(snapshot: scan::Snapshot, config: Config) -> Self {
        let palette = Palette::from_name(&config.theme);
        let mut app = Self {
            sessions: snapshot.sessions,
            known_workspaces: snapshot.workspaces,
            workspaces: Vec::new(),
            roots: snapshot.roots,
            warnings: snapshot.warnings,
            rows: Vec::new(),
            selected: 0,
            workspace_selected: 0,
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_editing: false,
            workspace_filter: None,
            client_filter: None,
            view: View::Sessions,
            modal: None,
            show_help: false,
            help_scroll: 0,
            detail_scroll: 0,
            detail_max: 0,
            notice: Notice::default(),
            palette,
            config,
            dirty: true,
            quitting: false,
            list_area: Rect::default(),
            list_offset: 0,
            workspace_list_offset: 0,
            last_scan: Instant::now(),
        };
        app.rebuild_rows();
        let total = app.sessions.len();
        let workspaces = app.workspaces.len();
        let warnings = app.warnings.len();
        if warnings > 0 {
            app.set_notice(
                NoticeKind::Warn,
                format!(
                    "{total} 个会话 · {workspaces} 个工作区 · {warnings} 条扫描警告（? 查看帮助）"
                ),
            );
        } else {
            app.set_notice(
                NoticeKind::Info,
                format!("{total} 个会话 · {workspaces} 个工作区 · 就绪"),
            );
        }
        app
    }

    pub fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Notice {
            text: text.into(),
            kind,
        };
    }

    pub fn reload(&mut self) {
        // 先记下当前选中项，重扫后按会话标识恢复；删除会让下标整体错位。
        let previous = self.current().map(|session| session.key());
        let snapshot = scan::scan();
        self.sessions = snapshot.sessions;
        self.known_workspaces = snapshot.workspaces;
        self.roots = snapshot.roots;
        self.warnings = snapshot.warnings;
        // 勾选集合里可能还留着刚被删掉的会话。
        let existing: BTreeSet<String> = self.sessions.iter().map(Session::key).collect();
        self.marked.retain(|key| existing.contains(key));
        self.rebuild_rows();
        if let Some(key) = previous
            && let Some(position) = self
                .rows
                .iter()
                .position(|index| self.sessions[*index].key() == key)
        {
            self.selected = position;
        }
        self.last_scan = Instant::now();
        self.dirty = true;
    }

    /// 依据过滤条件重建工作区与会话行，尽量保持选中项不变。
    pub fn rebuild_rows(&mut self) {
        self.rebuild_workspaces();

        // rows 可能来自上一轮扫描（会话已被删除），下标要当成不可信输入。
        let previous = self
            .rows
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
            .map(|session| session.key());
        let filter = self.filter.trim().to_lowercase();
        let client = self.client_filter;
        let workspace_key = self.workspace_filter.clone();
        self.rows = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                if let Some(client) = client
                    && session.client != client
                {
                    return false;
                }
                if let Some(key) = &workspace_key
                    && &session.workspace_group_key() != key
                {
                    return false;
                }
                if filter.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{} {} {} {} {}",
                    session.title,
                    session.id,
                    session.cwd.clone().unwrap_or_default(),
                    session
                        .workspace
                        .as_ref()
                        .map(|w| w.path.as_str())
                        .unwrap_or(""),
                    session.client.label()
                )
                .to_lowercase();
                haystack.contains(&filter)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = previous
            .and_then(|key| {
                self.rows
                    .iter()
                    .position(|index| self.sessions[*index].key() == key)
            })
            .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
        if self.rows.is_empty() {
            self.selected = 0;
        }
    }

    /// 聚合工作区行，并应用客户端与关键字过滤。
    pub fn rebuild_workspaces(&mut self) {
        let previous = self
            .workspaces
            .get(self.workspace_selected)
            .map(|group| group.key.clone());
        let client = self.client_filter;
        let filter = self.filter.trim().to_lowercase();
        self.workspaces = model::group_workspaces(&self.sessions, &self.known_workspaces);
        self.workspaces.retain(|group| {
            if let Some(client) = client
                && group.client != client
            {
                return false;
            }
            filter.is_empty() || group.matches(&filter)
        });
        self.workspace_selected = previous
            .and_then(|key| self.workspaces.iter().position(|group| group.key == key))
            .unwrap_or_else(|| {
                self.workspace_selected
                    .min(self.workspaces.len().saturating_sub(1))
            });
        if self.workspaces.is_empty() {
            self.workspace_selected = 0;
        }
        // 钻取中的工作区被删掉（或过滤掉）后，会话列表不再按它过滤。
        if let Some(key) = &self.workspace_filter
            && !self.workspaces.iter().any(|group| &group.key == key)
        {
            self.workspace_filter = None;
        }
    }

    pub fn current(&self) -> Option<&Session> {
        self.rows
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
    }

    pub fn current_workspace(&self) -> Option<&WorkspaceGroup> {
        self.workspaces.get(self.workspace_selected)
    }

    /// 会话视图正在钻取的某个工作区。
    pub fn filtered_workspace(&self) -> Option<&WorkspaceGroup> {
        let key = self.workspace_filter.as_ref()?;
        self.workspaces.iter().find(|group| &group.key == key)
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    pub fn marked_bytes(&self) -> u64 {
        self.sessions
            .iter()
            .filter(|session| self.marked.contains(&session.key()))
            .map(|session| session.bytes)
            .sum()
    }

    /// 确认框里要展示的会话。
    pub fn confirm_sessions(&self, target: &ConfirmTarget) -> Vec<&Session> {
        let indices: Vec<usize> = match target {
            ConfirmTarget::Sessions(indices) => indices.clone(),
            ConfirmTarget::Workspace(key) => self
                .workspaces
                .iter()
                .find(|group| &group.key == key)
                .map(|group| group.indices.clone())
                .unwrap_or_default(),
        };
        indices
            .iter()
            .filter_map(|index| self.sessions.get(*index))
            .collect()
    }

    pub fn confirm_workspace(&self, target: &ConfirmTarget) -> Option<&WorkspaceGroup> {
        match target {
            ConfirmTarget::Workspace(key) => self.workspaces.iter().find(|group| &group.key == key),
            ConfirmTarget::Sessions(_) => None,
        }
    }

    /// 删除目标：有勾选就删勾选，否则删当前行。
    fn delete_indices(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            return self.rows.get(self.selected).copied().into_iter().collect();
        }
        self.rows
            .iter()
            .copied()
            .filter(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| self.marked.contains(&session.key()))
            })
            .collect()
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, len - 1);
        if next as usize != self.selected {
            self.selected = next as usize;
            self.detail_scroll = 0;
        }
    }

    fn move_workspace_by(&mut self, delta: isize) {
        if self.workspaces.is_empty() {
            return;
        }
        let len = self.workspaces.len() as isize;
        let next = (self.workspace_selected as isize + delta).clamp(0, len - 1);
        if next as usize != self.workspace_selected {
            self.workspace_selected = next as usize;
            self.detail_scroll = 0;
        }
    }

    fn toggle_mark(&mut self) {
        let Some(session) = self.current() else {
            return;
        };
        let key = session.key();
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
        self.dirty = true;
        self.move_by(1);
    }

    fn mark_all(&mut self) {
        let all_marked = self
            .rows
            .iter()
            .all(|index| self.marked.contains(&self.sessions[*index].key()));
        if all_marked {
            self.marked.clear();
            self.set_notice(NoticeKind::Info, "已取消全部勾选");
        } else {
            for index in &self.rows {
                self.marked.insert(self.sessions[*index].key());
            }
            let count = self.marked.len();
            self.set_notice(NoticeKind::Info, format!("已勾选 {count} 个会话"));
        }
        self.dirty = true;
    }

    fn request_delete(&mut self) {
        let indices = self.delete_indices();
        if indices.is_empty() {
            self.set_notice(NoticeKind::Warn, "没有可删除的会话");
            return;
        }
        if self.config.confirm_delete {
            self.modal = Some(Confirm {
                target: ConfirmTarget::Sessions(indices),
                dry_run: false,
            });
        } else {
            self.apply_delete(indices, false);
        }
        self.dirty = true;
    }

    fn request_workspace_delete(&mut self) {
        let Some(group) = self.current_workspace() else {
            self.set_notice(NoticeKind::Warn, "没有选中的工作区");
            return;
        };
        if group.indices.is_empty() && group.workspace.is_none() {
            self.set_notice(NoticeKind::Warn, "这个工作区没有可删除的会话");
            return;
        }
        let key = group.key.clone();
        if self.config.confirm_delete {
            self.modal = Some(Confirm {
                target: ConfirmTarget::Workspace(key),
                dry_run: false,
            });
        } else {
            self.apply_workspace_delete(&key, false);
        }
        self.dirty = true;
    }

    fn apply_delete(&mut self, indices: Vec<usize>, dry_run: bool) {
        let sessions: Vec<Session> = indices
            .iter()
            .filter_map(|index| self.sessions.get(*index).cloned())
            .collect();
        let report = delete::delete_many(&sessions, dry_run);
        for session in &sessions {
            if !report.failed.contains(&session.key()) {
                self.marked.remove(&session.key());
            }
        }
        if dry_run {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "演练：将删除 {} 个会话，释放 {}",
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
            return;
        }
        self.reload();
        if report.failed() {
            let first = report.errors.first().cloned().unwrap_or_default();
            self.set_notice(
                NoticeKind::Bad,
                format!(
                    "{} 个已删除，{} 个失败 · {first}",
                    report.deleted,
                    report.errors.len()
                ),
            );
        } else {
            self.set_notice(
                NoticeKind::Ok,
                format!(
                    "已删除 {} 个会话，释放 {}",
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
        }
        self.detail_scroll = 0;
    }

    fn apply_workspace_delete(&mut self, key: &str, dry_run: bool) {
        let Some(group) = self
            .workspaces
            .iter()
            .find(|group| group.key == key)
            .cloned()
        else {
            self.set_notice(NoticeKind::Warn, "工作区已不存在，请重新扫描");
            return;
        };
        let sessions: Vec<Session> = group
            .indices
            .iter()
            .filter_map(|index| self.sessions.get(*index).cloned())
            .collect();
        let report = delete::delete_workspace(&sessions, group.workspace.as_ref(), dry_run);
        if dry_run {
            self.set_notice(
                NoticeKind::Warn,
                format!(
                    "演练：将删除工作区「{}」的 {} 个会话，释放 {}",
                    group.title(),
                    report.deleted,
                    util::human_bytes(report.bytes)
                ),
            );
            return;
        }
        for session in &sessions {
            self.marked.remove(&session.key());
        }
        self.reload();
        if report.failed() {
            let first = report.errors.first().cloned().unwrap_or_default();
            self.set_notice(
                NoticeKind::Bad,
                format!(
                    "「{}」已删除 {} 个会话，{} 个未处理 · {first}",
                    group.title(),
                    report.deleted,
                    report.skipped
                ),
            );
        } else {
            let mut text = format!(
                "已删除工作区「{}」· {} 个会话 · {}",
                group.title(),
                report.deleted,
                util::human_bytes(report.bytes)
            );
            if report.deregistered {
                text.push_str(" · 已移除登记");
            }
            self.set_notice(NoticeKind::Ok, text);
        }
        self.detail_scroll = 0;
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Sessions => View::Workspaces,
            View::Workspaces => View::Sessions,
        };
        self.detail_scroll = 0;
        self.dirty = true;
        let text = match self.view {
            View::Sessions => "视图：会话",
            View::Workspaces => "视图：工作区（Enter 查看会话，d 删除工作区）",
        };
        self.set_notice(NoticeKind::Info, text);
    }

    /// 从工作区视图钻取：只看这个工作区下的会话。
    fn drill_workspace(&mut self) {
        let Some(group) = self.current_workspace() else {
            return;
        };
        let key = group.key.clone();
        let title = group.title().to_string();
        let count = group.count();
        self.workspace_filter = Some(key);
        self.view = View::Sessions;
        self.selected = 0;
        self.rebuild_rows();
        self.detail_scroll = 0;
        self.set_notice(
            NoticeKind::Info,
            format!("工作区「{title}」· {count} 个会话（Esc 清除工作区过滤）"),
        );
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => self.show_help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char('J') => {
                    self.help_scroll = self.help_scroll.saturating_add(8)
                }
                KeyCode::PageUp | KeyCode::Char('K') => {
                    self.help_scroll = self.help_scroll.saturating_sub(8)
                }
                _ => {}
            }
            return;
        }
        if self.filter_editing {
            self.on_filter_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quitting = true,
            KeyCode::Esc => self.on_escape(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quitting = true
            }
            KeyCode::Tab => self.cycle_client(1),
            KeyCode::BackTab => self.cycle_client(-1),
            KeyCode::Char('/') => {
                self.filter_editing = true;
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char('r') => {
                self.set_notice(NoticeKind::Info, "正在重新扫描…");
                self.reload();
                let total = self.sessions.len();
                self.set_notice(NoticeKind::Info, format!("扫描完成 · {total} 个会话"));
            }
            KeyCode::Char('t') => {
                let (palette, name) = self.palette.next();
                self.palette = palette;
                self.set_notice(NoticeKind::Info, format!("主题：{name}"));
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            KeyCode::Char('w') => self.toggle_view(),
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(1),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            _ => match self.view {
                View::Sessions => self.on_sessions_key(key),
                View::Workspaces => self.on_workspaces_key(key),
            },
        }
    }

    fn on_escape(&mut self) {
        match self.view {
            View::Workspaces => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_rows();
                } else {
                    self.view = View::Sessions;
                    self.detail_scroll = 0;
                }
            }
            View::Sessions => {
                if self.workspace_filter.take().is_some() {
                    self.rebuild_rows();
                    self.set_notice(NoticeKind::Info, "已清除工作区过滤");
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_rows();
                } else {
                    self.quitting = true;
                }
            }
        }
    }

    fn on_sessions_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home => {
                self.selected = 0;
                self.detail_scroll = 0;
            }
            KeyCode::End => {
                self.selected = self.rows.len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('a') => self.mark_all(),
            KeyCode::Char('d') => self.request_delete(),
            _ => {}
        }
    }

    fn on_workspaces_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_workspace_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_workspace_by(-1),
            KeyCode::PageDown => self.move_workspace_by(10),
            KeyCode::PageUp => self.move_workspace_by(-10),
            KeyCode::Home => {
                self.workspace_selected = 0;
                self.detail_scroll = 0;
            }
            KeyCode::End => {
                self.workspace_selected = self.workspaces.len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::Enter | KeyCode::Char('l') => self.drill_workspace(),
            KeyCode::Char('d') => self.request_workspace_delete(),
            _ => {}
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.filter_editing = false;
                self.rebuild_rows();
                let count = self.rows.len();
                let workspaces = self.workspaces.len();
                self.set_notice(
                    NoticeKind::Info,
                    format!("过滤后 {count} 个会话 · {workspaces} 个工作区"),
                );
            }
            KeyCode::Esc => {
                self.filter_editing = false;
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.rebuild_rows();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char(ch) => {
                self.filter.push(ch);
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.clone() else {
            return;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.modal = None;
                match modal.target {
                    ConfirmTarget::Sessions(indices) => self.apply_delete(indices, modal.dry_run),
                    ConfirmTarget::Workspace(key) => {
                        self.apply_workspace_delete(&key, modal.dry_run)
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                self.modal = None;
                self.set_notice(NoticeKind::Info, "已取消删除");
            }
            _ => {}
        }
    }

    fn cycle_client(&mut self, step: isize) {
        let mut order: Vec<Option<Client>> = vec![None];
        order.extend(Client::ALL.iter().copied().map(Some));
        let current = order
            .iter()
            .position(|item| *item == self.client_filter)
            .unwrap_or(0) as isize;
        let len = order.len() as isize;
        let next = ((current + step) % len + len) % len;
        self.client_filter = order[next as usize];
        self.rebuild_rows();
        let label = self
            .client_filter
            .map(|client| client.label())
            .unwrap_or("全部");
        self.set_notice(NoticeKind::Info, format!("视图：{label}"));
        self.detail_scroll = 0;
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
            MouseEventKind::ScrollUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                let inside = mouse.column > area.x
                    && mouse.column < area.x + area.width.saturating_sub(1)
                    && mouse.row > area.y
                    && mouse.row < area.y + area.height.saturating_sub(1);
                if !inside {
                    return;
                }
                let offset = match self.view {
                    View::Sessions => self.list_offset,
                    View::Workspaces => self.workspace_list_offset,
                };
                let len = match self.view {
                    View::Sessions => self.rows.len(),
                    View::Workspaces => self.workspaces.len(),
                };
                let row = (mouse.row - area.y - 1) as usize + offset;
                if row < len {
                    match self.view {
                        View::Sessions => self.selected = row,
                        View::Workspaces => self.workspace_selected = row,
                    }
                    self.detail_scroll = 0;
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        if self.config.rescan_secs == 0
            || self.modal.is_some()
            || self.show_help
            || self.filter_editing
        {
            return;
        }
        if self.last_scan.elapsed() >= self.config.rescan_interval() {
            self.reload();
        }
    }

    pub fn status_line(&self) -> (String, NoticeKind) {
        (self.notice.text.clone(), self.notice.kind)
    }
}

/// 启动 TUI 主循环。
pub fn run(config: Config) -> Result<()> {
    let mouse = config.mouse;
    let mut terminal = ratatui::init();
    if mouse {
        let mut stdout = std::io::stdout();
        ratatui::crossterm::execute!(stdout, ratatui::crossterm::event::EnableMouseCapture)?;
    }
    let mut app = App::new(config);

    let result = loop {
        if app.dirty {
            terminal.draw(|frame| crate::ui::draw(frame, &mut app))?;
            app.dirty = false;
        }

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                event::Event::Key(key) => app.on_key(key),
                event::Event::Mouse(mouse_event) if app.mouse_enabled() => {
                    app.on_mouse(mouse_event)
                }
                event::Event::Resize(_, _) => app.dirty = true,
                _ => {}
            }
        }

        app.tick();
        if app.quitting {
            break Ok(());
        }
    };

    if mouse {
        let mut stdout = std::io::stdout();
        let _ =
            ratatui::crossterm::execute!(stdout, ratatui::crossterm::event::DisableMouseCapture);
    }
    ratatui::restore();
    result
}

impl App {
    pub fn mouse_enabled(&self) -> bool {
        self.config.mouse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(sessions: Vec<Session>) -> App {
        let mut app = App {
            sessions,
            known_workspaces: Vec::new(),
            workspaces: Vec::new(),
            roots: Vec::new(),
            warnings: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            workspace_selected: 0,
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_editing: false,
            workspace_filter: None,
            client_filter: None,
            view: View::Sessions,
            modal: None,
            show_help: false,
            help_scroll: 0,
            detail_scroll: 0,
            detail_max: 0,
            notice: Notice::default(),
            palette: Palette::from_name("ocean"),
            config: Config::default(),
            dirty: false,
            quitting: false,
            list_area: Rect::default(),
            list_offset: 0,
            workspace_list_offset: 0,
            last_scan: Instant::now(),
        };
        app.rebuild_rows();
        app
    }

    fn session(client: Client, id: &str, title: &str) -> Session {
        Session {
            client,
            id: id.to_string(),
            title: title.to_string(),
            preview: String::new(),
            path: "/tmp/x".into(),
            root: "/tmp".into(),
            cwd: None,
            workspace: None,
            created: None,
            updated: None,
            bytes: 1024,
            messages: 1,
            tokens: None,
            cost: None,
            locked: false,
        }
    }

    fn in_workspace(mut session: Session, id: &str, path: &str) -> Session {
        session.workspace = Some(Workspace {
            client: session.client,
            id: id.to_string(),
            path: path.to_string(),
            title: util::path_title(path),
            root: "/tmp".into(),
        });
        session
    }

    #[test]
    fn filter_matches_title_and_client() {
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "修复 Wayland 崩溃"),
            session(Client::Codex, "b", "重构扫描器"),
        ]);
        app.filter = "wayland".into();
        app.rebuild_rows();
        assert_eq!(app.rows.len(), 1);
        app.filter = "codex".into();
        app.rebuild_rows();
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.sessions[app.rows[0]].id, "b");
    }

    #[test]
    fn mark_all_toggles_and_counts() {
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "A"),
            session(Client::Dsh, "b", "B"),
        ]);
        app.mark_all();
        assert_eq!(app.marked_count(), 2);
        app.mark_all();
        assert_eq!(app.marked_count(), 0);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn delete_opens_confirm_and_escape_cancels() {
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "A"),
            session(Client::Dsh, "b", "B"),
        ]);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.marked_count(), 1);
        press(&mut app, KeyCode::Char('d'));
        let modal = app.modal.as_ref().expect("确认框应打开");
        assert_eq!(modal.target, ConfirmTarget::Sessions(vec![0]));
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
        assert!(!app.quitting);
    }

    #[test]
    fn delete_without_selection_uses_current_row() {
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "A"),
            session(Client::Codex, "b", "B"),
        ]);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            app.modal.as_ref().expect("确认框应打开").target,
            ConfirmTarget::Sessions(vec![1])
        );
    }

    #[test]
    fn shrinking_session_list_does_not_break_selection() {
        // 删除会话后重扫会拿到更短的列表，旧 rows 里的下标已经失效。
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "A"),
            session(Client::Dsh, "b", "B"),
        ]);
        app.selected = 1;
        app.sessions.remove(0);
        app.rebuild_rows();
        assert_eq!(app.rows, vec![0]);
        assert_eq!(app.current().expect("仍应选中剩下的会话").id, "b");
    }

    #[test]
    fn filter_mode_edits_and_applies() {
        let mut app = app_with(vec![
            session(Client::Dsh, "a", "修复 Wayland 崩溃"),
            session(Client::Codex, "b", "重构扫描器"),
        ]);
        press(&mut app, KeyCode::Char('/'));
        assert!(app.filter_editing);
        for ch in "codex".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        assert_eq!(app.filter, "codex");
        press(&mut app, KeyCode::Enter);
        assert!(!app.filter_editing);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.sessions[app.rows[0]].id, "b");
    }

    #[test]
    fn quit_key_sets_quitting() {
        let mut app = app_with(vec![]);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quitting);
    }

    #[test]
    fn client_cycle_wraps() {
        let mut app = app_with(vec![]);
        app.cycle_client(1);
        assert_eq!(app.client_filter, Some(Client::Dsh));
        app.cycle_client(-1);
        assert_eq!(app.client_filter, None);
        app.cycle_client(-1);
        assert_eq!(app.client_filter, Some(Client::Opencode));
    }

    #[test]
    fn workspace_view_groups_and_drills_in() {
        let mut app = app_with(vec![
            in_workspace(session(Client::Dsh, "a", "A"), "ws-1", "/work/alpha"),
            in_workspace(session(Client::Dsh, "b", "B"), "ws-1", "/work/alpha"),
            in_workspace(
                session(Client::Codex, "c", "C"),
                "path:/work/beta",
                "/work/beta",
            ),
        ]);
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.view, View::Workspaces);
        assert_eq!(app.workspaces.len(), 2);
        assert_eq!(app.current_workspace().map(WorkspaceGroup::count), Some(2));

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::Sessions);
        assert_eq!(app.rows.len(), 2, "只保留这个工作区的会话");
        assert_eq!(app.workspace_filter.as_deref(), Some("dsh:ws-1"));

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.workspace_filter, None);
        assert_eq!(app.rows.len(), 3);
        assert!(!app.quitting);
    }

    #[test]
    fn escape_in_workspace_view_returns_to_sessions() {
        let mut app = app_with(vec![session(Client::Dsh, "a", "A")]);
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.view, View::Sessions);
        assert!(!app.quitting);
    }

    #[test]
    fn workspace_delete_opens_confirm() {
        let mut app = app_with(vec![
            in_workspace(session(Client::Dsh, "a", "A"), "ws-1", "/work/alpha"),
            in_workspace(session(Client::Dsh, "b", "B"), "ws-1", "/work/alpha"),
        ]);
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('d'));
        let modal = app.modal.as_ref().expect("确认框应打开");
        assert_eq!(modal.target, ConfirmTarget::Workspace("dsh:ws-1".into()));
        assert_eq!(app.confirm_sessions(&modal.target).len(), 2);
        assert!(app.confirm_workspace(&modal.target).is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
    }

    #[test]
    fn empty_registered_workspace_shows_up() {
        let mut app = app_with(Vec::new());
        app.known_workspaces.push(Workspace {
            client: Client::Dsh,
            id: "ws-empty".into(),
            path: "/work/empty".into(),
            title: "Empty".into(),
            root: "/tmp".into(),
        });
        app.rebuild_rows();
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.workspaces[0].count(), 0);
    }
}
