//! Drawing: a pure function of [`App`]. Layout helpers are shared with `update` so paging and
//! scrolling agree with what is on screen.

use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::identity::Identity;
use crate::index::Entry;
use crate::stats::{self, ModelRow, Table};
use crate::transcript::{Role, one_line};
use crate::usage::{self, Resets, UsageRow};
use crate::{text, usage::format_age};

use super::app::{
    AccountState, App, Confirm, Form, FormKind, Level, ListState, Mode, Overlay, PREVIEW_MESSAGES,
    Pick, PickFor, ResumeCodex, View, short_id,
};
use super::{privacy, timeline};
use crate::live::Control;
use crate::provider::Provider;
use crate::registry::Account;

/// From this width on, the preview sits beside the list instead of below it.
pub const WIDE: u16 = 120;

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);
const WARN: Style = Style::new().fg(Color::Yellow);
const CRIT: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
const OK: Style = Style::new().fg(Color::Green);
const USER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const CODEX_STYLE: Style = Style::new().fg(Color::Magenta);

/// Header, body, status and hints.
struct Frame4 {
    header: Rect,
    body: Rect,
    status: Rect,
    hints: Rect,
}

fn frame_areas(area: Rect) -> Frame4 {
    let [header, body, status, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    Frame4 {
        header,
        body,
        status,
        hints,
    }
}

fn screen(app: &App) -> Rect {
    Rect::new(0, 0, app.size.0, app.size.1)
}

/// Areas of a list view (live, history): the list with its column header (and search line)
/// and the preview; the list is absent while the preview is expanded.
struct Split {
    list: Option<Rect>,
    preview: Rect,
}

fn split(app: &App, body: Rect) -> Split {
    if app.preview.expanded {
        return Split {
            list: None,
            preview: body,
        };
    }
    let (list, preview) = if body.width >= WIDE {
        let [list, _, preview] = Layout::horizontal([
            Constraint::Percentage(58),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(body);
        (list, preview)
    } else {
        let [list, preview] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]).areas(body);
        (list, preview)
    };
    Split {
        list: Some(list),
        preview,
    }
}

/// Lines above the rows of a list: column header, plus the search line in history.
fn list_chrome(app: &App, view: View) -> u16 {
    let search = view == View::History && (app.history.searching || !app.history.query.is_empty());
    1 + u16::from(search)
}

/// Rows the list of `view` can show.
pub fn list_height(app: &App, view: View) -> usize {
    let body = frame_areas(screen(app)).body;
    match view {
        View::Accounts => app.accounts.len(),
        View::Live | View::History => {
            let area = split(app, body).list.unwrap_or_default();
            area.height.saturating_sub(list_chrome(app, view)) as usize
        }
        View::Stats => stats_height(app),
    }
}

/// The preview's text area (below its title line).
fn preview_text_area(app: &App) -> Rect {
    let body = frame_areas(screen(app)).body;
    let area = split(app, body).preview;
    Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    }
}

pub fn preview_height(app: &App) -> usize {
    preview_text_area(app).height as usize
}

/// Wrapped preview lines at the current width.
pub fn preview_line_count(app: &App) -> usize {
    preview_lines(app, preview_text_area(app).width as usize).len()
}

/// Draws `app`; in private mode, only its redacted copy is drawn (R21).
pub fn render(app: &App, f: &mut Frame) {
    render_with(app, &mut privacy::Snapshot::default(), f);
}

/// [`render`], with the redacted copy kept in `snapshot` from frame to frame.
pub fn render_with(app: &App, snapshot: &mut privacy::Snapshot, f: &mut Frame) {
    if app.private {
        draw(snapshot.of(app), f);
    } else {
        draw(app, f);
    }
}

fn draw(app: &App, f: &mut Frame) {
    let areas = frame_areas(f.area());
    let header = header(app);
    let keys = Line::styled("?: help · q: quit ", DIM);
    // The views come first where both do not fit (private mode's mark takes room).
    let room = header.width() + keys.width() <= areas.header.width as usize;
    f.render_widget(header, areas.header);
    if room {
        f.render_widget(
            Paragraph::new(keys).alignment(Alignment::Right),
            areas.header,
        );
    }
    match app.view {
        View::Accounts => accounts_view(app, f, areas.body),
        View::Live => {
            let s = split(app, areas.body);
            if let Some(list) = s.list {
                live_list(app, f, list);
            }
            preview(app, f, s.preview);
        }
        View::History => {
            let s = split(app, areas.body);
            if let Some(list) = s.list {
                history_list(app, f, list);
            }
            preview(app, f, s.preview);
        }
        View::Stats => stats_view(app, f, areas.body),
    }
    f.render_widget(Paragraph::new(status_line(app)), areas.status);
    f.render_widget(Paragraph::new(Line::styled(hints(app), DIM)), areas.hints);
    match &app.overlay {
        Some(Overlay::Pick(pick)) => pick_box(app, pick, f, f.area()),
        Some(Overlay::Form(form)) => form_box(form, f, f.area()),
        Some(Overlay::Confirm(confirm)) => confirm_box(confirm, f, f.area()),
        Some(Overlay::ResumeCodex(confirm)) => resume_codex_box(app, confirm, f, f.area()),
        Some(Overlay::RemoveAccount(account)) => remove_account_box(app, account, f, f.area()),
        None => {}
    }
    if app.help {
        help(f, f.area());
    }
}

/// Private mode's mark in the header (R21).
const PRIVATE: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);

fn header(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled(" remuda ", BOLD)];
    if app.private {
        spans.push(Span::styled(" PRIVATE ", PRIVATE));
        spans.push(Span::raw(" "));
    }
    if app.mode == Mode::PickForRun {
        spans.push(Span::raw(" run: pick an account "));
        return Line::from(spans);
    }
    for (i, view) in View::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, view.title());
        spans.push(if *view == app.view {
            Span::styled(label, SELECTED)
        } else {
            Span::raw(label)
        });
    }
    Line::from(spans)
}

fn index_status(app: &App) -> Span<'static> {
    if let Some((done, total)) = app.indexing {
        return Span::styled(format!("indexing {done}/{total}"), WARN);
    }
    match app.index_refreshed {
        Some(at) => Span::raw(format!(
            "{} sessions · refreshed {}",
            app.index.entries.len(),
            at.to_zoned(app.tz.clone()).strftime("%H:%M:%S")
        )),
        None => Span::raw("loading index…"),
    }
}

fn status_line(app: &App) -> Line<'static> {
    if let Some(notice) = &app.notice {
        let style = match notice.level {
            Level::Info => OK,
            Level::Warn => WARN,
            Level::Error => CRIT,
        };
        return Line::styled(format!(" {}", notice.text), style);
    }
    if let Some((_, request)) = &app.pending {
        let what = match request.resumes() {
            Some(id) => format!("that {} is not running", short_id(&id)),
            None => request.what.clone(),
        };
        return Line::raw(format!(" checking {what}… (esc: cancel)"));
    }
    let sep = || Span::styled(" │ ", DIM);
    let mut spans = vec![Span::raw(" ")];
    match app.view {
        View::History => {
            let (shown, total) = (app.history.rows.len(), app.index.entries.len());
            let toggle = if app.history.show_all {
                "a: hide teammate/sdk"
            } else {
                "a: show all"
            };
            spans.push(Span::raw(format!("showing {shown} of {total} ({toggle})")));
            spans.push(sep());
        }
        View::Live => {
            let what = if app.live_loaded {
                let stopped = app.live.iter().filter(|s| s.is_inactive()).count();
                let running = app.live.len() - stopped;
                match (stopped, app.live_show_inactive) {
                    (0, _) => format!("{running} running"),
                    (n, false) => format!("{running} running · {n} stopped (a: show)"),
                    (n, true) => format!("{running} running · {n} stopped (a: hide)"),
                }
            } else {
                "collecting…".to_string()
            };
            spans.push(Span::raw(what));
            spans.push(sep());
            if app.accounts.iter().any(|a| !a.account.provider.has_live()) {
                spans.push(Span::styled(
                    "codex sessions can't be listed as running",
                    DIM,
                ));
                spans.push(sep());
            }
        }
        View::Accounts => {}
        View::Stats => {
            spans.push(stats_status(app));
            if let Some(e) = &app.stats.error {
                spans.push(sep());
                spans.push(Span::styled(format!("stats cache: {e}"), CRIT));
            }
            return Line::from(spans);
        }
    }
    if app.mode == Mode::PickForRun {
        return Line::from(spans);
    }
    spans.push(index_status(app));
    if let Some(e) = &app.index_error {
        spans.push(sep());
        spans.push(Span::styled(format!("index cache: {e}"), CRIT));
    }
    Line::from(spans)
}

fn hints(app: &App) -> String {
    let text = if let Some(overlay) = &app.overlay {
        match overlay {
            Overlay::Pick(_) => "j/k: move · enter: choose · esc: cancel",
            Overlay::Confirm(_) | Overlay::ResumeCodex(_) | Overlay::RemoveAccount(_) => {
                "y: yes · any other key: cancel"
            }
            Overlay::Form(form) => match &form.kind {
                FormKind::NewSession { account } => {
                    let agent = account.provider.program();
                    return format!(" tab/↑/↓: field · enter: start {agent} · esc: cancel");
                }
                FormKind::Setup => "tab/↑/↓: field · enter: set up and log in · esc: cancel",
            },
        }
    } else if app.mode == Mode::PickForRun {
        return format!(
            " enter: launch {} · u: live usage · r: refresh · esc/q: cancel",
            selected_agent(app)
        );
    } else if app.history.searching && app.view == View::History {
        "type to filter · ↑/↓: move · enter: keep filter · esc: clear"
    } else if app.preview.expanded && app.view != View::Accounts {
        "j/k/pgup/pgdn: scroll · esc/p: back · enter: resume · f: fork · c: continue as…"
    } else {
        match app.view {
            View::Accounts => {
                "n: new session · s: set up an account · D: remove · u: live usage · r: refresh"
            }
            View::Live => {
                "enter: attach · f: fork · c: continue as… · p: preview · l: logs · x: stop · \
                 D: rm · a: stopped"
            }
            View::History => {
                "enter: resume · f: fork · c: continue as… · p: preview · /: search · a: show all"
            }
            View::Stats => "t: period · j/k: scroll · r: refresh",
        }
    };
    format!(" {text}")
}

/// The agent of the selected account (`claude`, `codex`).
fn selected_agent(app: &App) -> &'static str {
    app.accounts
        .get(app.accounts_list.selected)
        .map_or("claude", |a| a.account.provider.program())
}

/// A section title with a rule after it.
fn section(title: &str, width: u16) -> Line<'static> {
    let rule = (width as usize).saturating_sub(text::width(title) + 1);
    Line::from(vec![
        Span::styled(title.to_string(), BOLD),
        Span::styled(format!(" {}", "─".repeat(rule)), DIM),
    ])
}

/// Column widths for `constraints` over `width`, one space between columns.
fn column_widths(width: u16, constraints: &[Constraint]) -> Vec<usize> {
    Layout::horizontal(constraints.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, width, 1))
        .iter()
        .map(|r| r.width as usize)
        .collect()
}

/// A table row: each cell truncated and padded to its column.
fn row(cells: Vec<(String, Style)>, widths: &[usize]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, ((cell, style), w)) in cells.into_iter().zip(widths).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            text::pad(&text::truncate(&cell, *w), *w),
            style,
        ));
    }
    Line::from(spans)
}

fn plain(s: impl Into<String>) -> (String, Style) {
    (s.into(), Style::new())
}

fn dim(s: impl Into<String>) -> (String, Style) {
    (s.into(), DIM)
}

/// Rows of a list with the selection highlighted, from the scroll offset.
fn visible_rows(
    list: ListState,
    len: usize,
    height: usize,
    mut make: impl FnMut(usize) -> Line<'static>,
) -> Vec<Line<'static>> {
    let offset = list.offset.min(len.saturating_sub(height));
    (offset..len.min(offset + height))
        .map(|i| {
            let line = make(i);
            if i == list.selected {
                line.style(SELECTED)
            } else {
                line
            }
        })
        .collect()
}

/// Short account name: the provider prefix is dropped for claude.
fn short(account: &str) -> &str {
    account.strip_prefix("claude:").unwrap_or(account)
}

/// `$HOME/...` as `~/...`.
fn tilde(app: &App, path: &str) -> String {
    match app.home.as_deref() {
        Some(home) if !home.is_empty() => match path.strip_prefix(home) {
            Some("") => "~".to_string(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// The end of a path when it is too wide: `…/space/remuda`.
fn tail(s: &str, max: usize) -> String {
    if text::width(s) <= max || max == 0 {
        return s.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = text::width(c.encode_utf8(&mut [0; 4]));
        if used + w > max {
            break;
        }
        kept.push(c);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

// ---- Accounts ----------------------------------------------------------------------

/// A usage column: its header and the row label it shows.
fn usage_columns(app: &App) -> Vec<(String, String)> {
    let mut labels: Vec<String> = vec!["Session".into(), "Week (all models)".into()];
    for a in &app.accounts {
        for r in a.rows() {
            if !labels.contains(&r.label) {
                labels.push(r.label.clone());
            }
        }
    }
    labels
        .into_iter()
        .map(|label| {
            let header = match label.as_str() {
                "Session" => "SESSION".to_string(),
                "Week (all models)" => "WEEK".to_string(),
                other => match scope(other) {
                    Some(("Week", model)) => model.to_string(),
                    Some((_, model)) => format!("{model} 5h"),
                    None => other.to_string(),
                },
            };
            (header, label)
        })
        .collect()
}

fn severity_style(r: &UsageRow) -> Style {
    match usage::severity(r) {
        "critical" => CRIT,
        "warning" => WARN,
        _ => Style::new(),
    }
}

fn identity_cells(a: &AccountState) -> [(String, Style); 3] {
    let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
    match &a.identity {
        None => [dim("…"), dim("…"), dim("…")],
        Some(identity @ Identity::LoggedIn { org, plan, .. }) => [
            plain(identity.who()),
            plain(or_dash(org)),
            plain(or_dash(plan)),
        ],
        Some(Identity::NotLoggedIn) => [(String::from("not logged in"), WARN), dim("-"), dim("-")],
        Some(Identity::Unknown) => [dim("unknown"), dim("-"), dim("-")],
    }
}

fn source_cell(a: &AccountState, now: Timestamp) -> (String, Style) {
    if a.live_pending {
        return ("live…".to_string(), WARN);
    }
    match (&a.live, &a.cached) {
        (Some(Ok((_, at))), _) => (format!("live {}", format_age(*at, now)), OK),
        (Some(Err(_)), _) => ("live failed".to_string(), CRIT),
        (None, None) => dim("…"),
        (None, Some(Ok(c))) => match c.fetched_at {
            Some(at) => dim(format!("cached {}", format_age(at, now))),
            None => dim("cached"),
        },
        (None, Some(Err(_))) => dim("no cache"),
    }
}

fn accounts_view(app: &App, f: &mut Frame, mut area: Rect) {
    if app.mode == Mode::PickForRun {
        let cwd = app
            .cwd
            .as_ref()
            .map_or("the current directory".to_string(), |d| {
                tilde(app, &d.display().to_string())
            });
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(
                    "choose an account to launch {} in {cwd}",
                    selected_agent(app)
                ),
                WARN.add_modifier(Modifier::BOLD),
            )),
            Rect { height: 1, ..area },
        );
        area.y += 1;
        area.height = area.height.saturating_sub(1);
    }
    let n = app.accounts.len() as u16;
    let [table, timeline_area, checks_area] = Layout::vertical([
        Constraint::Length(n + 2),
        Constraint::Length(n + 3),
        Constraint::Fill(1),
    ])
    .areas(area);

    // Table.
    let columns = usage_columns(app);
    let name_w = app
        .accounts
        .iter()
        .map(|a| text::width(short(&a.account.qualified())))
        .max()
        .unwrap_or(7)
        .max(7) as u16;
    // Email and org take what their widest value needs (shrinking on a narrow terminal); a
    // wide terminal leaves the rest empty after the table instead of spreading it out.
    let content_w = |i: usize, header: &str| {
        app.accounts
            .iter()
            .map(|a| text::width(&identity_cells(a)[i].0))
            .chain([header.len()])
            .max()
            .unwrap_or(0) as u16
    };
    let mut constraints = vec![
        Constraint::Length(name_w),
        Constraint::Max(content_w(0, "EMAIL")),
        Constraint::Max(content_w(1, "ORG")),
        Constraint::Length(content_w(2, "PLAN")),
    ];
    for (header, _) in &columns {
        constraints.push(Constraint::Length(text::width(header).max(5) as u16));
    }
    constraints.push(Constraint::Length(15));
    constraints.push(Constraint::Fill(1));
    let widths = column_widths(table.width, &constraints);
    let mut lines = vec![section("Accounts", table.width)];
    let mut head: Vec<(String, Style)> = ["ACCOUNT", "EMAIL", "ORG", "PLAN"]
        .into_iter()
        .map(dim)
        .collect();
    head.extend(columns.iter().map(|(h, _)| dim(h.clone())));
    head.push(dim("SOURCE"));
    lines.push(row(head, &widths));
    lines.extend(visible_rows(
        app.accounts_list,
        app.accounts.len(),
        app.accounts.len(),
        |i| {
            let a = &app.accounts[i];
            let mut cells = vec![plain(short(&a.account.qualified()))];
            cells.extend(identity_cells(a));
            let rows = a.rows();
            let loading = a.cached.is_none() && a.live.is_none();
            for (k, (_, label)) in columns.iter().enumerate() {
                // Right-aligned in its column.
                let w = widths[4 + k];
                cells.push(match rows.iter().find(|r| &r.label == label) {
                    Some(r) => (
                        format!("{:>w$}", usage::format_percent(r.percent)),
                        severity_style(r),
                    ),
                    None if loading => dim(format!("{:>w$}", "…")),
                    None => dim(format!("{:>w$}", "-")),
                });
            }
            cells.push(source_cell(a, app.now));
            row(cells, &widths)
        },
    ));
    f.render_widget(Paragraph::new(lines), table);

    timeline_view(app, f, timeline_area, name_w as usize);
    checks_view(app, f, checks_area);
}

/// A model-scoped limit's label, `Week (<model>)` or `Session (<model>)` (codex, R10):
/// `("Week" | "Session", model)`.
fn scope(label: &str) -> Option<(&'static str, &str)> {
    ["Week", "Session"].into_iter().find_map(|kind| {
        let model = label
            .strip_prefix(kind)?
            .strip_prefix(" (")?
            .strip_suffix(')')?;
        Some((kind, model))
    })
}

/// Marker for a usage row on the timeline: `S` session, `W` all-models week, the lowercase
/// initial of the model for a model-scoped week or session.
fn marker(label: &str) -> char {
    match label {
        "Session" => 'S',
        "Week (all models)" => 'W',
        other => scope(other)
            .and_then(|(_, model)| model.chars().next())
            .map_or('?', |c| c.to_ascii_lowercase()),
    }
}

/// How the timeline's legend names a row other than `S` and `W`: `week (<model>)`,
/// `session (<model>)`, else its label.
fn legend_text(label: &str) -> String {
    match scope(label) {
        Some((kind, model)) => format!("{} ({model})", kind.to_ascii_lowercase()),
        None => label.to_string(),
    }
}

/// When a row resets: its own time, or (for live wording remuda cannot read) the cached row
/// of the same limit if that is still ahead.
fn reset_of(a: &AccountState, r: &UsageRow, now: Timestamp) -> Option<Timestamp> {
    if let Some(t) = r.resets.as_ref().and_then(|x| usage::reset_instant(x, now)) {
        return Some(t);
    }
    let cached = a.cached.as_ref()?.as_ref().ok()?;
    let same = cached.rows.iter().find(|c| c.label == r.label)?;
    match same.resets {
        Some(Resets::At(t)) if t > now => Some(t),
        _ => None,
    }
}

fn timeline_view(app: &App, f: &mut Frame, area: Rect, name_w: usize) {
    const SUMMARY: usize = 18;
    let axis_w = (area.width as usize).saturating_sub(name_w + 2 + SUMMARY);
    let mut lines = vec![section("Resets · next 7 days", area.width)];
    lines.push(Line::styled(
        format!(
            "{} {} {}",
            text::pad("", name_w),
            timeline::axis(axis_w),
            "next"
        ),
        DIM,
    ));
    let mut scoped: Vec<(char, String)> = Vec::new();
    for a in &app.accounts {
        let mut marks: Vec<(char, Timestamp)> = Vec::new();
        let mut next: Vec<String> = Vec::new();
        // Model-scoped weeks first, so S and W win a shared column.
        let mut rows: Vec<&UsageRow> = a.rows().iter().collect();
        rows.sort_by_key(|r| match marker(&r.label) {
            'S' => 2,
            'W' => 1,
            _ => 0,
        });
        for r in rows {
            let Some(t) = reset_of(a, r, app.now) else {
                continue;
            };
            let m = marker(&r.label);
            if m != 'S' && m != 'W' {
                let entry = (m, legend_text(&r.label));
                if !scoped.contains(&entry) {
                    scoped.push(entry);
                }
            } else {
                next.push(format!("{m} {}", timeline::until(app.now, t)));
            }
            marks.push((m, t));
        }
        next.reverse();
        let track = timeline::track(app.now, &marks, axis_w);
        let mut spans = vec![Span::raw(format!(
            "{} ",
            text::pad(short(&a.account.qualified()), name_w)
        ))];
        for c in track.chars() {
            let style = match c {
                'S' | 'W' => BOLD.fg(Color::Cyan),
                '·' | '|' => DIM,
                _ => Style::new().fg(Color::Magenta),
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        spans.push(Span::raw(format!(" {}", next.join(" "))));
        lines.push(Line::from(spans));
    }
    let mut legend = String::from("S session · W week (all models)");
    for (c, text) in &scoped {
        legend.push_str(&format!(" · {c} {text}"));
    }
    lines.push(Line::styled(
        format!("{} {legend}", text::pad("", name_w)),
        DIM,
    ));
    f.render_widget(Paragraph::new(lines), area);
}

fn checks_view(app: &App, f: &mut Frame, area: Rect) {
    let mut lines = vec![section("Checks", area.width)];
    let mut problems: Vec<String> = Vec::new();
    if let Some(checks) = &app.checks {
        problems.extend(checks.iter().map(|c| match &c.account {
            Some(a) => format!("{}: {}", short(a), c.message),
            None => c.message.clone(),
        }));
    }
    for a in &app.accounts {
        let name = short(&a.account.qualified()).to_string();
        if a.identity == Some(Identity::NotLoggedIn) {
            problems.push(format!("{name}: not logged in"));
        }
        if let Some(Err(e)) = &a.live {
            problems.push(format!("{name}: live usage failed: {e}"));
        }
    }
    if app.checks.is_none() {
        lines.push(Line::styled("checking…", DIM));
    } else if problems.is_empty() {
        lines.push(Line::styled("✓ no problems found", OK));
    }
    for p in problems {
        for (i, l) in text::wrap(&p, (area.width as usize).saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 { "! " } else { "  " };
            lines.push(Line::styled(format!("{prefix}{l}"), WARN));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

// ---- Live ----------------------------------------------------------------------------

fn live_list(app: &App, f: &mut Frame, area: Rect) {
    let widths = column_widths(
        area.width,
        &[
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Fill(2),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Fill(3),
            Constraint::Length(8),
        ],
    );
    let mut lines = vec![row(
        [
            "ACCOUNT", "STATUS", "NAME", "KIND", "PID/ID", "STARTED", "CWD", "SESSION",
        ]
        .into_iter()
        .map(dim)
        .collect(),
        &widths,
    )];
    if !app.live_loaded {
        lines.push(Line::styled("collecting live sessions…", DIM));
    } else if app.live_rows.is_empty() {
        lines.push(Line::styled("no running sessions", DIM));
    }
    let height = list_height(app, View::Live);
    lines.extend(visible_rows(
        app.live_list,
        app.live_rows.len(),
        height,
        |row_i| {
            let s = &app.live[app.live_rows[row_i]];
            let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
            let status = dash(&s.status);
            let status_style = match status.as_str() {
                "busy" => WARN,
                "idle" => OK,
                _ if s.is_inactive() => DIM,
                _ => Style::new(),
            };
            let started = s
                .started_at
                .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                .map_or("-".to_string(), |t| format_age(t, app.now));
            let cwd = s.cwd.as_deref().map_or("-".to_string(), |c| tilde(app, c));
            let id = s.session_id.as_deref().map_or("-".to_string(), short_id);
            let handle = match (&s.pid, &s.short_id) {
                (Some(pid), _) => pid.to_string(),
                (None, Some(short)) => short.clone(),
                (None, None) => "-".to_string(),
            };
            row(
                vec![
                    plain(short(&s.account)),
                    (status, status_style),
                    plain(dash(&s.name)),
                    plain(dash(&s.kind)),
                    plain(handle),
                    plain(started),
                    plain(tail(&cwd, widths[6])),
                    dim(id),
                ],
                &widths,
            )
        },
    ));
    f.render_widget(Paragraph::new(lines), area);
}

// ---- History -------------------------------------------------------------------------

fn history_list(app: &App, f: &mut Frame, area: Rect) {
    let widths = column_widths(
        area.width,
        &[
            Constraint::Length(11),
            // `codex:default` fits.
            Constraint::Length(13),
            Constraint::Fill(3),
            Constraint::Fill(2),
        ],
    );
    let mut lines = Vec::new();
    let h = &app.history;
    if h.searching {
        lines.push(Line::from(vec![
            Span::styled("/", WARN),
            Span::raw(h.query.clone()),
            Span::styled("▏", WARN),
        ]));
    } else if !h.query.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("filter: {}", h.query), WARN),
            Span::styled("  (/: edit · esc: clear)", DIM),
        ]));
    }
    lines.push(row(
        ["TIME", "ACCOUNTS", "TITLE", "CWD"]
            .into_iter()
            .map(dim)
            .collect(),
        &widths,
    ));
    if h.rows.is_empty() {
        let note = if !app.index_loaded {
            "loading sessions…".to_string()
        } else if !h.query.is_empty() {
            format!("no matches for “{}”", h.query)
        } else if app.indexing.is_some() || app.index_in_flight {
            "indexing…".to_string()
        } else if app.index.entries.is_empty() {
            "no sessions found".to_string()
        } else {
            "every session is hidden (a: show all)".to_string()
        };
        lines.push(Line::styled(note, DIM));
    }
    let height = list_height(app, View::History);
    lines.extend(visible_rows(h.list, h.rows.len(), height, |i| {
        let Some(e) = app.index.entries.get(&h.rows[i]) else {
            return Line::raw("");
        };
        history_row(app, e, &widths)
    }));
    f.render_widget(Paragraph::new(lines), area);
}

fn history_row(app: &App, e: &Entry, widths: &[usize]) -> Line<'static> {
    let when = e
        .last_activity()
        .or_else(|| Timestamp::from_nanosecond(e.mtime_ns).ok())
        .map_or("-".to_string(), |t| {
            t.to_zoned(app.tz.clone())
                .strftime("%m-%d %H:%M")
                .to_string()
        });
    let accounts = app.entry_accounts(e);
    let accounts = if accounts.is_empty() {
        dim("-")
    } else {
        let names = accounts
            .iter()
            .map(|a| short(a))
            .collect::<Vec<_>>()
            .join(",");
        // Codex sessions stand out (`codex:` is in the name as well).
        match e.provider {
            Provider::Codex => (names, CODEX_STYLE),
            Provider::Claude => plain(names),
        }
    };
    let title = e
        .display_title()
        .map_or("-".to_string(), |t| one_line(t, 400));
    let cwd = e
        .cwd_last
        .as_deref()
        .map_or("-".to_string(), |c| tilde(app, c));
    row(
        vec![
            dim(when),
            accounts,
            plain(title),
            dim(tail(&cwd, widths[3])),
        ],
        widths,
    )
}

// ---- Preview -------------------------------------------------------------------------

/// Why there is nothing to show, or `None` when messages are loaded.
fn preview_note(app: &App) -> Option<String> {
    if let Some(logs) = &app.logs {
        return match &logs.result {
            None => Some("loading logs…".to_string()),
            Some(Err(e)) => Some(format!("cannot read logs: {e}")),
            Some(Ok(text)) if text.trim().is_empty() => Some("no output".to_string()),
            Some(Ok(_)) => None,
        };
    }
    let p = &app.preview;
    let Some(target) = &p.target else {
        return Some(match app.view {
            View::Live => match app.selected_live() {
                None => "no session selected".to_string(),
                Some(s) if s.session_id.is_none() => "session id unknown".to_string(),
                Some(_) => "transcript not indexed yet".to_string(),
            },
            _ => "no session selected".to_string(),
        });
    };
    match &p.loaded {
        Some((path, Ok(messages))) if path == target => {
            messages.is_empty().then(|| "no messages".to_string())
        }
        Some((path, Err(e))) if path == target => Some(format!("cannot read transcript: {e}")),
        _ => Some("loading preview…".to_string()),
    }
}

/// The preview's lines at `width`: messages wrapped, a blank line between them (or the
/// logs of a background session, wrapped).
fn preview_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    if let Some(logs) = &app.logs {
        let Some(Ok(text)) = &logs.result else {
            return Vec::new();
        };
        return text::wrap(text, width).into_iter().map(Line::raw).collect();
    }
    let p = &app.preview;
    let Some((path, Ok(messages))) = &p.loaded else {
        return Vec::new();
    };
    if p.target.as_ref() != Some(path) {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        let (first, style) = match m.role {
            Role::User => ("› ", USER),
            Role::Assistant => ("  ", Style::new()),
        };
        for (j, l) in text::wrap(&m.text, width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let prefix = if j == 0 { first } else { "  " };
            lines.push(Line::styled(format!("{prefix}{l}"), style));
        }
    }
    lines
}

fn preview_title(app: &App) -> String {
    let name = match app.view {
        View::Live => app.selected_live().map(|s| {
            s.name
                .clone()
                .or_else(|| {
                    s.session_id
                        .as_deref()
                        .and_then(|id| app.transcript_of(id))
                        .and_then(|p| app.index.entries.get(p))
                        .and_then(|e| e.display_title().map(|t| one_line(t, 200)))
                })
                .unwrap_or_default()
        }),
        _ => app
            .selected_entry()
            .and_then(|e| e.display_title().map(|t| one_line(t, 200))),
    };
    match name {
        Some(n) if !n.is_empty() => format!("Preview · {n}"),
        _ => "Preview".to_string(),
    }
}

fn preview(app: &App, f: &mut Frame, area: Rect) {
    let title = match &app.logs {
        Some(logs) => format!("Logs · {} (claude logs · esc: back)", logs.short_id),
        None => format!("{} (last {PREVIEW_MESSAGES} messages)", preview_title(app)),
    };
    f.render_widget(
        Paragraph::new(section(
            &text::truncate(&title, area.width as usize),
            area.width,
        )),
        Rect { height: 1, ..area },
    );
    let text_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    if let Some(note) = preview_note(app) {
        f.render_widget(Paragraph::new(Line::styled(note, DIM)), text_area);
        return;
    }
    let lines = preview_lines(app, text_area.width as usize);
    let height = text_area.height as usize;
    let scroll = app.preview.scroll.min(lines.len().saturating_sub(height));
    let end = lines.len() - scroll;
    let start = end.saturating_sub(height);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), text_area);
}

// ---- Help ----------------------------------------------------------------------------

pub const KEYS: &[(&str, &str)] = &[
    (
        "1 2 3 4 / tab / shift-tab",
        "switch view: Accounts, Live, History, Stats",
    ),
    ("j k / ↑ ↓", "move"),
    ("g G / home end / pgup pgdn", "first / last / page"),
    (
        "enter",
        "history: resume the session (codex: asks first) · live: attach a background session",
    ),
    (
        "f",
        "fork the selected session (a new id; the original stays as it is)",
    ),
    (
        "c",
        "continue a claude session under another account (copied into its store, then forked)",
    ),
    ("p / space", "expand the preview"),
    ("n", "accounts: new session with the selected account"),
    (
        "s",
        "accounts: set up a new account, claude or codex (remuda setup)",
    ),
    ("l", "live: show a background session's logs in the preview"),
    ("x", "live: stop a background session (asks first)"),
    (
        "D",
        "accounts: remove the account (asks first; its home stays) · live: remove a stopped \
         background session (asks first)",
    ),
    (
        "esc",
        "back / clear the search / cancel (a form, a launch check)",
    ),
    ("/", "search history (fuzzy: title, cwd, accounts)"),
    (
        "a",
        "history: show teammate, SDK and codex subagent sessions · live: show stopped ones",
    ),
    ("t", "stats: next period (all, today, 7 days, 30 days)"),
    ("u", "query live usage for every account"),
    ("r", "refresh index, identities, live sessions, checks"),
    (
        "ctrl-p",
        "private mode: hide account names, emails, paths, titles and previews (for screenshots)",
    ),
    ("? · q / ctrl-c", "this help · quit"),
];

// ---- Stats ---------------------------------------------------------------------------

/// The Stats view's numeric columns: header and width. The name column takes the rest.
const STATS_COLUMNS: [(&str, usize); 6] = [
    ("INPUT", 7),
    ("CACHE READ", 10),
    ("CACHE WRITE", 11),
    ("OUTPUT", 7),
    ("REASONING", 9),
    ("TOTAL", 7),
];
/// When the name column would be narrower, REASONING is dropped, then CACHE WRITE.
const STATS_MIN_NAME: usize = 16;
const STATS_NAME: &str = "ACCOUNT / MODEL";
/// The title and the column header stay put above the scrolled lines.
const STATS_CHROME: u16 = 2;

/// Lines of the Stats body below its title and column header.
pub fn stats_height(app: &App) -> usize {
    frame_areas(screen(app))
        .body
        .height
        .saturating_sub(STATS_CHROME) as usize
}

/// Lines the Stats view scrolls over at the current width.
pub fn stats_line_count(app: &App) -> usize {
    stats_lines(app, frame_areas(screen(app)).body.width).len()
}

fn stats_table(app: &App) -> Option<&Table> {
    Some(app.stats.report.as_ref()?.table(app.stats.period))
}

/// A section's accounts, short (`default + max`), or `unattributed`.
fn stats_label(accounts: &[String]) -> String {
    match accounts.is_empty() {
        true => "unattributed".to_string(),
        false => accounts
            .iter()
            .map(|a| short(a))
            .collect::<Vec<_>>()
            .join(" + "),
    }
}

/// The width of the name column and the numeric columns shown (indexes into
/// [`STATS_COLUMNS`]) at `width`: the name column is as wide as its widest name (so a wide
/// terminal keeps the numbers next to the names), at most what the numbers leave; narrower
/// than [`STATS_MIN_NAME`], it takes REASONING's place, then CACHE WRITE's.
fn stats_columns(app: &App, width: u16) -> (usize, Vec<usize>) {
    let mut widest = text::width(STATS_NAME);
    if let Some(table) = stats_table(app) {
        for s in &table.sections {
            widest = widest.max(text::width(&stats_label(&s.accounts)));
        }
        for m in table.sections.iter().flat_map(|s| &s.models) {
            widest = widest.max(2 + text::width(&m.model));
        }
    }
    let mut shown: Vec<usize> = (0..STATS_COLUMNS.len()).collect();
    let room = |shown: &[usize]| {
        let numbers: usize = shown.iter().map(|&i| STATS_COLUMNS[i].1 + 1).sum();
        (width as usize).saturating_sub(numbers)
    };
    for dropped in [4, 2] {
        if room(&shown) >= STATS_MIN_NAME {
            break;
        }
        shown.retain(|&i| i != dropped);
    }
    (room(&shown).min(widest), shown)
}

/// A Stats line: the name truncated and padded to its column, then the counts right-aligned.
fn stats_row(
    name: &str,
    counts: &[String; 6],
    style: Style,
    columns: &(usize, Vec<usize>),
) -> Line<'static> {
    let (name_w, shown) = columns;
    let mut spans = vec![Span::styled(
        text::pad(&text::truncate(name, *name_w), *name_w),
        style,
    )];
    for &i in shown {
        let pad = STATS_COLUMNS[i].1.saturating_sub(text::width(&counts[i]));
        spans.push(Span::styled(
            format!(" {}{}", " ".repeat(pad), counts[i]),
            style,
        ));
    }
    Line::from(spans)
}

/// A bold row with the models' total, then a row per model (codex's in its color); an empty
/// section is one row saying so.
fn stats_block(
    label: &str,
    models: &[ModelRow],
    columns: &(usize, Vec<usize>),
    lines: &mut Vec<Line<'static>>,
) {
    if models.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                text::pad(&text::truncate(label, columns.0), columns.0),
                BOLD,
            ),
            Span::styled(" no tokens", DIM),
        ]));
        return;
    }
    let (total, providers) = stats::sum(models);
    lines.push(stats_row(
        label,
        &stats::counts(&total, &providers),
        BOLD,
        columns,
    ));
    for m in models {
        let style = match m.provider {
            Provider::Codex => CODEX_STYLE,
            Provider::Claude => Style::new(),
        };
        let counts = stats::counts(&m.tokens, &[m.provider]);
        lines.push(stats_row(
            &format!("  {}", m.model),
            &counts,
            style,
            columns,
        ));
    }
}

/// The scrolled part of the Stats view: every section of the period, then Overall; while the
/// first report is computed, what is being done. Reads only `app.stats` (R20).
pub fn stats_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(table) = stats_table(app) else {
        let doing = match app.stats.progress {
            Some((done, total)) if done < total => format!("reading transcripts {done}/{total}…"),
            _ => "computing…".to_string(),
        };
        return vec![Line::styled(doing, DIM)];
    };
    let columns = stats_columns(app, width);
    let mut lines = Vec::new();
    for s in &table.sections {
        stats_block(&stats_label(&s.accounts), &s.models, &columns, &mut lines);
    }
    lines.push(Line::raw(""));
    stats_block("Overall", &table.overall, &columns, &mut lines);
    lines
}

fn stats_view(app: &App, f: &mut Frame, area: Rect) {
    let title = format!("Tokens · {} (t: period)", app.stats.period.label());
    let columns = stats_columns(app, area.width);
    let header = STATS_COLUMNS.map(|(h, _)| h.to_string());
    let mut lines = vec![
        section(&title, area.width),
        stats_row(STATS_NAME, &header, DIM, &columns),
    ];
    let body = stats_lines(app, area.width);
    let height = area.height.saturating_sub(STATS_CHROME) as usize;
    let scroll = app.stats.scroll.min(body.len().saturating_sub(height));
    lines.extend(body.into_iter().skip(scroll).take(height));
    f.render_widget(Paragraph::new(lines), area);
}

/// The Stats status: the computation's progress, else when the report was computed.
fn stats_status(app: &App) -> Span<'static> {
    let s = &app.stats;
    match (s.in_flight, s.progress, s.computed) {
        (true, Some((done, total)), _) if done < total => {
            Span::styled(format!("reading transcripts {done}/{total}…"), WARN)
        }
        (true, _, _) => Span::styled("computing statistics…", WARN),
        (false, _, Some(at)) => Span::raw(format!(
            "computed {} · {} transcripts",
            at.to_zoned(app.tz.clone()).strftime("%H:%M:%S"),
            s.report.as_ref().map_or(0, |r| r.files)
        )),
        (false, _, None) => Span::raw(""),
    }
}

/// A centered box with a border and a title; `lines` are clipped to it.
fn boxed(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let w = (lines
        .iter()
        .map(Line::width)
        .chain([text::width(title)])
        .max()
        .unwrap_or(0) as u16
        + 4)
    .min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" {title} ")),
        ),
        rect,
    );
}

fn pick_box(app: &App, pick: &Pick, f: &mut Frame, area: Rect) {
    let verb = match pick.action {
        PickFor::Resume => "Resume",
        PickFor::Fork => "Fork",
        PickFor::Relay => "Continue",
    };
    let title = format!("{verb} {} as…", short_id(&pick.session_id));
    let name_w = pick
        .options
        .iter()
        .map(|q| text::width(short(q)))
        .max()
        .unwrap_or(0);
    let lines = pick
        .options
        .iter()
        .enumerate()
        .map(|(k, q)| {
            let mut spans = vec![
                Span::raw(if k == pick.selected { "› " } else { "  " }),
                Span::styled(
                    text::pad(short(q), name_w),
                    if k == pick.selected {
                        BOLD
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    if pick.attributed.contains(q) {
                        " ●"
                    } else {
                        "  "
                    },
                    OK,
                ),
            ];
            // The registry may have changed while the picker is open (R17).
            let registered = app.accounts.iter().any(|a| a.account.qualified() == *q);
            // A relay goes to another store by design: that is not a problem there.
            let problem = if !registered {
                Some("no longer registered")
            } else if pick.action == PickFor::Relay {
                None
            } else {
                app.store_problem(q, &pick.path)
            };
            if let Some(problem) = problem {
                spans.push(Span::styled(format!("  {problem}"), DIM));
            }
            Line::from(spans)
        })
        .collect();
    boxed(f, area, &title, lines);
}

/// Width of the value column in a form.
const FIELD_W: usize = 48;

fn form_box(form: &Form, f: &mut Frame, area: Rect) {
    let title = match &form.kind {
        FormKind::NewSession { account } => {
            format!("New session as {}", short(&account.qualified()))
        }
        FormKind::Setup => "Set up a new account (remuda setup)".to_string(),
    };
    let label_w = form
        .fields
        .iter()
        .map(|field| text::width(field.label))
        .max()
        .unwrap_or(0);
    let value_w = FIELD_W.min((area.width as usize).saturating_sub(label_w + 8));
    let mut lines = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        // The end of a long value stays visible while typing.
        let mut value = tail(&field.value, value_w.saturating_sub(1));
        if focused {
            value.push('▏');
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", text::pad(field.label, label_w)),
                if focused { BOLD } else { DIM },
            ),
            Span::styled(
                text::pad(&value, value_w),
                if focused { SELECTED } else { Style::new() },
            ),
        ]));
    }
    if let FormKind::Setup = form.kind {
        lines.push(Line::styled(
            "creates $REMUDA_HOME/homes/<provider>/<name>, registers it, then logs in",
            DIM,
        ));
        lines.push(Line::styled(
            "(`claude auth login`, or `codex login`, which takes no email)",
            DIM,
        ));
    }
    if let Some(error) = &form.error {
        for l in text::wrap(error, label_w + 1 + value_w) {
            lines.push(Line::styled(l, CRIT));
        }
    }
    boxed(f, area, &title, lines);
}

fn confirm_box(confirm: &Confirm, f: &mut Frame, area: Rect) {
    let (title, what) = match confirm.verb {
        Control::Stop => (
            "Stop",
            "claude stop: the conversation is kept (enter attaches it again)",
        ),
        Control::Remove => ("Remove", "claude rm: deletes the session and its worktree"),
    };
    let lines = vec![
        Line::styled(
            format!(
                "{title} background session {} ({})?",
                confirm.short_id,
                short(&confirm.account)
            ),
            BOLD,
        ),
        Line::styled(what, DIM),
        Line::from(vec![
            Span::styled("y", BOLD),
            Span::raw(": yes · any other key: cancel"),
        ]),
    ];
    boxed(f, area, title, lines);
}

/// R14a, R16: unregistering keeps the home; the prompt says which.
fn remove_account_box(app: &App, account: &Account, f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::styled(
            format!("Remove {} from the registry?", short(&account.qualified())),
            BOLD,
        ),
        Line::styled(
            format!(
                "its home {} is kept; `remuda add` can register it again",
                tilde(app, &account.home.to_string())
            ),
            DIM,
        ),
        Line::from(vec![
            Span::styled("y", BOLD),
            Span::raw(": yes · any other key: cancel"),
        ]),
    ];
    boxed(f, area, "Remove account", lines);
}

/// R17: nothing says whether a codex session runs elsewhere; the user decides.
fn resume_codex_box(app: &App, confirm: &ResumeCodex, f: &mut Frame, area: Rect) {
    let mut what = confirm.request.what.clone();
    if let Some(first) = what.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let mut lines = vec![
        Line::styled(format!("{what}?"), BOLD),
        Line::styled(
            "remuda cannot confirm that this session is not running elsewhere",
            DIM,
        ),
        Line::styled(
            "(codex has no list of running sessions; two codex processes on one session conflict)",
            DIM,
        ),
    ];
    let written = confirm
        .written
        .filter(|at| app.now.duration_since(*at) < RECENTLY_WRITTEN);
    if let Some(at) = written {
        lines.push(Line::styled(
            format!(
                "it is still being written (last written {}): codex may be running it",
                format_age(at, app.now)
            ),
            WARN,
        ));
    }
    lines.push(Line::from(vec![
        Span::styled("y", BOLD),
        Span::raw(": resume anyway · any other key: cancel"),
    ]));
    boxed(f, area, "Resume codex session", lines);
}

/// A rollout written this recently may belong to a running codex (R17).
const RECENTLY_WRITTEN: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

fn help(f: &mut Frame, area: Rect) {
    let key_w = KEYS.iter().map(|(k, _)| text::width(k)).max().unwrap_or(0);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(text::pad(k, key_w + 2), BOLD),
                Span::raw(*what),
            ])
        })
        .collect();
    let w = (lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4).min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Keys · any key but ctrl-p closes "),
        ),
        rect,
    );
}
