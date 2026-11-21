//! Windows installer media, built by file copy: partition the drive,
//! format it, copy the *files* of the ISO, split `install.wim` when FAT32
//! cannot hold it, apply the Windows user-experience customisations and make
//! the drive bootable (UEFI directly from FAT32, through UEFI:NTFS for NTFS /
//! exFAT, through GRUB's `ntldr` for legacy BIOS).
//!
//! Runs as root inside the worker process (see worker.rs). The only external
//! programs are standard ones: parted, mkfs.*, mount, wimlib-imagex and
//! grub-install; everything else (walking the ISO, copying, verifying,
//! building the answer file) is done here.

use crate::disks;
use crate::unattend::{self, Params};
use crate::winopts::{Fs, Scheme, WinOptions};
use crate::worker::{
    check_target, hash_file, partition_path, progress, read_full, stage, unmount_all, Job, BLOCK,
    REPORT_EVERY, SYNC_EVERY,
};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The 1 MiB FAT image with the UEFI:NTFS boot loader and file system drivers
/// (third-party GPL binary, see data/NOTICE.md).
const UEFI_NTFS_IMG: &[u8] = include_bytes!("../data/uefi-ntfs.img");

/// Windows 11 24H2+ Setup wrapper (third-party GPL binary, see data/NOTICE.md): sets the hardware-check
/// bypass registry keys for in-place upgrades, then starts the real Setup.
const SETUP_WRAPPER_X64: &[u8] = include_bytes!("../data/setup/setup_x64.exe");
const SETUP_WRAPPER_ARM64: &[u8] = include_bytes!("../data/setup/setup_arm64.exe");

/// uefi-md5sum boot loaders (third-party GPL binaries, see data/NOTICE.md; x64, ia32 and aa64 are Secure
/// Boot signed): they check md5sum.txt, then start `boot*_original.efi`.
const VALIDATORS: [(&str, &[u8]); 6] = [
    ("bootx64.efi", include_bytes!("../data/md5/bootx64.efi")),
    ("bootia32.efi", include_bytes!("../data/md5/bootia32.efi")),
    ("bootaa64.efi", include_bytes!("../data/md5/bootaa64.efi")),
    ("bootarm.efi", include_bytes!("../data/md5/bootarm.efi")),
    (
        "bootloongarch64.efi",
        include_bytes!("../data/md5/bootloongarch64.efi"),
    ),
    (
        "bootriscv64.efi",
        include_bytes!("../data/md5/bootriscv64.efi"),
    ),
];

pub(crate) const MIB: u64 = 1024 * 1024;
/// FAT32 cannot store a file of 4 GiB or more.
const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;
/// Size of the pieces `install.wim` is split into for FAT32 (MiB, < 4096).
const SPLIT_MIB: u64 = 3800;
/// Free space we want to keep on the data partition (boot.wim update, slack).
const SLACK: u64 = 48 * MIB;

pub(crate) fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("cancelled by user".into())
    } else {
        Ok(())
    }
}

// ------------------------------------------------------------- tools

pub(crate) struct Tool {
    pub(crate) names: &'static [&'static str],
    pub(crate) install: &'static str,
}

pub(crate) const PARTED: Tool = Tool {
    names: &["parted"],
    install: "parted",
};
pub(crate) const MKFS_FAT: Tool = Tool {
    names: &["mkfs.vfat", "mkfs.fat"],
    install: "dosfstools",
};
pub(crate) const MKFS_NTFS: Tool = Tool {
    names: &["mkfs.ntfs", "mkntfs"],
    install: "ntfs-3g (Fedora: ntfsprogs)",
};
const MKFS_EXFAT: Tool = Tool {
    names: &["mkfs.exfat"],
    install: "exfatprogs",
};
pub(crate) const WIMLIB: Tool = Tool {
    names: &["wimlib-imagex"],
    install: "wimlib (Fedora: wimlib-utils, Debian/Ubuntu: wimtools, Arch/openSUSE: wimlib)",
};
pub(crate) const GRUB: Tool = Tool {
    names: &["grub-install", "grub2-install"],
    install: "GRUB for BIOS (Fedora: grub2-pc, Debian/Ubuntu: grub-pc-bin, Arch: grub, openSUSE: grub2-i386-pc)",
};

fn find_tool(t: &Tool) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in [
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ] {
        dirs.push(PathBuf::from(d));
    }
    for n in t.names {
        for d in &dirs {
            let p = d.join(n);
            if let Ok(m) = std::fs::metadata(&p) {
                if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                    return Some(p);
                }
            }
        }
    }
    None
}

pub(crate) fn require(t: &Tool) -> Result<PathBuf, String> {
    find_tool(t).ok_or_else(|| format!("'{}' is not installed. Install: {}", t.names[0], t.install))
}

// ------------------------------------------------------ running programs

fn pump<R: Read + Send + 'static>(mut r: R, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let flush = |line: &mut Vec<u8>| {
            if !line.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(line).into_owned());
                line.clear();
            }
        };
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        // Progress bars use '\r', so split on both.
                        if b == b'\n' || b == b'\r' {
                            flush(&mut line);
                        } else {
                            line.push(b);
                        }
                    }
                }
            }
        }
        flush(&mut line);
    });
}

/// Run a program, hand every output line to `on_line`, stop it when the user
/// cancels, and turn a failure into an error that carries the last lines.
pub(crate) fn run_stream(
    mut cmd: Command,
    cancel: &AtomicBool,
    mut on_line: impl FnMut(&str),
) -> Result<(), String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let short = Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {}: {}", short, e))?;
    let (tx, rx) = mpsc::channel::<String>();
    pump(child.stdout.take().expect("piped stdout"), tx.clone());
    pump(child.stderr.take().expect("piped stderr"), tx);

    let mut tail: Vec<String> = Vec::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled by user".into());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => {
                on_line(&l);
                tail.push(l);
                if tail.len() > 5 {
                    tail.remove(0);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("{} did not finish: {}", short, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} failed: {}", short, tail.join(" | ")))
    }
}

pub(crate) fn run_quiet(cmd: Command, cancel: &AtomicBool) -> Result<(), String> {
    run_stream(cmd, cancel, |_| {})
}

pub(crate) fn command(path: &Path, args: &[String]) -> Command {
    let mut c = Command::new(path);
    c.args(args);
    c
}

pub(crate) fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

// ---------------------------------------------------------- work area

/// Mount points under /run, removed (and unmounted) even on error or cancel.
pub(crate) struct Workdir {
    root: PathBuf,
    mounted: Vec<PathBuf>,
}

impl Workdir {
    pub(crate) fn new() -> Result<Workdir, String> {
        let base = if Path::new("/run").is_dir() {
            "/run"
        } else {
            "/tmp"
        };
        let root = PathBuf::from(format!("{}/ignite-{}", base, std::process::id()));
        for d in ["iso", "usb", "tmp", "esp"] {
            std::fs::create_dir_all(root.join(d))
                .map_err(|e| format!("cannot create {}: {}", root.join(d).display(), e))?;
        }
        Ok(Workdir {
            root,
            mounted: Vec::new(),
        })
    }
    pub(crate) fn iso(&self) -> PathBuf {
        self.root.join("iso")
    }
    pub(crate) fn usb(&self) -> PathBuf {
        self.root.join("usb")
    }
    pub(crate) fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub(crate) fn esp(&self) -> PathBuf {
        self.root.join("esp")
    }

    pub(crate) fn mount(&mut self, args: &[&str], target: &Path) -> Result<(), String> {
        let out = Command::new("mount")
            .args(args)
            .arg(target)
            .output()
            .map_err(|e| format!("cannot run mount: {}", e))?;
        if out.status.success() {
            self.mounted.push(target.to_path_buf());
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    pub(crate) fn unmount(&mut self, target: &Path) -> Result<(), String> {
        let Some(pos) = self.mounted.iter().position(|m| m == target) else {
            return Ok(());
        };
        let _ = Command::new("sync").output();
        let out = Command::new("umount")
            .arg(target)
            .output()
            .map_err(|e| format!("cannot run umount: {}", e))?;
        if out.status.success() {
            self.mounted.remove(pos);
            Ok(())
        } else {
            Err(format!(
                "could not unmount {}: {}",
                target.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        for t in self.mounted.iter().rev() {
            let ok = Command::new("umount")
                .arg(t)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                let _ = Command::new("umount").arg("-l").arg(t).output();
            }
        }
        // remove_dir only works on empty directories: it can never delete
        // files that are still reachable through a mount.
        for d in ["iso", "usb", "tmp", "esp"] {
            if d == "tmp" {
                let _ = std::fs::remove_dir_all(self.root.join(d));
            } else {
                let _ = std::fs::remove_dir(self.root.join(d));
            }
        }
        let _ = std::fs::remove_dir(&self.root);
    }
}

// ------------------------------------------------------- reading the ISO

/// Case-insensitive path lookup ("Sources/Install.WIM" == "sources/install.wim").
pub fn find_ci(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        let direct = cur.join(comp);
        if direct.exists() {
            cur = direct;
            continue;
        }
        let found = std::fs::read_dir(&cur)
            .ok()?
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp))?;
        cur = found.path();
    }
    Some(cur)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    Wim,
    Esd,
    Swm,
}

#[derive(Debug, Clone)]
pub struct Install {
    pub path: PathBuf,
    pub kind: InstallKind,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub boot_wim: Option<PathBuf>,
    pub bootmgr: bool,
    /// `efi/boot/boot*.efi` files of the media.
    pub efi_boot: Vec<PathBuf>,
    pub install: Option<Install>,
    /// "x86", "amd64", "arm64" or "arm", guessed from the EFI file names.
    pub arch: Option<&'static str>,
}

impl Report {
    pub fn has_efi(&self) -> bool {
        !self.efi_boot.is_empty()
    }
}

pub fn inspect(root: &Path) -> Result<Report, String> {
    let boot_wim = find_ci(root, "sources/boot.wim");
    let bootmgr = find_ci(root, "bootmgr").map_or(false, |p| p.is_file());
    let mut efi_boot = Vec::new();
    if let Some(dir) = find_ci(root, "efi/boot") {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(Result::ok) {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("boot") && n.ends_with(".efi") {
                    efi_boot.push(e.path());
                }
            }
        }
    }
    efi_boot.sort();

    let mut install = None;
    for (name, kind) in [
        ("sources/install.wim", InstallKind::Wim),
        ("sources/install.esd", InstallKind::Esd),
    ] {
        if let Some(p) = find_ci(root, name) {
            if let Ok(m) = std::fs::metadata(&p) {
                if m.is_file() {
                    install = Some(Install {
                        path: p,
                        kind,
                        size: m.len(),
                    });
                    break;
                }
            }
        }
    }
    if install.is_none() {
        if let Some(dir) = find_ci(root, "sources") {
            let mut size = 0;
            let mut first = None;
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.filter_map(Result::ok) {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    if n.starts_with("install") && n.ends_with(".swm") {
                        size += e.metadata().map(|m| m.len()).unwrap_or(0);
                        first.get_or_insert(e.path());
                    }
                }
            }
            if let Some(path) = first {
                install = Some(Install {
                    path,
                    kind: InstallKind::Swm,
                    size,
                });
            }
        }
    }

    let arch = efi_boot.iter().find_map(|p| {
        match p.file_name()?.to_string_lossy().to_lowercase().as_str() {
            "bootx64.efi" => Some("amd64"),
            "bootaa64.efi" => Some("arm64"),
            "bootia32.efi" => Some("x86"),
            "bootarm.efi" => Some("arm"),
            _ => None,
        }
    });

    if boot_wim.is_none() || (!bootmgr && efi_boot.is_empty()) {
        return Err(
            "This is not a Windows installation ISO (sources/boot.wim and a boot manager were not found). Use DD mode, or the standard Ignite page, for other images."
                .into(),
        );
    }
    Ok(Report {
        boot_wim,
        bootmgr,
        efi_boot,
        install,
        arch,
    })
}

// ----------------------------------------------------------- the plan

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    None,
    /// 1 MiB FAT partition with the UEFI:NTFS boot loader.
    UefiNtfs,
    /// 2 MiB FAT partition the silent install uses to recognise its own USB drive.
    Marker,
}

impl Extra {
    fn mib(self) -> u64 {
        match self {
            Extra::None => 0,
            Extra::UefiNtfs => 1,
            Extra::Marker => 2,
        }
    }
}

pub fn extra_partition(o: &WinOptions, r: &Report) -> Extra {
    if o.needs_uefi_ntfs() && r.has_efi() {
        Extra::UefiNtfs
    } else if o.silent {
        Extra::Marker
    } else {
        Extra::None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub data_start_mib: u64,
    pub data_end_mib: u64,
    /// (start, end) in MiB
    pub extra: Option<(u64, u64)>,
}

impl Layout {
    pub fn data_bytes(&self) -> u64 {
        (self.data_end_mib - self.data_start_mib) * MIB
    }
}

/// Whole-MiB layout; the last MiB stays free (room for the GPT backup).
pub fn layout(size_bytes: u64, extra: Extra) -> Result<Layout, String> {
    let total = size_bytes / MIB;
    let need = 1 + extra.mib() + 1 + 64;
    if total < need {
        return Err(format!(
            "the drive is too small ({})",
            disks::human_size(size_bytes)
        ));
    }
    let data_end = total - 1 - extra.mib();
    Ok(Layout {
        data_start_mib: 1,
        data_end_mib: data_end,
        extra: (extra != Extra::None).then_some((data_end, data_end + extra.mib())),
    })
}

/// `parted` invocations (without the program name) that create the layout.
pub fn parted_commands(
    dev: &str,
    o: &WinOptions,
    lay: &Layout,
    extra: Extra,
) -> Vec<Vec<String>> {
    let gpt = o.scheme == Scheme::Gpt;
    let mut v: Vec<Vec<String>> = Vec::new();
    v.push(s(&["-s", dev, "mklabel", if gpt { "gpt" } else { "msdos" }]));
    // The file system argument only picks the MBR partition type
    // (0x0c for FAT32, 0x07 for NTFS and exFAT).
    let hint = if o.fs == Fs::Fat32 { "fat32" } else { "ntfs" };
    let mk = |name: &str, hint: &str, a: u64, b: u64| -> Vec<String> {
        s(&[
            "-s",
            dev,
            "mkpart",
            if gpt { name } else { "primary" },
            hint,
            &format!("{}MiB", a),
            &format!("{}MiB", b),
        ])
    };
    v.push(mk("Windows", hint, lay.data_start_mib, lay.data_end_mib));
    if let Some((a, b)) = lay.extra {
        let name = if extra == Extra::UefiNtfs {
            "UEFI_NTFS"
        } else {
            "IGNITE_BOOT"
        };
        v.push(mk(name, "fat16", a, b));
        if extra == Extra::UefiNtfs {
            v.push(s(&["-s", dev, "set", "2", "esp", "on"]));
        }
    }
    if !gpt && o.target.wants_bios() {
        // The "active" flag: the BIOS only boots an active partition.
        v.push(s(&["-s", dev, "set", "1", "boot", "on"]));
    }
    v
}

pub fn mkfs_args(fs: Fs, label: &str, part: &str) -> Vec<String> {
    match fs {
        Fs::Fat32 => s(&["-F", "32", "-n", label, part]),
        Fs::Ntfs => s(&["-Q", "-F", "-L", label, part]),
        Fs::Exfat => s(&["-L", label, part]),
    }
}

/// What to do with one file of the ISO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Copy,
    /// Create an empty file instead (appraiserres.dll for the hardware bypass).
    Empty,
    /// install.wim is too big for FAT32: it is split with wimlib afterwards.
    SplitWim,
}

pub fn classify(rel_lower: &str, size: u64, fs: Fs, bypass_hw: bool) -> Result<Action, String> {
    if bypass_hw && rel_lower == "sources/appraiserres.dll" {
        return Ok(Action::Empty);
    }
    if fs == Fs::Fat32 && size > FAT32_MAX_FILE {
        return if rel_lower == "sources/install.wim" {
            Ok(Action::SplitWim)
        } else {
            Err(format!(
                "{} is larger than 4 GiB and cannot be stored on FAT32; choose NTFS or exFAT instead.",
                rel_lower
            ))
        };
    }
    Ok(Action::Copy)
}

// ----------------------------------------------------------- file tree

struct FileItem {
    src: PathBuf,
    /// Path inside the media, '/' separated, original case.
    rel: String,
    size: u64,
    action: Action,
}

pub(crate) fn walk(
    root: &Path,
    dir: &Path,
    dirs: &mut Vec<String>,
    files: &mut Vec<(PathBuf, String, u64)>,
) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {}", dir.display(), e))?;
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        let md = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if md.is_dir() {
            dirs.push(rel);
            walk(root, &path, dirs, files)?;
        } else if md.is_file() {
            files.push((path, rel, md.len()));
        }
    }
    Ok(())
}

// --------------------------------------------------------- copy + verify

fn copy_file(
    src: &Path,
    dst: &Path,
    done: &mut u64,
    since_sync: &mut u64,
    total: u64,
    last: &mut Instant,
    cancel: &AtomicBool,
    buf: &mut [u8],
) -> Result<(), String> {
    let mut inp = File::open(src).map_err(|e| format!("cannot read {}: {}", src.display(), e))?;
    let mut out = File::create(dst).map_err(|e| format!("cannot create {}: {}", dst.display(), e))?;
    loop {
        check_cancel(cancel)?;
        let n = read_full(&mut inp, buf).map_err(|e| format!("read error in {}: {}", src.display(), e))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("write error in {}: {}", dst.display(), e))?;
        *done += n as u64;
        *since_sync += n as u64;
        if *since_sync >= SYNC_EVERY {
            out.sync_data()
                .map_err(|e| format!("sync error in {}: {}", dst.display(), e))?;
            *since_sync = 0;
        }
        if last.elapsed() >= REPORT_EVERY {
            progress("write", *done, total);
            *last = Instant::now();
        }
    }
    Ok(())
}

fn verify_file(
    a: &Path,
    b: &Path,
    done: &mut u64,
    total: u64,
    last: &mut Instant,
    cancel: &AtomicBool,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<(), String> {
    let mut fa = File::open(a).map_err(|e| format!("cannot read {}: {}", a.display(), e))?;
    let mut fb = File::open(b).map_err(|e| format!("cannot read {}: {}", b.display(), e))?;
    let mut offset = 0u64;
    loop {
        check_cancel(cancel)?;
        let n = read_full(&mut fa, buf_a).map_err(|e| e.to_string())?;
        let m = read_full(&mut fb, &mut buf_b[..n.max(1)]).map_err(|e| e.to_string())?;
        if n != m || buf_a[..n] != buf_b[..n] {
            return Err(format!(
                "verification FAILED: {} differs from the image near byte {}",
                b.display(),
                offset
            ));
        }
        if n == 0 {
            return Ok(());
        }
        offset += n as u64;
        *done += n as u64;
        if last.elapsed() >= REPORT_EVERY {
            progress("verify", *done, total);
            *last = Instant::now();
        }
    }
}

// ------------------------------------------------------------- wimlib

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WimImageInfo {
    pub arch: Option<String>,
    pub language: Option<String>,
    /// Windows build number, e.g. 26100.
    pub build: Option<u32>,
    /// Edition name, e.g. "Windows 11 Pro".
    pub name: Option<String>,
}

/// "Image Count:    2" from `wimlib-imagex info file.wim`.
pub fn parse_image_count(info: &str) -> Option<u32> {
    info.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "Image Count").then(|| v.trim().parse().ok())?
    })
}

/// Architecture and language from `wimlib-imagex info file.wim N`.
pub fn parse_image_info(info: &str) -> WimImageInfo {
    let mut r = WimImageInfo::default();
    for l in info.lines() {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "Architecture" => {
                r.arch = match v.to_lowercase().as_str() {
                    "x86_64" => Some("amd64".into()),
                    "x86" => Some("x86".into()),
                    "arm64" => Some("arm64".into()),
                    "arm" => Some("arm".into()),
                    _ => None,
                }
            }
            "Build" => r.build = v.parse().ok(),
            "Display Name" if !v.is_empty() => r.name = Some(v.to_string()),
            "Name" if r.name.is_none() && !v.is_empty() => r.name = Some(v.to_string()),
            "Default Language" if !v.is_empty() => r.language = Some(v.to_string()),
            "Languages" if r.language.is_none() && !v.is_empty() => {
                r.language = v.split_whitespace().next().map(|x| x.to_string())
            }
            _ => {}
        }
    }
    r
}

pub(crate) fn wim_info_text(wimlib: &Path, wim: &Path, index: Option<u32>) -> Result<String, String> {
    let mut c = Command::new(wimlib);
    c.arg("info").arg(wim);
    if let Some(i) = index {
        c.arg(i.to_string());
    }
    let out = c
        .output()
        .map_err(|e| format!("cannot run wimlib-imagex: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "wimlib-imagex info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Windows Setup is index 2 of boot.wim; "unofficial" media may only have one.
fn setup_index(wimlib: &Path, boot_wim: &Path) -> Result<u32, String> {
    let n = parse_image_count(&wim_info_text(wimlib, boot_wim, None)?).unwrap_or(1);
    Ok(if n >= 2 { 2 } else { 1 })
}

/// "segoen_slboot_EX.ttf" -> "segoen_slboot.ttf"
pub fn strip_ex(name: &str) -> String {
    name.replace("_EX", "")
}

// ------------------------------------------------------- grub (BIOS)

pub const GRUB_CFG: &str = "\
# Written by Ignite: hand over to the Windows boot manager.
set timeout=0
search --no-floppy --file --set=root /bootmgr
ntldr /bootmgr
boot
";

// ------------------------------------------------------------ the job

fn write_empty(path: &Path) -> Result<(), String> {
    File::create(path)
        .map(|_| ())
        .map_err(|e| format!("cannot create {}: {}", path.display(), e))
}

pub(crate) fn wait_for_node(path: &str, cancel: &AtomicBool) -> Result<(), String> {
    let start = Instant::now();
    while !Path::new(path).exists() {
        check_cancel(cancel)?;
        if start.elapsed() > Duration::from_secs(15) {
            return Err(format!("partition {} did not appear", path));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

pub(crate) fn mount_data(wd: &mut Workdir, fs: Fs, part: &str) -> Result<(), String> {
    let usb = wd.usb();
    match fs {
        Fs::Fat32 => wd.mount(&["-t", "vfat", part], &usb),
        Fs::Exfat => wd.mount(&["-t", "exfat", part], &usb),
        Fs::Ntfs => wd
            .mount(&["-t", "ntfs3", part], &usb)
            .or_else(|_| wd.mount(&["-t", "ntfs-3g", part], &usb))
            .or_else(|e| {
                Err(format!(
                    "cannot mount the NTFS partition ({}). Install ntfs-3g or use a kernel with the ntfs3 driver.",
                    e
                ))
            }),
    }
    .map_err(|e| format!("cannot mount {}: {}", part, e))
}

/// Mount the ISO read-only. Windows ISOs are UDF (with a tiny ISO 9660 stub),
/// so UDF is tried first.
fn open_image(wd: &mut Workdir, image: &str) -> Result<(PathBuf, Report), String> {
    if Path::new(image).is_dir() {
        // An already extracted image (handy for tests and for advanced users).
        let root = PathBuf::from(image);
        let r = inspect(&root)?;
        return Ok((root, r));
    }
    let iso = wd.iso();
    let mut last_err = String::new();
    for fstype in ["udf", "iso9660"] {
        match wd.mount(&["-o", "loop,ro", "-t", fstype, image], &iso) {
            Ok(()) => match inspect(&iso) {
                Ok(r) => return Ok((iso, r)),
                Err(e) => {
                    last_err = e;
                    wd.unmount(&iso)?;
                }
            },
            Err(e) => last_err = format!("cannot mount the image as {}: {}", fstype, e),
        }
    }
    Err(last_err)
}

pub fn write_windows(job: &Job, cancel: &AtomicBool) -> Result<(), String> {
    let info = check_target(&job.dev, job.any)?;
    run_windows(job, &info, cancel)
}

/// The real work. `info` describes the (already validated) target drive.
pub(crate) fn run_windows(
    job: &Job,
    info: &disks::Disk,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dev = job.dev.as_str();
    let o = job.win.normalized()?;
    let regional = job.regional.as_ref();

    // ---- 1. the image: exists, checksum (BEFORE anything is erased)
    let is_dir = Path::new(&job.iso).is_dir();
    if !is_dir {
        let md = std::fs::metadata(&job.iso).map_err(|e| format!("cannot open image: {}", e))?;
        if md.len() == 0 {
            return Err("image file is empty".into());
        }
        if let Some(expected) = &job.sha256 {
            stage("Checking SHA256 of the image");
            let got = hash_file(&job.iso, cancel)?;
            if &got != expected {
                return Err(format!(
                    "SHA256 mismatch (expected {}, got {}). The image is corrupted or incomplete; nothing was written.",
                    expected, got
                ));
            }
            stage("SHA256 matches");
        }
    }
    check_cancel(cancel)?;

    // ---- 2. read the ISO
    stage("Reading the Windows image");
    let mut wd = Workdir::new()?;
    let (src_root, report) = open_image(&mut wd, &job.iso)?;
    match &report.install {
        Some(i) => stage(&format!(
            "Found {} ({})",
            i.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            disks::human_size(i.size)
        )),
        None => stage("Warning: no install.wim/install.esd found (Windows PE or custom media?)"),
    }
    if o.target.wants_uefi() && !report.has_efi() {
        stage("Warning: this image has no UEFI boot files (efi/boot/boot*.efi); UEFI boot will not work");
    }
    if o.target.wants_bios() && !report.bootmgr {
        return Err("This image has no 'bootmgr' file, so it cannot boot in legacy BIOS mode. Pick a UEFI target.".into());
    }

    if o.wtg {
        return crate::wtg::run(job, info, &o, &mut wd, &src_root, &report, cancel);
    }

    // ---- 3. file list and plan
    let mut dirs = Vec::new();
    let mut raw_files = Vec::new();
    walk(&src_root, &src_root, &mut dirs, &mut raw_files)?;
    let mut files: Vec<FileItem> = Vec::with_capacity(raw_files.len());
    for (src, rel, size) in raw_files {
        let action = classify(&rel.to_lowercase(), size, o.fs, o.bypass_hw)?;
        files.push(FileItem {
            src,
            rel,
            size,
            action,
        });
    }
    let needs_split = files.iter().any(|f| f.action == Action::SplitWim);
    let total_bytes: u64 = files
        .iter()
        .filter(|f| f.action != Action::Empty)
        .map(|f| f.size)
        .sum();

    let extra = extra_partition(&o, &report);
    let lay = layout(info.size, extra)?;
    if total_bytes + SLACK > lay.data_bytes() {
        return Err(format!(
            "The Windows files need {} but the drive only has {} available.",
            disks::human_size(total_bytes + SLACK),
            disks::human_size(lay.data_bytes())
        ));
    }

    // ---- 4. programs we need, checked before anything is erased
    let parted = require(&PARTED)?;
    let mkfs = match o.fs {
        Fs::Fat32 => require(&MKFS_FAT)?,
        Fs::Ntfs => require(&MKFS_NTFS)?,
        Fs::Exfat => require(&MKFS_EXFAT)?,
    };
    let mkfs_fat = match extra {
        Extra::Marker => Some(require(&MKFS_FAT)?),
        _ => None,
    };
    let needs_wimlib = needs_split || o.bypass_hw || o.silent || o.modern_secure_boot;
    let wimlib = if needs_wimlib {
        Some(require(&WIMLIB)?)
    } else {
        None
    };
    let grub = if o.target.wants_bios() {
        Some(require(&GRUB)?)
    } else {
        None
    };
    if needs_split {
        stage("install.wim is larger than 4 GiB: it will be split into 3.8 GB pieces for FAT32");
    }
    check_cancel(cancel)?;

    // ---- 5. partition
    unmount_all(dev)?;
    stage("Erasing old signatures");
    run_quiet(command(Path::new("wipefs"), &s(&["-a", dev])), cancel)?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .open(dev)
            .map_err(|e| format!("cannot open {} for writing: {}", dev, e))?;
        f.write_all(&vec![0u8; MIB as usize])
            .map_err(|e| format!("write error: {}", e))?;
        f.sync_all().map_err(|e| format!("sync failed: {}", e))?;
    }
    check_cancel(cancel)?;
    stage(&format!(
        "Creating the {} partition table",
        if o.scheme == Scheme::Gpt { "GPT" } else { "MBR" }
    ));
    for args in parted_commands(dev, &o, &lay, extra) {
        run_quiet(command(&parted, &args), cancel)?;
    }
    let _ = Command::new("partprobe").arg(dev).output();
    let _ = Command::new("udevadm").arg("settle").output();
    let part1 = partition_path(dev, 1);
    let part2 = partition_path(dev, 2);
    wait_for_node(&part1, cancel)?;
    if extra != Extra::None {
        wait_for_node(&part2, cancel)?;
    }

    // ---- 6. small extra partition
    match extra {
        Extra::UefiNtfs => {
            stage("Writing the UEFI:NTFS boot loader (so UEFI can boot from NTFS/exFAT)");
            let mut f = OpenOptions::new()
                .write(true)
                .open(&part2)
                .map_err(|e| format!("cannot open {}: {}", part2, e))?;
            f.write_all(UEFI_NTFS_IMG)
                .map_err(|e| format!("write error: {}", e))?;
            f.sync_all().map_err(|e| format!("sync failed: {}", e))?;
        }
        Extra::Marker => {
            let mk = mkfs_fat.as_ref().expect("checked above");
            run_quiet(
                command(mk, &s(&["-n", unattend::BOOT_MARKER_LABEL, &part2])),
                cancel,
            )?;
        }
        Extra::None => {}
    }

    // ---- 7. format + mount
    stage(&format!("Formatting {} as {}", part1, o.fs));
    run_quiet(command(&mkfs, &mkfs_args(o.fs, &o.label, &part1)), cancel)?;
    let _ = Command::new("udevadm").arg("settle").output();
    mount_data(&mut wd, o.fs, &part1)?;
    let usb = wd.usb();
    check_cancel(cancel)?;

    // ---- 8. copy the files
    stage(&format!(
        "Copying the Windows files ({})",
        disks::human_size(total_bytes)
    ));
    for d in &dirs {
        std::fs::create_dir_all(usb.join(d))
            .map_err(|e| format!("cannot create {}: {}", d, e))?;
    }
    let mut done = 0u64;
    let mut since_sync = 0u64;
    let mut last = Instant::now();
    let mut buf = vec![0u8; BLOCK];
    let mut split_item: Option<&FileItem> = None;
    for f in &files {
        match f.action {
            Action::Copy => copy_file(
                &f.src,
                &usb.join(&f.rel),
                &mut done,
                &mut since_sync,
                total_bytes,
                &mut last,
                cancel,
                &mut buf,
            )?,
            Action::Empty => write_empty(&usb.join(&f.rel))?,
            Action::SplitWim => split_item = Some(f),
        }
    }
    if let Some(item) = split_item {
        // Done last, so the bar keeps moving from where the copy ended.
        let before = done;
        let wimlib = wimlib.as_ref().expect("checked above");
        stage("Splitting install.wim for FAT32 (this takes a while)");
        let sources = find_ci(&usb, "sources").unwrap_or_else(|| usb.join("sources"));
        let dst = sources.join("install.swm");
        let mut c = Command::new(wimlib);
        c.arg("split")
            .arg(&item.src)
            .arg(&dst)
            .arg(SPLIT_MIB.to_string());
        let size = item.size;
        run_stream(c, cancel, |line| {
            if let Some(p) = parse_percent(line) {
                progress("write", before + size / 100 * p as u64, total_bytes);
            }
        })?;
    }
    progress("write", total_bytes, total_bytes);
    check_cancel(cancel)?;

    // ---- 9. customisation (answer file, bootloader swap)
    let mut modified: HashSet<String> = HashSet::new();
    if o.bypass_hw {
        modified.insert("sources/appraiserres.dll".into());
    }
    let build = customize(
        &o,
        regional,
        &report,
        &usb,
        wimlib.as_deref(),
        &wd.tmp(),
        &mut modified,
        cancel,
    )?;
    if o.bypass_hw {
        apply_setup_wrapper(&usb, build, &mut modified)?;
    }
    check_cancel(cancel)?;

    // ---- 10. legacy BIOS boot code
    if let Some(grub) = &grub {
        stage("Installing the BIOS boot loader (GRUB, hands over to bootmgr)");
        let boot = usb.join("boot");
        std::fs::create_dir_all(&boot).map_err(|e| format!("cannot create {}: {}", boot.display(), e))?;
        let mut c = Command::new(grub);
        c.arg("--target=i386-pc")
            .arg(format!("--boot-directory={}", boot.display()))
            .arg("--modules=part_msdos fat ntfs exfat ntldr search")
            .arg("--force")
            .arg(dev);
        run_stream(c, cancel, |_| {})?;
        let cfg_dir = boot.join("grub");
        std::fs::create_dir_all(&cfg_dir).map_err(|e| e.to_string())?;
        std::fs::write(cfg_dir.join("grub.cfg"), GRUB_CFG)
            .map_err(|e| format!("cannot write grub.cfg: {}", e))?;
    }

    // ---- 10b. UEFI:Validation (must come last: it lists every final file)
    if o.uefi_validation {
        if report.has_efi() {
            stage("Adding UEFI:Validation (the drive checks its own files at every UEFI boot)");
            apply_validation(&usb, &mut modified, cancel)?;
        } else {
            stage("Warning: no UEFI boot files on this image; UEFI:Validation skipped");
        }
    }

    // ---- 11. flush, then verify what is really on the drive
    stage("Flushing caches to the device");
    let _ = Command::new("sync").output();
    if job.verify {
        stage("Verifying the copied files");
        let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
        let to_check: Vec<&FileItem> = files
            .iter()
            .filter(|f| f.action == Action::Copy && !modified.contains(&f.rel.to_lowercase()))
            .collect();
        let vtotal: u64 = to_check.iter().map(|f| f.size).sum();
        let mut vdone = 0u64;
        let mut last = Instant::now();
        let mut a = vec![0u8; BLOCK];
        let mut b = vec![0u8; BLOCK];
        for f in to_check {
            verify_file(
                &f.src,
                &usb.join(&f.rel),
                &mut vdone,
                vtotal,
                &mut last,
                cancel,
                &mut a,
                &mut b,
            )?;
        }
        progress("verify", vtotal, vtotal);
    }

    wd.unmount(&usb)?;
    let _ = Command::new("udevadm").arg("settle").output();
    Ok(())
}

pub fn parse_percent(line: &str) -> Option<u32> {
    let i = line.rfind('%')?;
    let digits: String = line[..i]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok().filter(|p| *p <= 100)
}

/// Answer file, `appraiserres.dll`, Secure Boot 2023 boot loaders.
#[allow(clippy::too_many_arguments)]
fn customize(
    o: &WinOptions,
    regional: Option<&crate::winopts::Regional>,
    report: &Report,
    usb: &Path,
    wimlib: Option<&Path>,
    tmp: &Path,
    modified: &mut HashSet<String>,
    cancel: &AtomicBool,
) -> Result<Option<u32>, String> {
    let boot_wim = find_ci(usb, "sources/boot.wim");
    let want_answer = unattend::is_needed(o, regional);
    if !want_answer && !o.modern_secure_boot {
        return Ok(None);
    }
    stage("Applying the Windows customisation");
    for line in unattend::summary(o, regional) {
        stage(&format!("  - {}", line));
    }

    // Setup is index 2 of boot.wim ("unofficial" media may differ).
    let mut idx = 2;
    let mut wim_info = WimImageInfo::default();
    if let (Some(w), Some(bw)) = (wimlib, boot_wim.as_deref()) {
        idx = setup_index(w, bw)?;
        wim_info = parse_image_info(&wim_info_text(w, bw, Some(idx))?);
    }

    if want_answer {
        // The EFI boot file names are the most reliable hint; boot.wim's own
        // metadata is the fallback for BIOS-only media.
        let arch = report
            .arch
            .or(wim_info.arch.as_deref())
            .unwrap_or("amd64")
            .to_string();
        let ui_lang = wim_info.language.clone().unwrap_or_else(|| "en-US".into());
        let built = unattend::build(&Params {
            opts: o,
            regional,
            arch: &arch,
            ui_language: &ui_lang,
        });
        if let Some(u) = built {
            if u.winpe {
                // A windowsPE pass only works from the root of boot.wim.
                let (w, bw) = match (wimlib, boot_wim.as_deref()) {
                    (Some(w), Some(bw)) => (w, bw),
                    _ => return Err("boot.wim not found on the drive".into()),
                };
                let xml = tmp.join("Autounattend.xml");
                std::fs::write(&xml, &u.xml).map_err(|e| format!("cannot write the answer file: {}", e))?;
                stage(&format!("Adding Autounattend.xml to boot.wim (image {})", idx));
                let mut c = Command::new(w);
                c.arg("update")
                    .arg(bw)
                    .arg(idx.to_string())
                    .arg(format!(
                        "--command=add \"{}\" \"/Autounattend.xml\"",
                        xml.display()
                    ));
                run_stream(c, cancel, |_| {})?;
                modified.insert("sources/boot.wim".into());
            } else {
                // Without a windowsPE pass Setup copies sources\$OEM$\$$\* into
                // %WINDIR%, so the file lands in Windows\Panther\unattend.xml.
                let sources = find_ci(usb, "sources").unwrap_or_else(|| usb.join("sources"));
                let dir = sources.join("$OEM$").join("$$").join("Panther");
                std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
                std::fs::write(dir.join("unattend.xml"), &u.xml)
                    .map_err(|e| format!("cannot write unattend.xml: {}", e))?;
                stage("Added sources/$OEM$/$$/Panther/unattend.xml");
            }
        }
    }

    if o.modern_secure_boot {
        modern_secure_boot(wimlib, boot_wim.as_deref(), idx, usb, tmp, modified, cancel)?;
    }
    Ok(wim_info.build)
}

/// Machine type of a PE file (0x8664 = x64, 0xAA64 = ARM64).
pub fn pe_machine(head: &[u8]) -> Option<u16> {
    if head.len() < 0x40 || &head[..2] != b"MZ" {
        return None;
    }
    let off = u32::from_le_bytes([head[0x3c], head[0x3d], head[0x3e], head[0x3f]]) as usize;
    if head.len() < off + 6 || &head[off..off + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([head[off + 4], head[off + 5]]))
}

/// Windows 11 24H2 and later ignore the LabConfig keys for in-place upgrades
/// unless they are set before Setup starts, so the original
/// `setup.exe` becomes `setup.dll` and a small wrapper takes its place.
fn apply_setup_wrapper(
    usb: &Path,
    build: Option<u32>,
    modified: &mut HashSet<String>,
) -> Result<(), String> {
    if build.map_or(true, |b| b < 26000) {
        return Ok(());
    }
    let Some(setup) = find_ci(usb, "setup.exe").filter(|p| p.is_file()) else {
        return Ok(());
    };
    let mut head = vec![0u8; 4096];
    let n = File::open(&setup)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| format!("cannot read setup.exe: {}", e))?;
    let wrapper = match pe_machine(&head[..n]) {
        Some(0x8664) => SETUP_WRAPPER_X64,
        Some(0xAA64) => SETUP_WRAPPER_ARM64,
        _ => {
            stage("Warning: setup.exe is not x64 or ARM64; the in-place upgrade wrapper was not added");
            return Ok(());
        }
    };
    let dll = setup.with_file_name("setup.dll");
    std::fs::rename(&setup, &dll).map_err(|e| format!("cannot rename setup.exe: {}", e))?;
    std::fs::write(&setup, wrapper).map_err(|e| format!("cannot write the setup.exe wrapper: {}", e))?;
    modified.insert("setup.exe".into());
    stage("Added the Windows 11 24H2 in-place upgrade wrapper (setup.exe -> setup.dll)");
    Ok(())
}

/// The text of md5sum.txt: a size header (used by uefi-md5sum for its
/// progress bar) and one "<md5>  ./path" line per file.
pub fn md5sum_text(entries: &[(String, String)], total_bytes: u64) -> String {
    let mut s = format!("# md5sum_totalbytes = 0x{:x}\n", total_bytes);
    for (hash, rel) in entries {
        s.push_str(&format!("{}  ./{}\n", hash, rel));
    }
    s
}

/// Install the uefi-md5sum boot loaders and write md5sum.txt for the final
/// content of the drive.
fn apply_validation(
    usb: &Path,
    modified: &mut HashSet<String>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let efi_dir = find_ci(usb, "efi/boot").ok_or("efi/boot not found on the drive")?;
    let mut installed: HashSet<String> = HashSet::new();
    for (name, data) in VALIDATORS {
        let Some(orig) = find_ci(&efi_dir, name).filter(|p| p.is_file()) else {
            continue;
        };
        let rel = orig
            .strip_prefix(usb)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_lowercase();
        let renamed = orig.with_file_name(name.replace(".efi", "_original.efi"));
        std::fs::rename(&orig, &renamed)
            .map_err(|e| format!("cannot rename {}: {}", orig.display(), e))?;
        std::fs::write(&orig, data)
            .map_err(|e| format!("cannot write the validation boot loader: {}", e))?;
        modified.insert(rel.clone());
        installed.insert(rel);
    }
    if installed.is_empty() {
        stage("Warning: no known EFI boot file found; UEFI:Validation skipped");
        return Ok(());
    }

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    walk(usb, usb, &mut dirs, &mut files)?;
    files.retain(|(_, rel, _)| {
        let l = rel.to_lowercase();
        l != "md5sum.txt" && !installed.contains(&l)
    });
    let total: u64 = files.iter().map(|f| f.2).sum();
    let mut entries = Vec::with_capacity(files.len());
    let mut done = 0u64;
    let mut last = Instant::now();
    let mut buf = vec![0u8; BLOCK];
    for (path, rel, _) in &files {
        let mut f = File::open(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let mut h = crate::md5::Md5::new();
        loop {
            check_cancel(cancel)?;
            let n = read_full(&mut f, &mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            done += n as u64;
            if last.elapsed() >= REPORT_EVERY {
                progress("validate", done, total);
                last = Instant::now();
            }
        }
        entries.push((crate::md5::hex(&h.finalize()), rel.clone()));
    }
    progress("validate", total, total);
    std::fs::write(usb.join("md5sum.txt"), md5sum_text(&entries, total))
        .map_err(|e| format!("cannot write md5sum.txt: {}", e))?;
    Ok(())
}

/// Replace the boot files with the ones signed by "Windows UEFI CA 2023",
/// taken from `\Windows\Boot\EFI_EX` inside boot.wim.
fn modern_secure_boot(
    wimlib: Option<&Path>,
    boot_wim: Option<&Path>,
    idx: u32,
    usb: &Path,
    tmp: &Path,
    modified: &mut HashSet<String>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (Some(w), Some(bw)) = (wimlib, boot_wim) else {
        stage("Warning: cannot apply the 2023 boot loaders (boot.wim or wimlib missing)");
        return Ok(());
    };
    let out = tmp.join("ca2023");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut c = Command::new(w);
    c.arg("extract")
        .arg(bw)
        .arg(idx.to_string())
        .arg("/Windows/Boot/EFI_EX")
        .arg("/Windows/Boot/Fonts_EX")
        .arg(format!("--dest-dir={}", out.display()))
        // wimlib-imagex flattens the extracted paths by default, which is
        // what we want: <dest>/EFI_EX and <dest>/Fonts_EX.
        .arg("--no-acls");
    if let Err(e) = run_stream(c, cancel, |_| {}) {
        if e.contains("cancelled") {
            return Err(e);
        }
        stage(&format!(
            "Warning: no 'Windows UEFI CA 2023' boot loaders found in boot.wim; option ignored ({})",
            e
        ));
        return Ok(());
    }
    let efi_ex = find_ci(&out, "EFI_EX");
    let fonts_ex = find_ci(&out, "Fonts_EX");
    let mut replaced = 0;

    let mut replace = |from: &Path, to: &Path, rel: &str| -> bool {
        if std::fs::copy(from, to).is_ok() {
            modified.insert(rel.to_lowercase());
            true
        } else {
            false
        }
    };

    if let (Some(fonts), Some(dest_dir)) = (fonts_ex, find_ci(usb, "efi/microsoft/boot/fonts")) {
        if let Ok(rd) = std::fs::read_dir(fonts) {
            for e in rd.filter_map(Result::ok) {
                let name = strip_ex(&e.file_name().to_string_lossy());
                let to = dest_dir.join(&name);
                let rel = format!("efi/microsoft/boot/fonts/{}", name);
                if e.path().is_file() && replace(&e.path(), &to, &rel) {
                    replaced += 1;
                }
            }
        }
    }
    if let Some(efi_ex) = efi_ex {
        // efi/boot/boot<arch>.efi
        if let Some(src) = find_ci(&efi_ex, "bootmgfw_EX.efi") {
            for target in ["efi/boot/bootx64.efi", "efi/boot/bootaa64.efi", "efi/boot/bootia32.efi"] {
                if let Some(dst) = find_ci(usb, target) {
                    if dst.is_file() && replace(&src, &dst, target) {
                        replaced += 1;
                        break;
                    }
                }
            }
        }
        // bootmgr.efi (not present in every build)
        if let (Some(src), Some(dst)) = (find_ci(&efi_ex, "bootmgr_EX.efi"), find_ci(usb, "bootmgr.efi")) {
            if replace(&src, &dst, "bootmgr.efi") {
                replaced += 1;
            }
        }
    }
    if replaced > 0 {
        stage(&format!(
            "Replaced {} EFI files with 'Windows UEFI CA 2023' signed versions",
            replaced
        ));
        stage("To boot this drive, the PC must trust the 'Windows UEFI CA 2023' Secure Boot certificate (or have Secure Boot off)");
    } else {
        stage("Warning: no 2023 boot loaders could be applied");
    }
    Ok(())
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winopts::{all_off, Regional, Target};

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ignite-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(root: &Path, rel: &str, size: usize) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; size]).unwrap();
    }

    fn fake_windows(root: &Path) {
        touch(root, "Sources/Boot.WIM", 10);
        touch(root, "Sources/Install.WIM", 20);
        touch(root, "Sources/appraiserres.dll", 5);
        touch(root, "bootmgr", 3);
        touch(root, "EFI/Boot/bootx64.efi", 4);
        touch(root, "boot/bcd", 1);
    }

    #[test]
    fn embedded_uefi_ntfs_image_has_the_expected_size() {
        assert_eq!(UEFI_NTFS_IMG.len() as u64, MIB);
        // FAT boot sector signature
        assert_eq!(&UEFI_NTFS_IMG[510..512], &[0x55, 0xAA]);
    }

    #[test]
    fn case_insensitive_lookup() {
        let d = tmpdir("ci");
        fake_windows(&d);
        assert!(find_ci(&d, "sources/install.wim").unwrap().ends_with("Sources/Install.WIM"));
        assert!(find_ci(&d, "EFI/BOOT/BOOTX64.EFI").is_some());
        assert!(find_ci(&d, "sources/nothing.wim").is_none());
        assert!(find_ci(&d, "nodir/x").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inspects_a_windows_tree() {
        let d = tmpdir("inspect");
        fake_windows(&d);
        let r = inspect(&d).unwrap();
        assert!(r.bootmgr && r.has_efi());
        assert_eq!(r.arch, Some("amd64"));
        let i = r.install.unwrap();
        assert_eq!(i.kind, InstallKind::Wim);
        assert_eq!(i.size, 20);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inspects_split_and_esd_media() {
        let d = tmpdir("inspect2");
        touch(&d, "sources/boot.wim", 1);
        touch(&d, "bootmgr", 1);
        touch(&d, "sources/install.swm", 100);
        touch(&d, "sources/install2.swm", 50);
        let r = inspect(&d).unwrap();
        let i = r.install.as_ref().unwrap();
        assert_eq!((i.kind, i.size), (InstallKind::Swm, 150));
        assert!(!r.has_efi());
        let _ = std::fs::remove_dir_all(&d);
        let d = tmpdir("inspect3");
        touch(&d, "sources/boot.wim", 1);
        touch(&d, "sources/install.esd", 9);
        touch(&d, "efi/boot/bootaa64.efi", 1);
        let r = inspect(&d).unwrap();
        assert_eq!(r.install.unwrap().kind, InstallKind::Esd);
        assert_eq!(r.arch, Some("arm64"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_non_windows_media() {
        let d = tmpdir("notwin");
        touch(&d, "casper/vmlinuz", 5);
        touch(&d, "isolinux/isolinux.bin", 5);
        assert!(inspect(&d).unwrap_err().contains("not a Windows installation ISO"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn extra_partition_decision() {
        let d = tmpdir("extra");
        fake_windows(&d);
        let r = inspect(&d).unwrap();
        let mut o = all_off();
        assert_eq!(extra_partition(&o, &r), Extra::None);
        o.fs = Fs::Ntfs;
        assert_eq!(extra_partition(&o, &r), Extra::UefiNtfs);
        o.silent = true;
        assert_eq!(extra_partition(&o, &r), Extra::UefiNtfs);
        o.fs = Fs::Fat32;
        assert_eq!(extra_partition(&o, &r), Extra::Marker);
        o.fs = Fs::Exfat;
        o.scheme = Scheme::Mbr;
        o.target = Target::Bios;
        o.silent = false;
        assert_eq!(extra_partition(&o, &r), Extra::None, "BIOS only needs no UEFI loader");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn layout_math() {
        let gib = 1024 * MIB;
        let l = layout(8 * gib, Extra::None).unwrap();
        assert_eq!((l.data_start_mib, l.data_end_mib, l.extra), (1, 8191, None));
        let l = layout(8 * gib, Extra::UefiNtfs).unwrap();
        assert_eq!(l.data_end_mib, 8190);
        assert_eq!(l.extra, Some((8190, 8191)));
        let l = layout(8 * gib, Extra::Marker).unwrap();
        assert_eq!(l.extra, Some((8189, 8191)));
        assert_eq!(l.data_bytes(), 8188 * MIB);
        // never overlaps and the last MiB stays free
        assert!(l.extra.unwrap().1 < 8 * 1024);
        assert!(layout(50 * MIB, Extra::None).is_err());
        // a 16 GB stick that is really 15.9 GB
        assert!(layout(15_931_539_456, Extra::UefiNtfs).is_ok());
    }

    #[test]
    fn parted_gpt_fat32() {
        let o = all_off(); // GPT, UEFI, FAT32
        let l = layout(8 * 1024 * MIB, Extra::None).unwrap();
        let c = parted_commands("/dev/sdx", &o, &l, Extra::None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], s(&["-s", "/dev/sdx", "mklabel", "gpt"]));
        assert_eq!(
            c[1],
            s(&["-s", "/dev/sdx", "mkpart", "Windows", "fat32", "1MiB", "8191MiB"])
        );
    }

    #[test]
    fn parted_mbr_ntfs_dual() {
        let mut o = all_off();
        o.scheme = Scheme::Mbr;
        o.target = Target::Both;
        o.fs = Fs::Ntfs;
        let l = layout(8 * 1024 * MIB, Extra::UefiNtfs).unwrap();
        let c = parted_commands("/dev/sdx", &o, &l, Extra::UefiNtfs);
        assert_eq!(c[0][3], "msdos");
        assert_eq!(c[1][3..], s(&["primary", "ntfs", "1MiB", "8190MiB"])[..]);
        assert_eq!(c[2][3..], s(&["primary", "fat16", "8190MiB", "8191MiB"])[..]);
        assert_eq!(c[3], s(&["-s", "/dev/sdx", "set", "2", "esp", "on"]));
        assert_eq!(c[4], s(&["-s", "/dev/sdx", "set", "1", "boot", "on"]));
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn parted_uefi_only_mbr_has_no_boot_flag_and_marker_is_not_esp() {
        let mut o = all_off();
        o.scheme = Scheme::Mbr;
        o.target = Target::Uefi;
        o.silent = true;
        let l = layout(8 * 1024 * MIB, Extra::Marker).unwrap();
        let c = parted_commands("/dev/sdx", &o, &l, Extra::Marker);
        assert!(!c.iter().any(|a| a.contains(&"boot".to_string())));
        assert!(!c.iter().any(|a| a.contains(&"esp".to_string())));
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn mkfs_arguments() {
        assert_eq!(
            mkfs_args(Fs::Fat32, "WIN", "/dev/sdx1"),
            s(&["-F", "32", "-n", "WIN", "/dev/sdx1"])
        );
        assert_eq!(
            mkfs_args(Fs::Ntfs, "Win 11", "/dev/sdx1"),
            s(&["-Q", "-F", "-L", "Win 11", "/dev/sdx1"])
        );
        assert_eq!(
            mkfs_args(Fs::Exfat, "W", "/dev/sdx1"),
            s(&["-L", "W", "/dev/sdx1"])
        );
    }

    #[test]
    fn what_happens_to_each_file() {
        let big = 5 * 1024 * MIB;
        assert_eq!(classify("sources/install.wim", 1000, Fs::Fat32, false), Ok(Action::Copy));
        assert_eq!(classify("sources/install.wim", big, Fs::Fat32, false), Ok(Action::SplitWim));
        assert_eq!(classify("sources/install.wim", big, Fs::Ntfs, false), Ok(Action::Copy));
        assert_eq!(classify("sources/install.wim", big, Fs::Exfat, false), Ok(Action::Copy));
        assert!(classify("sources/install.esd", big, Fs::Fat32, false).is_err());
        assert!(classify("other/huge.bin", big, Fs::Fat32, false).is_err());
        assert_eq!(
            classify("sources/appraiserres.dll", 100, Fs::Ntfs, true),
            Ok(Action::Empty)
        );
        assert_eq!(
            classify("sources/appraiserres.dll", 100, Fs::Ntfs, false),
            Ok(Action::Copy)
        );
        // exactly 4 GiB - 1 still fits on FAT32
        assert_eq!(classify("a", FAT32_MAX_FILE, Fs::Fat32, false), Ok(Action::Copy));
        assert!(classify("a", FAT32_MAX_FILE + 1, Fs::Fat32, false).is_err());
    }

    #[test]
    fn walks_a_tree() {
        let d = tmpdir("walk");
        fake_windows(&d);
        let mut dirs = vec![];
        let mut files = vec![];
        walk(&d, &d, &mut dirs, &mut files).unwrap();
        assert!(dirs.contains(&"Sources".to_string()));
        assert!(dirs.contains(&"EFI/Boot".to_string()) || dirs.contains(&"EFI".to_string()));
        assert_eq!(files.len(), 6);
        assert_eq!(files.iter().map(|f| f.2).sum::<u64>(), 10 + 20 + 5 + 3 + 4 + 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn wimlib_output_parsing() {
        let top = "WIM Information:\n----------------\nPath:           boot.wim\nVersion:        68864\nImage Count:    2\nCompression:    LZX\n";
        assert_eq!(parse_image_count(top), Some(2));
        assert_eq!(parse_image_count("nothing"), None);
        let img = "Index:                  2\nName:                   Microsoft Windows Setup (x64)\nArchitecture:           x86_64\nLanguages:              fa-IR en-US\nDefault Language:       en-US\n";
        let i = parse_image_info(img);
        assert_eq!(i.arch.as_deref(), Some("amd64"));
        assert_eq!(i.language.as_deref(), Some("en-US"));
        let i = parse_image_info("Architecture: ARM64\nLanguages: de-DE\n");
        assert_eq!(i.arch.as_deref(), Some("arm64"));
        assert_eq!(i.language.as_deref(), Some("de-DE"));
        assert_eq!(parse_image_info("Architecture: x86\n").arch.as_deref(), Some("x86"));
    }

    #[test]
    fn percent_from_wimlib_progress() {
        assert_eq!(
            parse_percent("Writing split WIM part 1 (1000 MiB of 4000 MiB) 25% complete"),
            Some(25)
        );
        assert_eq!(parse_percent("100% done"), Some(100));
        assert_eq!(parse_percent("no percent here"), None);
        assert_eq!(parse_percent("abc %"), None);
        assert_eq!(parse_percent("101%"), None);
    }

    #[test]
    fn bootloader_names() {
        assert_eq!(strip_ex("segoen_slboot_EX.ttf"), "segoen_slboot.ttf");
        assert_eq!(strip_ex("wgl4_boot.ttf"), "wgl4_boot.ttf");
        assert!(GRUB_CFG.contains("ntldr /bootmgr"));
        assert!(GRUB_CFG.contains("boot\n"));
    }

    #[test]
    fn streaming_runner_reports_output_and_failure() {
        let cancel = AtomicBool::new(false);
        let mut lines = vec![];
        let mut c = Command::new("sh");
        c.arg("-c").arg("printf 'a\\rb\\nc'; exit 0");
        run_stream(c, &cancel, |l| lines.push(l.to_string())).unwrap();
        assert_eq!(lines, ["a", "b", "c"]);

        let mut c = Command::new("sh");
        c.arg("-c").arg("echo boom >&2; exit 3");
        let e = run_stream(c, &cancel, |_| {}).unwrap_err();
        assert!(e.contains("sh failed") && e.contains("boom"), "{}", e);

        let e = run_stream(Command::new("/no/such/tool"), &cancel, |_| {}).unwrap_err();
        assert!(e.contains("cannot run"));
    }

    #[test]
    fn streaming_runner_can_be_cancelled() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = Instant::now();
        let e = run_stream(c, &cancel, |_| {}).unwrap_err();
        assert!(e.contains("cancelled"));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn tool_lookup() {
        assert!(find_tool(&Tool { names: &["sh"], install: "x" }).is_some());
        let e = require(&Tool { names: &["definitely-not-a-tool"], install: "pkg" }).unwrap_err();
        assert!(e.contains("definitely-not-a-tool") && e.contains("pkg"));
    }

    #[test]
    fn answer_file_goes_where_it_must() {
        // No windowsPE pass (no bypass, not silent): file lands in $OEM$.
        let d = tmpdir("answer");
        fake_windows(&d);
        let report = inspect(&d).unwrap();
        let mut o = all_off();
        o.no_online_account = true;
        let mut modified = HashSet::new();
        let cancel = AtomicBool::new(false);
        customize(&o, None, &report, &d, None, &d, &mut modified, &cancel).unwrap();
        let f = d.join("Sources/$OEM$/$$/Panther/unattend.xml");
        let xml = std::fs::read_to_string(f).unwrap();
        assert!(xml.contains("BypassNRO"));
        assert!(modified.is_empty());
        let _ = std::fs::remove_dir_all(&d);

        // Nothing selected: nothing written.
        let d = tmpdir("answer2");
        fake_windows(&d);
        let report = inspect(&d).unwrap();
        customize(&all_off(), None, &report, &d, None, &d, &mut modified, &cancel).unwrap();
        assert!(!d.join("Sources/$OEM$").exists());
        let _ = std::fs::remove_dir_all(&d);

        // regional with detection: still $OEM$ (no windowsPE)
        let d = tmpdir("answer3");
        fake_windows(&d);
        let report = inspect(&d).unwrap();
        let mut o = all_off();
        o.regional = true;
        let r = Regional { locale: "fa-IR".into(), timezone: None };
        customize(&o, Some(&r), &report, &d, None, &d, &mut modified, &cancel).unwrap();
        let xml = std::fs::read_to_string(d.join("Sources/$OEM$/$$/Panther/unattend.xml")).unwrap();
        assert!(xml.contains("fa-IR"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pe_machine_detection() {
        let mut pe = vec![0u8; 0x100];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3c] = 0x80;
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84] = 0x64;
        pe[0x85] = 0x86;
        assert_eq!(pe_machine(&pe), Some(0x8664));
        pe[0x84] = 0x64;
        pe[0x85] = 0xAA;
        assert_eq!(pe_machine(&pe), Some(0xAA64));
        assert_eq!(pe_machine(b"nope"), None);
        // the embedded wrappers really are what their names say
        assert_eq!(pe_machine(&SETUP_WRAPPER_X64[..4096]), Some(0x8664));
        assert_eq!(pe_machine(&SETUP_WRAPPER_ARM64[..4096]), Some(0xAA64));
        // and so are the validation boot loaders
        assert_eq!(pe_machine(&VALIDATORS[0].1[..4096]), Some(0x8664));
        assert_eq!(pe_machine(&VALIDATORS[2].1[..4096]), Some(0xAA64));
    }

    #[test]
    fn setup_wrapper_only_for_24h2_and_later() {
        let d = tmpdir("wrapper");
        let mut exe = vec![0u8; 0x100];
        exe[0] = b'M';
        exe[1] = b'Z';
        exe[0x3c] = 0x80;
        exe[0x80..0x84].copy_from_slice(b"PE\0\0");
        exe[0x84] = 0x64;
        exe[0x85] = 0x86;
        std::fs::write(d.join("Setup.exe"), &exe).unwrap();
        let mut m = HashSet::new();
        apply_setup_wrapper(&d, Some(22631), &mut m).unwrap();
        assert!(d.join("Setup.exe").exists() && !d.join("setup.dll").exists());
        apply_setup_wrapper(&d, None, &mut m).unwrap();
        assert!(!d.join("setup.dll").exists());
        apply_setup_wrapper(&d, Some(26100), &mut m).unwrap();
        assert_eq!(std::fs::read(d.join("setup.dll")).unwrap(), exe, "original kept");
        assert_eq!(std::fs::read(d.join("Setup.exe")).unwrap(), SETUP_WRAPPER_X64);
        assert!(m.contains("setup.exe"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn md5sum_file_format() {
        let t = md5sum_text(
            &[("d41d8cd98f00b204e9800998ecf8427e".into(), "a/b.txt".into())],
            255,
        );
        assert_eq!(
            t,
            "# md5sum_totalbytes = 0xff\nd41d8cd98f00b204e9800998ecf8427e  ./a/b.txt\n"
        );
    }

    #[test]
    fn validation_renames_loaders_and_lists_every_other_file() {
        let d = tmpdir("validate");
        fake_windows(&d);
        touch(&d, "EFI/Boot/bootaa64.efi", 6);
        let mut m = HashSet::new();
        let cancel = AtomicBool::new(false);
        apply_validation(&d, &mut m, &cancel).unwrap();
        let boot = d.join("EFI/Boot");
        assert_eq!(std::fs::read(boot.join("bootx64.efi")).unwrap(), VALIDATORS[0].1);
        assert_eq!(std::fs::read(boot.join("bootaa64.efi")).unwrap(), VALIDATORS[2].1);
        assert_eq!(std::fs::read(boot.join("bootx64_original.efi")).unwrap(), vec![7u8; 4]);
        assert!(m.contains("efi/boot/bootx64.efi") && m.contains("efi/boot/bootaa64.efi"));
        let txt = std::fs::read_to_string(d.join("md5sum.txt")).unwrap();
        let total: u64 = 10 + 20 + 5 + 3 + 4 + 1 + 6;
        assert!(txt.starts_with(&format!("# md5sum_totalbytes = 0x{:x}\n", total)), "{}", txt);
        assert!(txt.contains("  ./EFI/Boot/bootx64_original.efi\n"));
        assert!(!txt.contains("./EFI/Boot/bootx64.efi"), "validator itself is not listed");
        assert!(!txt.contains("md5sum.txt"));
        assert_eq!(txt.lines().count(), 1 + 7);
        // every listed hash is the real MD5 of that file
        for line in txt.lines().skip(1) {
            let (h, p) = line.split_once("  ./").unwrap();
            let mut x = crate::md5::Md5::new();
            x.update(&std::fs::read(d.join(p)).unwrap());
            assert_eq!(h, crate::md5::hex(&x.finalize()), "{}", p);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Needs root, a loop device and a prepared source tree; run by hand:
    ///   IGNITE_TEST_DEV=/dev/loopN IGNITE_TEST_SRC=/path/tree IGNITE_TEST_FS=ntfs \
    ///   IGNITE_TEST_SCHEME=mbr IGNITE_TEST_TARGET=both cargo test loop_device -- --ignored --nocapture
    #[test]
    #[ignore]
    fn loop_device_end_to_end() {
        let dev = std::env::var("IGNITE_TEST_DEV").expect("IGNITE_TEST_DEV");
        let src = std::env::var("IGNITE_TEST_SRC").expect("IGNITE_TEST_SRC");
        let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let mut args = s(&["--windows", "--image", &src, "--device", &dev, "--verify"]);
        args.extend(s(&[
            "--scheme", &get("IGNITE_TEST_SCHEME", "gpt"),
            "--target", &get("IGNITE_TEST_TARGET", "uefi"),
            "--fs", &get("IGNITE_TEST_FS", "fat32"),
            "--label", "TESTWIN",
            "--bypass-hw", "--validate", "--no-msa", "--no-telemetry", "--qol",
            "--user", "Ali",
            "--locale", "fa-IR", "--timezone", "Iran Standard Time",
        ]));
        if get("IGNITE_TEST_SILENT", "") == "1" {
            args.extend(s(&["--silent", "--edition", "1"]));
        }
        if get("IGNITE_TEST_SB", "") == "1" {
            args.push("--modern-sb".into());
        }
        let job = Job::from_args(&args).unwrap();
        let size = std::fs::metadata(format!("/sys/class/block/{}/size", dev.trim_start_matches("/dev/")))
            .ok()
            .and_then(|_| std::fs::read_to_string(format!("/sys/class/block/{}/size", dev.trim_start_matches("/dev/"))).ok())
            .and_then(|t| t.trim().parse::<u64>().ok())
            .unwrap()
            * 512;
        let info = disks::Disk {
            path: dev.clone(),
            size,
            model: "loop".into(),
            tran: String::new(),
            kind: "disk".into(),
            removable: true,
        };
        let cancel = AtomicBool::new(false);
        run_windows(&job, &info, &cancel).unwrap();
    }
}
