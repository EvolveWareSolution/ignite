//! Privileged worker: runs as root (through pkexec) and does the real work.
//!
//! Invoked as:  ignite --worker [--restore] --device DEV [--image FILE]
//!                           [--verify] [--any-disk] [--sha256 HEX] [--eject]
//!
//! Protocol (one line per message on stdout):
//!   STAGE <text>
//!   PROGRESS <hash|write|verify> <done_bytes> <total_bytes>
//!   DONE
//!   ERROR <text>
//! The parent can send "cancel\n" on stdin (or simply close stdin) to abort.

use crate::winopts::{self, Regional, WinOptions};
use crate::{disks, sha256, winworker};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const BLOCK: usize = 4 * 1024 * 1024;
pub(crate) const SYNC_EVERY: u64 = 32 * 1024 * 1024;
pub(crate) const REPORT_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
pub struct Job {
    pub restore: bool,
    pub iso: String,
    pub dev: String,
    pub verify: bool,
    pub any: bool,
    pub sha256: Option<String>,
    pub eject: bool,
    /// Windows installer mode (`--windows ...`): extract the ISO instead of copying it raw.
    pub windows: bool,
    pub win: WinOptions,
    pub regional: Option<Regional>,
}

impl Default for Job {
    fn default() -> Job {
        Job {
            restore: false,
            iso: String::new(),
            dev: String::new(),
            verify: false,
            any: false,
            sha256: None,
            eject: false,
            windows: false,
            win: winopts::all_off(),
            regional: None,
        }
    }
}

impl Job {
    pub fn from_args(args: &[String]) -> Result<Job, String> {
        let mut j = Job::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--windows" => j.windows = true,
                "--restore" => j.restore = true,
                "--verify" => j.verify = true,
                "--any-disk" => j.any = true,
                "--eject" => j.eject = true,
                "--image" => j.iso = it.next().ok_or("--image needs a value")?.clone(),
                "--device" => j.dev = it.next().ok_or("--device needs a value")?.clone(),
                "--sha256" => {
                    let h = it.next().ok_or("--sha256 needs a value")?.to_lowercase();
                    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err("--sha256 must be 64 hex characters".into());
                    }
                    j.sha256 = Some(h);
                }
                other => {
                    if !winopts::parse_arg(&mut j.win, &mut j.regional, other, &mut it)? {
                        return Err(format!("unknown option {}", other));
                    }
                }
            }
        }
        if j.dev.is_empty() {
            return Err("--device is required".into());
        }
        if !j.restore && j.iso.is_empty() {
            return Err("--image is required".into());
        }
        if j.windows && j.restore {
            return Err("--windows cannot be combined with --restore".into());
        }
        Ok(j)
    }
}

pub(crate) fn say(msg: &str) {
    println!("{}", msg.replace('\n', " "));
    let _ = std::io::stdout().flush();
}

pub(crate) fn stage(msg: &str) {
    say(&format!("STAGE {}", msg));
}

pub(crate) fn progress(phase: &str, done: u64, total: u64) {
    say(&format!("PROGRESS {} {} {}", phase, done, total));
}

/// Fill `buf` as much as possible; returns the number of bytes read (0 = EOF).
pub(crate) fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

pub(crate) fn run_cmd(cmd: &str, args: &[&str], package: &str) -> Result<(), String> {
    match Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "{} failed: {}",
            cmd,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "'{}' is not installed (sudo dnf install {})",
            cmd, package
        )),
        Err(e) => Err(format!("cannot run {}: {}", cmd, e)),
    }
}

pub fn run(args: &[String]) -> i32 {
    let job = match Job::from_args(args) {
        Ok(j) => j,
        Err(e) => {
            say(&format!("ERROR {}", e));
            return 2;
        }
    };

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = cancel.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        c.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {
                        if line.trim() == "cancel" {
                            c.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        });
    }

    let result = if job.restore {
        restore(&job, &cancel)
    } else if job.windows {
        winworker::write_windows(&job, &cancel)
    } else {
        write_image(&job, &cancel)
    };
    let result = result.and_then(|_| {
        if job.eject {
            eject(&job.dev);
        }
        Ok(())
    });

    match result {
        Ok(()) => {
            say("DONE");
            0
        }
        Err(e) => {
            say(&format!("ERROR {}", e));
            1
        }
    }
}

// ---------------------------------------------------------------- helpers

/// Re-validate the target as root. Never trust the GUI.
pub(crate) fn check_target(dev: &str, any_disk: bool) -> Result<disks::Disk, String> {
    if !dev.starts_with("/dev/") {
        return Err(format!("{} is not a /dev path", dev));
    }
    let meta = std::fs::metadata(dev).map_err(|e| format!("cannot stat {}: {}", dev, e))?;
    if !meta.file_type().is_block_device() {
        return Err(format!("{} is not a block device", dev));
    }
    let info = disks::disk_info(dev).ok_or_else(|| format!("lsblk cannot describe {}", dev))?;
    if info.kind != "disk" {
        return Err(format!(
            "{} is a '{}', not a whole disk (do not pick a partition)",
            dev, info.kind
        ));
    }
    if !info.removable && !any_disk {
        return Err(format!(
            "{} does not look like a removable/USB disk; refusing",
            dev
        ));
    }
    for mp in disks::mountpoints(dev) {
        if disks::is_system_mount(&mp) {
            return Err(format!(
                "{} holds the mounted system path {}; refusing",
                dev, mp
            ));
        }
    }
    Ok(info)
}

pub(crate) fn unmount_all(dev: &str) -> Result<(), String> {
    stage("Unmounting partitions");
    for p in disks::device_paths(dev) {
        // udisks first (cleans up /run/media/...), plain umount as fallback.
        let _ = Command::new("udisksctl")
            .args(["unmount", "--no-user-interaction", "-b", &p])
            .output();
        let _ = Command::new("umount").arg(&p).output();
    }
    if !disks::mountpoints(dev).is_empty() {
        return Err("could not unmount all partitions of the device".into());
    }
    Ok(())
}

fn eject(dev: &str) {
    stage("Powering off the drive (safe to remove)");
    let _ = Command::new("sync").output();
    match Command::new("udisksctl")
        .args(["power-off", "--no-user-interaction", "-b", dev])
        .output()
    {
        Ok(o) if o.status.success() => {}
        _ => stage("Could not power off automatically; data is flushed, you may unplug the drive"),
    }
}

pub(crate) fn hash_file(path: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("cannot open image: {}", e))?;
    let total = f.metadata().map_err(|e| e.to_string())?.len();
    let mut buf = vec![0u8; BLOCK];
    let mut h = sha256::Sha256::new();
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled by user".into());
        }
        let n = read_full(&mut f, &mut buf).map_err(|e| format!("read error: {}", e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
        if last.elapsed() >= REPORT_EVERY {
            progress("hash", done, total);
            last = Instant::now();
        }
    }
    progress("hash", total, total);
    Ok(sha256::hex(&h.finalize()))
}

// ---------------------------------------------------------------- write

fn write_image(job: &Job, cancel: &AtomicBool) -> Result<(), String> {
    let dev = job.dev.as_str();
    let info = check_target(dev, job.any)?;

    let mut src = File::open(&job.iso).map_err(|e| format!("cannot open image: {}", e))?;
    let total = src
        .metadata()
        .map_err(|e| format!("cannot stat image: {}", e))?
        .len();
    if total == 0 {
        return Err("image file is empty".into());
    }
    if info.size > 0 && total > info.size {
        return Err(format!(
            "image ({}) is larger than the device ({})",
            disks::human_size(total),
            disks::human_size(info.size)
        ));
    }

    // Checksum BEFORE anything destructive happens.
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

    unmount_all(dev)?;

    stage(&format!("Writing {} to {}", disks::human_size(total), dev));
    let mut dst = OpenOptions::new()
        .write(true)
        .open(dev)
        .map_err(|e| format!("cannot open {} for writing: {}", dev, e))?;

    let mut buf = vec![0u8; BLOCK];
    let mut written: u64 = 0;
    let mut since_sync: u64 = 0;
    let mut last = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled by user".into());
        }
        let n = read_full(&mut src, &mut buf).map_err(|e| format!("read error: {}", e))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .map_err(|e| format!("write error at byte {}: {}", written, e))?;
        written += n as u64;
        since_sync += n as u64;
        if since_sync >= SYNC_EVERY {
            dst.sync_data()
                .map_err(|e| format!("sync error at byte {}: {}", written, e))?;
            since_sync = 0;
        }
        if last.elapsed() >= REPORT_EVERY {
            progress("write", written, total);
            last = Instant::now();
        }
    }
    stage("Flushing caches to the device");
    dst.sync_all()
        .map_err(|e| format!("final sync failed: {}", e))?;
    drop(dst);
    progress("write", total, total);

    if job.verify {
        stage("Verifying written data");
        // Compare against the device, not the page cache.
        let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
        let mut src = File::open(&job.iso).map_err(|e| format!("cannot reopen image: {}", e))?;
        let mut dev_file = File::open(dev).map_err(|e| format!("cannot reopen {}: {}", dev, e))?;
        let mut a = vec![0u8; BLOCK];
        let mut b = vec![0u8; BLOCK];
        let mut done: u64 = 0;
        let mut last = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("cancelled by user".into());
            }
            let n = read_full(&mut src, &mut a).map_err(|e| format!("read error: {}", e))?;
            if n == 0 {
                break;
            }
            dev_file
                .read_exact(&mut b[..n])
                .map_err(|e| format!("device read error at byte {}: {}", done, e))?;
            if a[..n] != b[..n] {
                let off = a[..n]
                    .iter()
                    .zip(&b[..n])
                    .position(|(x, y)| x != y)
                    .unwrap_or(0);
                return Err(format!(
                    "verification FAILED: data differs at byte {}",
                    done + off as u64
                ));
            }
            done += n as u64;
            if last.elapsed() >= REPORT_EVERY {
                progress("verify", done, total);
                last = Instant::now();
            }
        }
        progress("verify", total, total);
    }

    let _ = Command::new("udevadm").arg("settle").output();
    Ok(())
}

// ---------------------------------------------------------------- restore

pub(crate) fn partition_path(dev: &str, n: u32) -> String {
    // /dev/sdb -> /dev/sdb1 ; /dev/mmcblk0 or /dev/nvme0n1 -> ...p1
    if dev.chars().last().map_or(false, |c| c.is_ascii_digit()) {
        format!("{}p{}", dev, n)
    } else {
        format!("{}{}", dev, n)
    }
}

/// Turn the drive back into a normal single-partition FAT32 stick.
fn restore(job: &Job, cancel: &AtomicBool) -> Result<(), String> {
    let dev = job.dev.as_str();
    check_target(dev, job.any)?;
    unmount_all(dev)?;

    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled by user".into());
    }

    stage("Erasing old signatures");
    run_cmd("wipefs", &["-a", dev], "util-linux")?;
    {
        // Clear the first MiB too (old boot code / hybrid ISO leftovers).
        let mut f = OpenOptions::new()
            .write(true)
            .open(dev)
            .map_err(|e| format!("cannot open {} for writing: {}", dev, e))?;
        f.write_all(&vec![0u8; 1024 * 1024])
            .map_err(|e| format!("write error: {}", e))?;
        f.sync_all().map_err(|e| format!("sync failed: {}", e))?;
    }

    stage("Creating partition table (MBR) with one FAT32 partition");
    run_cmd(
        "parted",
        &[
            "-s", dev, "mklabel", "msdos", "mkpart", "primary", "fat32", "1MiB", "100%",
        ],
        "parted",
    )?;
    let _ = Command::new("partprobe").arg(dev).output();
    let _ = Command::new("udevadm").arg("settle").output();

    let part = partition_path(dev, 1);
    let start = Instant::now();
    while !Path::new(&part).exists() {
        if start.elapsed() > Duration::from_secs(15) {
            return Err(format!("partition {} did not appear", part));
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled by user".into());
    }

    stage("Formatting FAT32");
    run_cmd("mkfs.vfat", &["-F", "32", "-n", "USB", &part], "dosfstools")?;
    let _ = Command::new("sync").output();
    let _ = Command::new("udevadm").arg("settle").output();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_write_job() {
        let j = Job::from_args(&v(&[
            "--image",
            "/tmp/a.iso",
            "--device",
            "/dev/sdb",
            "--verify",
            "--eject",
        ]))
        .unwrap();
        assert!(j.verify && j.eject && !j.any && !j.restore);
        assert_eq!(j.dev, "/dev/sdb");
    }

    #[test]
    fn parses_restore_job_and_rejects_bad_input() {
        assert!(Job::from_args(&v(&["--restore", "--device", "/dev/sdb"])).is_ok());
        assert!(Job::from_args(&v(&["--device", "/dev/sdb"])).is_err());
        assert!(Job::from_args(&v(&["--image", "x", "--device", "d", "--sha256", "abc"])).is_err());
        assert!(Job::from_args(&v(&["--bogus"])).is_err());
    }

    #[test]
    fn partition_names() {
        assert_eq!(partition_path("/dev/sdb", 1), "/dev/sdb1");
        assert_eq!(partition_path("/dev/mmcblk0", 1), "/dev/mmcblk0p1");
        assert_eq!(partition_path("/dev/nvme0n1", 1), "/dev/nvme0n1p1");
    }
}
