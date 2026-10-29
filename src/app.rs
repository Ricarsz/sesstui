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
use crate::model::{Client, Session};
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

#[derive(Debug, Clone)]
pub struct Confirm {
    pub indices: Vec<usize>,
    pub dry_run: bool,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
    /// 过滤后可见的会话下标，顺序即列表顺序。
    pub rows: Vec<usize>,
    pub selected: usize,
    pub marked: BTreeSet<String>,
    pub filter: String,
    pub filter_editing: bool,
    pub client_filter: Option<Client>,
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
            roots: snapshot.roots,
            warnings: snapshot.warnings,
            rows: Vec::new(),
            selected: 0,
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_editing: false,
            client_filter: None,
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
            last_scan: Instant::now(),
        };
        app.rebuild_rows();
        let total = app.sessions.len();
        let warnings = app.warnings.len();
        if warnings > 0 {
            app.set_notice(
                NoticeKind::Warn,
                format!("{total} 个会话 · {warnings} 条扫描警告（? 查看帮助）"),
            );
        } else {
            app.set_notice(NoticeKind::Info, format!("{total} 个会话 · 就绪"));
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
        let snapshot = scan::scan();
        self.sessions = snapshot.sessions;
        self.roots = snapshot.roots;
        self.warnings = snapshot.warnings;
        self.rebuild_rows();
        self.last_scan = Instant::now();
        self.dirty = true;
    }

    /// 依据过滤条件重建可见行，尽量保持当前选中项不变。
    pub fn rebuild_rows(&mut self) {
        let previous = self
            .rows
            .get(self.selected)
            .map(|index| self.sessions[*index].key());
        let filter = self.filter.trim().to_lowercase();
        let client = self.client_filter;
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
                if filter.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{} {} {} {}",
                    session.title,
                    session.id,
                    session.cwd.clone().unwrap_or_default(),
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

    pub fn current(&self) -> Option<&Session> {
        self.rows
            .get(self.selected)
            .map(|index| &self.sessions[*index])
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

    /// 删除目标：有勾选就删勾选，否则删当前行。
    fn delete_indices(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            return self.rows.get(self.selected).copied().into_iter().collect();
        }
        self.rows
            .iter()
            .copied()
            .filter(|index| self.marked.contains(&self.sessions[*index].key()))
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
                indices,
                dry_run: false,
            });
        } else {
            self.apply_delete(indices, false);
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
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_rows();
                } else {
                    self.quitting = true;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quitting = true
            }
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
            KeyCode::Tab => {
                self.cycle_client(1);
            }
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
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(1),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            _ => {}
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.filter_editing = false;
                self.rebuild_rows();
                let count = self.rows.len();
                self.set_notice(NoticeKind::Info, format!("过滤后 {count} 个会话"));
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
                self.apply_delete(modal.indices, modal.dry_run);
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
                let row = (mouse.row - area.y - 1) as usize + self.list_offset;
                if row < self.rows.len() {
                    self.selected = row;
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
            roots: Vec::new(),
            warnings: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            marked: BTreeSet::new(),
            filter: String::new(),
            filter_editing: false,
            client_filter: None,
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
            created: None,
            updated: None,
            bytes: 1024,
            messages: 1,
            tokens: None,
            cost: None,
            locked: false,
        }
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
        assert_eq!(modal.indices, vec![0]);
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
        assert_eq!(app.modal.as_ref().expect("确认框应打开").indices, vec![1]);
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
}
