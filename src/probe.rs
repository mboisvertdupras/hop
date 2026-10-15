use std::io::Read as _;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::ssh_config::Alias;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Probing,
    Up { latency: Duration },
    Down,
    ViaJump,
}

/// Bounded so a 300-host config does not spawn 300 `ssh -G` processes at once
/// (macOS default fd limit is 256).
const WORKERS: usize = 16;
const SSH_G_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Prober {
    results: Receiver<(Alias, Reach)>,
    cancel: Arc<AtomicBool>,
}

impl Prober {
    pub fn start(aliases: Vec<Alias>) -> Prober {
        Prober::with_ssh(PathBuf::from("ssh"), aliases)
    }

    fn with_ssh(ssh: PathBuf, aliases: Vec<Alias>) -> Prober {
        let (tx, results) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let workers = WORKERS.min(aliases.len());
        let jobs = Arc::new(Mutex::new(aliases.into_iter()));
        let ssh = Arc::new(ssh);
        for _ in 0..workers {
            let (tx, cancel, jobs, ssh) = (tx.clone(), cancel.clone(), jobs.clone(), ssh.clone());
            thread::spawn(move || {
                while !cancel.load(Ordering::Relaxed) {
                    let Some(alias) = jobs.lock().ok().and_then(|mut jobs| jobs.next()) else { break };
                    let reach = probe(&ssh, &alias);
                    if tx.send((alias, reach)).is_err() {
                        break;
                    }
                }
            });
        }
        Prober { results, cancel }
    }

    pub fn drain(&self) -> impl Iterator<Item = (Alias, Reach)> + '_ {
        self.results.try_iter()
    }
}

impl Drop for Prober {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Target {
    Direct { host: String, port: u16 },
    ViaJump,
}

fn probe(ssh: &Path, alias: &Alias) -> Reach {
    match ssh_g(ssh, alias).as_deref().and_then(parse_ssh_g) {
        None => Reach::Down,
        Some(Target::ViaJump) => Reach::ViaJump,
        Some(Target::Direct { host, port }) => {
            let addrs = (host.as_str(), port).to_socket_addrs().into_iter().flatten();
            addrs
                .filter_map(|addr| {
                    let start = Instant::now();
                    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok().map(|_| start.elapsed())
                })
                .next()
                .map_or(Reach::Down, |latency| Reach::Up { latency })
        }
    }
}

/// stdin is null so neither ssh nor a `Match exec` command can read the terminal.
fn ssh_g(ssh: &Path, alias: &Alias) -> Option<String> {
    let mut child = Command::new(ssh)
        .args(["-G", "--", alias.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SSH_G_TIMEOUT;
    // ssh -G prints a few KB, well under a pipe buffer, so polling before reading cannot deadlock.
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    status.success().then_some(output)
}

fn parse_ssh_g(output: &str) -> Option<Target> {
    let (mut host, mut port) = (None, None);
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else { continue };
        match key {
            "hostname" => host = Some(value),
            "port" => port = value.parse().ok(),
            "proxyjump" | "proxycommand" if value != "none" => return Some(Target::ViaJump),
            _ => {}
        }
    }
    Some(Target::Direct { host: host?.to_owned(), port: port? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::net::TcpListener;

    fn alias(s: &str) -> Alias {
        Alias::parse(s).unwrap()
    }

    fn fake_ssh(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("ssh");
        // Written by a child process: a write fd held here would leak into children that
        // parallel tests fork, and Linux then refuses to exec the script (ETXTBSY).
        let status = Command::new("sh")
            .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh"])
            .arg(format!("#!/bin/sh\n{script}\n"))
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    #[test]
    fn parse_direct() {
        let out = "user me\nhostname 10.0.0.5\nport 2222\nproxyjump none\nproxycommand none\n";
        assert_eq!(parse_ssh_g(out), Some(Target::Direct { host: "10.0.0.5".into(), port: 2222 }));
    }

    #[test]
    fn parse_proxyjump_or_proxycommand_is_jump() {
        assert_eq!(parse_ssh_g("hostname h\nport 22\nproxyjump bastion\n"), Some(Target::ViaJump));
        assert_eq!(parse_ssh_g("hostname h\nport 22\nproxycommand nc %h %p\n"), Some(Target::ViaJump));
    }

    #[test]
    fn parse_missing_port_is_none() {
        assert_eq!(parse_ssh_g("hostname h\n"), None);
    }

    #[test]
    fn probe_up_against_local_listener() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ssh = fake_ssh(dir.path(), &format!("printf 'hostname 127.0.0.1\\nport {port}\\n'"));
        assert!(matches!(probe(&ssh, &alias("web")), Reach::Up { .. }));
    }

    #[test]
    fn probe_down_against_closed_port() {
        let dir = tempfile::tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ssh = fake_ssh(dir.path(), &format!("printf 'hostname 127.0.0.1\\nport {port}\\n'"));
        assert_eq!(probe(&ssh, &alias("web")), Reach::Down);
    }

    #[test]
    fn probe_via_jump_and_failed_ssh_g() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(dir.path(), "printf 'hostname 10.255.255.1\\nport 22\\nproxyjump bastion\\n'");
        assert_eq!(probe(&ssh, &alias("web")), Reach::ViaJump);
        let ssh = fake_ssh(dir.path(), "exit 255");
        assert_eq!(probe(&ssh, &alias("web")), Reach::Down);
    }

    #[test]
    fn ssh_g_receives_alias_after_double_dash() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args");
        let ssh = fake_ssh(dir.path(), &format!("echo \"$@\" > '{}'; exit 1", log.display()));
        probe(&ssh, &alias("web"));
        assert_eq!(fs::read_to_string(log).unwrap(), "-G -- web\n");
    }

    #[test]
    fn prober_reports_every_alias() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(dir.path(), "printf 'hostname h\\nport 22\\nproxyjump j\\n'");
        let aliases: Vec<Alias> = (0..40).map(|n| alias(&format!("h{n}"))).collect();
        let prober = Prober::with_ssh(ssh, aliases.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = BTreeMap::new();
        while seen.len() < aliases.len() && Instant::now() < deadline {
            seen.extend(prober.drain());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(seen.values().all(|r| *r == Reach::ViaJump));
        assert_eq!(seen.into_keys().collect::<BTreeSet<_>>(), aliases.into_iter().collect());
    }

    #[test]
    fn dropping_prober_stops_workers_taking_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let ssh = fake_ssh(dir.path(), &format!("echo x >> '{}'; sleep 0.3; exit 1", log.display()));
        let aliases: Vec<Alias> = (0..4 * WORKERS).map(|n| alias(&format!("h{n}"))).collect();
        drop(Prober::with_ssh(ssh, aliases));
        thread::sleep(Duration::from_millis(1500));
        let calls = fs::read_to_string(&log).map_or(0, |s| s.lines().count());
        assert!(calls <= WORKERS, "{calls} ssh -G runs after cancel");
    }
}
