//! The ssh config as original text plus spans into it.
//!
//! hop never re-serializes a file. Each `ConfigFile` keeps the exact bytes it
//! read, and every parsed thing (line, directive value, Host name token, block)
//! is a byte range into those bytes. An edit is a list of `(range, replacement)`
//! splices, so any byte outside a replaced range is the original byte by
//! construction, not by a careful printer.
//!
//! Everything below `load` and `commit` is pure: `&str` in, `String`/spans out.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub type Span = Range<usize>;

/// A concrete host name usable as `ssh <alias>`.
/// Invariant: non-empty, no whitespace, none of `* ? ! ,`, does not start with `-`
/// (so it can never be read as an ssh option).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Alias(String);

impl Alias {
    pub fn parse(raw: &str) -> Result<Alias, AliasError> {
        if raw.is_empty() {
            Err(AliasError::Empty)
        } else if raw.contains(char::is_whitespace) {
            Err(AliasError::Whitespace)
        } else if raw.contains(['*', '?', '!', ',']) {
            Err(AliasError::Pattern)
        } else if raw.starts_with('-') {
            Err(AliasError::LeadingDash)
        } else {
            Ok(Alias(raw.to_owned()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Alias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Alias {
    type Error = AliasError;
    fn try_from(raw: String) -> Result<Alias, AliasError> {
        Alias::parse(&raw)
    }
}

impl From<Alias> for String {
    fn from(alias: Alias) -> String {
        alias.0
    }
}

#[derive(Debug)]
pub enum AliasError {
    Empty,
    Pattern,
    Whitespace,
    LeadingDash,
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AliasError::Empty => "required",
            AliasError::Pattern => "no * ? ! or , (that would be a pattern)",
            AliasError::Whitespace => "no spaces",
            AliasError::LeadingDash => "must not start with -",
        })
    }
}

/// The five directives the form edits as fields. This enum is the one list:
/// parsing, patching, and the form's field order all iterate `CoreKey::ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKey {
    HostName,
    User,
    Port,
    IdentityFile,
    ProxyJump,
}

impl CoreKey {
    pub const ALL: [CoreKey; 5] = [
        CoreKey::HostName,
        CoreKey::User,
        CoreKey::Port,
        CoreKey::IdentityFile,
        CoreKey::ProxyJump,
    ];
    /// Spelling used for lines hop inserts. Existing lines keep their own casing.
    pub fn keyword(self) -> &'static str {
        match self {
            CoreKey::HostName => "HostName",
            CoreKey::User => "User",
            CoreKey::Port => "Port",
            CoreKey::IdentityFile => "IdentityFile",
            CoreKey::ProxyJump => "ProxyJump",
        }
    }
    pub fn from_keyword(key: &str) -> Option<CoreKey> {
        CoreKey::ALL.into_iter().find(|k| k.keyword().eq_ignore_ascii_case(key))
    }
}

/// Raw core values as written in the file, unquoted. Indexed by `CoreKey as usize`.
/// hop does not interpret them (probing uses `ssh -G`), so it does not type them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Core(pub [Option<String>; 5]);

impl std::ops::Index<CoreKey> for Core {
    type Output = Option<String>;
    fn index(&self, k: CoreKey) -> &Option<String> {
        &self.0[k as usize]
    }
}

impl std::ops::IndexMut<CoreKey> for Core {
    fn index_mut(&mut self, k: CoreKey) -> &mut Option<String> {
        &mut self.0[k as usize]
    }
}

/// `Core` after form validation. Values stay verbatim (Port "022" is not
/// normalized to "22") so an untouched field compares equal to the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidCore(Core);

impl ValidCore {
    pub fn parse(raw: Core) -> Result<ValidCore, (CoreKey, &'static str)> {
        for k in CoreKey::ALL {
            let Some(value) = &raw[k] else { continue };
            if value.trim().is_empty() {
                return Err((k, "must not be blank"));
            }
            if value.contains('\n') {
                return Err((k, "must be one line"));
            }
            if k == CoreKey::Port && !value.parse::<u16>().is_ok_and(|p| p != 0) {
                return Err((k, "must be a number from 1 to 65535"));
            }
        }
        Ok(ValidCore(raw))
    }
    pub fn get(&self, k: CoreKey) -> Option<&str> {
        self.0[k].as_deref()
    }
}

/// The "other directives" text. Verbatim lines of the block that are not the
/// first occurrence of a core key, de-indented: further directives, comments,
/// blank lines, a second IdentityFile. Invariant: no `Host` or `Match` line,
/// because one would split the block on write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extras(String);

impl Extras {
    pub fn parse(text: &str) -> Result<Extras, ExtrasError> {
        for (n, line) in text.split('\n').enumerate() {
            if let LineKind::Directive { key, .. } = lex(line, 0)
                && is_header_key(&line[key])
            {
                return Err(ExtrasError { line: n + 1 });
            }
        }
        Ok(Extras(text.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 1-based line of the offending `Host` or `Match` line.
#[derive(Debug)]
pub struct ExtrasError {
    pub line: usize,
}

/// What the form submits. Built only from validated parts.
#[derive(Clone, Debug)]
pub struct HostSpec {
    pub alias: Alias,
    pub core: ValidCore,
    pub extras: Extras,
}

pub enum HostChange {
    Add(HostSpec),
    Update { alias: Alias, spec: HostSpec },
    Delete(Alias),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub file: FileId,
    pub block: usize,
}

/// All files reachable from the root through `Include`, plus the host list
/// derived from them at load. Immutable; a reload builds a new one.
pub struct ConfigSet {
    /// `files[0]` is the root (`~/.ssh/config`), possibly empty if it does not exist.
    files: Vec<ConfigFile>,
    /// ssh evaluation order (Includes inlined where they appear), one entry per
    /// concrete name, deduplicated by alias with the first definition winning.
    hosts: Vec<HostEntry>,
}

struct ConfigFile {
    /// Canonical path, symlinks resolved, so a write lands in the real file
    /// (dotfiles setups symlink `~/.ssh/config`).
    path: PathBuf,
    text: String,
    parsed: Parsed,
    /// access(2) W_OK on the file and on its directory (the rename needs both).
    writable: bool,
}

struct Parsed {
    /// A partition of the text: concatenating every span reproduces it.
    lines: Vec<Line>,
    /// In file order. Lines before the first block form the preamble and belong
    /// to no block.
    blocks: Vec<Block>,
}

struct Line {
    /// Includes the trailing '\n' (the last line may lack one).
    span: Span,
    kind: LineKind,
}

enum LineKind {
    Blank,
    Comment,
    /// `Key value`, `Key=value`, `Key = value`. Quotes stay inside `value`.
    Directive { key: Span, value: Span },
}

struct Block {
    /// Leading attached comments through the last non-blank body line.
    /// This exact byte range is the block's identity for stale detection.
    span: Span,
    /// Index into `lines` of the first attached comment, or of the header.
    first: usize,
    /// Index into `lines` of the `Host`/`Match` line.
    header: usize,
    /// Line indices after the header up to the end of `span`.
    body: Range<usize>,
    kind: BlockKind,
}

enum BlockKind {
    /// Every token on the Host line, in order, with its span.
    Host(Vec<(HostToken, Span)>),
    /// Never listed, never edited. Kept because a Match line ends the previous block.
    Match,
}

enum HostToken {
    Concrete(Alias),
    /// Wildcards, negations, or anything `Alias::parse` rejects.
    Pattern,
}

/// One list row's worth of ssh data. `core` and `extras` are derived from the block's lines.
pub struct HostEntry {
    pub alias: Alias,
    pub block: BlockRef,
    pub core: Core,
    pub extras: Extras,
    /// Other concrete names on the same Host line. The form warns that field
    /// edits apply to all of them.
    pub siblings: Vec<Alias>,
}

#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub source: io::Error,
}

/// ssh's own limit on nested Includes.
const MAX_INCLUDE_DEPTH: usize = 16;

struct Loader<'a> {
    ssh_dir: &'a Path,
    files: Vec<ConfigFile>,
    hosts: Vec<HostEntry>,
    visited: HashSet<PathBuf>,
    seen: HashSet<Alias>,
}

/// What a file contributes, in line order, so Includes inline where they appear.
enum Step {
    Include(Vec<PathBuf>),
    Header(usize),
}

impl Loader<'_> {
    fn visit(&mut self, path: PathBuf, depth: usize) -> Result<(), LoadError> {
        if depth > MAX_INCLUDE_DEPTH || !self.visited.insert(path.clone()) {
            return Ok(());
        }
        let text = fs::read_to_string(&path).map_err(|source| LoadError { path: path.clone(), source })?;
        self.push_file(path, text);
        let id = FileId(self.files.len() - 1);
        for step in self.steps(id) {
            match step {
                Step::Include(paths) => {
                    for path in paths {
                        self.visit(path, depth + 1)?;
                    }
                }
                Step::Header(block) => self.add_hosts(BlockRef { file: id, block }),
            }
        }
        Ok(())
    }

    fn push_file(&mut self, path: PathBuf, text: String) {
        let parsed = parse(&text);
        let writable = writable(&path);
        self.files.push(ConfigFile { path, text, parsed, writable });
    }

    fn steps(&self, id: FileId) -> Vec<Step> {
        let file = &self.files[id.0];
        let mut steps = Vec::new();
        for (n, line) in file.parsed.lines.iter().enumerate() {
            if let Some(block) = file.parsed.blocks.iter().position(|b| b.header == n) {
                steps.push(Step::Header(block));
            } else if let LineKind::Directive { key, value } = &line.kind
                && file.text[key.clone()].eq_ignore_ascii_case("include")
            {
                let words = file.text[value.clone()].split_whitespace();
                steps.push(Step::Include(words.flat_map(|w| self.expand(w)).collect()));
            }
        }
        steps
    }

    /// `~` is the parent of the ssh dir; relative paths resolve against the ssh dir, as ssh does
    /// for the user config. Unmatched globs and non-files are skipped, as ssh skips them.
    fn expand(&self, word: &str) -> Vec<PathBuf> {
        let word = unquote(word);
        let path = match (word.strip_prefix("~/"), self.ssh_dir.parent()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => self.ssh_dir.join(word),
        };
        let Some(pattern) = path.to_str() else { return Vec::new() };
        let Ok(paths) = glob::glob(pattern) else { return Vec::new() };
        paths
            .filter_map(Result::ok)
            .filter_map(|p| fs::canonicalize(p).ok())
            .filter(|p| p.is_file())
            .collect()
    }

    fn add_hosts(&mut self, at: BlockRef) {
        let file = &self.files[at.file.0];
        let block = &file.parsed.blocks[at.block];
        let BlockKind::Host(tokens) = &block.kind else { return };
        let names: Vec<&Alias> = tokens
            .iter()
            .filter_map(|(t, _)| match t {
                HostToken::Concrete(a) => Some(a),
                HostToken::Pattern => None,
            })
            .collect();
        let (core, extras) = block_fields(file, block);
        for &alias in &names {
            if !self.seen.insert(alias.clone()) {
                continue;
            }
            let (core, extras) = (core.clone(), extras.clone());
            let siblings = names.iter().filter(|&&a| a != alias).map(|&a| a.clone()).collect();
            self.hosts.push(HostEntry { alias: alias.clone(), block: at, core, extras, siblings });
        }
    }
}

impl ConfigSet {
    /// Reads `root` and everything it Includes. A missing root yields an empty,
    /// writable root file (adding a host creates it). Unmatched globs are
    /// ignored, as ssh ignores them.
    pub fn load(root: &Path, ssh_dir: &Path) -> Result<ConfigSet, LoadError> {
        let mut loader = Loader {
            ssh_dir,
            files: Vec::new(),
            hosts: Vec::new(),
            visited: HashSet::new(),
            seen: HashSet::new(),
        };
        match fs::canonicalize(root) {
            Ok(path) => loader.visit(path, 0)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => loader.push_file(root.to_path_buf(), String::new()),
            Err(source) => return Err(LoadError { path: root.to_path_buf(), source }),
        }
        Ok(ConfigSet { files: loader.files, hosts: loader.hosts })
    }

    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    pub fn host(&self, alias: &Alias) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.alias == *alias)
    }

    /// File and 1-based line of the Host line, for the detail pane and confirm prompt.
    pub fn location(&self, entry: &HostEntry) -> (&Path, usize) {
        let file = &self.files[entry.block.file.0];
        (&file.path, file.parsed.blocks[entry.block.block].header + 1)
    }

    pub fn is_writable(&self, entry: &HostEntry) -> bool {
        self.files[entry.block.file.0].writable
    }

    /// The Host line as written, for the delete prompt.
    pub fn host_line(&self, entry: &HostEntry) -> &str {
        let file = &self.files[entry.block.file.0];
        let line = &file.parsed.lines[file.parsed.blocks[entry.block.block].header];
        file.text[line.span.clone()].trim()
    }

    /// Pure. Turns a change into a write plan against the loaded text.
    /// `Ok(None)` means the ssh config needs no change (meta-only edits, or a
    /// form saved untouched), which is why a read-only host can still be
    /// grouped, favorited, and annotated.
    pub fn plan(&self, change: &HostChange) -> Result<Option<Plan>, PlanError> {
        match change {
            HostChange::Add(spec) => {
                if self.host(&spec.alias).is_some() {
                    return Err(PlanError::AliasTaken(spec.alias.clone()));
                }
                let root = &self.files[0];
                if !root.writable {
                    return Err(PlanError::ReadOnly(root.path.clone()));
                }
                let block = render_new_block(spec, default_indent(root));
                Ok(Some(Plan { path: root.path.clone(), op: PlanOp::Insert { block } }))
            }
            HostChange::Update { alias, spec } => {
                let entry = self.host(alias).ok_or_else(|| PlanError::NotFound(alias.clone()))?;
                if spec.alias != *alias && self.host(&spec.alias).is_some() {
                    return Err(PlanError::AliasTaken(spec.alias.clone()));
                }
                let file = &self.files[entry.block.file.0];
                let replacement = rewrite_block(file, entry.block.block, alias, spec);
                if replacement == file.block_text(entry.block.block) {
                    return Ok(None);
                }
                let (expected, ordinal) = self.address(entry)?;
                Ok(Some(Plan { path: file.path.clone(), op: PlanOp::Rewrite { expected, ordinal, replacement } }))
            }
            HostChange::Delete(alias) => {
                let entry = self.host(alias).ok_or_else(|| PlanError::NotFound(alias.clone()))?;
                let file = &self.files[entry.block.file.0];
                let (expected, ordinal) = self.address(entry)?;
                let BlockKind::Host(tokens) = &file.parsed.blocks[entry.block.block].kind else {
                    unreachable!("host entries come from Host blocks")
                };
                let op = if tokens.len() == 1 {
                    PlanOp::Remove { expected, ordinal }
                } else {
                    let replacement = remove_name(file, entry.block.block, alias);
                    PlanOp::Rewrite { expected, ordinal, replacement }
                };
                Ok(Some(Plan { path: file.path.clone(), op }))
            }
        }
    }

    /// The block's bytes and which of the byte-identical blocks it is, refusing read-only files.
    fn address(&self, entry: &HostEntry) -> Result<(String, usize), PlanError> {
        let file = &self.files[entry.block.file.0];
        if !file.writable {
            return Err(PlanError::ReadOnly(file.path.clone()));
        }
        let expected = file.block_text(entry.block.block);
        let ordinal = (0..entry.block.block).filter(|&b| file.block_text(b) == expected).count();
        Ok((expected.to_owned(), ordinal))
    }
}

impl ConfigFile {
    fn block_text(&self, block: usize) -> &str {
        &self.text[self.parsed.blocks[block].span.clone()]
    }
}

#[derive(Debug)]
pub enum PlanError {
    NotFound(Alias),
    AliasTaken(Alias),
    ReadOnly(PathBuf),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NotFound(a) => write!(f, "{a} is no longer in the ssh config"),
            PlanError::AliasTaken(a) => write!(f, "{a} is already defined"),
            PlanError::ReadOnly(p) => write!(f, "{} is read-only", p.display()),
        }
    }
}

/// A content-addressed edit. It names the block by its exact bytes, not by
/// offset, so it can be re-applied to a file that changed elsewhere.
#[derive(Clone, Debug)]
pub struct Plan {
    path: PathBuf,
    op: PlanOp,
}

#[derive(Clone, Debug)]
enum PlanOp {
    /// `ordinal` disambiguates byte-identical blocks: the n-th block whose text
    /// equals `expected`.
    Rewrite { expected: String, ordinal: usize, replacement: String },
    Remove { expected: String, ordinal: usize },
    /// A new Host block for the root file. Position is decided against fresh text.
    Insert { block: String },
}

#[derive(Debug)]
pub enum CommitError {
    /// The block is no longer in the file byte-for-byte.
    Stale,
    Io(io::Error),
}

/// IO shell around `rebase`: re-read `plan.path`, rebase, write a temp file in
/// the same directory with the original mode, fsync, rename over the target.
/// No backup file. Running it twice: the second run sees the block already
/// rewritten and refuses with `Stale`, which is the compare-and-swap working.
pub fn commit(plan: &Plan) -> Result<(), CommitError> {
    let fresh = match fs::read_to_string(&plan.path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CommitError::Io(e)),
    };
    let text = rebase(&fresh, &plan.op)?;
    write_atomic(&plan.path, &text).map_err(CommitError::Io)
}

/// Temp file in the target's directory, the target's mode (0600 for a new
/// file, in a directory created 0700), fsync, rename.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mode = match fs::metadata(path) {
        Ok(meta) => meta.permissions().mode() & 0o7777,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            0o600
        }
        Err(e) => return Err(e),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Pure core of `commit`. Finds the plan's block in freshly read text and
/// applies the plan there. Changes to other blocks in the same file survive.
fn rebase(fresh: &str, op: &PlanOp) -> Result<String, CommitError> {
    let parsed = parse(fresh);
    let find = |expected: &str, ordinal: usize| {
        (0..parsed.blocks.len())
            .filter(|&b| &fresh[parsed.blocks[b].span.clone()] == expected)
            .nth(ordinal)
            .ok_or(CommitError::Stale)
    };
    let mut edit = match op {
        PlanOp::Rewrite { expected, ordinal, replacement } => {
            (parsed.blocks[find(expected, *ordinal)?].span.clone(), replacement.clone())
        }
        PlanOp::Remove { expected, ordinal } => (removal_range(&parsed, find(expected, *ordinal)?), String::new()),
        PlanOp::Insert { block } => {
            let at = insertion_point(&parsed, fresh);
            let mut text = String::new();
            if at > 0 {
                if !fresh[..at].ends_with('\n') {
                    text.push('\n');
                }
                let prev = parsed.lines.iter().rfind(|l| l.span.end <= at);
                if !prev.is_some_and(|l| matches!(l.kind, LineKind::Blank)) {
                    text.push('\n');
                }
            }
            text.push_str(block);
            if at < fresh.len() {
                text.push('\n');
            }
            (at..at, text)
        }
    };
    Ok(splice(fresh, std::slice::from_mut(&mut edit)))
}

fn lex(line: &str, at: usize) -> LineKind {
    let content = line.trim_end_matches(['\n', '\r']);
    let body = content.trim_start();
    if body.trim_end().is_empty() {
        return LineKind::Blank;
    }
    if body.starts_with('#') {
        return LineKind::Comment;
    }
    let key_start = at + content.len() - body.len();
    let key_len = body.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(body.len());
    let rest = &body[key_len..];
    let after_ws = rest.trim_start();
    let value = after_ws.strip_prefix('=').map_or(after_ws, str::trim_start);
    let value_start = key_start + key_len + rest.len() - value.len();
    LineKind::Directive {
        key: key_start..key_start + key_len,
        value: value_start..value_start + value.trim_end().len(),
    }
}

fn is_header_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("host") || key.eq_ignore_ascii_case("match")
}

fn indent_len(text: &str, line: &Line) -> usize {
    let content = &text[line.span.clone()];
    content.len() - content.trim_start().len()
}

/// Lines, then blocks. A Host or Match directive starts a block. A run of
/// comment lines directly above the header (no blank line between, indent no
/// deeper than the header's) is attached to it. A block ends at its last
/// non-blank line before the next block's attached comments.
fn parse(text: &str) -> Parsed {
    let mut lines = Vec::new();
    let mut at = 0;
    for piece in text.split_inclusive('\n') {
        lines.push(Line { span: at..at + piece.len(), kind: lex(piece, at) });
        at += piece.len();
    }
    let headers: Vec<usize> = (0..lines.len())
        .filter(|&i| matches!(&lines[i].kind, LineKind::Directive { key, .. } if is_header_key(&text[key.clone()])))
        .collect();
    let firsts: Vec<usize> = headers
        .iter()
        .map(|&h| {
            let indent = indent_len(text, &lines[h]);
            let mut first = h;
            while first > 0
                && matches!(lines[first - 1].kind, LineKind::Comment)
                && indent_len(text, &lines[first - 1]) <= indent
            {
                first -= 1;
            }
            first
        })
        .collect();
    let blocks = headers
        .iter()
        .enumerate()
        .map(|(n, &header)| {
            let mut end = firsts.get(n + 1).copied().unwrap_or(lines.len());
            while end > header + 1 && matches!(lines[end - 1].kind, LineKind::Blank) {
                end -= 1;
            }
            let LineKind::Directive { key, value } = &lines[header].kind else { unreachable!() };
            let kind = if text[key.clone()].eq_ignore_ascii_case("match") {
                BlockKind::Match
            } else {
                BlockKind::Host(host_tokens(text, value.clone()))
            };
            Block {
                span: lines[firsts[n]].span.start..lines[end - 1].span.end,
                first: firsts[n],
                header,
                body: header + 1..end,
                kind,
            }
        })
        .collect();
    Parsed { lines, blocks }
}

fn host_tokens(text: &str, value: Span) -> Vec<(HostToken, Span)> {
    let mut tokens = Vec::new();
    let mut at = value.start;
    for word in text[value.clone()].split_whitespace() {
        let start = at + text[at..].find(word).unwrap_or(0);
        at = start + word.len();
        let token = Alias::parse(unquote(word)).map_or(HostToken::Pattern, HostToken::Concrete);
        tokens.push((token, start..at));
    }
    tokens
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// Line indices of the first occurrence of each core key, and of every other body line.
/// A core keyword with no value is left among the extras, since it sets nothing.
fn classify(file: &ConfigFile, block: &Block) -> ([Option<usize>; 5], Vec<usize>) {
    let mut core = [None; 5];
    let mut extras = Vec::new();
    for i in block.body.clone() {
        let key = match &file.parsed.lines[i].kind {
            LineKind::Directive { key, value } if !value.is_empty() => CoreKey::from_keyword(&file.text[key.clone()]),
            _ => None,
        };
        match key {
            Some(k) if core[k as usize].is_none() => core[k as usize] = Some(i),
            _ => extras.push(i),
        }
    }
    (core, extras)
}

fn value_of(file: &ConfigFile, line: usize) -> &str {
    match &file.parsed.lines[line].kind {
        LineKind::Directive { value, .. } => unquote(&file.text[value.clone()]),
        _ => "",
    }
}

fn block_fields(file: &ConfigFile, block: &Block) -> (Core, Extras) {
    let (lines, extras) = classify(file, block);
    let core = Core(lines.map(|l| l.map(|l| value_of(file, l).to_owned())));
    let extras = extras
        .iter()
        .map(|&i| file.text[file.parsed.lines[i].span.clone()].trim_end_matches('\n').trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    (core, Extras(extras))
}

/// Applies non-overlapping splices; edits are sorted by (start, end) so an
/// insertion at `p` lands before a deletion starting at `p`.
fn splice(text: &str, edits: &mut [(Span, String)]) -> String {
    edits.sort_by_key(|(span, _)| (span.start, span.end));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (span, replacement) in edits.iter() {
        out.push_str(&text[at..span.start]);
        out.push_str(replacement);
        at = span.end;
    }
    out.push_str(&text[at..]);
    out
}

fn line_indent<'a>(file: &'a ConfigFile, line: &Line) -> &'a str {
    &file.text[line.span.start..line.span.start + indent_len(&file.text, line)]
}

/// The file's first indented directive's indent, else four spaces.
fn default_indent(file: &ConfigFile) -> &str {
    file.parsed
        .lines
        .iter()
        .filter(|l| matches!(l.kind, LineKind::Directive { .. }))
        .map(|l| line_indent(file, l))
        .find(|indent| !indent.is_empty())
        .unwrap_or("    ")
}

/// The insertion after `anchor`; when the anchor is the file's unterminated
/// last line, the newline moves to the front so the file keeps ending without one.
fn insert_after(file: &ConfigFile, anchor: usize, lines: &[String]) -> Option<(Span, String)> {
    if lines.is_empty() {
        return None;
    }
    let end = file.parsed.lines[anchor].span.end;
    let text = lines.concat();
    let text = if file.text[..end].ends_with('\n') {
        text
    } else {
        format!("\n{}", text.trim_end_matches('\n'))
    };
    Some((end..end, text))
}

fn render_extras(extras: &Extras, indent: &str) -> Vec<String> {
    extras
        .as_str()
        .lines()
        .map(str::trim_start)
        .map(|l| if l.is_empty() { "\n".to_owned() } else { format!("{indent}{l}\n") })
        .collect()
}

/// New text for one block, built as splices relative to the block start.
/// Identity law: `rewrite_block(f, b, &e.alias, &spec_of(e)) == &f.text[b.span]`.
fn rewrite_block(file: &ConfigFile, block: usize, editing: &Alias, spec: &HostSpec) -> String {
    let b = &file.parsed.blocks[block];
    let BlockKind::Host(tokens) = &b.kind else { unreachable!("host entries come from Host blocks") };
    let mut edits = Vec::new();
    if spec.alias != *editing
        && let Some((_, span)) = tokens.iter().find(|(t, _)| matches!(t, HostToken::Concrete(a) if a == editing))
    {
        edits.push((span.clone(), spec.alias.to_string()));
    }

    let (core, extra_lines) = classify(file, b);
    let indent = b.body.clone()
        .map(|i| &file.parsed.lines[i])
        .find(|l| matches!(l.kind, LineKind::Directive { .. }))
        .map_or_else(|| default_indent(file), |l| line_indent(file, l));
    let mut deleted = HashSet::new();
    let mut new_core = Vec::new();
    for k in CoreKey::ALL {
        match (core[k as usize], spec.core.get(k)) {
            (Some(line), Some(v)) if value_of(file, line) != v => {
                let LineKind::Directive { value, .. } = &file.parsed.lines[line].kind else { unreachable!() };
                edits.push((value.clone(), render_value(v).into_owned()));
            }
            (Some(line), None) => {
                edits.push((file.parsed.lines[line].span.clone(), String::new()));
                deleted.insert(line);
            }
            (None, Some(v)) => new_core.push(format!("{indent}{} {}\n", k.keyword(), render_value(v))),
            _ => {}
        }
    }

    let (_, current_extras) = block_fields(file, b);
    let new_extras = if spec.extras == current_extras {
        Vec::new()
    } else {
        for &line in &extra_lines {
            edits.push((file.parsed.lines[line].span.clone(), String::new()));
            deleted.insert(line);
        }
        render_extras(&spec.extras, indent)
    };

    let core_anchor = core.into_iter().flatten().filter(|l| !deleted.contains(l)).max().unwrap_or(b.header);
    let end_anchor = (b.header..b.body.end).rev().find(|l| !deleted.contains(l)).unwrap_or(b.header);
    edits.extend(insert_after(file, core_anchor, &new_core));
    edits.extend(insert_after(file, end_anchor, &new_extras));

    for (span, _) in &mut edits {
        *span = span.start - b.span.start..span.end - b.span.start;
    }
    splice(file.block_text(block), &mut edits)
}

/// Removes one token from a Host line with one adjacent whitespace run.
fn remove_name(file: &ConfigFile, block: usize, alias: &Alias) -> String {
    let b = &file.parsed.blocks[block];
    let BlockKind::Host(tokens) = &b.kind else { unreachable!("host entries come from Host blocks") };
    let i = tokens
        .iter()
        .position(|(t, _)| matches!(t, HostToken::Concrete(a) if a == alias))
        .expect("alias is on its own Host line");
    let span = match tokens.get(i + 1) {
        Some((_, next)) => tokens[i].1.start..next.start,
        None => tokens[i - 1].1.end..tokens[i].1.end,
    };
    let at = b.span.start;
    splice(file.block_text(block), &mut [(span.start - at..span.end - at, String::new())])
}

/// The block span, extended over the following blank lines when the block is
/// preceded by a blank line or the start of file, so deletions do not leave
/// doubled gaps. Comments outside the span are never removed.
fn removal_range(parsed: &Parsed, block: usize) -> Span {
    let b = &parsed.blocks[block];
    let after_gap = b.first == 0 || matches!(parsed.lines[b.first - 1].kind, LineKind::Blank);
    let end = if after_gap {
        parsed.lines[b.body.end..]
            .iter()
            .take_while(|l| matches!(l.kind, LineKind::Blank))
            .last()
            .map_or(b.span.end, |l| l.span.end)
    } else {
        b.span.end
    };
    b.span.start..end
}

/// Where a new host goes in the root file: before the trailing run of
/// catch-all blocks (`Host *`-style pattern-only blocks and `Match` blocks),
/// else at EOF. ssh takes the first value it sees for each option, so a host
/// appended after a trailing `Host *` would have its User or Port overridden.
fn insertion_point(parsed: &Parsed, text: &str) -> usize {
    parsed
        .blocks
        .iter()
        .rev()
        .take_while(|b| match &b.kind {
            BlockKind::Match => true,
            BlockKind::Host(tokens) => tokens.iter().all(|(t, _)| matches!(t, HostToken::Pattern)),
        })
        .last()
        .map_or(text.len(), |b| b.span.start)
}

fn render_new_block(spec: &HostSpec, indent: &str) -> String {
    let mut block = format!("Host {}\n", spec.alias);
    for k in CoreKey::ALL {
        if let Some(v) = spec.core.get(k) {
            block.push_str(&format!("{indent}{} {}\n", k.keyword(), render_value(v)));
        }
    }
    block.extend(render_extras(&spec.extras, indent));
    block
}

/// Wraps in double quotes when the value contains whitespace.
fn render_value(value: &str) -> Cow<'_, str> {
    if value.contains(char::is_whitespace) {
        Cow::Owned(format!("\"{value}\""))
    } else {
        Cow::Borrowed(value)
    }
}

/// access(2) W_OK on the file (when it exists) and on the nearest existing
/// directory above it, since the atomic rename writes the directory.
fn writable(path: &Path) -> bool {
    let can_write = |p: &Path| rustix::fs::access(p, rustix::fs::Access::WRITE_OK).is_ok();
    let dir = path.parent().and_then(|d| d.ancestors().find(|a| a.exists()));
    (!path.exists() || can_write(path)) && dir.is_some_and(can_write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const WORK: &str = "Include conf.d/*\n\n# work\nHost web web.example.com\n  HostName=10.0.0.5\n\tUser deploy  \n\nHost *\n  ServerAliveInterval 30\n";
    const QUIRKS: &str = "host db\n\thostname  db.internal\n\tPORT 022\n\tIdentityFile \"~/.ssh/my key\"\n\t# db note\n\tForwardAgent yes\n\nMatch host *.prod\n  User ops\n\n# trailing\nHost=jump\n    HostName jump.example.com";
    const COMMENTS: &str = "# global\nServerAliveInterval 10\n\nHost *\n  User me\n\n# about a\nHost a\n  HostName a.example\n  # note for a\nHost b c *.x\n  Port 2222\n";
    const FIXTURES: [&str; 5] = [WORK, QUIRKS, COMMENTS, "", "\n\n"];

    fn alias(s: &str) -> Alias {
        Alias::parse(s).unwrap()
    }

    fn fixture(text: &str) -> (TempDir, ConfigSet) {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("config"), text).unwrap();
        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        (dir, set)
    }

    fn spec_of(entry: &HostEntry) -> HostSpec {
        HostSpec {
            alias: entry.alias.clone(),
            core: ValidCore::parse(entry.core.clone()).unwrap(),
            extras: entry.extras.clone(),
        }
    }

    fn with_core(entry: &HostEntry, k: CoreKey, v: Option<&str>) -> HostSpec {
        let mut core = entry.core.clone();
        core[k] = v.map(str::to_owned);
        HostSpec { core: ValidCore::parse(core).unwrap(), ..spec_of(entry) }
    }

    /// The root file's text after applying `change` to its loaded text.
    fn edited(set: &ConfigSet, change: HostChange) -> String {
        let plan = set.plan(&change).unwrap().expect("a change to the ssh config");
        rebase(&set.files[0].text, &plan.op).unwrap()
    }

    fn update(set: &ConfigSet, name: &str, spec: impl FnOnce(&HostEntry) -> HostSpec) -> String {
        let spec = spec(set.host(&alias(name)).unwrap());
        edited(set, HostChange::Update { alias: alias(name), spec })
    }

    #[test]
    fn parse_partitions_text() {
        for text in FIXTURES {
            let parsed = parse(text);
            let mut at = 0;
            for line in &parsed.lines {
                assert_eq!(line.span.start, at, "{text:?}");
                at = line.span.end;
            }
            assert_eq!(at, text.len(), "{text:?}");
            let mut prev_end = 0;
            for block in &parsed.blocks {
                assert!(prev_end <= block.span.start && block.span.end <= text.len(), "{text:?}");
                prev_end = block.span.end;
            }
        }
    }

    #[test]
    fn identity_law() {
        for text in FIXTURES {
            let (_dir, set) = fixture(text);
            for entry in set.hosts() {
                let file = &set.files[entry.block.file.0];
                let spec = spec_of(entry);
                assert_eq!(rewrite_block(file, entry.block.block, &entry.alias, &spec), file.block_text(entry.block.block));
                let change = HostChange::Update { alias: entry.alias.clone(), spec };
                assert!(set.plan(&change).unwrap().is_none(), "{} in {text:?}", entry.alias);
            }
        }
    }

    #[test]
    fn hosts_skip_patterns_and_split_shared_lines() {
        let (_dir, set) = fixture(COMMENTS);
        let names: Vec<&str> = set.hosts().iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(set.host(&alias("b")).unwrap().siblings, [alias("c")]);
        assert_eq!(set.host(&alias("b")).unwrap().core[CoreKey::Port].as_deref(), Some("2222"));
    }

    #[test]
    fn core_values_are_unquoted_and_verbatim() {
        let (_dir, set) = fixture(QUIRKS);
        let db = set.host(&alias("db")).unwrap();
        assert_eq!(db.core[CoreKey::HostName].as_deref(), Some("db.internal"));
        assert_eq!(db.core[CoreKey::Port].as_deref(), Some("022"));
        assert_eq!(db.core[CoreKey::IdentityFile].as_deref(), Some("~/.ssh/my key"));
        assert_eq!(db.extras.as_str(), "# db note\nForwardAgent yes");
    }

    #[test]
    fn change_value_keeps_separator_casing_and_indent() {
        let (_dir, set) = fixture(WORK);
        let out = update(&set, "web", |e| with_core(e, CoreKey::HostName, Some("10.0.0.6")));
        assert_eq!(out, WORK.replace("HostName=10.0.0.5", "HostName=10.0.0.6"));

        let (_dir, set) = fixture(QUIRKS);
        let out = update(&set, "db", |e| with_core(e, CoreKey::HostName, Some("db2.internal")));
        assert_eq!(out, QUIRKS.replace("\thostname  db.internal", "\thostname  db2.internal"));
    }

    #[test]
    fn clear_field_deletes_line() {
        let (_dir, set) = fixture(WORK);
        let out = update(&set, "web", |e| with_core(e, CoreKey::User, None));
        assert_eq!(out, WORK.replace("\tUser deploy  \n", ""));
    }

    #[test]
    fn add_field_uses_block_indent_after_last_core_line() {
        let (_dir, set) = fixture(QUIRKS);
        let out = update(&set, "db", |e| with_core(e, CoreKey::User, Some("admin")));
        assert_eq!(out, QUIRKS.replace("my key\"\n", "my key\"\n\tUser admin\n"));
    }

    #[test]
    fn add_field_keeps_missing_final_newline() {
        let (_dir, set) = fixture(QUIRKS);
        let out = update(&set, "jump", |e| with_core(e, CoreKey::User, Some("admin")));
        assert_eq!(out, format!("{QUIRKS}\n    User admin"));
    }

    #[test]
    fn rename_in_shared_block_replaces_only_the_token() {
        let (_dir, set) = fixture(WORK);
        let out = update(&set, "web", |e| HostSpec { alias: alias("www"), ..spec_of(e) });
        assert_eq!(out, WORK.replace("Host web web.example.com", "Host www web.example.com"));
    }

    #[test]
    fn rename_onto_existing_alias_is_refused() {
        let (_dir, set) = fixture(WORK);
        let spec = HostSpec { alias: alias("web.example.com"), ..spec_of(set.host(&alias("web")).unwrap()) };
        let result = set.plan(&HostChange::Update { alias: alias("web"), spec });
        assert!(matches!(result, Err(PlanError::AliasTaken(a)) if a == alias("web.example.com")));
    }

    #[test]
    fn value_with_space_is_quoted() {
        let (_dir, set) = fixture(WORK);
        let out = update(&set, "web", |e| with_core(e, CoreKey::IdentityFile, Some("~/.ssh/id work")));
        assert_eq!(out, WORK.replace("\tUser deploy  \n", "\tUser deploy  \n  IdentityFile \"~/.ssh/id work\"\n"));
    }

    #[test]
    fn edited_extras_are_normalized_at_block_end() {
        let (_dir, set) = fixture(QUIRKS);
        let out = update(&set, "db", |e| HostSpec { extras: Extras::parse("ForwardAgent no\n  Compression yes").unwrap(), ..spec_of(e) });
        assert_eq!(out, QUIRKS.replace("\t# db note\n\tForwardAgent yes\n", "\tForwardAgent no\n\tCompression yes\n"));
    }

    #[test]
    fn untouched_extras_stay_byte_identical_when_core_changes() {
        let (_dir, set) = fixture(QUIRKS);
        let out = update(&set, "db", |e| with_core(e, CoreKey::Port, None));
        assert_eq!(out, QUIRKS.replace("\tPORT 022\n", ""));
    }

    #[test]
    fn extras_reject_host_and_match() {
        assert_eq!(Extras::parse("Foo 1\n  host x").unwrap_err().line, 2);
        assert_eq!(Extras::parse("Match all").unwrap_err().line, 1);
        assert_eq!(Extras::parse("Host=x").unwrap_err().line, 1);
        assert!(Extras::parse("HostName x\n# Host y").is_ok());
    }

    #[test]
    fn attached_comment_belongs_to_block_and_indented_one_to_previous() {
        let parsed = parse(COMMENTS);
        let texts: Vec<&str> = parsed.blocks.iter().map(|b| &COMMENTS[b.span.clone()]).collect();
        assert_eq!(texts, [
            "Host *\n  User me\n",
            "# about a\nHost a\n  HostName a.example\n  # note for a\n",
            "Host b c *.x\n  Port 2222\n",
        ]);
    }

    #[test]
    fn delete_sole_name_removes_block_and_keeps_gap_comments() {
        let text = "Host x\n  User u\n\n# gap comment\n\nHost a\n  HostName h\n\nHost y\n  User v\n";
        let (_dir, set) = fixture(text);
        let out = edited(&set, HostChange::Delete(alias("a")));
        assert_eq!(out, "Host x\n  User u\n\n# gap comment\n\nHost y\n  User v\n");
    }

    #[test]
    fn delete_shared_name_keeps_block() {
        let (_dir, set) = fixture(WORK);
        let out = edited(&set, HostChange::Delete(alias("web")));
        assert_eq!(out, WORK.replace("Host web web.example.com", "Host web.example.com"));
        let out = edited(&set, HostChange::Delete(alias("web.example.com")));
        assert_eq!(out, WORK.replace("Host web web.example.com", "Host web"));
    }

    #[test]
    fn delete_from_line_with_pattern_leaves_pattern() {
        let (_dir, set) = fixture("Host web *.prod\n  User u\n");
        assert_eq!(edited(&set, HostChange::Delete(alias("web"))), "Host *.prod\n  User u\n");
    }

    fn new_host() -> HostSpec {
        let mut core = Core::default();
        core[CoreKey::HostName] = Some("10.0.0.9".into());
        HostSpec { alias: alias("new"), core: ValidCore::parse(core).unwrap(), extras: Extras::default() }
    }

    #[test]
    fn insert_lands_before_trailing_host_star() {
        let (_dir, set) = fixture(WORK);
        let out = edited(&set, HostChange::Add(new_host()));
        assert_eq!(out, WORK.replace("\nHost *", "\nHost new\n  HostName 10.0.0.9\n\nHost *"));
    }

    #[test]
    fn insert_lands_before_trailing_match_and_its_comment() {
        let text = "Host a\n  User u\n\n# defaults\nMatch all\n  User x\n";
        let (_dir, set) = fixture(text);
        let out = edited(&set, HostChange::Add(new_host()));
        assert_eq!(out, "Host a\n  User u\n\nHost new\n  HostName 10.0.0.9\n\n# defaults\nMatch all\n  User x\n");
    }

    #[test]
    fn insert_into_empty_file() {
        let (_dir, set) = fixture("");
        assert_eq!(edited(&set, HostChange::Add(new_host())), "Host new\n    HostName 10.0.0.9\n");
    }

    #[test]
    fn insert_without_final_newline() {
        let (_dir, set) = fixture("Host a\n\tUser u");
        assert_eq!(edited(&set, HostChange::Add(new_host())), "Host a\n\tUser u\n\nHost new\n\tHostName 10.0.0.9\n");
    }

    #[test]
    fn add_existing_alias_is_refused() {
        let (_dir, set) = fixture(WORK);
        let spec = HostSpec { alias: alias("web"), ..new_host() };
        assert!(matches!(set.plan(&HostChange::Add(spec)), Err(PlanError::AliasTaken(_))));
    }

    #[test]
    fn rebase_ordinal_picks_nth_identical_block() {
        let text = "Host a\n  User x\n\nHost a\n  User x\n";
        let op = PlanOp::Rewrite { expected: "Host a\n  User x\n".into(), ordinal: 1, replacement: "Host a\n  User y\n".into() };
        assert_eq!(rebase(text, &op).unwrap(), "Host a\n  User x\n\nHost a\n  User y\n");
        let op = PlanOp::Remove { expected: "Host a\n  User x\n".into(), ordinal: 2 };
        assert!(matches!(rebase(text, &op), Err(CommitError::Stale)));
    }

    #[test]
    fn commit_keeps_concurrent_edit_to_another_block() {
        let (dir, set) = fixture(WORK);
        let path = dir.path().join(".ssh/config");
        let web = set.host(&alias("web")).unwrap();
        let spec = with_core(web, CoreKey::HostName, Some("10.0.0.6"));
        let plan = set.plan(&HostChange::Update { alias: alias("web"), spec }).unwrap().unwrap();
        fs::write(&path, WORK.replace("ServerAliveInterval 30", "ServerAliveInterval 60")).unwrap();
        commit(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "Include conf.d/*\n\n# work\nHost web web.example.com\n  HostName=10.0.0.6\n\tUser deploy  \n\nHost *\n  ServerAliveInterval 60\n"
        );
    }

    #[test]
    fn commit_refuses_stale_block_and_leaves_file_unchanged() {
        let (dir, set) = fixture(WORK);
        let path = dir.path().join(".ssh/config");
        let spec = with_core(set.host(&alias("web")).unwrap(), CoreKey::User, Some("ops"));
        let plan = set.plan(&HostChange::Update { alias: alias("web"), spec }).unwrap().unwrap();
        let changed = WORK.replace("User deploy", "User root");
        fs::write(&path, &changed).unwrap();
        assert!(matches!(commit(&plan), Err(CommitError::Stale)));
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
    }

    #[test]
    fn commit_twice_applies_once() {
        let (dir, set) = fixture(WORK);
        let plan = set.plan(&HostChange::Delete(alias("web"))).unwrap().unwrap();
        commit(&plan).unwrap();
        let once = fs::read_to_string(dir.path().join(".ssh/config")).unwrap();
        assert!(matches!(commit(&plan), Err(CommitError::Stale)));
        assert_eq!(fs::read_to_string(dir.path().join(".ssh/config")).unwrap(), once);
    }

    #[test]
    fn commit_writes_through_symlink_and_keeps_mode() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&dotfiles).unwrap();
        fs::create_dir(&ssh).unwrap();
        let real = dotfiles.join("ssh_config");
        fs::write(&real, WORK).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&real, ssh.join("config")).unwrap();

        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        let spec = with_core(set.host(&alias("web")).unwrap(), CoreKey::User, Some("ops"));
        commit(&set.plan(&HostChange::Update { alias: alias("web"), spec }).unwrap().unwrap()).unwrap();

        assert!(fs::symlink_metadata(ssh.join("config")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), WORK.replace("User deploy", "User ops"));
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o7777, 0o640);
    }

    #[test]
    fn add_creates_missing_ssh_dir_and_file_privately() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        assert!(set.hosts().is_empty());
        commit(&set.plan(&HostChange::Add(new_host())).unwrap().unwrap()).unwrap();
        assert_eq!(fs::read_to_string(ssh.join("config")).unwrap(), "Host new\n    HostName 10.0.0.9\n");
        assert_eq!(fs::metadata(&ssh).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(ssh.join("config")).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn load_resolves_relative_absolute_home_and_glob_includes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir_all(ssh.join("conf.d")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::write(ssh.join("conf.d/b"), "Host b1\n").unwrap();
        fs::write(ssh.join("conf.d/a"), "Host a1\nInclude nested\n").unwrap();
        fs::write(ssh.join("nested"), "Host n1\n").unwrap();
        fs::write(ssh.join("homed"), "Host h1\n").unwrap();
        fs::write(&elsewhere, "Host e1\n").unwrap();
        let root = format!("Host r0\nInclude conf.d/* ~/.ssh/homed\nInclude \"{}\"\nHost r1\n", elsewhere.display());
        fs::write(ssh.join("config"), root).unwrap();

        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        let names: Vec<&str> = set.hosts().iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["r0", "a1", "n1", "b1", "h1", "e1", "r1"]);
        let (path, line) = set.location(set.host(&alias("n1")).unwrap());
        assert_eq!((path, line), (fs::canonicalize(ssh.join("nested")).unwrap().as_path(), 1));
    }

    #[test]
    fn load_survives_include_cycle_and_unmatched_glob() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("config"), "Include other missing.d/*\nHost a\n").unwrap();
        fs::write(ssh.join("other"), "Include config other\nHost b\n").unwrap();
        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        let names: Vec<&str> = set.hosts().iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[test]
    fn first_definition_wins_across_includes() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("config"), "Include inc\nHost dup\n  HostName two\n").unwrap();
        fs::write(ssh.join("inc"), "Host dup\n  HostName one\n").unwrap();
        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        assert_eq!(set.hosts().len(), 1);
        let dup = set.host(&alias("dup")).unwrap();
        assert_eq!(dup.core[CoreKey::HostName].as_deref(), Some("one"));
        assert_eq!(set.location(dup).0, fs::canonicalize(ssh.join("inc")).unwrap());
    }

    #[test]
    fn read_only_include_refuses_ssh_changes_but_not_untouched_saves() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("config"), "Include ro\n").unwrap();
        fs::write(ssh.join("ro"), "Host locked\n  User u\n").unwrap();
        fs::set_permissions(ssh.join("ro"), fs::Permissions::from_mode(0o444)).unwrap();
        let set = ConfigSet::load(&ssh.join("config"), &ssh).unwrap();
        let locked = set.host(&alias("locked")).unwrap();
        assert!(!set.is_writable(locked));
        let untouched = HostChange::Update { alias: alias("locked"), spec: spec_of(locked) };
        assert!(set.plan(&untouched).unwrap().is_none());
        let changed = HostChange::Update { alias: alias("locked"), spec: with_core(locked, CoreKey::User, Some("x")) };
        assert!(matches!(set.plan(&changed), Err(PlanError::ReadOnly(_))));
        assert!(matches!(set.plan(&HostChange::Delete(alias("locked"))), Err(PlanError::ReadOnly(_))));
    }

    #[test]
    fn alias_parse_rejects_patterns_and_option_lookalikes() {
        assert!(Alias::parse("web-1.example.com").is_ok());
        assert!(matches!(Alias::parse(""), Err(AliasError::Empty)));
        assert!(matches!(Alias::parse("a b"), Err(AliasError::Whitespace)));
        for p in ["*", "web?", "!web", "a,b"] {
            assert!(matches!(Alias::parse(p), Err(AliasError::Pattern)), "{p}");
        }
        assert!(matches!(Alias::parse("-oProxyCommand=x"), Err(AliasError::LeadingDash)));
    }

    #[test]
    fn valid_core_checks_port_and_keeps_text_verbatim() {
        let port = |p: &str| {
            let mut core = Core::default();
            core[CoreKey::Port] = Some(p.into());
            ValidCore::parse(core)
        };
        assert_eq!(port("022").unwrap().get(CoreKey::Port), Some("022"));
        for bad in ["0", "65536", "ssh", " "] {
            assert!(matches!(port(bad), Err((CoreKey::Port, _))), "{bad}");
        }
    }
}
