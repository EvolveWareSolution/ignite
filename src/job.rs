//! Everything the UI needs that does not depend on the GUI toolkit, so it can
//! be unit-tested without a window:
//!   - the line protocol spoken by the privileged worker,
//!   - starting the worker (through pkexec) and collecting its output,
//!   - progress / speed / ETA maths,
//!   - building worker arguments and parsing the checksum field.

use crate::disks::{self, Disk};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ protocol

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Stage(String),
    Progress {
        phase: String,
        done: u64,
        total: u64,
    },
    Done,
    Error(String),
    Log(String),
    Exit(Option<i32>),
    /// Windows ISO download: languages are ready.
    Langs(Result<crate::download::Fetched, String>),
    /// Windows ISO download finished: path of the saved file.
    Downloaded(String),
}

pub fn parse_line(line: &str) -> Msg {
    if let Some(t) = line.strip_prefix("STAGE ") {
        return Msg::Stage(t.to_string());
    }
    if let Some(t) = line.strip_prefix("PROGRESS ") {
        let p: Vec<&str> = t.split_whitespace().collect();
        if p.len() == 3 {
            if let (Ok(done), Ok(total)) = (p[1].parse::<u64>(), p[2].parse::<u64>()) {
                return Msg::Progress {
                    phase: p[0].to_string(),
                    done,
                    total,
                };
            }
        }
        return Msg::Log(line.to_string());
    }
    if line == "DONE" {
        return Msg::Done;
    }
    if let Some(t) = line.strip_prefix("ERROR ") {
        return Msg::Error(t.to_string());
    }
    Msg::Log(line.to_string())
}

// -------------------------------------------------------------- shared state

/// Data handed from background threads to the UI (the UI polls it on a timer).
pub struct Shared {
    queue: Mutex<VecDeque<Msg>>,
    disks: Mutex<Option<Vec<Disk>>>,
    pub show_all: AtomicBool,
    pub busy: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            disks: Mutex::new(None),
            show_all: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        })
    }

    pub fn push(&self, m: Msg) {
        lock(&self.queue).push_back(m);
    }

    pub fn drain(&self) -> Vec<Msg> {
        lock(&self.queue).drain(..).collect()
    }

    pub fn take_disks(&self) -> Option<Vec<Disk>> {
        lock(&self.disks).take()
    }

    pub fn publish_disks(&self, list: Vec<Disk>) {
        *lock(&self.disks) = Some(list);
    }
}

/// Re-scans drives every two seconds (but not while a job is running) and
/// publishes the list only when something changed.
pub fn start_disk_poller(shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut last: Option<(bool, Vec<(String, u64, String)>)> = None;
        loop {
            if !shared.busy.load(Ordering::SeqCst) {
                let all = shared.show_all.load(Ordering::SeqCst);
                let list = disks::list_disks(all);
                let key: Vec<_> = list
                    .iter()
                    .map(|d| (d.path.clone(), d.size, d.model.clone()))
                    .collect();
                let key = (all, key);
                if last.as_ref() != Some(&key) {
                    last = Some(key);
                    shared.publish_disks(list);
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

// ------------------------------------------------------------ worker process

/// Start `cmd` with piped stdio and forward its output to `shared`.
/// Returns the child's stdin (write "cancel\n" to abort).
pub fn spawn(mut cmd: Command, shared: Arc<Shared>) -> std::io::Result<ChildStdin> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let sh = shared.clone();
    let err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            sh.push(Msg::Log(line));
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            shared.push(parse_line(&line));
        }
        let _ = err_thread.join(); // keep ordering: Exit always comes last
        let code = child.wait().ok().and_then(|s| s.code());
        shared.push(Msg::Exit(code));
    });
    Ok(stdin)
}

/// Re-run this very executable as root (polkit asks for the password).
pub fn spawn_worker(args: &[String], shared: Arc<Shared>) -> std::io::Result<ChildStdin> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new("pkexec");
    cmd.arg(exe).arg("--worker").args(args);
    spawn(cmd, shared)
}

// ------------------------------------------------------ arguments / checksum

pub struct WriteOptions<'a> {
    pub image: &'a str,
    pub device: &'a str,
    pub verify: bool,
    pub eject: bool,
    pub any_disk: bool,
    pub sha256: Option<&'a str>,
}

pub fn write_args(o: &WriteOptions) -> Vec<String> {
    let mut a = vec![
        "--image".to_string(),
        o.image.to_string(),
        "--device".to_string(),
        o.device.to_string(),
    ];
    if o.verify {
        a.push("--verify".into());
    }
    if o.eject {
        a.push("--eject".into());
    }
    if o.any_disk {
        a.push("--any-disk".into());
    }
    if let Some(h) = o.sha256 {
        a.push("--sha256".into());
        a.push(h.to_string());
    }
    a
}

pub fn restore_args(device: &str, eject: bool, any_disk: bool) -> Vec<String> {
    let mut a = vec![
        "--restore".to_string(),
        "--device".to_string(),
        device.to_string(),
    ];
    if eject {
        a.push("--eject".into());
    }
    if any_disk {
        a.push("--any-disk".into());
    }
    a
}

/// Accepts a bare hash or a whole `sha256sum` line ("<hash>  file.iso").
pub fn extract_sha256(text: &str) -> Result<Option<String>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.split_whitespace()
        .find(|w| w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|h| Some(h.to_lowercase()))
        .ok_or_else(|| "The checksum must be 64 hexadecimal characters.".to_string())
}

// ----------------------------------------------------------------- display

/// Shortens `s` to at most `max` characters by cutting out the middle
/// ("ubuntu-24.04-desktop-amd64.iso" -> "ubuntu-24.0...amd64.iso"), keeping the
/// start and the extension visible. Counts characters, never splits one.
pub fn ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return chars.iter().take(max).collect();
    }
    let keep = max - 3;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push_str("...");
    out.extend(chars[chars.len() - tail..].iter());
    out
}

// ---------------------------------------------------------------- progress

pub struct Reading {
    pub fraction: f32,
    pub rate: f64, // bytes per second
    pub eta_secs: u64,
    pub label: &'static str,
}

impl Reading {
    pub fn describe(&self) -> String {
        format!(
            "{}  {:.0}%  -  {}/s  -  ETA {:02}:{:02}",
            self.label,
            self.fraction * 100.0,
            disks::human_size(self.rate as u64),
            self.eta_secs / 60,
            self.eta_secs % 60
        )
    }
}

/// Measures speed per phase (hash / write / verify).
pub struct Meter {
    phase: String,
    start: Instant,
}

impl Meter {
    pub fn new() -> Meter {
        Meter {
            phase: String::new(),
            start: Instant::now(),
        }
    }

    pub fn update(&mut self, phase: &str, done: u64, total: u64) -> Reading {
        if phase != self.phase {
            self.phase = phase.to_string();
            self.start = Instant::now();
        }
        reading(phase, done, total, self.start.elapsed().as_secs_f64())
    }
}

pub fn reading(phase: &str, done: u64, total: u64, elapsed_secs: f64) -> Reading {
    let fraction = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let rate = done as f64 / elapsed_secs.max(0.001);
    let eta_secs = if rate > 1.0 {
        (total.saturating_sub(done) as f64 / rate) as u64
    } else {
        0
    };
    let label = match phase {
        "hash" => "Checking checksum",
        "verify" => "Verifying",
        "validate" => "Writing checksum list",
        "download" => "Downloading",
        _ => "Writing",
    };
    Reading {
        fraction,
        rate,
        eta_secs,
        label,
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_exit(shared: &Shared) -> Vec<Msg> {
        let mut all = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            all.extend(shared.drain());
            if matches!(all.last(), Some(Msg::Exit(_))) {
                return all;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("worker did not exit in time: {:?}", all);
    }

    #[test]
    fn parses_protocol_lines() {
        assert_eq!(parse_line("STAGE Writing"), Msg::Stage("Writing".into()));
        assert_eq!(
            parse_line("PROGRESS write 5 10"),
            Msg::Progress {
                phase: "write".into(),
                done: 5,
                total: 10
            }
        );
        assert_eq!(parse_line("DONE"), Msg::Done);
        assert_eq!(parse_line("ERROR boom"), Msg::Error("boom".into()));
        assert!(matches!(parse_line("PROGRESS write x y"), Msg::Log(_)));
        assert!(matches!(parse_line("random text"), Msg::Log(_)));
    }

    #[test]
    fn collects_worker_output_in_order() {
        let shared = Shared::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'STAGE hello\\nPROGRESS write 5 10\\nDONE\\n'; echo oops >&2; exit 0");
        let _stdin = spawn(cmd, shared.clone()).unwrap();
        let msgs = wait_for_exit(&shared);
        assert!(msgs.contains(&Msg::Stage("hello".into())));
        assert!(msgs.contains(&Msg::Progress {
            phase: "write".into(),
            done: 5,
            total: 10
        }));
        assert!(msgs.contains(&Msg::Done));
        assert!(msgs.contains(&Msg::Log("oops".into())));
        assert_eq!(msgs.last(), Some(&Msg::Exit(Some(0))));
    }

    #[test]
    fn cancel_reaches_the_worker() {
        use std::io::Write;
        let shared = Shared::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("read l; echo \"ERROR got $l\"; exit 1");
        let mut stdin = spawn(cmd, shared.clone()).unwrap();
        stdin.write_all(b"cancel\n").unwrap();
        stdin.flush().unwrap();
        let msgs = wait_for_exit(&shared);
        assert!(msgs.contains(&Msg::Error("got cancel".into())));
        assert_eq!(msgs.last(), Some(&Msg::Exit(Some(1))));
    }

    #[test]
    fn missing_program_is_an_error_not_a_hang() {
        let shared = Shared::new();
        let cmd = Command::new("/definitely/not/here");
        assert!(spawn(cmd, shared).is_err());
    }

    #[test]
    fn exit_code_from_killed_worker_is_none() {
        let shared = Shared::new();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("kill -9 $$");
        let _stdin = spawn(cmd, shared.clone()).unwrap();
        let msgs = wait_for_exit(&shared);
        assert_eq!(msgs.last(), Some(&Msg::Exit(None)));
    }

    #[test]
    fn builds_worker_arguments() {
        let a = write_args(&WriteOptions {
            image: "/tmp/a.iso",
            device: "/dev/sdb",
            verify: true,
            eject: false,
            any_disk: true,
            sha256: Some("ab"),
        });
        assert_eq!(
            a,
            [
                "--image",
                "/tmp/a.iso",
                "--device",
                "/dev/sdb",
                "--verify",
                "--any-disk",
                "--sha256",
                "ab"
            ]
        );
        assert_eq!(
            restore_args("/dev/sdb", true, false),
            ["--restore", "--device", "/dev/sdb", "--eject"]
        );
    }

    #[test]
    fn checksum_field_parsing() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(extract_sha256("").unwrap(), None);
        assert_eq!(
            extract_sha256(&format!("  {} ", h)).unwrap(),
            Some(h.into())
        );
        assert_eq!(
            extract_sha256(&format!("{}  ubuntu.iso", h.to_uppercase())).unwrap(),
            Some(h.into())
        );
        assert!(extract_sha256("not a hash").is_err());
        assert!(extract_sha256(&h[..63]).is_err());
    }

    #[test]
    fn speed_and_eta_maths() {
        // 100 MiB in 10 s of a 400 MiB job -> 10 MiB/s, 30 s left, 25 %.
        let mib = 1024 * 1024;
        let r = reading("write", 100 * mib, 400 * mib, 10.0);
        assert!((r.fraction - 0.25).abs() < 1e-6);
        assert!((r.rate - 10.0 * mib as f64).abs() < 1.0);
        assert_eq!(r.eta_secs, 30);
        assert_eq!(r.describe(), "Writing  25%  -  10.0 MiB/s  -  ETA 00:30");
        // edge cases: nothing done yet, zero total, finished
        assert_eq!(reading("hash", 0, 100, 0.0).eta_secs, 0);
        assert_eq!(reading("verify", 5, 0, 1.0).fraction, 0.0);
        assert_eq!(reading("write", 100, 100, 1.0).fraction, 1.0);
        assert_eq!(reading("hash", 1, 2, 1.0).label, "Checking checksum");
    }

    #[test]
    fn ellipsize_keeps_ends_and_length() {
        assert_eq!(ellipsize("short.iso", 20), "short.iso");
        assert_eq!(ellipsize("exactly10!", 10), "exactly10!");
        let e = ellipsize("ubuntu-24.04-desktop-amd64.iso", 20);
        assert_eq!(e.chars().count(), 20);
        // 20 chars = 9 head + "..." + 8 tail
        assert_eq!(e, "ubuntu-24...md64.iso");
        assert_eq!(ellipsize("abcdefghij", 3), "abc");
        assert_eq!(ellipsize("abcdefghij", 0), "");
        // multi-byte characters are not split
        let p = ellipsize("نصب-فدورا-نسخه-جدید.iso", 12);
        assert_eq!(p.chars().count(), 12);
    }

    #[test]
    fn shared_queue_and_disks() {
        let s = Shared::new();
        s.push(Msg::Done);
        s.push(Msg::Log("x".into()));
        assert_eq!(s.drain(), vec![Msg::Done, Msg::Log("x".into())]);
        assert!(s.drain().is_empty());
        assert!(s.take_disks().is_none());
        s.publish_disks(Vec::new());
        assert!(s.take_disks().is_some());
        assert!(s.take_disks().is_none());
    }
}
