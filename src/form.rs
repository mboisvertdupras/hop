use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};

use crate::meta::{GroupPath, Meta, MetaOp};
use crate::ssh_config::{Alias, Core, CoreKey, Extras, HostChange, HostEntry, HostSpec, ValidCore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldId {
    Alias,
    Core(CoreKey),
    Group,
    Notes,
    Extras,
}

impl FieldId {
    pub fn all() -> impl Iterator<Item = FieldId> {
        std::iter::once(FieldId::Alias)
            .chain(CoreKey::ALL.into_iter().map(FieldId::Core))
            .chain([FieldId::Group, FieldId::Notes, FieldId::Extras])
    }
    pub fn label(self) -> &'static str {
        match self {
            FieldId::Alias => "Alias",
            FieldId::Core(k) => k.keyword(),
            FieldId::Group => "Group",
            FieldId::Notes => "Notes",
            FieldId::Extras => "Other directives",
        }
    }
    fn index(self) -> usize {
        FieldId::all().position(|f| f == self).expect("every field is in all()")
    }
}

pub enum FormKind {
    Add,
    Edit { original: Alias, siblings: Vec<Alias> },
}

pub struct Form {
    pub kind: FormKind,
    pub inputs: Vec<TextArea<'static>>,
    pub focus: usize,
    pub error: Option<(FieldId, String)>,
    pub banner: Option<String>,
}

pub enum FormAction {
    Continue,
    Submit,
    Cancel,
}

pub struct Submission {
    pub change: HostChange,
    pub meta: Vec<MetaOp>,
}

/// Split on '\n' rather than `lines()` so a trailing empty line survives the round trip.
fn text_area(text: &str) -> TextArea<'static> {
    let mut input = TextArea::new(text.split('\n').map(str::to_owned).collect());
    input.move_cursor(CursorMove::End);
    input
}

impl Form {
    pub fn add() -> Form {
        Form {
            kind: FormKind::Add,
            inputs: FieldId::all().map(|_| TextArea::default()).collect(),
            focus: 0,
            error: None,
            banner: None,
        }
    }

    pub fn edit(entry: &HostEntry, meta: &Meta) -> Form {
        let value = |field| match field {
            FieldId::Alias => entry.alias.to_string(),
            FieldId::Core(k) => entry.core[k].clone().unwrap_or_default(),
            FieldId::Group => meta.group.as_ref().map(ToString::to_string).unwrap_or_default(),
            FieldId::Notes => meta.notes.clone().unwrap_or_default(),
            FieldId::Extras => entry.extras.as_str().to_owned(),
        };
        Form {
            kind: FormKind::Edit { original: entry.alias.clone(), siblings: entry.siblings.clone() },
            inputs: FieldId::all().map(|f| text_area(&value(f))).collect(),
            focus: 0,
            error: None,
            banner: None,
        }
    }

    pub fn focused(&self) -> FieldId {
        FieldId::all().nth(self.focus).unwrap_or(FieldId::Alias)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        let fields = self.inputs.len();
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => return FormAction::Submit,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // The widget's own Ctrl-U is undo.
                let input = &mut self.inputs[self.focus];
                input.select_all();
                input.cut();
            }
            KeyCode::Tab => self.focus = (self.focus + 1) % fields,
            KeyCode::BackTab => self.focus = (self.focus + fields - 1) % fields,
            KeyCode::Enter if self.focused() != FieldId::Extras => self.focus = (self.focus + 1) % fields,
            _ => {
                self.inputs[self.focus].input(key);
            }
        }
        FormAction::Continue
    }

    fn text(&self, field: FieldId) -> String {
        self.inputs[field.index()].lines().join("\n")
    }

    fn value(&self, field: FieldId) -> Option<String> {
        let text = self.text(field);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    pub fn submission(&self) -> Result<Submission, (FieldId, String)> {
        let alias = Alias::parse(self.text(FieldId::Alias).trim()).map_err(|e| (FieldId::Alias, e.to_string()))?;
        let core = Core(CoreKey::ALL.map(|k| self.value(FieldId::Core(k))));
        let core = ValidCore::parse(core).map_err(|(k, e)| (FieldId::Core(k), e.to_owned()))?;
        let group = GroupPath::parse(&self.text(FieldId::Group)).map_err(|e| (FieldId::Group, e.to_string()))?;
        let notes = self.value(FieldId::Notes);
        let extras = Extras::parse(&self.text(FieldId::Extras)).map_err(|e| {
            (FieldId::Extras, format!("line {}: a Host or Match line would split the block", e.line))
        })?;

        let spec = HostSpec { alias: alias.clone(), core, extras };
        let mut meta = Vec::new();
        let change = match &self.kind {
            FormKind::Add => HostChange::Add(spec),
            FormKind::Edit { original, .. } => {
                if *original != alias {
                    meta.push(MetaOp::Rename { from: original.clone(), to: alias.clone() });
                }
                HostChange::Update { alias: original.clone(), spec }
            }
        };
        meta.push(MetaOp::SetGroup(alias.clone(), group));
        meta.push(MetaOp::SetNotes(alias, notes));
        Ok(Submission { change, meta })
    }

    pub fn target_alias(&self) -> Option<Alias> {
        Alias::parse(self.text(FieldId::Alias).trim()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_config::ConfigSet;
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(form: &mut Form, text: &str) {
        for c in text.chars() {
            form.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn focus(form: &mut Form, field: FieldId) {
        while form.focused() != field {
            form.handle_key(key(KeyCode::Tab));
        }
    }


    fn entry_fixture(text: &str) -> (tempfile::TempDir, ConfigSet) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config"), text).unwrap();
        let set = ConfigSet::load(&dir.path().join("config"), dir.path()).unwrap();
        (dir, set)
    }

    fn group(s: &str) -> GroupPath {
        GroupPath::parse(s).unwrap().unwrap()
    }

    const CONFIG: &str = "Host web web.example.com\n  HostName 10.0.0.5\n  Port 022\n  # keep\n  ForwardAgent yes\n\n";

    #[test]
    fn edit_untouched_submits_entry_spec() {
        let (_dir, set) = entry_fixture(CONFIG);
        let web = &set.hosts()[0];
        let meta = Meta { group: Some(group("a/b")), notes: Some("vpn".into()), ..Meta::default() };
        let sub = Form::edit(web, &meta).submission().unwrap();
        let HostChange::Update { alias, spec } = &sub.change else { panic!("expected update") };
        assert_eq!(alias.as_str(), "web");
        assert_eq!(spec.alias, web.alias);
        assert_eq!(spec.core, ValidCore::parse(web.core.clone()).unwrap());
        assert_eq!(spec.extras, web.extras);
        assert!(set.plan(&sub.change).unwrap().is_none());
        assert_eq!(sub.meta, [MetaOp::SetGroup(web.alias.clone(), Some(group("a/b"))), MetaOp::SetNotes(web.alias.clone(), Some("vpn".into()))]);
    }

    #[test]
    fn invalid_port_flags_port_field() {
        let mut form = Form::add();
        type_text(&mut form, "web");
        focus(&mut form, FieldId::Core(CoreKey::Port));
        type_text(&mut form, "99999");
        assert_eq!(form.submission().err().map(|e| e.0), Some(FieldId::Core(CoreKey::Port)));
    }

    #[test]
    fn wildcard_alias_flags_alias_field() {
        let mut form = Form::add();
        type_text(&mut form, "web*");
        assert_eq!(form.submission().err().map(|e| e.0), Some(FieldId::Alias));
    }

    #[test]
    fn host_line_in_extras_flags_extras() {
        let mut form = Form::add();
        type_text(&mut form, "web");
        focus(&mut form, FieldId::Extras);
        type_text(&mut form, "User x");
        form.handle_key(key(KeyCode::Enter));
        type_text(&mut form, "Host other");
        assert_eq!(form.submission().err(), Some((FieldId::Extras, "line 2: a Host or Match line would split the block".into())));
    }

    #[test]
    fn enter_moves_focus_except_in_extras() {
        let mut form = Form::add();
        form.handle_key(key(KeyCode::Enter));
        assert_eq!(form.focused(), FieldId::Core(CoreKey::HostName));
        focus(&mut form, FieldId::Extras);
        form.handle_key(key(KeyCode::Enter));
        assert_eq!(form.focused(), FieldId::Extras);
        assert_eq!(form.inputs[form.focus].lines().len(), 2);
    }

    #[test]
    fn rename_emits_rename_before_group_and_notes() {
        let (_dir, set) = entry_fixture(CONFIG);
        let mut form = Form::edit(&set.hosts()[0], &Meta::default());
        type_text(&mut form, "2");
        let sub = form.submission().unwrap();
        let (from, to) = (set.hosts()[0].alias.clone(), Alias::parse("web2").unwrap());
        assert_eq!(sub.meta, [
            MetaOp::Rename { from, to: to.clone() },
            MetaOp::SetGroup(to.clone(), None),
            MetaOp::SetNotes(to.clone(), None),
        ]);
        assert_eq!(form.target_alias(), Some(to));
    }

    #[test]
    fn blank_group_clears_group() {
        let (_dir, set) = entry_fixture(CONFIG);
        let web = &set.hosts()[0];
        let mut form = Form::edit(web, &Meta { group: Some(group("old")), ..Meta::default() });
        focus(&mut form, FieldId::Group);
        form.handle_key(ctrl('u'));
        type_text(&mut form, "  ");
        let sub = form.submission().unwrap();
        assert!(sub.meta.contains(&MetaOp::SetGroup(web.alias.clone(), None)));
    }

    #[test]
    fn ctrl_u_clears_every_line_of_the_focused_field() {
        let mut form = Form::add();
        type_text(&mut form, "web");
        focus(&mut form, FieldId::Extras);
        type_text(&mut form, "User x");
        form.handle_key(key(KeyCode::Enter));
        type_text(&mut form, "Port 22");
        form.handle_key(ctrl('u'));
        assert_eq!(form.text(FieldId::Extras), "");
        assert_eq!(form.text(FieldId::Alias), "web");
    }

    #[test]
    fn back_tab_moves_focus_backwards() {
        let mut form = Form::add();
        form.handle_key(key(KeyCode::Tab));
        form.handle_key(key(KeyCode::BackTab));
        assert_eq!(form.focused(), FieldId::Alias);
    }

    #[test]
    fn ctrl_s_submits_and_esc_cancels() {
        let mut form = Form::add();
        assert!(matches!(form.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), FormAction::Submit));
        assert!(matches!(form.handle_key(key(KeyCode::Esc)), FormAction::Cancel));
    }
}
