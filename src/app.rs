use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::mem;
use std::path::{Path, PathBuf};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::form::{FieldId, Form, FormAction};
use crate::meta::{GroupPath, Meta, MetaMap, MetaOp};
use crate::probe::Reach;
use crate::ssh_config::{Alias, ConfigEdit, ConfigSet, CoreKey, HostChange, HostEntry, PlanError};

pub struct Catalog {
    pub config: ConfigSet,
    pub meta: MetaMap,
}

const NO_META: &Meta = &Meta { group: None, favorite: false, last_used: None, notes: None };

impl Catalog {
    pub fn meta(&self, alias: &Alias) -> &Meta {
        self.meta.get(alias).unwrap_or(NO_META)
    }
    pub fn aliases(&self) -> Vec<Alias> {
        self.config.hosts().iter().map(|h| h.alias.clone()).collect()
    }
}

pub struct App {
    pub catalog: Catalog,
    pub canonical_home: PathBuf,
    pub reach: HashMap<Alias, Reach>,
    pub view: View,
    pub collapsed: BTreeSet<GroupPath>,
    pub cursor: Option<RowKey>,
    pub mode: Mode,
    pub status: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Tree,
    Recent,
}

pub enum Mode {
    List,
    Search { query: String },
    Form(Form),
    Confirm { prompt: String, on_yes: Effect },
    SetGroup { alias: Alias, input: String },
    Help,
}

pub enum Event {
    Key(KeyEvent),
    Probe(Alias, Reach),
    Loaded { catalog: Catalog, reason: LoadReason },
    MetaSaved(MetaMap),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadReason {
    Refresh,
    Saved,
    Refused,
}

pub enum Effect {
    Quit,
    Connect(Alias),
    Reload,
    Meta(Vec<MetaOp>),
    Save { ssh: ConfigEdit, meta: Vec<MetaOp> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKey {
    Group(GroupPath),
    Host(Alias),
}

pub enum Row<'a> {
    Section(&'static str),
    Group { path: GroupPath, depth: usize, expanded: bool, hosts: usize },
    Host { entry: &'a HostEntry, depth: usize },
}

impl Row<'_> {
    pub fn key(&self) -> Option<RowKey> {
        match self {
            Row::Section(_) => None,
            Row::Group { path, .. } => Some(RowKey::Group(path.clone())),
            Row::Host { entry, .. } => Some(RowKey::Host(entry.alias.clone())),
        }
    }
}

impl App {
    pub fn new(catalog: Catalog, canonical_home: PathBuf) -> App {
        let mut app = App {
            reach: probing(&catalog),
            catalog,
            canonical_home,
            view: View::Tree,
            collapsed: BTreeSet::new(),
            cursor: None,
            mode: Mode::List,
            status: None,
        };
        reveal_cursor(&mut app);
        app
    }

    pub fn selected(&self) -> Option<&HostEntry> {
        match &self.cursor {
            Some(RowKey::Host(alias)) => self.catalog.config.host(alias),
            _ => None,
        }
    }
}

fn probing(catalog: &Catalog) -> HashMap<Alias, Reach> {
    catalog.aliases().into_iter().map(|a| (a, Reach::Probing)).collect()
}

const REFUSED: &str = "This host changed on disk. Review and save again.";

pub fn update(app: &mut App, event: Event) -> Option<Effect> {
    let effect = match event {
        Event::Key(key) => {
            app.status = None;
            match mem::replace(&mut app.mode, Mode::List) {
                Mode::List => list_key(app, key),
                Mode::Search { query } => search_key(app, query, key),
                Mode::Form(form) => form_key(app, form, key),
                Mode::Confirm { prompt, on_yes } => confirm_key(app, prompt, on_yes, key),
                Mode::SetGroup { alias, input } => set_group_key(app, alias, input, key),
                Mode::Help => None,
            }
        }
        Event::Probe(alias, reach) => {
            app.reach.insert(alias, reach);
            None
        }
        Event::Loaded { catalog, reason } => {
            app.reach = probing(&catalog);
            app.catalog = catalog;
            app.mode = match (reason, mem::replace(&mut app.mode, Mode::List)) {
                (LoadReason::Saved, Mode::Form(form)) => {
                    app.cursor = form.target_alias().map(RowKey::Host);
                    Mode::List
                }
                (LoadReason::Refused, Mode::Form(mut form)) => {
                    form.banner = Some(REFUSED.to_owned());
                    Mode::Form(form)
                }
                (LoadReason::Refused, _) => {
                    app.status = Some(format!("{REFUSED} Nothing was saved."));
                    Mode::List
                }
                (_, mode) => mode,
            };
            None
        }
        Event::MetaSaved(meta) => {
            app.catalog.meta = meta;
            None
        }
        Event::Failed(message) => {
            match &mut app.mode {
                Mode::Form(form) => form.banner = Some(message),
                _ => app.status = Some(message),
            }
            None
        }
    };
    reveal_cursor(app);
    effect
}

fn plain_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => Some(c),
        _ => None,
    }
}

fn list_key(app: &mut App, key: KeyEvent) -> Option<Effect> {
    let alias = app.selected().map(|h| h.alias.clone());
    match (key.code, plain_char(&key)) {
        (KeyCode::Down, _) | (_, Some('j')) => move_cursor(app, 1),
        (KeyCode::Up, _) | (_, Some('k')) => move_cursor(app, -1),
        (KeyCode::Left, _) | (_, Some('h')) => collapse(app),
        (KeyCode::Right, _) | (_, Some('l')) => {
            if let Some(RowKey::Group(group)) = &app.cursor {
                app.collapsed.remove(group);
            }
        }
        (KeyCode::Enter, _) => match app.cursor.clone() {
            Some(RowKey::Host(alias)) => return Some(Effect::Connect(alias)),
            Some(RowKey::Group(group)) => toggle(&mut app.collapsed, group),
            None => {}
        },
        (KeyCode::Tab, _) => {
            app.view = match app.view {
                View::Tree => View::Recent,
                View::Recent => View::Tree,
            }
        }
        (_, Some('/')) => app.mode = Mode::Search { query: String::new() },
        (_, Some('a')) => app.mode = Mode::Form(Form::add()),
        (_, Some('r')) => return Some(Effect::Reload),
        (_, Some('?')) => app.mode = Mode::Help,
        (_, Some('q')) => return Some(Effect::Quit),
        (_, Some(c)) => {
            let alias = alias?;
            let meta = app.catalog.meta(&alias);
            match c {
                'e' => {
                    let entry = app.catalog.config.host(&alias)?;
                    app.mode = Mode::Form(Form::edit(entry, meta));
                }
                'f' => return Some(Effect::Meta(vec![MetaOp::SetFavorite(alias, !meta.favorite)])),
                'g' => {
                    let input = meta.group.as_ref().map(ToString::to_string).unwrap_or_default();
                    app.mode = Mode::SetGroup { alias, input };
                }
                'd' => confirm_delete(app, alias),
                _ => {}
            }
        }
        _ => {}
    }
    None
}

fn toggle(collapsed: &mut BTreeSet<GroupPath>, group: GroupPath) {
    if !collapsed.remove(&group) {
        collapsed.insert(group);
    }
}

pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn confirm_delete(app: &mut App, alias: Alias) {
    let config = &app.catalog.config;
    let Some(entry) = config.host(&alias) else { return };
    match config.plan(&HostChange::Delete(alias.clone())) {
        Ok(ssh) => {
            let (path, line) = config.location(entry);
            let prompt = format!("Delete {alias} from `{}` in {}:{line}? (y/n)", config.host_line(entry), tilde(path, &app.canonical_home));
            let on_yes = Effect::Save { ssh, meta: vec![MetaOp::Remove(alias)] };
            app.mode = Mode::Confirm { prompt, on_yes };
        }
        Err(e) => app.status = Some(e.to_string()),
    }
}

fn move_cursor(app: &mut App, delta: isize) {
    let keys: Vec<RowKey> = rows(app).iter().filter_map(Row::key).collect();
    let Some(last) = keys.len().checked_sub(1) else { return };
    let next = match app.cursor.as_ref().and_then(|c| keys.iter().position(|k| k == c)) {
        Some(i) => i.saturating_add_signed(delta).min(last),
        None => 0,
    };
    app.cursor = Some(keys[next].clone());
}

fn collapse(app: &mut App) {
    let group = match &app.cursor {
        Some(RowKey::Group(group)) => Some(group.clone()),
        Some(RowKey::Host(alias)) => {
            let in_tree = rows(app).iter().any(|r| matches!(r, Row::Host { entry, depth } if entry.alias == *alias && *depth > 0));
            in_tree.then(|| app.catalog.meta(alias).group.clone()).flatten()
        }
        None => None,
    };
    if let Some(group) = group {
        app.cursor = Some(RowKey::Group(group.clone()));
        app.collapsed.insert(group);
    }
}

fn search_key(app: &mut App, mut query: String, key: KeyEvent) -> Option<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, ctrl) {
        (KeyCode::Esc, _) => {
            app.view = View::Tree;
            return None;
        }
        (KeyCode::Enter, _) => {
            app.view = View::Tree;
            return app.selected().map(|h| Effect::Connect(h.alias.clone()));
        }
        (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
            app.mode = Mode::Search { query };
            move_cursor(app, -1);
        }
        (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
            app.mode = Mode::Search { query };
            move_cursor(app, 1);
        }
        (KeyCode::Char(_), false) | (KeyCode::Backspace, _) => {
            match key.code {
                KeyCode::Char(c) => query.push(c),
                _ => _ = query.pop(),
            }
            app.mode = Mode::Search { query };
            app.cursor = rows(app).iter().find_map(Row::key);
        }
        _ => app.mode = Mode::Search { query },
    }
    None
}

fn form_key(app: &mut App, mut form: Form, key: KeyEvent) -> Option<Effect> {
    let effect = match form.handle_key(key) {
        FormAction::Cancel => return None,
        FormAction::Continue => None,
        FormAction::Submit => submit(&app.catalog.config, &app.canonical_home, &mut form),
    };
    app.mode = Mode::Form(form);
    effect
}

fn submit(config: &ConfigSet, canonical_home: &Path, form: &mut Form) -> Option<Effect> {
    form.error = None;
    form.banner = None;
    let sub = form.submission().map_err(|e| form.error = Some(e)).ok()?;
    match config.plan(&sub.change) {
        Ok(ssh) => return Some(Effect::Save { ssh, meta: sub.meta }),
        Err(PlanError::AliasTaken(alias)) => form.error = Some((FieldId::Alias, format!("{alias} is already defined"))),
        Err(PlanError::ReadOnly(path)) => {
            form.banner = Some(format!("{} is read-only. Only group and notes can change.", tilde(&path, canonical_home)));
        }
        Err(e) => form.banner = Some(e.to_string()),
    }
    None
}

fn confirm_key(app: &mut App, prompt: String, on_yes: Effect, key: KeyEvent) -> Option<Effect> {
    match (key.code, plain_char(&key)) {
        (_, Some('y')) => Some(on_yes),
        (KeyCode::Esc, _) | (_, Some('n')) => None,
        _ => {
            app.mode = Mode::Confirm { prompt, on_yes };
            None
        }
    }
}

fn set_group_key(app: &mut App, alias: Alias, mut input: String, key: KeyEvent) -> Option<Effect> {
    match (key.code, plain_char(&key)) {
        (KeyCode::Esc, _) => return None,
        (KeyCode::Enter, _) => match GroupPath::parse(&input) {
            Ok(group) => return Some(Effect::Meta(vec![MetaOp::SetGroup(alias, group)])),
            Err(e) => app.status = Some(e.to_string()),
        },
        (KeyCode::Backspace, _) => _ = input.pop(),
        (_, Some(c)) => input.push(c),
        _ => {}
    }
    app.mode = Mode::SetGroup { alias, input };
    None
}

fn reveal_cursor(app: &mut App) {
    let visible = |app: &App| rows(app).iter().filter_map(Row::key).any(|k| Some(&k) == app.cursor.as_ref());
    if visible(app) {
        return;
    }
    if let Some(RowKey::Host(alias)) = &app.cursor
        && let Some(group) = &app.catalog.meta(alias).group
    {
        for g in group.ancestors_and_self() {
            app.collapsed.remove(&g);
        }
        if visible(app) {
            return;
        }
    }
    app.cursor = rows(app).iter().find_map(Row::key);
}

pub fn rows(app: &App) -> Vec<Row<'_>> {
    let catalog = &app.catalog;
    if let Mode::Search { query } = &app.mode {
        return flat(search(catalog, query));
    }
    let mut hosts: Vec<&HostEntry> = catalog.config.hosts().iter().collect();
    hosts.sort_by(|a, b| a.alias.cmp(&b.alias));
    if app.view == View::Recent {
        hosts.sort_by_key(|h| {
            let used = catalog.meta(&h.alias).last_used;
            (used.is_none(), Reverse(used))
        });
        return flat(hosts);
    }

    let (favorites, rest): (Vec<&HostEntry>, Vec<&HostEntry>) =
        hosts.into_iter().partition(|h| catalog.meta(&h.alias).favorite);
    let mut out = Vec::new();
    if !favorites.is_empty() {
        out.push(Row::Section("Favorites"));
        out.extend(favorites.into_iter().map(|entry| Row::Host { entry, depth: 0 }));
    }
    let groups: BTreeSet<GroupPath> = rest
        .iter()
        .filter_map(|h| catalog.meta(&h.alias).group.as_ref())
        .flat_map(GroupPath::ancestors_and_self)
        .collect();
    tree(app, &rest, &groups, &[], &mut out);
    out
}

fn flat(hosts: Vec<&HostEntry>) -> Vec<Row<'_>> {
    hosts.into_iter().map(|entry| Row::Host { entry, depth: 0 }).collect()
}

fn group_of<'a>(catalog: &'a Catalog, entry: &HostEntry) -> &'a [String] {
    catalog.meta(&entry.alias).group.as_ref().map_or(&[], GroupPath::segments)
}

fn tree<'a>(app: &'a App, hosts: &[&'a HostEntry], groups: &BTreeSet<GroupPath>, parent: &[String], out: &mut Vec<Row<'a>>) {
    let depth = parent.len();
    for group in groups.iter().filter(|g| g.segments().len() == depth + 1 && g.segments().starts_with(parent)) {
        let expanded = !app.collapsed.contains(group);
        let count = hosts.iter().filter(|h| group_of(&app.catalog, h).starts_with(group.segments())).count();
        out.push(Row::Group { path: group.clone(), depth, expanded, hosts: count });
        if expanded {
            tree(app, hosts, groups, group.segments(), out);
        }
    }
    out.extend(
        hosts
            .iter()
            .filter(|h| group_of(&app.catalog, h) == parent)
            .map(|&entry| Row::Host { entry, depth }),
    );
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum MatchedOn {
    Details,
    Alias,
}

pub fn search<'a>(catalog: &'a Catalog, query: &str) -> Vec<&'a HostEntry> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut score = |text: &str| pattern.score(Utf32Str::new(text, &mut buf), &mut matcher);
    let mut ranked: Vec<(MatchedOn, u32, &HostEntry)> = catalog
        .config
        .hosts()
        .iter()
        .filter_map(|h| {
            if let Some(s) = score(h.alias.as_str()) {
                return Some((MatchedOn::Alias, s, h));
            }
            let meta = catalog.meta(&h.alias);
            let haystack = [
                Some(h.alias.to_string()),
                h.core[CoreKey::HostName].clone(),
                h.core[CoreKey::User].clone(),
                meta.group.as_ref().map(ToString::to_string),
                meta.notes.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            score(&haystack).map(|s| (MatchedOn::Details, s, h))
        })
        .collect();
    ranked.sort_by(|a, b| (&b.0, b.1).cmp(&(&a.0, a.1)).then_with(|| a.2.alias.cmp(&b.2.alias)));
    ranked.into_iter().map(|(_, _, h)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::UnixTime;
    use crate::ssh_config::commit;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn tilde_abbreviates_only_paths_under_home() {
        let home = Path::new("/Users/marc");
        assert_eq!(tilde(Path::new("/Users/marc/.ssh/config"), home), "~/.ssh/config");
        assert_eq!(tilde(Path::new("/Users/marc"), home), "~");
        assert_eq!(tilde(Path::new("/etc/ssh/ssh_config"), home), "/etc/ssh/ssh_config");
        assert_eq!(tilde(Path::new("/Users/marcx/.ssh/config"), home), "/Users/marcx/.ssh/config");
    }

    const CONFIG: &str = "Host web\n  HostName 10.0.0.5\n\nHost db\n  HostName 10.0.0.6\n\nHost api\n  User svc\n\nHost zdb\n\nHost *\n  User me\n";

    struct Fixture {
        dir: tempfile::TempDir,
        app: App,
    }

    fn alias(s: &str) -> Alias {
        Alias::parse(s).unwrap()
    }

    fn group(s: &str) -> GroupPath {
        GroupPath::parse(s).unwrap().unwrap()
    }

    fn meta(group_path: Option<&str>, favorite: bool, last_used: Option<u64>, notes: Option<&str>) -> Meta {
        Meta {
            group: group_path.map(group),
            favorite,
            last_used: last_used.map(UnixTime),
            notes: notes.map(str::to_owned),
        }
    }

    fn catalog(dir: &tempfile::TempDir, meta: &[(&str, Meta)]) -> Catalog {
        let config = ConfigSet::load(&dir.path().join("config"), dir.path()).unwrap();
        Catalog { config, meta: meta.iter().map(|(a, m)| (alias(a), m.clone())).collect() }
    }

    fn fixture(text: &str, meta: &[(&str, Meta)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config"), text).unwrap();
        let app = App::new(catalog(&dir, meta), PathBuf::new());
        Fixture { dir, app }
    }

    fn plain() -> Fixture {
        fixture(CONFIG, &[])
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn press(app: &mut App, keys: &str) -> Vec<Effect> {
        keys.chars().filter_map(|c| update(app, key(KeyCode::Char(c)))).collect()
    }

    fn mode(app: &App) -> &'static str {
        match app.mode {
            Mode::List => "list",
            Mode::Search { .. } => "search",
            Mode::Form(_) => "form",
            Mode::Confirm { .. } => "confirm",
            Mode::SetGroup { .. } => "set_group",
            Mode::Help => "help",
        }
    }

    fn labels(app: &App) -> Vec<String> {
        rows(app)
            .iter()
            .map(|r| match r {
                Row::Section(s) => format!("[{s}]"),
                Row::Group { path, depth, expanded, hosts } => {
                    format!("{}{}{path} ({hosts})", "  ".repeat(*depth), if *expanded { "v " } else { "> " })
                }
                Row::Host { entry, depth } => format!("{}{}", "  ".repeat(*depth), entry.alias),
            })
            .collect()
    }

    fn cursor_on(app: &mut App, name: &str) {
        app.cursor = Some(RowKey::Host(alias(name)));
    }

    #[test]
    fn rows_list_hosts_by_alias_and_hide_patterns() {
        assert_eq!(labels(&plain().app), ["api", "db", "web", "zdb"]);
    }

    #[test]
    fn favorites_pinned_and_not_duplicated() {
        let f = fixture(CONFIG, &[("web", meta(Some("prod"), true, None, None)), ("db", meta(Some("prod"), false, None, None))]);
        assert_eq!(labels(&f.app), ["[Favorites]", "web", "v prod (1)", "  db", "api", "zdb"]);
    }

    #[test]
    fn nested_groups_from_paths_and_collapse_hides_subtree() {
        let mut f = fixture(CONFIG, &[
            ("web", meta(Some("clients/acme"), false, None, None)),
            ("db", meta(Some("clients"), false, None, None)),
            ("api", meta(Some("clients/zeta/eu"), false, None, None)),
        ]);
        assert_eq!(labels(&f.app), [
            "v clients (3)",
            "  v clients/acme (1)",
            "    web",
            "  v clients/zeta (1)",
            "    v clients/zeta/eu (1)",
            "      api",
            "  db",
            "zdb",
        ]);
        f.app.collapsed.insert(group("clients/zeta"));
        assert_eq!(labels(&f.app), ["v clients (3)", "  v clients/acme (1)", "    web", "  > clients/zeta (1)", "  db", "zdb"]);
    }

    #[test]
    fn recent_orders_by_last_used_then_alias() {
        let mut f = fixture(CONFIG, &[("db", meta(None, false, Some(10), None)), ("zdb", meta(None, false, Some(20), None))]);
        update(&mut f.app, key(KeyCode::Tab));
        assert_eq!(f.app.view, View::Recent);
        assert_eq!(labels(&f.app), ["zdb", "db", "api", "web"]);
        update(&mut f.app, key(KeyCode::Tab));
        assert_eq!(f.app.view, View::Tree);
    }

    #[test]
    fn orphan_meta_ignored() {
        let f = fixture(CONFIG, &[("gone", meta(Some("ghosts"), true, None, None))]);
        assert_eq!(labels(&f.app), ["api", "db", "web", "zdb"]);
    }

    #[test]
    fn cursor_moves_skip_section_and_stop_at_ends() {
        let mut f = fixture(CONFIG, &[("web", meta(None, true, None, None))]);
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("web"))));
        press(&mut f.app, "k");
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("web"))));
        press(&mut f.app, "jj");
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("db"))));
        update(&mut f.app, key(KeyCode::Down));
        update(&mut f.app, key(KeyCode::Down));
        update(&mut f.app, key(KeyCode::Down));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("zdb"))));
    }

    #[test]
    fn enter_on_host_connects() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        let effect = update(&mut f.app, key(KeyCode::Enter));
        assert!(matches!(effect, Some(Effect::Connect(a)) if a == alias("db")));
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn enter_on_group_toggles_and_h_on_member_collapses_its_group() {
        let mut f = fixture(CONFIG, &[("web", meta(Some("prod"), false, None, None))]);
        f.app.cursor = Some(RowKey::Group(group("prod")));
        assert!(update(&mut f.app, key(KeyCode::Enter)).is_none());
        assert!(f.app.collapsed.contains(&group("prod")));
        press(&mut f.app, "l");
        assert!(f.app.collapsed.is_empty());
        cursor_on(&mut f.app, "web");
        press(&mut f.app, "h");
        assert!(f.app.collapsed.contains(&group("prod")));
        assert_eq!(f.app.cursor, Some(RowKey::Group(group("prod"))));
    }

    #[test]
    fn slash_type_enter_connects_top_result() {
        let mut f = plain();
        press(&mut f.app, "/");
        assert_eq!(mode(&f.app), "search");
        press(&mut f.app, "zd");
        let effect = update(&mut f.app, key(KeyCode::Enter));
        assert!(matches!(effect, Some(Effect::Connect(a)) if a == alias("zdb")));
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn esc_in_search_keeps_cursor_alias_and_expands_its_groups() {
        let mut f = fixture(CONFIG, &[("api", meta(Some("a/b"), false, None, None))]);
        f.app.collapsed.insert(group("a"));
        f.app.collapsed.insert(group("a/b"));
        press(&mut f.app, "/svc");
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("api"))));
        assert!(update(&mut f.app, key(KeyCode::Esc)).is_none());
        assert_eq!((mode(&f.app), f.app.view), ("list", View::Tree));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("api"))));
        assert!(f.app.collapsed.is_empty());
    }

    #[test]
    fn search_ranks_alias_hit_over_notes_hit() {
        let f = fixture(CONFIG, &[("api", meta(None, false, None, Some("db backups")))]);
        let hits: Vec<&str> = search(&f.app.catalog, "db").iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(hits, ["db", "zdb", "api"]);
    }

    #[test]
    fn search_ctrl_n_and_backspace() {
        let mut f = plain();
        press(&mut f.app, "/");
        update(&mut f.app, ctrl('n'));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("db"))));
        press(&mut f.app, "x");
        assert!(rows(&f.app).is_empty());
        assert_eq!(f.app.cursor, None);
        update(&mut f.app, key(KeyCode::Backspace));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("api"))));
        assert!(matches!(&f.app.mode, Mode::Search { query } if query.is_empty()));
    }

    #[test]
    fn d_on_sole_host_confirms_then_saves_remove() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        assert!(press(&mut f.app, "d").is_empty());
        let Mode::Confirm { prompt, .. } = &f.app.mode else { panic!("expected confirm") };
        assert!(prompt.starts_with("Delete db from `Host db` in "), "{prompt}");
        let effects = press(&mut f.app, "y");
        assert!(matches!(&effects[..], [Effect::Save { ssh: ConfigEdit::Write(_), meta }] if *meta == [MetaOp::Remove(alias("db"))]));
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn d_then_n_emits_nothing() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        assert!(press(&mut f.app, "dxn").is_empty());
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn d_on_read_only_host_sets_status_no_confirm() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config"), "Include ro\n").unwrap();
        fs::write(dir.path().join("ro"), "Host locked\n").unwrap();
        fs::set_permissions(dir.path().join("ro"), fs::Permissions::from_mode(0o444)).unwrap();
        let mut app = App::new(catalog(&dir, &[]), PathBuf::new());
        assert!(press(&mut app, "d").is_empty());
        assert_eq!(mode(&app), "list");
        assert!(app.status.as_deref().is_some_and(|s| s.ends_with("is read-only")), "{:?}", app.status);
    }

    #[test]
    fn f_emits_set_favorite_with_inverse_of_shown_value() {
        let mut f = fixture(CONFIG, &[("web", meta(None, true, None, None))]);
        cursor_on(&mut f.app, "web");
        let effects = press(&mut f.app, "f");
        assert!(matches!(&effects[..], [Effect::Meta(ops)] if *ops == [MetaOp::SetFavorite(alias("web"), false)]));
        cursor_on(&mut f.app, "db");
        let effects = press(&mut f.app, "f");
        assert!(matches!(&effects[..], [Effect::Meta(ops)] if *ops == [MetaOp::SetFavorite(alias("db"), true)]));
    }

    #[test]
    fn unpinned_favorite_in_collapsed_group_stays_under_cursor() {
        let mut f = fixture(CONFIG, &[("web", meta(Some("prod"), true, None, None))]);
        f.app.collapsed.insert(group("prod"));
        cursor_on(&mut f.app, "web");
        let unpinned = [(alias("web"), meta(Some("prod"), false, None, None))].into_iter().collect();
        update(&mut f.app, Event::MetaSaved(unpinned));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("web"))));
        assert!(f.app.collapsed.is_empty());
    }

    #[test]
    fn g_prompt_emits_set_group() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        press(&mut f.app, "g");
        assert_eq!(mode(&f.app), "set_group");
        press(&mut f.app, "ops/eu");
        let effect = update(&mut f.app, key(KeyCode::Enter));
        assert!(matches!(effect, Some(Effect::Meta(ops)) if ops == [MetaOp::SetGroup(alias("db"), Some(group("ops/eu")))]));
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn g_prompt_rejects_empty_segment_and_stays_open() {
        let mut f = plain();
        press(&mut f.app, "ga//b");
        assert!(update(&mut f.app, key(KeyCode::Enter)).is_none());
        assert_eq!(mode(&f.app), "set_group");
        assert!(f.app.status.is_some());
    }

    #[test]
    fn e_untouched_ctrl_s_emits_save_with_ssh_unchanged() {
        let mut f = fixture(CONFIG, &[("db", meta(Some("ops"), false, None, Some("n")))]);
        cursor_on(&mut f.app, "db");
        press(&mut f.app, "e");
        assert_eq!(mode(&f.app), "form");
        let effect = update(&mut f.app, ctrl('s'));
        let Some(Effect::Save { ssh: ConfigEdit::Unchanged, meta }) = effect else { panic!("expected meta-only save") };
        assert_eq!(meta, [MetaOp::SetGroup(alias("db"), Some(group("ops"))), MetaOp::SetNotes(alias("db"), Some("n".into()))]);
        assert_eq!(mode(&f.app), "form");
    }

    #[test]
    fn form_invalid_save_stays_open_with_field_error() {
        let mut f = plain();
        press(&mut f.app, "a");
        press(&mut f.app, "db");
        assert!(update(&mut f.app, ctrl('s')).is_none());
        let Mode::Form(form) = &f.app.mode else { panic!("expected form") };
        assert_eq!(form.error, Some((FieldId::Alias, "db is already defined".into())));
        update(&mut f.app, key(KeyCode::Esc));
        assert_eq!(mode(&f.app), "list");
    }

    #[test]
    fn loaded_saved_moves_cursor_to_renamed_alias() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        press(&mut f.app, "e2");
        let Some(Effect::Save { ssh: ConfigEdit::Write(plan), .. }) = update(&mut f.app, ctrl('s')) else { panic!("expected save") };
        commit(&plan).unwrap();
        let reloaded = catalog(&f.dir, &[]);
        update(&mut f.app, Event::Loaded { catalog: reloaded, reason: LoadReason::Saved });
        assert_eq!(mode(&f.app), "list");
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("db2"))));
        assert_eq!(f.app.reach.get(&alias("db2")), Some(&Reach::Probing));
    }

    #[test]
    fn loaded_refused_keeps_form_draft_and_banner() {
        let mut f = plain();
        cursor_on(&mut f.app, "db");
        press(&mut f.app, "e2");
        update(&mut f.app, ctrl('s'));
        let reloaded = catalog(&f.dir, &[]);
        update(&mut f.app, Event::Loaded { catalog: reloaded, reason: LoadReason::Refused });
        let Mode::Form(form) = &f.app.mode else { panic!("expected form") };
        assert_eq!(form.banner.as_deref(), Some(REFUSED));
        assert_eq!(form.target_alias(), Some(alias("db2")));
    }

    #[test]
    fn failed_while_form_awaits_save_shows_banner() {
        let mut f = plain();
        press(&mut f.app, "a");
        update(&mut f.app, Event::Failed("disk full".into()));
        let Mode::Form(form) = &f.app.mode else { panic!("expected form") };
        assert_eq!(form.banner.as_deref(), Some("disk full"));
        update(&mut f.app, key(KeyCode::Esc));
        update(&mut f.app, Event::Failed("meta.toml: bad".into()));
        assert_eq!(f.app.status.as_deref(), Some("meta.toml: bad"));
    }

    #[test]
    fn probe_result_does_not_move_cursor() {
        let mut f = plain();
        cursor_on(&mut f.app, "web");
        update(&mut f.app, Event::Probe(alias("api"), Reach::Up { latency: Duration::from_millis(3) }));
        assert_eq!(f.app.cursor, Some(RowKey::Host(alias("web"))));
        assert_eq!(f.app.reach[&alias("api")], Reach::Up { latency: Duration::from_millis(3) });
    }

    #[test]
    fn list_keys_reload_help_quit() {
        let mut f = plain();
        assert!(matches!(&press(&mut f.app, "r")[..], [Effect::Reload]));
        press(&mut f.app, "?");
        assert_eq!(mode(&f.app), "help");
        assert!(press(&mut f.app, "q").is_empty());
        assert_eq!(mode(&f.app), "list");
        assert!(matches!(&press(&mut f.app, "q")[..], [Effect::Quit]));
        assert!(update(&mut f.app, ctrl('q')).is_none());
    }
}
