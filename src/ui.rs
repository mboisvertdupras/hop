use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Mode, Row, View, rows, tilde};
use crate::form::{FieldId, Form, FormKind};
use crate::meta::{UnixTime, age};
use crate::probe::Reach;
use crate::ssh_config::{CoreKey, HostEntry};

const HINTS: &str = "Enter connect  / search  a e d add/edit/delete  f favorite  g group  Tab recent  r reload  ? help  q quit";

const HELP: &str = "\
j k / arrows   move
h l / arrows   collapse / expand group
Enter          connect (on a group: toggle)
/              fuzzy search, Esc returns to the tree
a e d          add, edit, delete
f              toggle favorite
g              set group (a/b for nested)
Tab            tree / recent
r              reload config and re-probe
q              quit

In the form: Tab / Shift-Tab move, Ctrl-U clears the field,\nCtrl-S saves, Esc cancels.";

pub fn render(app: &App, frame: &mut Frame) {
    let [header, body, status] = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let [list, detail] = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(body);

    render_header(app, frame, header);
    render_list(app, frame, list);
    render_detail(app, frame, detail);
    render_status(app, frame, status);

    match &app.mode {
        Mode::List | Mode::Search { .. } => {}
        Mode::Form(form) => render_form(form, frame),
        Mode::Confirm { prompt, .. } => {
            const WIDTH: u16 = 70;
            let paragraph = Paragraph::new(prompt.as_str()).wrap(Wrap { trim: false });
            let rows = paragraph.line_count(WIDTH - 2) as u16;
            let area = popup(frame, WIDTH, rows + 2, " Confirm ");
            frame.render_widget(paragraph, area);
        }
        Mode::SetGroup { alias, input } => {
            let area = popup(frame, 50, 4, &format!(" Group for {alias} "));
            let [hint, field] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
            frame.render_widget(Line::from("a/b for nested, blank to clear. Enter saves, Esc cancels.").dim(), hint);
            frame.set_cursor_position((field.x + input.chars().count() as u16, field.y));
            frame.render_widget(input.as_str(), field);
        }
        Mode::Help => {
            let area = popup(frame, 64, HELP.lines().count() as u16 + 2, " Keys ");
            frame.render_widget(Paragraph::new(HELP), area);
        }
    }
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let total = app.reach.len();
    let pending = app.reach.values().filter(|r| **r == Reach::Probing).count();
    let view = match app.view {
        View::Tree => "tree",
        View::Recent => "recent",
    };
    let mut spans = vec![Span::from(" hop ").bold().reversed(), Span::from(format!("  {view}"))];
    if pending > 0 {
        spans.push(Span::from(format!("  probing {}/{total}", total - pending)).dim());
    }
    frame.render_widget(Line::from(spans), area);
}

fn reach_label(reach: Option<&Reach>) -> Span<'static> {
    match reach {
        None => Span::from(""),
        Some(Reach::Probing) => Span::from("·").dim(),
        Some(Reach::Up { latency }) => Span::from(format!("{}ms", latency.as_millis())).green(),
        Some(Reach::Down) => Span::from("down").red(),
        Some(Reach::ViaJump) => Span::from("via jump").cyan(),
    }
}

fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let rows = rows(app);
    let now = UnixTime::now();
    let selected = rows.iter().position(|r| r.key().is_some() && r.key() == app.cursor);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let line = match row {
                Row::Section(name) => Line::from(*name).bold(),
                Row::Group { path, depth, expanded, hosts } => {
                    let name = path.segments().last().map_or("", String::as_str);
                    let arrow = if *expanded { "▾" } else { "▸" };
                    Line::from(vec![
                        Span::from(format!("{}{arrow} {name}", "  ".repeat(*depth))).bold(),
                        Span::from(format!(" ({hosts})")).dim(),
                    ])
                }
                Row::Host { entry, depth } => {
                    let meta = app.catalog.meta(&entry.alias);
                    let mut spans = vec![Span::from(format!("{}{}", "  ".repeat(*depth), entry.alias))];
                    if let Some(host) = &entry.core[CoreKey::HostName] {
                        spans.push(Span::from(format!("  {host}")).dim());
                    }
                    if *depth == 0
                        && let Some(group) = &meta.group
                    {
                        spans.push(Span::from(format!("  [{group}]")).dim());
                    }
                    if app.view == View::Recent
                        && let Some(used) = meta.last_used
                    {
                        spans.push(Span::from(format!("  {}", age(used, now))).dim());
                    }
                    spans.push(Span::from("  "));
                    spans.push(reach_label(app.reach.get(&entry.alias)));
                    Line::from(spans)
                }
            };
            ListItem::new(line)
        })
        .collect();
    let title = match &app.mode {
        Mode::Search { .. } => " Search ",
        _ => " Hosts ",
    };
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut ListState::default().with_selected(selected));
}

fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::bordered().title(" Details ");
    let Some(entry) = app.selected() else {
        let empty = if app.catalog.config.hosts().is_empty() { "No hosts yet. Press a to add one." } else { "" };
        frame.render_widget(Paragraph::new(empty).block(block), area);
        return;
    };
    frame.render_widget(Paragraph::new(detail_text(app, entry)).block(block).wrap(Wrap { trim: false }), area);
}

fn detail_text<'a>(app: &'a App, entry: &'a HostEntry) -> Text<'a> {
    let config = &app.catalog.config;
    let meta = app.catalog.meta(&entry.alias);
    let field = |label: &str, value: String| Line::from(vec![Span::from(format!("{label:<13}")).dim(), Span::from(value)]);
    let mut lines = vec![Line::from(entry.alias.to_string()).bold()];
    for k in CoreKey::ALL {
        if let Some(value) = &entry.core[k] {
            lines.push(field(k.keyword(), value.clone()));
        }
    }
    if !entry.siblings.is_empty() {
        let names: Vec<String> = entry.siblings.iter().map(ToString::to_string).collect();
        lines.push(field("Also named", names.join(" ")));
    }
    if let Some(group) = &meta.group {
        lines.push(field("Group", group.to_string()));
    }
    if let Some(used) = meta.last_used {
        lines.push(field("Last used", age(used, UnixTime::now())));
    }
    let (path, line) = config.location(entry);
    let lock = if config.is_writable(entry) { "" } else { "  (read-only)" };
    lines.push(field("Source", format!("{}:{line}{lock}", tilde(path, &app.canonical_home))));
    if let Some(notes) = &meta.notes {
        lines.push(Line::from(""));
        lines.extend(notes.lines().map(Line::from));
    }
    Text::from(lines)
}

fn render_status(app: &App, frame: &mut Frame, area: Rect) {
    let line = match (&app.mode, &app.status) {
        (Mode::Search { query }, _) => {
            frame.set_cursor_position((area.x + 1 + query.chars().count() as u16, area.y));
            Line::from(format!("/{query}"))
        }
        (_, Some(status)) => Line::from(status.as_str()).yellow(),
        _ => Line::from(HINTS).dim(),
    };
    frame.render_widget(line, area);
}

fn popup(frame: &mut Frame, width: u16, height: u16, title: &str) -> Rect {
    let area = frame.area().centered(Constraint::Length(width), Constraint::Length(height));
    let block = Block::bordered().title(title.to_owned());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

fn render_form(form: &Form, frame: &mut Frame) {
    const EXTRAS_ROWS: u16 = 6;
    let title = match &form.kind {
        FormKind::Add => " Add host ".to_owned(),
        FormKind::Edit { original, .. } => format!(" Edit {original} "),
    };
    let notices: Vec<Line> = [
        form.banner.as_ref().map(|b| Line::from(b.as_str()).yellow()),
        match &form.kind {
            FormKind::Edit { siblings, .. } if !siblings.is_empty() => {
                let names: Vec<String> = siblings.iter().map(ToString::to_string).collect();
                Some(Line::from(format!("Field changes also apply to {}.", names.join(", "))).dim())
            }
            _ => None,
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    let heights: Vec<u16> = form
        .fields
        .iter()
        .map(|(f, _)| {
            let rows = if *f == FieldId::Extras { EXTRAS_ROWS } else { 1 };
            let error = u16::from(form.error.as_ref().is_some_and(|(e, _)| e == f));
            rows + error
        })
        .collect();
    let height = notices.len() as u16 + heights.iter().sum::<u16>() + 3;
    let area = popup(frame, 76, height, &title);

    let constraints = std::iter::once(Constraint::Length(notices.len() as u16))
        .chain(heights.iter().map(|&h| Constraint::Length(h)))
        .chain([Constraint::Length(1)]);
    let areas = Layout::vertical(constraints).split(area);
    frame.render_widget(Text::from(notices), areas[0]);
    for (i, (field, textarea)) in form.fields.iter().enumerate() {
        let [label, input] = Layout::horizontal([Constraint::Length(18), Constraint::Min(0)]).areas(areas[i + 1]);
        let style = if i == form.focus { Style::default().bold() } else { Style::default().dim() };
        frame.render_widget(Line::from(field.label()).style(style), label);
        let [value, error] = Layout::vertical([Constraint::Min(1), Constraint::Length(u16::from(form.error.as_ref().is_some_and(|(e, _)| e == field)))]).areas(input);
        if i == form.focus {
            frame.render_widget(textarea, value);
        } else {
            frame.render_widget(Text::from(textarea.lines().iter().map(|l| Line::from(l.as_str())).collect::<Vec<_>>()), value);
        }
        if let Some((_, message)) = form.error.as_ref().filter(|(e, _)| e == field) {
            frame.render_widget(Line::from(message.as_str()).red(), error);
        }
    }
    frame.render_widget(Line::from("Tab/Shift-Tab move   Ctrl-U clear field   Ctrl-S save   Esc cancel").dim(), areas[form.fields.len() + 1]);
}
