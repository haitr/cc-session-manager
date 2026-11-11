//! Draw the current stage. Shared by the terminal and standalone hosts.
use std::collections::HashSet;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget,
    Widget, Wrap,
};
use ratatui_braille_bar::BrailleBar;
use ratatui_cheese::select::{Select, SelectOption, SelectStyles};
use ratatui_cheese::theme::Palette;

use crate::app::{Model, Pending, Stage, project};
use crate::sessions::{age, human_size};

/// Sessions owned by another account.
const FOREIGN: Color = Color::Magenta;

fn title(p: &Palette) -> Style {
    Style::default().fg(p.primary).add_modifier(Modifier::BOLD)
}

fn dim(p: &Palette) -> Style {
    Style::default().fg(p.faint)
}

pub fn view(frame: &mut Frame, m: &mut Model) {
    let area = frame.area();
    let [stats, groups, info, body, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(2),
    ])
    .areas(area);

    stats_row(frame, m, stats);
    group_row(frame, m, groups);
    info_row(frame, m, info);

    // A pending question replaces the body with a cheese Select.
    let question = match &m.stage {
        Stage::Confirm(Pending::Trash(targets)) => Some((
            format!("Move {} session(s) to the trash?", targets.len()),
            ["Cancel", "Move to trash"],
        )),
        Stage::Confirm(Pending::Claim(targets)) => Some((
            format!("Change owner of {} session(s) to the signed-in account?", targets.len()),
            ["Cancel", "Change owner"],
        )),
        Stage::Confirm(Pending::Purge(_)) => Some((
            "Delete this trashed session permanently? This cannot be undone.".to_string(),
            ["Cancel", "Delete forever"],
        )),
        _ => None,
    };
    let hint = match (&question, &m.stage) {
        (Some(_), _) => "↑↓ move · enter choose · esc cancel",
        (_, Stage::Loading) => "esc quit",
        (_, Stage::Filter) => "type to filter · ↑↓ move · ←→ folder · enter keep · esc clear · ? in browse for syntax",
        (_, Stage::Detail) => "↑↓ scroll · pgup/pgdn page · home/end · y copy resume · esc close",
        (_, Stage::Trash) => "↑↓ move · r restore · x delete forever · tab/esc back",
        _ => "↑↓ move · ←→ folder · space select · a all · enter open · / filter",
    };
    // Browse gets a second line of actions, ending in the trash key and how much is in there.
    let actions = question.is_none() && matches!(m.stage, Stage::Browse);
    match (question, &m.stage) {
        (Some((prompt, options)), _) => {
            let options: Vec<SelectOption> = options.iter().map(|&option| option.into()).collect();
            let select = Select::new(prompt.as_str(), &options)
                .cursor_indicator("›")
                .styles(SelectStyles::from_palette(&m.palette));
            StatefulWidget::render(&select, body, frame.buffer_mut(), &mut m.choice);
        }
        (_, Stage::Loading) => loading(frame, m, body),
        (_, Stage::Trash) => trash(frame, m, body),
        (_, Stage::Detail) => {
            browse(frame, m, body);
            detail(frame, m, body);
        }
        _ => browse(frame, m, body),
    }
    let [hint_area, actions_area] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(help);
    Widget::render(Line::styled(hint, dim(&m.palette)), hint_area, frame.buffer_mut());
    if actions {
        let badge = Style::default().fg(m.palette.on_highlight).bg(m.palette.highlight);
        let line = Line::from(vec![
            Span::styled("d trash · c claim owner · y copy resume · ? all keys · esc quit · ", dim(&m.palette)),
            Span::styled(format!(" tab Trash {} ", m.trash.len()), badge),
        ]);
        Widget::render(line, actions_area, frame.buffer_mut());
    }
    if m.help {
        key_guide(frame, m, area);
    }
}

/// Key guide rows: `("", heading)` or `(keys, action)`.
const KEYS: &[(&str, &str)] = &[
    ("", "BROWSE"),
    ("↑ ↓  j k", "move"),
    ("PgUp PgDn", "move 10 rows"),
    ("← →", "previous / next folder"),
    ("space", "toggle selection"),
    ("a", "select / unselect all shown"),
    ("enter", "open transcript"),
    ("/", "filter"),
    ("d", "move selection to trash"),
    ("c", "change owner to you"),
    ("y", "copy resume command"),
    ("tab", "trash view"),
    ("esc  q", "clear filter / quit"),
    ("?", "this guide"),
    ("", ""),
    ("", "FILTER  (comma-separated terms)"),
    ("text", "label, path or id contains it"),
    ("regex:<pattern>", "same, as a regular expression"),
    ("group:<text>", "folder name; group:regex:<pattern> too"),
    ("older:<span>", "idle at least 30D, 2W, 1M, T12H ..."),
    ("newer:<span>", "active within that span"),
    ("a, b", "either one; different kinds all hold"),
    ("← →", "previous / next folder"),
    ("enter  esc", "keep / clear"),
    ("", ""),
    ("", "TRANSCRIPT"),
    ("↑ ↓  PgUp PgDn", "scroll"),
    ("Home  End", "top / bottom"),
    ("y", "copy resume command"),
    ("esc", "close"),
    ("", ""),
    ("", "TRASH"),
    ("r", "restore"),
    ("x", "delete forever"),
    ("tab  esc", "back"),
];

/// One scrollable column, centred, so it also fits a small terminal.
fn key_guide(frame: &mut Frame, m: &mut Model, area: Rect) {
    let width = area.width.min(60);
    let outer = Rect { x: area.x + (area.width - width) / 2, width, ..area }.inner(Margin::new(0, 1));
    let p = &m.palette;
    let block = Block::bordered().border_style(title(p)).title(" keys · ↑↓ scroll · any other key closes ");
    let inner = block.inner(outer).inner(Margin::new(1, 0));
    frame.render_widget(Clear, outer);
    frame.render_widget(block, outer);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|&(keys, action)| match keys {
            "" => Line::styled(action, title(p)),
            _ => Line::from(vec![Span::styled(format!("{keys:<17}"), Style::default().fg(p.primary)), Span::raw(action)]),
        })
        .collect();
    let scroll = m.help_scroll.min((KEYS.len() as u16).saturating_sub(inner.height));
    let text_area = Rect { width: inner.width.saturating_sub(1), ..inner }; // keep the scrollbar column free
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), text_area);
    scrollbar(frame, inner, KEYS.len(), scroll as usize);
    m.help_scroll = scroll;
}

fn stats_row(frame: &mut Frame, m: &Model, area: Rect) {
    let total: u64 = m.sessions.iter().map(|s| s.size).sum();
    let projects = m.sessions.iter().filter_map(|s| s.path.parent()).collect::<HashSet<_>>().len();
    let foreign = m.sessions.iter().filter(|s| m.foreign(s)).count();
    let summary = format!("ccsm  {} sessions · {projects} projects · {}", m.sessions.len(), human_size(total));
    let mut spans = vec![Span::styled(summary, title(&m.palette))];
    if foreign > 0 {
        spans.push(Span::styled(format!("  · {foreign} foreign"), Style::default().fg(FOREIGN)));
    }
    Widget::render(Line::from(spans), area, frame.buffer_mut());
}

/// Folder tabs. Drawing starts at the latest tab that still leaves the active one on screen.
fn group_row(frame: &mut Frame, m: &Model, area: Rect) {
    let labels = m.group_labels();
    let width = |label: &str| Line::raw(label).width() + 2;
    let mut start = m.group;
    let mut used = width(labels[start]);
    while start > 0 && used + width(labels[start - 1]) <= area.width as usize {
        start -= 1;
        used += width(labels[start]);
    }
    let spans: Vec<Span> = labels
        .iter()
        .enumerate()
        .skip(start)
        .map(|(i, label)| {
            let style = if i == m.group {
                Style::default().fg(m.palette.on_highlight).bg(m.palette.highlight)
            } else {
                dim(&m.palette)
            };
            Span::styled(format!(" {label} "), style)
        })
        .collect();
    Widget::render(Line::from(spans), area, frame.buffer_mut());
}

fn info_row(frame: &mut Frame, m: &Model, area: Rect) {
    let mut spans = Vec::new();
    if matches!(m.stage, Stage::Filter) {
        spans.push(Span::styled(format!("/{}▏ ", m.filter), title(&m.palette)));
    } else if !m.filter.is_empty() {
        spans.push(Span::styled(format!("/{}  ", m.filter), dim(&m.palette)));
    }
    if let Some(pattern) = &m.filter_error {
        spans.push(Span::styled(format!("bad filter: {pattern}  "), Style::default().fg(Color::Red)));
    }
    spans.push(Span::styled(m.status.as_str(), Style::default().fg(Color::Yellow)));
    Widget::render(Line::from(spans), area, frame.buffer_mut());
}

fn loading(frame: &mut Frame, m: &mut Model, area: Rect) {
    let (done, total) = m.progress;
    let spin_w = Line::raw(m.spinner_state.frame_str()).width() as u16;
    let text = format!("Scanning sessions… {done}/{total}  ");
    let text_w = Line::raw(text.as_str()).width() as u16;
    let [row, _] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    let [spin_area, text_area, bar, _] = Layout::horizontal([
        Constraint::Length(spin_w + 1),
        Constraint::Length(text_w),
        Constraint::Length(12),
        Constraint::Fill(1),
    ])
    .areas(row);
    frame.render_stateful_widget(&m.spinner, spin_area, &mut m.spinner_state);
    Widget::render(Line::styled(text, title(&m.palette)), text_area, frame.buffer_mut());
    Widget::render(
        BrailleBar::new(done as f64, total.max(1) as f64).fill_color(m.palette.primary),
        bar,
        frame.buffer_mut(),
    );
}

/// Bordered, highlighted, self-scrolling list. cheese `Select` has no scroll offset, so long
/// lists use ratatui's `List` plus a scrollbar.
fn list(frame: &mut Frame, area: Rect, heading: String, items: Vec<ListItem>, state: &mut ListState, p: &Palette) {
    let block = Block::bordered().border_style(dim(p)).title(heading);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let len = items.len();
    let widget = List::new(items)
        .highlight_style(Style::default().fg(p.on_highlight).bg(p.highlight))
        .highlight_symbol("› ");
    frame.render_stateful_widget(widget, list_area(inner, len), state);
    scrollbar(frame, inner, len, state.offset());
}

/// Transcript lines: speaker tag on the first line of each message, blank separator after.
fn message_lines<'a>(messages: &'a [(bool, String)], p: &Palette) -> Vec<Line<'a>> {
    messages
        .iter()
        .flat_map(|(is_user, text)| {
            let who = if *is_user { Span::styled("you  ", title(p)) } else { Span::styled("claude  ", dim(p)) };
            let mut rest = text.lines();
            let first = Line::from(vec![who, Span::raw(rest.next().unwrap_or_default())]);
            std::iter::once(first).chain(rest.map(Line::raw)).chain(std::iter::once(Line::raw("")))
        })
        .collect()
}

fn browse(frame: &mut Frame, m: &mut Model, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);

    let items: Vec<ListItem> = m
        .visible
        .iter()
        .map(|&i| {
            let s = &m.sessions[i];
            let mark = if m.selected.contains(&s.path) { "[x]" } else { "[ ]" };
            let dot = if s.active(&m.live) { "●" } else { " " };
            let star = if s.starred { "★ " } else { "" };
            let row = format!("{mark} {dot} {:>3} {:<14.14} {star}{}", age(s.mtime), project(s), s.label);
            let style = if m.foreign(s) { Style::default().fg(FOREIGN) } else { Style::default() };
            ListItem::new(row).style(style)
        })
        .collect();
    let heading = format!(
        " sessions {}/{} · {} selected ",
        m.list.selected().map_or(0, |i| i + 1).min(m.visible.len()),
        m.visible.len(),
        m.selected.len()
    );
    list(frame, left, heading, items, &mut m.list, &m.palette);

    let heading = m
        .cursor()
        .map(|s| format!(" {} · {} · {} ", s.uuid.chars().take(8).collect::<String>(), s.cwd, human_size(s.size)))
        .unwrap_or_else(|| " preview ".into());
    let block = Block::bordered().border_style(dim(&m.palette)).title(heading);
    let inner = block.inner(right);
    frame.render_widget(block, right);
    let paragraph = Paragraph::new(message_lines(&m.preview, &m.palette)).wrap(Wrap { trim: true });
    // Pin to the bottom: the latest messages are the ones that identify a session.
    let overflow = paragraph.line_count(inner.width).saturating_sub(inner.height as usize);
    frame.render_widget(paragraph.scroll((overflow as u16, 0)), inner);
}

/// Full transcript, drawn over the list.
fn detail(frame: &mut Frame, m: &mut Model, area: Rect) {
    let outer = area.inner(Margin::new(2, 0));
    let heading = m.cursor().map_or_else(String::new, |s| {
        let owner = match &s.owner {
            None => "none recorded",
            Some(_) if m.foreign(s) => "other account",
            Some(_) => "you",
        };
        format!(" {} · {} · {} · owner: {owner} ", s.uuid, s.cwd, human_size(s.size))
    });
    let block = Block::bordered().border_style(title(&m.palette)).title(heading);
    let inner = block.inner(outer);
    frame.render_widget(Clear, outer);
    frame.render_widget(block, outer);

    let text_area = Rect { width: inner.width.saturating_sub(1), ..inner }; // keep the scrollbar column free
    // trim: false keeps the indentation of code and lists.
    let paragraph = Paragraph::new(message_lines(&m.detail, &m.palette)).wrap(Wrap { trim: false });
    let total = paragraph.line_count(text_area.width);
    // ponytail: scroll offset is u16 (ratatui's type), so a transcript past 65k wrapped lines
    // cannot reach its end; page through a windowed slice if one ever gets that long.
    let max = total.saturating_sub(inner.height as usize).min(u16::MAX as usize) as u16;
    let scroll = m.detail_scroll.min(max);
    frame.render_widget(paragraph.scroll((scroll, 0)), text_area);
    scrollbar(frame, inner, total, scroll as usize);
    m.detail_scroll = scroll;
    m.detail_page = inner.height.max(1);
}

fn trash(frame: &mut Frame, m: &mut Model, area: Rect) {
    let items: Vec<ListItem> = m
        .trash
        .iter()
        .map(|t| {
            let project = t.original.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy();
            let uuid = t.original.file_stem().unwrap_or_default().to_string_lossy();
            ListItem::new(format!("{:>4}  {:>9}  {uuid}  {project}", age(t.trashed), human_size(t.size)))
        })
        .collect();
    let heading = format!(" trash · {} item(s) ", m.trash.len());
    list(frame, area, heading, items, &mut m.trash_list, &m.palette);
}

/// Area left for a list: `area` minus the scrollbar column when `len` overflows it.
fn list_area(area: Rect, len: usize) -> Rect {
    if len <= area.height as usize {
        return area;
    }
    Rect { width: area.width.saturating_sub(1), ..area }
}

/// Draw the vertical scrollbar on `area`'s right edge; no-op when everything fits.
fn scrollbar(frame: &mut Frame, area: Rect, len: usize, offset: usize) {
    let height = area.height as usize;
    if len <= height {
        return;
    }
    let mut state = ScrollbarState::new(len - height + 1).position(offset);
    frame.render_stateful_widget(Scrollbar::new(ScrollbarOrientation::VerticalRight), area, &mut state);
}
