//! Disk discovery through `lsblk` (no extra crates needed).

use std::collections::HashMap;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub path: String,
    pub size: u64,
    pub model: String,
    pub tran: String,
    pub kind: String,
    pub removable: bool,
}

impl Disk {
    pub fn label(&self) -> String {
        let model = if self.model.trim().is_empty() {
            "Unknown device"
        } else {
            self.model.trim()
        };
        let tran = if self.tran.is_empty() {
            String::new()
        } else {
            format!(" [{}]", self.tran)
        };
        format!(
            "{}  -  {}  -  {}{}",
            self.path,
            human_size(self.size),
            model,
            tran
        )
    }
}

/// Used by the drive picker in the UI (iced's pick_list needs `Display`).
impl std::fmt::Display for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

/// lsblk escapes special characters as \xNN in -P / -r output.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let Ok(h) = std::str::from_utf8(&b[i + 2..i + 4]) {
                if let Ok(v) = u8::from_str_radix(h, 16) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse one line of `lsblk -P` output: KEY="value" KEY2="value2" ...
pub fn parse_pairs(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut rest = line.trim();
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim().to_string();
        let after = &rest[eq + 2..];
        match after.find('"') {
            Some(end) => {
                map.insert(key, unescape(&after[..end]));
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    map
}

fn to_disk(m: &HashMap<String, String>) -> Option<Disk> {
    let get = |k: &str| m.get(k).cloned().unwrap_or_default();
    let path = get("PATH");
    if path.is_empty() {
        return None;
    }
    let tran = get("TRAN");
    let removable = get("RM") == "1" || get("HOTPLUG") == "1" || tran == "usb";
    Some(Disk {
        path,
        size: get("SIZE").parse().unwrap_or(0),
        model: get("MODEL"),
        tran,
        kind: get("TYPE"),
        removable,
    })
}

fn lsblk_disks(extra: &[&str]) -> Vec<Disk> {
    let out = Command::new("lsblk")
        .args([
            "-d",
            "-b",
            "-P",
            "-o",
            "PATH,SIZE,MODEL,TRAN,RM,HOTPLUG,TYPE",
        ])
        .args(extra)
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| to_disk(&parse_pairs(l)))
        .collect()
}

/// List whole disks. Unless `all` is set, only removable / USB disks are returned.
pub fn list_disks(all: bool) -> Vec<Disk> {
    lsblk_disks(&[])
        .into_iter()
        .filter(|d| d.kind == "disk")
        .filter(|d| !d.path.contains("zram"))
        .filter(|d| all || d.removable)
        .filter(|d| d.size > 0)
        .collect()
}

pub fn disk_info(path: &str) -> Option<Disk> {
    lsblk_disks(&[path]).into_iter().next()
}

fn lsblk_column(col: &str, path: &str) -> Vec<String> {
    let out = Command::new("lsblk")
        .args(["-n", "-r", "-o", col, path])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(unescape)
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// All mount points of the disk and its children (partitions, LVM, ...).
pub fn mountpoints(path: &str) -> Vec<String> {
    lsblk_column("MOUNTPOINT", path)
}

/// The disk itself plus all child device paths.
pub fn device_paths(path: &str) -> Vec<String> {
    lsblk_column("PATH", path)
}

/// Mount points that must never be overwritten.
pub fn is_system_mount(mp: &str) -> bool {
    mp == "/"
        || ["/boot", "/home", "/usr", "/var", "/etc", "/opt", "/srv"]
            .iter()
            .any(|p| mp == *p || mp.starts_with(&format!("{}/", p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsblk_pairs() {
        let m = parse_pairs(
            r#"PATH="/dev/sdb" SIZE="16008609792" MODEL="Ultra\x20Fit" TRAN="usb" RM="1" HOTPLUG="1" TYPE="disk""#,
        );
        let d = to_disk(&m).unwrap();
        assert_eq!(d.path, "/dev/sdb");
        assert_eq!(d.model, "Ultra Fit");
        assert!(d.removable);
        assert_eq!(d.size, 16008609792);
    }

    #[test]
    fn system_mounts() {
        assert!(is_system_mount("/"));
        assert!(is_system_mount("/boot/efi"));
        assert!(is_system_mount("/home"));
        assert!(!is_system_mount("/run/media/user/USB"));
    }
}
