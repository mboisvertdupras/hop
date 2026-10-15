use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ssh_config::{Alias, write_atomic};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GroupPath(Vec<String>);

impl GroupPath {
    pub fn parse(raw: &str) -> Result<Option<GroupPath>, GroupError> {
        let trimmed = raw.trim().trim_matches('/');
        if trimmed.trim().is_empty() {
            return Ok(None);
        }
        let segments: Vec<String> = trimmed.split('/').map(|s| s.trim().to_owned()).collect();
        if segments.iter().any(String::is_empty) {
            return Err(GroupError);
        }
        Ok(Some(GroupPath(segments)))
    }
    pub fn segments(&self) -> &[String] {
        &self.0
    }
    pub fn ancestors_and_self(&self) -> impl Iterator<Item = GroupPath> + '_ {
        (1..=self.0.len()).map(|n| GroupPath(self.0[..n].to_vec()))
    }
}

impl TryFrom<String> for GroupPath {
    type Error = GroupError;
    fn try_from(raw: String) -> Result<GroupPath, GroupError> {
        GroupPath::parse(&raw)?.ok_or(GroupError)
    }
}

impl From<GroupPath> for String {
    fn from(group: GroupPath) -> String {
        group.to_string()
    }
}

impl fmt::Display for GroupPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

#[derive(Debug)]
pub struct GroupError;

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("empty group segment (use a/b, not a//b)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnixTime(pub u64);

impl UnixTime {
    pub fn now() -> UnixTime {
        UnixTime(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
    }
}

pub fn age(then: UnixTime, now: UnixTime) -> String {
    let secs = now.0.saturating_sub(then.0);
    match secs {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupPath>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<UnixTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

pub type MetaMap = BTreeMap<Alias, Meta>;

#[derive(Default, Serialize, Deserialize)]
struct MetaFile {
    #[serde(default)]
    hosts: MetaMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaOp {
    SetFavorite(Alias, bool),
    SetGroup(Alias, Option<GroupPath>),
    SetNotes(Alias, Option<String>),
    Touch(Alias, UnixTime),
    Rename { from: Alias, to: Alias },
    Remove(Alias),
}

pub fn apply(map: &mut MetaMap, op: &MetaOp) {
    match op {
        MetaOp::SetFavorite(alias, favorite) => map.entry(alias.clone()).or_default().favorite = *favorite,
        MetaOp::SetGroup(alias, group) => map.entry(alias.clone()).or_default().group = group.clone(),
        MetaOp::SetNotes(alias, notes) => map.entry(alias.clone()).or_default().notes = notes.clone(),
        MetaOp::Touch(alias, at) => {
            let meta = map.entry(alias.clone()).or_default();
            meta.last_used = meta.last_used.max(Some(*at));
        }
        MetaOp::Rename { from, to } => {
            if let Some(meta) = map.remove(from) {
                map.insert(to.clone(), meta);
            }
        }
        MetaOp::Remove(alias) => {
            map.remove(alias);
        }
    }
    map.retain(|_, meta| *meta != Meta::default());
}

#[derive(Debug)]
pub enum MetaError {
    Io(io::Error),
    Corrupt(String),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetaError::Io(e) => write!(f, "{e}"),
            MetaError::Corrupt(e) => write!(f, "not valid, left untouched: {e}"),
        }
    }
}

pub struct MetaStore {
    path: PathBuf,
}

impl MetaStore {
    pub fn new(path: PathBuf) -> MetaStore {
        MetaStore { path }
    }

    pub fn load(&self) -> Result<MetaMap, MetaError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MetaMap::new()),
            Err(e) => return Err(MetaError::Io(e)),
        };
        toml::from_str::<MetaFile>(&text)
            .map(|file| file.hosts)
            .map_err(|e| MetaError::Corrupt(e.to_string()))
    }

    pub fn update(&self, ops: &[MetaOp]) -> Result<MetaMap, MetaError> {
        let before = self.load()?;
        let mut hosts = before.clone();
        for op in ops {
            apply(&mut hosts, op);
        }
        if hosts != before {
            let file = MetaFile { hosts };
            let text = toml::to_string(&file).map_err(|e| MetaError::Corrupt(e.to_string()))?;
            write_atomic(&self.path, &text).map_err(MetaError::Io)?;
            hosts = file.hosts;
        }
        Ok(hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn alias(s: &str) -> Alias {
        Alias::parse(s).unwrap()
    }

    fn group(s: &str) -> GroupPath {
        GroupPath::parse(s).unwrap().unwrap()
    }

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MetaStore::new(dir.path().join("hop/meta.toml"));
        (dir, store)
    }

    fn seeded() -> MetaMap {
        let mut map = MetaMap::new();
        map.insert(alias("web"), Meta { group: Some(group("a/b")), favorite: true, last_used: Some(UnixTime(50)), notes: Some("n".into()) });
        map.insert(alias("orphan"), Meta { favorite: true, ..Meta::default() });
        map
    }

    #[test]
    fn each_op_is_idempotent() {
        let ops = [
            MetaOp::SetFavorite(alias("web"), false),
            MetaOp::SetFavorite(alias("new"), true),
            MetaOp::SetGroup(alias("web"), Some(group("x"))),
            MetaOp::SetGroup(alias("web"), None),
            MetaOp::SetNotes(alias("web"), None),
            MetaOp::Touch(alias("web"), UnixTime(100)),
            MetaOp::Touch(alias("web"), UnixTime(10)),
            MetaOp::Rename { from: alias("web"), to: alias("www") },
            MetaOp::Remove(alias("web")),
        ];
        for op in ops {
            let mut once = seeded();
            apply(&mut once, &op);
            let mut twice = once.clone();
            apply(&mut twice, &op);
            assert_eq!(once, twice, "{op:?}");
        }
    }

    #[test]
    fn touch_keeps_latest_in_either_order() {
        let (a, b) = (MetaOp::Touch(alias("web"), UnixTime(70)), MetaOp::Touch(alias("web"), UnixTime(60)));
        let (mut ab, mut ba) = (seeded(), seeded());
        apply(&mut ab, &a);
        apply(&mut ab, &b);
        apply(&mut ba, &b);
        apply(&mut ba, &a);
        assert_eq!(ab, ba);
        assert_eq!(ab[&alias("web")].last_used, Some(UnixTime(70)));
    }

    #[test]
    fn rename_moves_and_overwrites_orphan() {
        let mut map = seeded();
        apply(&mut map, &MetaOp::Rename { from: alias("web"), to: alias("orphan") });
        assert!(!map.contains_key(&alias("web")));
        assert_eq!(map[&alias("orphan")].notes.as_deref(), Some("n"));
    }

    #[test]
    fn rename_missing_source_is_noop() {
        let mut map = seeded();
        apply(&mut map, &MetaOp::Rename { from: alias("gone"), to: alias("orphan") });
        assert_eq!(map, seeded());
    }

    #[test]
    fn default_entry_is_removed() {
        let mut map = seeded();
        apply(&mut map, &MetaOp::SetFavorite(alias("orphan"), false));
        assert!(!map.contains_key(&alias("orphan")));
        apply(&mut map, &MetaOp::SetGroup(alias("fresh"), None));
        assert!(!map.contains_key(&alias("fresh")));
    }

    #[test]
    fn missing_file_loads_empty_and_noop_update_writes_nothing() {
        let (_dir, store) = store();
        assert!(store.load().unwrap().is_empty());
        store.update(&[MetaOp::SetNotes(alias("web"), None)]).unwrap();
        assert!(!store.path.exists());
    }

    #[test]
    fn orphans_survive_update() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, "[hosts.orphan]\nnotes = \"keep me\"\n\n[hosts.web]\nfavorite = true\n").unwrap();
        let map = store.update(&[MetaOp::SetFavorite(alias("web"), false)]).unwrap();
        assert_eq!(map, store.load().unwrap());
        assert_eq!(fs::read_to_string(&store.path).unwrap(), "[hosts.orphan]\nnotes = \"keep me\"\n");
    }

    #[test]
    fn concurrent_updates_merge() {
        let (_dir, first) = store();
        let second = MetaStore::new(first.path.clone());
        first.update(&[MetaOp::SetFavorite(alias("web"), true)]).unwrap();
        second.update(&[MetaOp::SetGroup(alias("db"), Some(group("prod")))]).unwrap();
        let map = first.update(&[MetaOp::Touch(alias("web"), UnixTime(9))]).unwrap();
        assert!(map[&alias("web")].favorite);
        assert_eq!(map[&alias("web")].last_used, Some(UnixTime(9)));
        assert_eq!(map[&alias("db")].group, Some(group("prod")));
    }

    #[test]
    fn corrupt_file_is_an_error_and_not_overwritten() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let corrupt = "[hosts.web\nfavorite = yes\n";
        fs::write(&store.path, corrupt).unwrap();
        assert!(matches!(store.load(), Err(MetaError::Corrupt(_))));
        assert!(matches!(store.update(&[MetaOp::SetFavorite(alias("web"), true)]), Err(MetaError::Corrupt(_))));
        assert_eq!(fs::read_to_string(&store.path).unwrap(), corrupt);
    }

    #[test]
    fn pattern_alias_or_empty_group_in_file_is_corrupt() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, "[hosts.\"web*\"]\nfavorite = true\n").unwrap();
        assert!(matches!(store.load(), Err(MetaError::Corrupt(_))));
        fs::write(&store.path, "[hosts.web]\ngroup = \"a//b\"\n").unwrap();
        assert!(matches!(store.load(), Err(MetaError::Corrupt(_))));
    }

    #[test]
    fn toml_round_trip_quotes_dotted_aliases() {
        let (_dir, store) = store();
        store.update(&[MetaOp::SetGroup(alias("web.example.com"), Some(group("clients/acme")))]).unwrap();
        assert_eq!(fs::read_to_string(&store.path).unwrap(), "[hosts.\"web.example.com\"]\ngroup = \"clients/acme\"\n");
        assert_eq!(store.load().unwrap()[&alias("web.example.com")].group, Some(group("clients/acme")));
    }

    #[test]
    fn group_parse_trims_and_rejects_empty_segments() {
        assert_eq!(group(" /clients / acme/ ").segments(), ["clients", "acme"]);
        assert!(GroupPath::parse("  ").unwrap().is_none());
        assert!(GroupPath::parse("/").unwrap().is_none());
        assert!(GroupPath::parse("a//b").is_err());
        assert!(GroupPath::parse("a/ /b").is_err());
        let all: Vec<String> = group("a/b/c").ancestors_and_self().map(|g| g.to_string()).collect();
        assert_eq!(all, ["a", "a/b", "a/b/c"]);
    }

    #[test]
    fn age_buckets() {
        let cases = [(0, "now"), (59, "now"), (60, "1m"), (3_599, "59m"), (3_600, "1h"), (86_400, "1d"), (604_800, "1w"), (6_048_000, "10w")];
        for (secs, expected) in cases {
            assert_eq!(age(UnixTime(1_000), UnixTime(1_000 + secs)), expected, "{secs}");
        }
        assert_eq!(age(UnixTime(10), UnixTime(5)), "now");
    }
}
