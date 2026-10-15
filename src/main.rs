mod app;
mod form;
mod meta;
mod probe;
mod ssh_config;
mod ui;

use std::collections::VecDeque;
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use ratatui::crossterm::{execute, terminal};

use app::{App, Catalog, Effect, Event, LoadReason};
use meta::{MetaError, MetaOp, MetaStore, UnixTime};
use probe::Prober;
use ssh_config::{Alias, CommitError, ConfigSet};

const TICK: Duration = Duration::from_millis(50);

struct Paths {
    home: PathBuf,
    ssh_dir: PathBuf,
    ssh_config: PathBuf,
    meta: PathBuf,
}

impl Paths {
    fn from_env() -> io::Result<Paths> {
        let home = env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME is not set"))?;
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let ssh_dir = home.join(".ssh");
        let meta = config_home.join("hop/meta.toml");
        let home = home.canonicalize().unwrap_or(home);
        Ok(Paths { home, ssh_config: ssh_dir.join("config"), ssh_dir, meta })
    }
}

struct Shell {
    paths: Paths,
    store: MetaStore,
    prober: Prober,
}

impl Shell {
    fn read(&mut self) -> Result<Catalog, String> {
        let config = ConfigSet::load(&self.paths.ssh_config, &self.paths.ssh_dir)
            .map_err(|e| format!("{}: {}", e.path.display(), e.source))?;
        let meta = self.store.load().map_err(|e| self.meta_error(e))?;
        let catalog = Catalog { config, meta };
        self.prober = Prober::start(catalog.aliases());
        Ok(catalog)
    }

    fn load(&mut self, reason: LoadReason) -> Event {
        match self.read() {
            Ok(catalog) => Event::Loaded { catalog, reason },
            Err(message) => Event::Failed(message),
        }
    }

    fn meta_error(&self, e: MetaError) -> String {
        format!("{}: {e}", self.paths.meta.display())
    }

    fn meta(&self, ops: &[MetaOp]) -> Event {
        match self.store.update(ops) {
            Ok(map) => Event::MetaSaved(map),
            Err(e) => Event::Failed(self.meta_error(e)),
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal, effect: Effect) -> io::Result<Vec<Event>> {
        Ok(match effect {
            Effect::Quit => unreachable!("run_loop returns on Quit before calling run"),
            Effect::Reload => vec![self.load(LoadReason::Refresh)],
            Effect::Meta(ops) => vec![self.meta(&ops)],
            Effect::Connect(alias) => {
                connect(terminal, &alias)?;
                vec![self.meta(&[MetaOp::Touch(alias, UnixTime::now())])]
            }
            Effect::Save { plan, meta } => match plan.as_ref().map(ssh_config::commit).transpose() {
                Ok(_) => {
                    let meta_failure = self.store.update(&meta).err();
                    let mut events = vec![self.load(LoadReason::Saved)];
                    events.extend(meta_failure.map(|e| Event::Failed(self.meta_error(e))));
                    events
                }
                Err(CommitError::Stale) => vec![self.load(LoadReason::Refused)],
                Err(CommitError::Io(e)) => vec![Event::Failed(e.to_string())],
            },
        })
    }
}

fn main() -> io::Result<()> {
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        eprintln!("hop: needs an interactive terminal");
        std::process::exit(1);
    }

    // While ssh runs, hop and ssh share the terminal's foreground process
    // group, so a Ctrl-C during connection setup signals both. A flag handler
    // keeps hop alive; exec resets handlers to default, so ssh still gets the
    // signal. In raw mode Ctrl-C is a key event, so the TUI never sees it.
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGQUIT] {
        signal_hook::flag::register(signal, Arc::new(AtomicBool::new(false)))?;
    }

    let paths = Paths::from_env()?;
    let store = MetaStore::new(paths.meta.clone());
    let mut shell = Shell { paths, store, prober: Prober::start(Vec::new()) };

    let catalog = shell.read().unwrap_or_else(|message| {
        eprintln!("hop: {message}");
        std::process::exit(1);
    });
    let mut app = App::new(catalog, shell.paths.home.clone());

    let mut terminal = ratatui::try_init().unwrap_or_else(|e| {
        ratatui::restore(); // init may fail after raw mode is on
        eprintln!("hop: could not set up the terminal: {e}");
        std::process::exit(1);
    });
    let result = run_loop(&mut terminal, &mut app, &mut shell);
    ratatui::restore();
    result
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App, shell: &mut Shell) -> io::Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| ui::render(app, frame))?;
            dirty = false;
        }

        let mut events: VecDeque<Event> = VecDeque::new();
        if event::poll(TICK)? {
            match event::read()? {
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => events.push_back(Event::Key(key)),
                TermEvent::Resize(..) => dirty = true,
                _ => {}
            }
        }
        events.extend(shell.prober.drain().map(|(alias, reach)| Event::Probe(alias, reach)));

        while let Some(event) = events.pop_front() {
            dirty = true;
            match app::update(app, event) {
                None => {}
                Some(Effect::Quit) => return Ok(()),
                Some(effect) => events.extend(shell.run(terminal, effect)?),
            }
        }
    }
}

fn connect(terminal: &mut DefaultTerminal, alias: &Alias) -> io::Result<()> {
    ratatui::restore();

    // `--` and Alias's no-leading-dash invariant: the alias is never an option.
    let status = Command::new("ssh").arg("--").arg(alias.as_str()).status();

    if let Some(message) = failure_message(alias, &status) {
        println!("\n{message} Press any key to return to hop.");
        wait_for_key()?;
    }

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    terminal.clear()?; // ratatui's back buffer no longer matches the screen
    Ok(())
}

fn failure_message(alias: &Alias, status: &io::Result<ExitStatus>) -> Option<String> {
    Some(match status {
        Ok(status) if status.success() => return None,
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("hop: ssh {alias} exited with status {code}."),
            (None, Some(signal)) => format!("hop: ssh {alias} was killed by signal {signal}."),
            (None, None) => format!("hop: ssh {alias} failed."),
        },
        Err(e) => format!("hop: could not run ssh: {e}."),
    })
}

fn wait_for_key() -> io::Result<()> {
    io::stdout().flush()?;
    terminal::enable_raw_mode()?;
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    loop {
        if let TermEvent::Key(_) = event::read()? {
            break;
        }
    }
    terminal::disable_raw_mode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_message_names_exit_code_signal_or_spawn_error() {
        let web = Alias::parse("web").unwrap();
        assert_eq!(failure_message(&web, &Ok(ExitStatus::from_raw(0))), None);
        assert_eq!(failure_message(&web, &Ok(ExitStatus::from_raw(255 << 8))).as_deref(), Some("hop: ssh web exited with status 255."));
        assert_eq!(failure_message(&web, &Ok(ExitStatus::from_raw(2))).as_deref(), Some("hop: ssh web was killed by signal 2."));
        let missing = Err(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(failure_message(&web, &missing).as_deref(), Some("hop: could not run ssh: entity not found."));
    }
}
