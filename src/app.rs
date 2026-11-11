//! App state, key handling and worker-event draining (shared by both render hosts).
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;

use ratatui::crossterm::event::KeyCode;
use ratatui::style::Style;
use ratatui::widgets::ListState;
use ratatui_cheese::select::SelectState;
use ratatui_cheese::spinner::{Spinner, SpinnerState, SpinnerType};
use ratatui_cheese::theme::Palette;

use crate::sessions::{self, ScanEvent, Session, TrashItem};

const PREVIEW_MESSAGES: usize = 12;

/// How a text or `group:` term matches: substring, or `regex:<pattern>`.
enum Attempt {
    /// Lowercased needle.
    Text(String),
    Regex(regex::Regex),
    /// A pattern that does not compile (usually still being typed).
    Never,
}

impl Attempt {
    fn parse(text: &str) -> Self {
        match text.strip_prefix("regex:") {
            None => Attempt::Text(text.to_lowercase()),
            Some(pattern) => regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_or(Attempt::Never, Attempt::Regex),
        }
    }

    fn matches(&self, haystack: &str) -> bool {
        match self {
            Attempt::Text(needle) => haystack.to_lowercase().contains(needle),
            Attempt::Regex(regex) => regex.is_match(haystack),
            Attempt::Never => false,
        }
    }
}

/// Seconds in an ISO 8601 duration written without its leading `P`: `30D`, `2W`, `1M`,
/// `1DT12H`, `T30M`. `M` is months before the `T` and minutes after it. Case-insensitive.
// ponytail: a month is 30 days and a year 365; calendar-exact spans need a date library.
fn span_secs(text: &str) -> Option<u64> {
    let (mut total, mut digits, mut time, mut units) = (0u64, String::new(), false, 0);
    for c in text.chars().map(|c| c.to_ascii_uppercase()) {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if c == 'T' && digits.is_empty() && !time {
            time = true;
        } else {
            let unit = match (c, time) {
                ('Y', false) => 365 * 86_400,
                ('M', false) => 30 * 86_400,
                ('W', false) => 7 * 86_400,
                ('D', false) => 86_400,
                ('H', true) => 3_600,
                ('M', true) => 60,
                ('S', true) => 1,
                _ => return None,
            };
            total = total.checked_add(digits.parse::<u64>().ok()?.checked_mul(unit)?)?;
            digits.clear();
            units += 1;
        }
    }
    (digits.is_empty() && units > 0).then_some(total)
}

/// Parsed filter text: comma-separated terms. Terms of one kind are alternatives, and every
/// kind present must hold. An empty filter matches everything.
// ponytail: a literal comma inside a term is not expressible; add escaping if anyone needs it.
#[derive(Default)]
struct Query {
    /// Plain and `regex:` terms, matched against label, cwd and uuid.
    text:   Vec<Attempt>,
    /// `group:` terms, matched against the folder name (last component of the cwd).
    groups: Vec<Attempt>,
    /// `older:<span>`: idle for at least this many seconds.
    older:  Option<u64>,
    /// `newer:<span>`: active within this many seconds.
    newer:  Option<u64>,
    /// First term that could not be understood; such a term matches nothing.
    error:  Option<String>,
}

impl Query {
    fn parse(filter: &str) -> Self {
        let mut query = Self::default();
        for term in filter.split(',').map(str::trim).filter(|term| !term.is_empty()) {
            let understood = if let Some(span) = term.strip_prefix("older:") {
                query.older = span_secs(span);
                query.older.is_some()
            } else if let Some(span) = term.strip_prefix("newer:") {
                query.newer = span_secs(span);
                query.newer.is_some()
            } else {
                let (list, attempt) = match term.strip_prefix("group:") {
                    Some(name) => (&mut query.groups, Attempt::parse(name)),
                    None => (&mut query.text, Attempt::parse(term)),
                };
                let understood = !matches!(attempt, Attempt::Never);
                list.push(attempt);
                understood
            };
            if !understood {
                query.error.get_or_insert_with(|| term.to_string());
                if term.starts_with("older:") || term.starts_with("newer:") {
                    query.older = Some(u64::MAX); // a bad span matches nothing, like a bad regex
                }
            }
        }
        query
    }

    fn matches(&self, s: &Session) -> bool {
        let any = |list: &[Attempt], fields: &[&str]| {
            list.is_empty() || list.iter().any(|attempt| fields.iter().any(|field| attempt.matches(field)))
        };
        let age = sessions::age_secs(s.mtime);
        any(&self.groups, &[project(s)])
            && any(&self.text, &[&s.label, &s.cwd, &s.uuid])
            && self.older.is_none_or(|secs| age >= secs)
            && self.newer.is_none_or(|secs| age <= secs)
    }
}

/// Apply a scroll key to `offset`; false when `code` is not one. The views clamp the result.
fn scroll_key(code: KeyCode, offset: &mut u16, page: u16) -> bool {
    *offset = match code {
        KeyCode::Up | KeyCode::Char('k') => offset.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => offset.saturating_add(1),
        KeyCode::PageUp => offset.saturating_sub(page),
        KeyCode::PageDown | KeyCode::Char(' ') => offset.saturating_add(page),
        KeyCode::Home => 0,
        KeyCode::End => u16::MAX,
        _ => return false,
    };
    true
}

pub enum Pending {
    /// Session paths to move to the trash.
    Trash(Vec<PathBuf>),
    /// Session paths whose owner becomes the signed-in account.
    Claim(Vec<PathBuf>),
    /// Trash item dirs to delete for good.
    Purge(Vec<PathBuf>),
}

pub enum Stage {
    Loading,
    Browse,
    Filter,
    /// Full transcript of the cursor session, drawn over the list.
    Detail,
    Trash,
    Confirm(Pending),
}

pub struct Model {
    pub root:          PathBuf,
    pub stage:         Stage,
    pub sessions:      Vec<Session>,
    /// Indices into `sessions` matching `group` and `filter`, in display order.
    pub visible:       Vec<usize>,
    pub selected:      HashSet<PathBuf>,
    pub live:          HashSet<String>,
    /// Signed-in `(accountUuid, organizationUuid)`; None when `.claude.json` has no account.
    pub me:            Option<(String, String)>,
    /// Project folders holding a session that passes `filter`, most recently used first.
    /// `group` 0 means all; n means `groups[n - 1]`.
    pub groups:        Vec<PathBuf>,
    pub group:         usize,
    pub filter:        String,
    /// Term of `filter` that could not be understood, for the info row.
    pub filter_error:  Option<String>,
    /// Key guide overlay is showing, scrolled to `help_scroll`.
    pub help:          bool,
    pub help_scroll:   u16,
    pub list:          ListState,
    pub trash:         Vec<TrashItem>,
    pub trash_list:    ListState,
    /// Selected trash items, by their dir.
    pub trash_selected: HashSet<PathBuf>,
    pub choice:        SelectState,
    pub preview:       Vec<(bool, String)>,
    preview_for:       Option<PathBuf>,
    pub detail:        Vec<(bool, String)>,
    /// First visible line of the detail view; the view clamps it to the content each draw.
    pub detail_scroll: u16,
    /// Rows the detail view showed last draw, for page-sized scrolling.
    pub detail_page:   u16,
    /// (parsed, total) while scanning.
    pub progress:      (usize, usize),
    rx:                Option<Receiver<ScanEvent>>,
    pub status:        String,
    pub palette:       Palette,
    pub spinner:       Spinner,
    pub spinner_state: SpinnerState,
    pub should_quit:   bool,
}

fn choice(options: usize) -> SelectState {
    let mut state = SelectState::new(options);
    state.set_focused(true);
    state
}

fn step(list: &mut ListState, len: usize, delta: isize) {
    let cur = list.selected().unwrap_or(0) as isize;
    list.select((len > 0).then(|| cur.saturating_add(delta).clamp(0, len as isize - 1) as usize));
}

/// Apply a list-movement key to `list`; false when `code` is not one.
fn list_key(code: KeyCode, list: &mut ListState, len: usize) -> bool {
    let delta = match code {
        KeyCode::Up | KeyCode::Char('k') => -1,
        KeyCode::Down | KeyCode::Char('j') => 1,
        KeyCode::PageUp => -10,
        KeyCode::PageDown => 10,
        KeyCode::Home => isize::MIN,
        KeyCode::End => isize::MAX,
        _ => return false,
    };
    step(list, len, delta);
    true
}

fn toggle(selected: &mut HashSet<PathBuf>, path: PathBuf) {
    if !selected.remove(&path) {
        selected.insert(path);
    }
}

/// Select everything in `shown`, or unselect it all when it already is.
fn toggle_all(selected: &mut HashSet<PathBuf>, shown: Vec<PathBuf>) {
    if shown.iter().all(|path| selected.contains(path)) {
        selected.retain(|path| !shown.contains(path));
    } else {
        selected.extend(shown);
    }
}

/// Last component of the session's working directory.
pub fn project(s: &Session) -> &str {
    Path::new(&s.cwd).file_name().and_then(|name| name.to_str()).unwrap_or(&s.cwd)
}

impl Model {
    pub fn new(root: PathBuf) -> Self {
        let palette = Palette::charm();
        Self {
            rx: Some(sessions::scan(root.clone())),
            live: sessions::live_ids(&root),
            me: sessions::current_account(&root),
            trash: sessions::list_trash(&root),
            root,
            stage: Stage::Loading,
            sessions: Vec::new(),
            visible: Vec::new(),
            selected: HashSet::new(),
            groups: Vec::new(),
            group: 0,
            filter: String::new(),
            filter_error: None,
            help: false,
            help_scroll: 0,
            list: ListState::default(),
            trash_list: ListState::default(),
            trash_selected: HashSet::new(),
            choice: choice(2),
            preview: Vec::new(),
            preview_for: None,
            detail: Vec::new(),
            detail_scroll: 0,
            detail_page: 1,
            progress: (0, 0),
            status: String::new(),
            spinner: Spinner::default().style(Style::default().fg(palette.primary)),
            spinner_state: SpinnerState::new(SpinnerType::Dot),
            palette,
            should_quit: false,
        }
    }

    pub fn cursor(&self) -> Option<&Session> {
        self.visible.get(self.list.selected()?).map(|&i| &self.sessions[i])
    }

    /// Owned by an account other than the signed-in one. Sessions with no owner record, or an
    /// unknown signed-in account, are not flagged.
    pub fn foreign(&self, s: &Session) -> bool {
        matches!((&self.me, &s.owner), (Some((me, _)), Some(owner)) if me != owner)
    }

    /// Tab labels: `[all]`, then each folder named after its newest session's project.
    pub fn group_labels(&self) -> Vec<&str> {
        let name = |dir: &PathBuf| {
            self.sessions.iter().find(|s| s.path.parent() == Some(dir)).map_or("?", project)
        };
        std::iter::once("[all]").chain(self.groups.iter().map(name)).collect()
    }

    /// Rebuild `groups` and `visible` after `sessions`, `group` or `filter` changed.
    fn refilter(&mut self) {
        let query = Query::parse(&self.filter);
        self.filter_error.clone_from(&query.error);
        let matching: Vec<usize> = (0..self.sessions.len()).filter(|&i| query.matches(&self.sessions[i])).collect();

        // Tabs are the folders the filter leaves something in.
        let current = self.group.checked_sub(1).and_then(|i| self.groups.get(i).cloned());
        let mut seen = HashSet::new();
        self.groups = matching
            .iter()
            .filter_map(|&i| self.sessions[i].path.parent())
            .filter(|dir| seen.insert(*dir))
            .map(Path::to_path_buf)
            .collect();
        // Follow the active folder by path; fall back to [all] when it no longer has a tab.
        self.group = current.and_then(|c| self.groups.iter().position(|g| *g == c)).map_or(0, |i| i + 1);

        let folder = self.group.checked_sub(1).map(|i| self.groups[i].as_path());
        self.visible =
            matching.into_iter().filter(|&i| folder.is_none() || self.sessions[i].path.parent() == folder).collect();
        step(&mut self.list, self.visible.len(), 0);
    }

    fn reload_trash(&mut self) {
        self.trash = sessions::list_trash(&self.root);
        self.trash_selected.retain(|dir| self.trash.iter().any(|item| item.dir == *dir));
        step(&mut self.trash_list, self.trash.len(), 0);
    }

    fn sort(&mut self) {
        self.sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    }

    /// Drain scan results and keep the preview in sync with the cursor. Call once per frame.
    pub fn apply(&mut self) {
        if let Some(rx) = self.rx.take() {
            let mut done = false;
            for event in rx.try_iter() {
                match event {
                    ScanEvent::Total(n) => self.progress.1 = n,
                    ScanEvent::Parsed(s) => {
                        self.progress.0 += 1;
                        self.sessions.extend(s);
                    }
                    ScanEvent::Done => done = true,
                }
            }
            if done {
                self.sort();
                sessions::save_cache(&self.root, &self.sessions);
                self.stage = Stage::Browse;
                self.refilter();
            } else {
                self.rx = Some(rx);
            }
        }
        let current = self.cursor().map(|s| s.path.clone());
        if current != self.preview_for {
            self.preview = current.as_ref().map(|p| sessions::preview(p, PREVIEW_MESSAGES)).unwrap_or_default();
            self.preview_for = current;
        }
    }

    pub fn handle(&mut self, code: KeyCode) {
        self.status.clear();
        // The key guide scrolls; any other key dismisses it without acting. `?` opens it,
        // except while typing a filter, where it is text (and regex syntax).
        if self.help {
            self.help = scroll_key(code, &mut self.help_scroll, 10);
            return;
        }
        if code == KeyCode::Char('?') && !matches!(self.stage, Stage::Filter) {
            (self.help, self.help_scroll) = (true, 0);
            return;
        }
        match self.stage {
            Stage::Loading => self.should_quit = matches!(code, KeyCode::Esc | KeyCode::Char('q')),
            Stage::Browse => self.browse(code),
            Stage::Filter => self.filter_key(code),
            Stage::Detail => self.detail_key(code),
            Stage::Trash => self.trash_view(code),
            Stage::Confirm(_) => self.confirm(code),
        }
    }

    fn browse(&mut self, code: KeyCode) {
        if list_key(code, &mut self.list, self.visible.len()) {
            return;
        }
        match code {
            KeyCode::Left => self.switch_group(-1),
            KeyCode::Right => self.switch_group(1),
            KeyCode::Char(' ') => {
                if let Some(path) = self.cursor().map(|s| s.path.clone()) {
                    toggle(&mut self.selected, path);
                }
            }
            KeyCode::Char('a') => {
                let shown = self.visible.iter().map(|&i| self.sessions[i].path.clone()).collect();
                toggle_all(&mut self.selected, shown);
            }
            KeyCode::Enter => {
                if let Some(path) = self.cursor().map(|s| s.path.clone()) {
                    self.detail = sessions::transcript(&path);
                    self.detail_scroll = 0;
                    self.stage = Stage::Detail;
                }
            }
            KeyCode::Char('/') => self.stage = Stage::Filter,
            KeyCode::Char('d') => self.ask_trash(),
            KeyCode::Char('c') => self.ask_claim(),
            KeyCode::Char('y') => self.copy_resume(),
            KeyCode::Tab => {
                self.reload_trash();
                self.stage = Stage::Trash;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Esc | KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    /// Cycle `[all]` → folders → `[all]`, wrapping in both directions.
    fn switch_group(&mut self, delta: isize) {
        let count = self.groups.len() as isize + 1;
        self.group = (self.group as isize + delta).rem_euclid(count) as usize;
        self.list = ListState::default();
        self.refilter();
    }

    fn filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => step(&mut self.list, self.visible.len(), -1),
            KeyCode::Down => step(&mut self.list, self.visible.len(), 1),
            // The filter box has no text cursor, so the arrows stay on folder tabs.
            KeyCode::Left => return self.switch_group(-1),
            KeyCode::Right => return self.switch_group(1),
            KeyCode::Enter => self.stage = Stage::Browse,
            KeyCode::Esc => {
                self.filter.clear();
                self.stage = Stage::Browse;
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => return,
        }
        self.refilter();
    }

    fn detail_key(&mut self, code: KeyCode) {
        if scroll_key(code, &mut self.detail_scroll, self.detail_page) {
            return;
        }
        match code {
            KeyCode::Char('y') => self.copy_resume(),
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                self.detail = Vec::new();
                self.stage = Stage::Browse;
            }
            _ => {}
        }
    }

    /// Paths an action applies to: the selection, or the cursor row when nothing is selected,
    /// narrowed to sessions that are not in use and pass `keep`.
    fn targets(&mut self, keep: impl Fn(&Self, &Session) -> bool) -> Vec<PathBuf> {
        self.live = sessions::live_ids(&self.root);
        let cursor = self.cursor().map(|s| &s.path);
        self.sessions
            .iter()
            .filter(|s| if self.selected.is_empty() { cursor == Some(&s.path) } else { self.selected.contains(&s.path) })
            .filter(|s| !s.active(&self.live) && keep(self, s))
            .map(|s| s.path.clone())
            .collect()
    }

    fn ask_trash(&mut self) {
        let targets = self.targets(|_, _| true);
        if targets.is_empty() {
            self.status = "nothing to trash (sessions in use are protected)".into();
        } else {
            self.choice = choice(2);
            self.stage = Stage::Confirm(Pending::Trash(targets));
        }
    }

    fn ask_claim(&mut self) {
        if self.me.is_none() {
            self.status = "no signed-in account found in .claude.json".into();
            return;
        }
        let targets = self.targets(Self::foreign);
        if targets.is_empty() {
            self.status = "nothing to claim (already yours, no owner recorded, or in use)".into();
        } else {
            self.choice = choice(2);
            self.stage = Stage::Confirm(Pending::Claim(targets));
        }
    }

    fn trash_view(&mut self, code: KeyCode) {
        if list_key(code, &mut self.trash_list, self.trash.len()) {
            return;
        }
        let dirs = || self.trash.iter().map(|item| item.dir.clone());
        let cursor = self.trash_list.selected().and_then(|i| dirs().nth(i));
        // What r / x act on: the selection, or the cursor row when nothing is selected.
        let targets: Vec<PathBuf> = if self.trash_selected.is_empty() {
            cursor.iter().cloned().collect()
        } else {
            dirs().filter(|dir| self.trash_selected.contains(dir)).collect()
        };
        match code {
            KeyCode::Char(' ') => cursor.into_iter().for_each(|dir| toggle(&mut self.trash_selected, dir)),
            KeyCode::Char('a') => {
                let all = dirs().collect();
                toggle_all(&mut self.trash_selected, all);
            }
            KeyCode::Char('r') => self.empty_trash(targets, true),
            KeyCode::Char('x') if !targets.is_empty() => {
                self.choice = choice(2);
                self.stage = Stage::Confirm(Pending::Purge(targets));
            }
            KeyCode::Tab | KeyCode::Esc => self.stage = Stage::Browse,
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    /// Restore (or, with `restore` false, permanently delete) the trash items in `dirs`.
    fn empty_trash(&mut self, dirs: Vec<PathBuf>, restore: bool) {
        let (mut done, mut failed, mut first) = (0, 0, None);
        for dir in dirs {
            let Some(item) = self.trash.iter().find(|item| item.dir == dir) else { continue };
            let result = if restore { sessions::restore(item) } else { sessions::purge(item).map_err(Into::into) };
            match result {
                Ok(()) => {
                    done += 1;
                    if restore && let Ok(session) = sessions::parse(&item.original) {
                        self.sessions.push(session);
                    }
                }
                Err(e) => {
                    failed += 1;
                    first.get_or_insert(format!("{e:#}"));
                }
            }
        }
        let verb = if restore { "restored" } else { "purged" };
        self.status = match first {
            None => format!("{verb} {done}"),
            Some(e) => format!("{verb} {done}, {failed} failed: {e}"),
        };
        self.sort();
        self.reload_trash();
        self.refilter();
    }

    fn confirm(&mut self, code: KeyCode) {
        let yes = match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.choice.prev();
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.choice.next();
                return;
            }
            KeyCode::Enter => self.choice.selected() == 1,
            KeyCode::Char('y') => true,
            KeyCode::Esc | KeyCode::Char('n') => false,
            _ => return,
        };
        match std::mem::replace(&mut self.stage, Stage::Browse) {
            Stage::Confirm(Pending::Trash(targets)) if yes => self.trash_sessions(targets),
            Stage::Confirm(Pending::Claim(targets)) if yes => self.claim_sessions(targets),
            Stage::Confirm(Pending::Purge(dirs)) => {
                if yes {
                    self.empty_trash(dirs, false);
                }
                self.stage = Stage::Trash;
            }
            _ => {}
        }
    }

    /// Run `action` on each target still in `sessions`; returns (succeeded, first error).
    fn for_each_target(
        &mut self,
        targets: Vec<PathBuf>,
        mut action: impl FnMut(&mut Self, usize) -> anyhow::Result<()>,
    ) -> (usize, usize, Option<String>) {
        let (mut done, mut failed, mut first) = (0, 0, None);
        for path in targets {
            let Some(i) = self.sessions.iter().position(|s| s.path == path) else { continue };
            match action(self, i) {
                Ok(()) => done += 1,
                Err(e) => {
                    failed += 1;
                    first.get_or_insert(format!("{e:#}"));
                }
            }
        }
        (done, failed, first)
    }

    fn trash_sessions(&mut self, targets: Vec<PathBuf>) {
        let (done, failed, error) = self.for_each_target(targets, |m, i| {
            sessions::trash(&m.root, &m.sessions[i])?;
            let gone = m.sessions.remove(i);
            m.selected.remove(&gone.path);
            Ok(())
        });
        self.status = match error {
            None => format!("trashed {done} — tab to view / restore"),
            Some(e) => format!("trashed {done}, {failed} failed: {e}"),
        };
        self.reload_trash();
        self.refilter();
    }

    fn claim_sessions(&mut self, targets: Vec<PathBuf>) {
        let Some((account, org)) = self.me.clone() else { return };
        let (done, failed, error) = self.for_each_target(targets, |m, i| {
            sessions::set_owner(&m.root, &m.sessions[i], &account, &org)?;
            m.sessions[i] = sessions::parse(&m.sessions[i].path)?;
            Ok(())
        });
        self.status = match error {
            None => format!("claimed {done} session(s)"),
            Some(e) => format!("claimed {done}, {failed} failed: {e}"),
        };
        // A claim keeps size and mtime, so the cache cannot notice it by itself.
        sessions::save_cache(&self.root, &self.sessions);
    }

    fn copy_resume(&mut self) {
        let Some(s) = self.cursor() else { return };
        let command = format!("cd \"{}\" && claude --resume {}", s.cwd, s.uuid);
        self.status = match copy_to_clipboard(&command) {
            Ok(()) => format!("copied: {command}"),
            Err(e) => format!("copy failed ({e}): {command}"),
        };
    }
}

// ponytail: shells out to the platform clipboard tool (clip.exe reads the console codepage, so a
// non-ASCII cwd may arrive garbled); swap in the arboard crate if that matters.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let tools: &[&[&str]] = if cfg!(windows) {
        &[&["clip"]]
    } else if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"]]
    };
    let mut last = std::io::Error::other("no clipboard tool");
    for tool in tools {
        let spawned = Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{ACCOUNT_JSON, OLD, OWNER_LINE, USER_LINE, backdate, fixture};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(m: &mut Model) -> String {
        screen_sized(m, 140, 20)
    }

    fn screen_sized(m: &mut Model, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::view::view(frame, m)).unwrap();
        terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect()
    }

    fn loaded(root: PathBuf) -> Model {
        let mut m = Model::new(root);
        assert!(screen(&mut m).contains("Scanning sessions"));
        while matches!(m.stage, Stage::Loading) {
            m.apply();
            std::thread::yield_now();
        }
        m.apply();
        m
    }

    /// Drives the real key handler and view over a fake Claude home: filter, trash, restore.
    #[test]
    fn browse_trash_restore_flow() {
        let (root, path) = fixture("flow", &[USER_LINE], OLD);
        let mut m = loaded(root);
        let shown = screen(&mut m);
        assert!(shown.contains("first prompt here") && shown.contains("1 sessions"), "{shown}");

        m.handle(KeyCode::Char('/'));
        "zzz".chars().for_each(|c| m.handle(KeyCode::Char(c)));
        assert!(m.visible.is_empty());
        screen(&mut m); // empty list must still draw
        m.handle(KeyCode::Esc);
        assert_eq!(m.visible.len(), 1);

        m.handle(KeyCode::Char('d'));
        assert!(screen(&mut m).contains("Move 1 session(s) to the trash?"));
        m.handle(KeyCode::Enter); // default choice is Cancel
        assert!(path.exists());

        m.handle(KeyCode::Char('d'));
        m.handle(KeyCode::Char('y'));
        assert!(!path.exists() && m.sessions.is_empty() && m.groups.is_empty());
        assert!(screen(&mut m).contains(" tab Trash 1 "));
        m.handle(KeyCode::Char('o')); // no longer a key
        assert!(matches!(m.stage, Stage::Browse));

        m.handle(KeyCode::Tab);
        assert!(screen(&mut m).contains("trash · 1 item(s)"));
        m.handle(KeyCode::Char('x'));
        m.handle(KeyCode::Esc); // declined purge keeps the item
        assert_eq!(m.trash.len(), 1);
        m.handle(KeyCode::Char('r'));
        assert!(path.exists() && m.trash.is_empty());
        assert_eq!(m.sessions.len(), 1);
    }

    #[test]
    fn trash_multi_select_and_jumps() {
        let (root, path, second) = two_folders("multi");
        let mut m = loaded(root);
        let trash_both = |m: &mut Model| {
            for key in [KeyCode::Char('a'), KeyCode::Char('d'), KeyCode::Char('y'), KeyCode::Tab] {
                m.handle(key);
            }
        };

        // Home / End jump in the session list, and are harmless on an empty one.
        m.handle(KeyCode::End);
        assert_eq!(m.list.selected(), Some(1));
        m.handle(KeyCode::Home);
        assert_eq!(m.list.selected(), Some(0));
        trash_both(&mut m);
        assert!(!path.exists() && !second.exists() && matches!(m.stage, Stage::Trash));
        assert_eq!((m.list.selected(), m.trash.len()), (None, 2));

        // Same keys in the trash list.
        m.handle(KeyCode::End);
        assert_eq!(m.trash_list.selected(), Some(1));
        m.handle(KeyCode::Home);
        // Space toggles the cursor row, whatever is already selected.
        m.handle(KeyCode::Char(' '));
        m.handle(KeyCode::Down);
        m.handle(KeyCode::Char(' '));
        assert_eq!(m.trash_selected.len(), 2);
        m.handle(KeyCode::Char(' '));
        let first_row = m.trash[0].dir.clone();
        assert_eq!(m.trash_selected.iter().collect::<Vec<_>>(), vec![&first_row]);
        m.handle(KeyCode::Char(' '));
        m.handle(KeyCode::Home);
        // A declined purge of the whole selection keeps everything.
        assert!(screen(&mut m).contains("2 selected"));
        m.handle(KeyCode::Char('x'));
        assert!(screen(&mut m).contains("Delete 2 trashed session(s) permanently?"));
        m.handle(KeyCode::Esc);
        assert_eq!((m.trash.len(), m.trash_selected.len()), (2, 2));
        // Restore the whole selection.
        m.handle(KeyCode::Char('r'));
        assert!(path.exists() && second.exists() && m.trash.is_empty() && m.trash_selected.is_empty());
        assert_eq!((m.sessions.len(), m.status.as_str()), (2, "restored 2"));

        // Trash both again, then purge only the one selected row.
        m.handle(KeyCode::Tab);
        trash_both(&mut m);
        m.handle(KeyCode::Char(' '));
        assert_eq!(m.trash_selected.len(), 1);
        m.handle(KeyCode::Char('x'));
        m.handle(KeyCode::Char('y'));
        assert_eq!((m.trash.len(), m.status.as_str()), (1, "purged 1"));
        assert!(matches!(m.stage, Stage::Trash) && m.trash_selected.is_empty());
        // With nothing selected, the cursor row is the target.
        m.handle(KeyCode::Char('r'));
        assert!(m.trash.is_empty() && m.sessions.len() == 1);
    }

    #[test]
    fn spans() {
        let (day, hour) = (86_400, 3_600);
        let good = [
            ("30D", 30 * day),
            ("2w", 14 * day),
            ("1M", 30 * day),
            ("1Y", 365 * day),
            ("T12H", 12 * hour),
            ("1DT12H", 36 * hour),
            ("T30M", 1_800),
            ("t1h30m15s", 5_415),
        ];
        for (text, secs) in good {
            assert_eq!(span_secs(text), Some(secs), "{text}");
        }
        for bad in ["", "30", "D", "12H", "T", "x", "1D2", "1S", "99999999999999999999D", "999999999999999Y"] {
            assert_eq!(span_secs(bad), None, "{bad}");
        }
    }

    /// Fake home with two folders: `proj` (newer session, owned by another account) and `other`.
    fn two_folders(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (root, path) = fixture(name, &[OWNER_LINE, USER_LINE], OLD);
        std::fs::write(root.join(".claude.json"), ACCOUNT_JSON).unwrap();
        let other = root.join("projects").join("D--other");
        std::fs::create_dir_all(&other).unwrap();
        let second = other.join("99999999-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&second, r#"{"type":"user","cwd":"D:\\other","message":{"content":"second session"}}"#).unwrap();
        backdate(&second, OLD * 2);
        (root, path, second)
    }

    #[test]
    fn filter_queries_and_key_guide() {
        let (root, path, second) = two_folders("query");
        let mut m = loaded(root);
        let shown = |m: &mut Model, filter: &str| -> Vec<PathBuf> {
            m.filter = filter.into();
            m.refilter();
            m.visible.iter().map(|&i| m.sessions[i].path.clone()).collect()
        };
        let (first, other, both) = (vec![path.clone()], vec![second.clone()], vec![path, second]);
        let none: Vec<PathBuf> = Vec::new();

        // Text terms: any may match, case-insensitively; blank terms are ignored.
        assert_eq!(shown(&mut m, "FIRST"), first);
        assert_eq!(shown(&mut m, "nomatch, second"), other);
        assert_eq!(shown(&mut m, "first, second"), both);
        assert_eq!(shown(&mut m, " , ,"), both);
        assert_eq!(shown(&mut m, "regex:^SECOND s"), other);
        assert_eq!(shown(&mut m, "regex:^first|^second"), both);
        assert_eq!(shown(&mut m, "nomatch, regex:prompt\\s+here$"), first);
        // Folder terms match the folder name; they combine with text terms.
        assert_eq!(shown(&mut m, "group:other"), other);
        assert_eq!(shown(&mut m, "group:regex:^pro, group:nothing"), first);
        assert_eq!(shown(&mut m, "group:proj, group:other, second"), other);
        assert_eq!(shown(&mut m, "group:proj, second"), none);
        // Age terms: the fixtures were last written 1 h (proj) and 2 h (other) ago.
        assert_eq!(shown(&mut m, "older:T90M"), other);
        assert_eq!(shown(&mut m, "newer:T90M"), first);
        assert_eq!(shown(&mut m, "older:T30M, newer:T90M"), first);
        assert_eq!(shown(&mut m, "older:1D"), none);
        assert_eq!(shown(&mut m, "older:t30m, group:other"), other);
        // A term that cannot be understood matches nothing and is reported, not a panic.
        assert_eq!(shown(&mut m, "regex:(unclosed"), none);
        assert_eq!(m.filter_error.as_deref(), Some("regex:(unclosed"));
        assert!(screen(&mut m).contains("bad filter: regex:(unclosed"));
        assert_eq!(shown(&mut m, "first, regex:(unclosed"), first);
        assert_eq!(shown(&mut m, "older:soon"), none);
        assert_eq!(m.filter_error.as_deref(), Some("older:soon"));
        assert_eq!(shown(&mut m, "newer:12H"), none); // hours need the T
        assert_eq!(shown(&mut m, ""), both);
        assert!(m.filter_error.is_none());

        // Folder tabs list only the folders the filter leaves something in.
        shown(&mut m, "group:other");
        assert_eq!(m.group_labels(), vec!["[all]", "other"]);
        shown(&mut m, "first");
        assert_eq!(m.group_labels(), vec!["[all]", "proj"]);
        shown(&mut m, "");
        assert_eq!(m.group_labels(), vec!["[all]", "proj", "other"]);

        // `?` opens the guide; it scrolls on a small terminal, and any other key only closes it.
        m.handle(KeyCode::Char('?'));
        let guide = screen_sized(&mut m, 60, 12);
        assert!(guide.contains("toggle selection") && !guide.contains("regex:<pattern>"), "{guide}");
        m.handle(KeyCode::Down);
        m.handle(KeyCode::PageDown);
        let guide = screen_sized(&mut m, 60, 12);
        assert!(m.help && m.help_scroll == 11 && guide.contains("regex:<pattern>"), "{guide}");
        m.handle(KeyCode::PageDown);
        m.handle(KeyCode::PageDown);
        m.handle(KeyCode::PageDown);
        screen_sized(&mut m, 60, 12);
        assert!(m.help_scroll < 30); // clamped to the content
        m.handle(KeyCode::Char('d'));
        assert!(!m.help && matches!(m.stage, Stage::Browse));

        // While typing a filter, `?` is text and the arrows still switch folder.
        m.handle(KeyCode::Char('/'));
        m.handle(KeyCode::Char('?'));
        assert_eq!((m.help, m.filter.as_str()), (false, "?"));
        m.handle(KeyCode::Backspace);
        m.handle(KeyCode::Right);
        assert_eq!((m.group, m.filter.as_str(), m.visible.len()), (1, "", 1));
        assert!(matches!(m.stage, Stage::Filter));
    }

    /// Two folders, one session owned by another account: selection, groups, detail, claim.
    #[test]
    fn groups_detail_and_claim() {
        let (root, path, second) = two_folders("groups");
        let mut m = loaded(root);
        assert_eq!((m.visible.len(), m.group_labels()), (2, vec!["[all]", "proj", "other"]));
        assert!(screen(&mut m).contains("1 foreign"));

        // Space toggles in place.
        m.handle(KeyCode::Char(' '));
        assert_eq!((m.list.selected(), m.selected.len()), (Some(0), 1));
        m.handle(KeyCode::Char(' '));
        assert!(m.selected.is_empty());

        // Right walks the folders and wraps back to [all]; left wraps the other way.
        m.handle(KeyCode::Right);
        assert_eq!((m.group, m.visible.len(), m.cursor().map(|s| s.path.clone())), (1, 1, Some(path.clone())));
        m.handle(KeyCode::Right);
        assert_eq!((m.group, m.cursor().map(|s| s.path.clone())), (2, Some(second.clone())));
        m.handle(KeyCode::Right);
        assert_eq!((m.group, m.visible.len()), (0, 2));
        m.handle(KeyCode::Left);
        assert_eq!(m.group, 2);
        m.handle(KeyCode::Right);

        // Detail overlay opens over the list and closes with esc.
        m.handle(KeyCode::Enter);
        assert!(matches!(m.stage, Stage::Detail));
        let shown = screen(&mut m);
        assert_eq!(m.detail_scroll, 0); // opens at the top
        assert!(shown.contains("you  first  prompt") && shown.contains("owner: other account"), "{shown}");
        for key in [KeyCode::Up, KeyCode::PageDown, KeyCode::Home, KeyCode::End] {
            m.handle(key);
            screen(&mut m);
        }
        m.handle(KeyCode::Esc);
        assert!(matches!(m.stage, Stage::Browse));

        // The unowned session cannot be claimed; the foreign one can.
        m.handle(KeyCode::Down);
        m.handle(KeyCode::Char('c'));
        assert!(matches!(m.stage, Stage::Browse) && m.status.starts_with("nothing to claim"));
        m.handle(KeyCode::Up);
        assert!(m.foreign(m.cursor().unwrap()));
        m.handle(KeyCode::Char('c'));
        assert!(screen(&mut m).contains("Change owner of 1 session(s)"));
        m.handle(KeyCode::Char('y'));
        assert_eq!(m.cursor().unwrap().owner.as_deref(), Some("me"));
        assert!(!m.foreign(m.cursor().unwrap()) && m.status == "claimed 1 session(s)");
        assert_eq!(sessions::parse(&path).unwrap().owner.as_deref(), Some("me"));
    }
}
