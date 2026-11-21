//! Builds Windows Boot Configuration Data (BCD) stores from scratch.
//!
//! This is what `bcdboot.exe` writes for a freshly applied Windows image, so
//! that a Windows To Go drive can be prepared without Windows. The object and
//! element layout follows a store written by Windows (the same objects, the
//! same element types and the same value encodings); the file itself is a
//! registry hive, written by [`crate::hive`].
//!
//! Two stores are needed: UEFI (`EFI\Microsoft\Boot\BCD`, boots
//! `winload.efi`) and legacy BIOS (`Boot\BCD`, boots `winload.exe`).

use crate::hive::{self, Key, REG_BINARY, REG_DWORD, REG_MULTI_SZ, REG_SZ};

// Well-known object identifiers.
pub const BOOTMGR: &str = "{9dea862c-5cdd-4e70-acc1-f32b344d4795}";
pub const MEMTEST: &str = "{b2721d73-1db4-4c62-bf78-c548a880142d}";
const EMS: &str = "{0ce4991b-e6b3-4b16-b23c-5e0d9250e5d9}";
const RESUME_SETTINGS: &str = "{1afa9c49-16ab-4a5c-901b-212802da9460}";
const DEBUGGER: &str = "{4636856e-540f-4170-a130-a84776f4c654}";
const BAD_MEMORY: &str = "{5189b25c-5558-4bf2-bca4-289b11bd29e2}";
const LOADER_SETTINGS: &str = "{6efb52bf-1766-41db-a6b3-0ee5eff72bd7}";
const GLOBAL_SETTINGS: &str = "{7ea2e1ac-2e61-4728-aaa3-896d9d0a9f0e}";
const HYPERVISOR: &str = "{7ff607e0-4395-11db-b0de-0800200c9a66}";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Firmware {
    Uefi,
    Bios,
}

/// Where a partition lives, in the terms the boot manager uses.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    /// GPT: disk GUID and partition GUID, in on-disk (mixed endian) order.
    Gpt { disk: [u8; 16], part: [u8; 16] },
    /// MBR: 32 bit disk signature and the partition's offset in bytes.
    Mbr { signature: u32, offset: u64 },
}

impl Part {
    pub fn gpt(disk_guid: &str, part_guid: &str) -> Result<Part, String> {
        Ok(Part::Gpt {
            disk: guid_bytes(disk_guid).ok_or_else(|| format!("bad disk GUID '{}'", disk_guid))?,
            part: guid_bytes(part_guid)
                .ok_or_else(|| format!("bad partition GUID '{}'", part_guid))?,
        })
    }

    /// The binary "device" element for this partition (0x58 bytes).
    pub fn device(&self) -> Vec<u8> {
        let mut d = vec![0u8; 0x58];
        // 0x00..0x10: no device identifier. Then a packet: type 6 (partition),
        // reserved, length 0x48 (counted from the packet start), reserved.
        d[0x10..0x14].copy_from_slice(&6u32.to_le_bytes());
        d[0x18..0x1C].copy_from_slice(&0x48u32.to_le_bytes());
        match self {
            Part::Gpt { disk, part } => {
                d[0x20..0x30].copy_from_slice(part);
                d[0x34..0x38].copy_from_slice(&0u32.to_le_bytes());
                d[0x38..0x48].copy_from_slice(disk);
            }
            Part::Mbr { signature, offset } => {
                d[0x20..0x28].copy_from_slice(&offset.to_le_bytes());
                d[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
                d[0x38..0x3C].copy_from_slice(&signature.to_le_bytes());
            }
        }
        d
    }
}

/// "{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}" (braces optional) to the 16 bytes
/// Windows stores: the first three groups little endian, the rest as written.
pub fn guid_bytes(s: &str) -> Option<[u8; 16]> {
    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = t.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    if parts.len() != 5 || parts.iter().zip(lens).any(|(p, l)| p.len() != l) {
        return None;
    }
    let mut hex = Vec::new();
    for p in &parts {
        for i in 0..p.len() / 2 {
            hex.push(u8::from_str_radix(&p[i * 2..i * 2 + 2], 16).ok()?);
        }
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    out[4..6].copy_from_slice(&[hex[5], hex[4]]);
    out[6..8].copy_from_slice(&[hex[7], hex[6]]);
    out[8..16].copy_from_slice(&hex[8..16]);
    Some(out)
}

/// A random (version 4) GUID, in braces.
pub fn new_guid() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !ok {
        // Extremely unlikely; fall back to the clock so we still get a usable id.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        b = n.to_le_bytes();
    }
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    format!(
        "{{{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

pub struct Store {
    pub firmware: Firmware,
    /// The partition that holds the boot manager and this BCD (the ESP).
    pub system: Part,
    /// The partition that holds `\Windows`.
    pub os: Part,
    /// Boot menu language, e.g. "en-US".
    pub locale: String,
    /// Name shown for the Windows entry.
    pub description: String,
    pub loader_guid: String,
    pub resume_guid: String,
    /// `bcdedit /set {default} recoveryenabled no`
    pub recovery_enabled: bool,
}

fn int(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

struct Obj<'a> {
    elements: &'a mut Key,
}

impl Obj<'_> {
    fn raw(&mut self, id: u32, kind: u32, data: Vec<u8>) {
        self.elements
            .key(&format!("{:08x}", id))
            .set("Element", kind, data);
    }
    fn string(&mut self, id: u32, s: &str) {
        self.raw(id, REG_SZ, hive::utf16z(s));
    }
    fn guid(&mut self, id: u32, g: &str) {
        self.raw(id, REG_SZ, hive::utf16z(g));
    }
    fn guids(&mut self, id: u32, g: &[&str]) {
        self.raw(id, REG_MULTI_SZ, hive::multi_sz(g));
    }
    fn int(&mut self, id: u32, v: u64) {
        self.raw(id, REG_BINARY, int(v));
    }
    fn boolean(&mut self, id: u32, v: bool) {
        self.raw(id, REG_BINARY, vec![v as u8]);
    }
    fn device(&mut self, id: u32, p: &Part) {
        self.raw(id, REG_BINARY, p.device());
    }
}

// Element types (class | format | id).
const E_DEVICE: u32 = 0x1100_0001;
const E_PATH: u32 = 0x1200_0002;
const E_DESCRIPTION: u32 = 0x1200_0004;
const E_LOCALE: u32 = 0x1200_0005;
const E_INHERIT: u32 = 0x1400_0006;
const E_RECOVERY: u32 = 0x1600_0009;
const E_BAD_MEMORY_ACCESS: u32 = 0x1600_000b;
const E_EMS: u32 = 0x1600_0020;
const E_ISOLATED: u32 = 0x1600_0060;
const E_ALLOWED_IN_MEMORY: u32 = 0x1700_0077;
const E_DEBUGGER_TYPE: u32 = 0x1500_0011;
const E_OS_DEVICE: u32 = 0x2100_0001;
const E_SYSTEM_ROOT: u32 = 0x2200_0002;
const E_RESUME_OBJECT: u32 = 0x2300_0003;
const E_BM_RESUME: u32 = 0x2300_0006;
const E_BM_ORDER: u32 = 0x2400_0001;
const E_BM_TOOLS_ORDER: u32 = 0x2400_0010;
const E_BM_TIMEOUT: u32 = 0x2500_0004;
const E_NX: u32 = 0x2500_0020;
const E_BOOT_MENU_POLICY: u32 = 0x2500_0008;
const E_LOADER_MENU_POLICY: u32 = 0x2500_00c2;
const E_HV_DEBUG_TYPE: u32 = 0x2500_00f3;
const E_HV_DEBUG_PORT: u32 = 0x2500_00f4;
const E_HV_DEBUG_BAUD: u32 = 0x2500_00f5;
const E_BIOS_ONLY_26000006: u32 = 0x2600_0006;

fn object(objects: &mut Key, guid: &str, kind: u32) -> Key {
    let _ = objects;
    let mut o = Key::new(guid);
    o.key("Description")
        .set("Type", REG_DWORD, kind.to_le_bytes().to_vec());
    o.key("Elements");
    o
}

fn add(objects: &mut Key, guid: &str, kind: u32, fill: impl FnOnce(&mut Obj)) {
    let mut o = object(objects, guid, kind);
    {
        let el = o.key("Elements");
        fill(&mut Obj { elements: el });
    }
    objects.children.push(o);
}

/// Builds the whole store and returns the hive file's bytes.
pub fn build(s: &Store) -> Result<Vec<u8>, String> {
    let uefi = s.firmware == Firmware::Uefi;
    let ext = if uefi { "efi" } else { "exe" };
    let mut root = Key::new("NewStoreRoot");
    {
        let d = root.key("Description");
        d.set("KeyName", REG_SZ, hive::utf16z("BCD00000000"));
        d.set("System", REG_DWORD, 1u32.to_le_bytes().to_vec());
    }
    let objects = root.key("Objects");

    // Settings groups the other objects inherit from.
    add(objects, EMS, 0x2010_0000, |o| o.boolean(E_EMS, false));
    add(objects, RESUME_SETTINGS, 0x2020_0004, |o| {
        o.guids(E_INHERIT, &[GLOBAL_SETTINGS])
    });
    add(objects, DEBUGGER, 0x2010_0000, |o| o.int(E_DEBUGGER_TYPE, 4));
    add(objects, BAD_MEMORY, 0x2010_0000, |_| {});
    add(objects, LOADER_SETTINGS, 0x2020_0003, |o| {
        o.guids(E_INHERIT, &[GLOBAL_SETTINGS, HYPERVISOR])
    });
    add(objects, GLOBAL_SETTINGS, 0x2010_0000, |o| {
        o.guids(E_INHERIT, &[DEBUGGER, EMS, BAD_MEMORY])
    });
    add(objects, HYPERVISOR, 0x2020_0003, |o| {
        o.int(E_HV_DEBUG_TYPE, 0);
        o.int(E_HV_DEBUG_PORT, 1);
        o.int(E_HV_DEBUG_BAUD, 115_200);
    });

    // Windows Boot Manager
    add(objects, BOOTMGR, 0x1010_0002, |o| {
        o.device(E_DEVICE, &s.system);
        if uefi {
            o.string(E_PATH, "\\EFI\\Microsoft\\Boot\\bootmgfw.efi");
        }
        o.string(E_DESCRIPTION, "Windows Boot Manager");
        o.string(E_LOCALE, &s.locale);
        o.guids(E_INHERIT, &[GLOBAL_SETTINGS]);
        o.guid(E_RESUME_OBJECT, &s.loader_guid);
        o.guid(E_BM_RESUME, &s.resume_guid);
        o.guids(E_BM_ORDER, &[&s.loader_guid]);
        o.guids(E_BM_TOOLS_ORDER, &[MEMTEST]);
        o.int(E_BM_TIMEOUT, 30);
    });

    // Windows Memory Diagnostic
    add(objects, MEMTEST, 0x1020_0005, |o| {
        o.device(E_DEVICE, &s.system);
        o.string(
            E_PATH,
            if uefi {
                "\\EFI\\Microsoft\\Boot\\memtest.efi"
            } else {
                "\\Boot\\memtest.exe"
            },
        );
        o.string(E_DESCRIPTION, "Windows Memory Diagnostic");
        o.string(E_LOCALE, &s.locale);
        o.guids(E_INHERIT, &[GLOBAL_SETTINGS]);
        o.boolean(E_BAD_MEMORY_ACCESS, true);
    });

    // Windows Resume Application
    add(objects, &s.resume_guid, 0x1020_0004, |o| {
        o.device(E_DEVICE, &s.os);
        o.string(E_PATH, &format!("\\Windows\\system32\\winresume.{}", ext));
        o.string(E_DESCRIPTION, "Windows Resume Application");
        o.string(E_LOCALE, &s.locale);
        o.guids(E_INHERIT, &[RESUME_SETTINGS]);
        if uefi {
            o.boolean(E_ISOLATED, true);
        }
        o.raw(E_ALLOWED_IN_MEMORY, REG_BINARY, int(0x1500_0075));
        o.device(E_OS_DEVICE, &s.os);
        o.string(E_SYSTEM_ROOT, "\\hiberfil.sys");
        o.int(E_BOOT_MENU_POLICY, 1);
        if !uefi {
            o.boolean(E_BIOS_ONLY_26000006, false);
        }
    });

    // Windows Boot Loader
    add(objects, &s.loader_guid, 0x1020_0003, |o| {
        o.device(E_DEVICE, &s.os);
        o.string(E_PATH, &format!("\\Windows\\system32\\winload.{}", ext));
        o.string(E_DESCRIPTION, &s.description);
        o.string(E_LOCALE, &s.locale);
        o.guids(E_INHERIT, &[LOADER_SETTINGS]);
        if uefi {
            o.boolean(E_ISOLATED, true);
        }
        o.raw(E_ALLOWED_IN_MEMORY, REG_BINARY, int(0x1500_0075));
        o.device(E_OS_DEVICE, &s.os);
        o.string(E_SYSTEM_ROOT, "\\Windows");
        o.guid(E_RESUME_OBJECT, &s.resume_guid);
        o.int(E_NX, 0);
        o.int(E_LOADER_MENU_POLICY, 1);
        if !s.recovery_enabled {
            o.boolean(E_RECOVERY, false);
        }
    });

    hive::write(&root, "BCD")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(fw: Firmware, os: Part, sys: Part) -> Store {
        Store {
            firmware: fw,
            system: sys,
            os,
            locale: "en-US".into(),
            description: "Windows 11".into(),
            loader_guid: "{11111111-2222-3333-4444-555555555555}".into(),
            resume_guid: "{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}".into(),
            recovery_enabled: false,
        }
    }

    fn gpt() -> (Part, Part) {
        (
            Part::gpt(
                "533fc85c-e6b6-4bd4-b5cd-4badc4b98d06",
                "8cd00792-e072-44aa-babe-ec71c6e27205",
            )
            .unwrap(),
            Part::gpt(
                "533fc85c-e6b6-4bd4-b5cd-4badc4b98d06",
                "d218db09-4505-4f72-b670-0683ee7d8036",
            )
            .unwrap(),
        )
    }

    #[test]
    fn guid_encoding_is_mixed_endian() {
        let b = guid_bytes("{00112233-4455-6677-8899-aabbccddeeff}").unwrap();
        assert_eq!(
            b,
            [0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        assert!(guid_bytes("not-a-guid").is_none());
        assert!(guid_bytes("{00112233-4455-6677-8899-aabbccddeef}").is_none());
    }

    #[test]
    fn new_guids_look_right_and_differ() {
        let a = new_guid();
        let b = new_guid();
        assert_ne!(a, b);
        assert!(guid_bytes(&a).is_some());
        assert_eq!(a.as_bytes()[15], b'4'); // version nibble
    }

    #[test]
    fn gpt_device_layout() {
        let (esp, _) = gpt();
        let d = esp.device();
        assert_eq!(d.len(), 0x58);
        assert_eq!(&d[0x10..0x14], &[6, 0, 0, 0]);
        assert_eq!(d[0x18], 0x48);
        // partition GUID 8cd00792-... starts with 92 07 d0 8c on disk
        assert_eq!(&d[0x20..0x24], &[0x92, 0x07, 0xd0, 0x8c]);
        assert_eq!(&d[0x34..0x38], &[0, 0, 0, 0]);
        // disk GUID 533fc85c-... starts with 5c c8 3f 53
        assert_eq!(&d[0x38..0x3C], &[0x5c, 0xc8, 0x3f, 0x53]);
    }

    #[test]
    fn mbr_device_layout() {
        let p = Part::Mbr {
            signature: 0x1234_5678,
            offset: 1_048_576,
        };
        let d = p.device();
        assert_eq!(&d[0x20..0x28], &1_048_576u64.to_le_bytes());
        assert_eq!(&d[0x34..0x38], &[1, 0, 0, 0]);
        assert_eq!(&d[0x38..0x3C], &0x1234_5678u32.to_le_bytes());
        assert!(d[0x3C..0x48].iter().all(|b| *b == 0));
    }

    #[test]
    fn uefi_store_has_the_expected_objects() {
        let (esp, os) = gpt();
        let s = sample(Firmware::Uefi, os, esp);
        let bytes = build(&s).unwrap();
        let root = hive::read(&bytes).unwrap();
        let objs = root.find("Objects").unwrap();
        assert_eq!(objs.children.len(), 11);
        let ldr = objs.find(&s.loader_guid).unwrap();
        let ty = &ldr.find("Description").unwrap().value("Type").unwrap().data;
        assert_eq!(ty, &0x1020_0003u32.to_le_bytes());
        let el = ldr.find("Elements").unwrap();
        let path = &el.find("12000002").unwrap().value("Element").unwrap().data;
        assert_eq!(path, &hive::utf16z("\\Windows\\system32\\winload.efi"));
        assert_eq!(
            el.find("11000001").unwrap().value("Element").unwrap().data,
            s.os.device()
        );
        // recovery disabled
        assert_eq!(
            el.find("16000009").unwrap().value("Element").unwrap().data,
            vec![0]
        );
        let bm = objs.find(BOOTMGR).unwrap().find("Elements").unwrap();
        assert!(bm.find("12000002").is_some());
        assert_eq!(
            bm.find("24000001").unwrap().value("Element").unwrap().data,
            hive::multi_sz(&[&s.loader_guid])
        );
    }

    #[test]
    fn bios_store_uses_winload_exe_and_has_no_bootmgr_path() {
        let os = Part::Mbr {
            signature: 0xabcd_0001,
            offset: 300 << 20,
        };
        let sys = Part::Mbr {
            signature: 0xabcd_0001,
            offset: 1 << 20,
        };
        let s = sample(Firmware::Bios, os, sys);
        let root = hive::read(&build(&s).unwrap()).unwrap();
        let objs = root.find("Objects").unwrap();
        let ldr = objs.find(&s.loader_guid).unwrap().find("Elements").unwrap();
        assert_eq!(
            ldr.find("12000002").unwrap().value("Element").unwrap().data,
            hive::utf16z("\\Windows\\system32\\winload.exe")
        );
        assert!(ldr.find("16000060").is_none());
        let bm = objs.find(BOOTMGR).unwrap().find("Elements").unwrap();
        assert!(bm.find("12000002").is_none());
    }

    /// Cross-check with an independent hive parser (hivex), when installed.
    #[test]
    fn hivex_can_read_the_store() {
        let have = std::process::Command::new("hivexsh")
            .arg("--help")
            .output()
            .is_ok();
        if !have {
            eprintln!("hivexsh not installed; skipping");
            return;
        }
        let (esp, os) = gpt();
        let s = sample(Firmware::Uefi, os, esp);
        let dir = std::env::temp_dir().join(format!("ignite-bcd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("BCD");
        std::fs::write(&f, build(&s).unwrap()).unwrap();
        let out = std::process::Command::new("hivexregedit")
            .arg("--export")
            .arg(&f)
            .arg("\\")
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(out) = out else {
            eprintln!("hivexregedit not installed; skipping");
            return;
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("winload.efi") || text.contains("77,00,69,00,6e,00,6c,00"));
        assert!(text.contains(&s.loader_guid));
        assert!(text.contains("\"Type\"=dword:10200003"));
    }
}

#[cfg(test)]
mod dump {
    /// `IGNITE_BCD_OUT=/path cargo test dump_store -- --ignored` writes a sample store.
    #[test]
    #[ignore]
    fn dump_store() {
        let Ok(p) = std::env::var("IGNITE_BCD_OUT") else { return };
        let (d, p2) = (
            super::Part::gpt("533fc85c-e6b6-4bd4-b5cd-4badc4b98d06", "8cd00792-e072-44aa-babe-ec71c6e27205").unwrap(),
            super::Part::gpt("533fc85c-e6b6-4bd4-b5cd-4badc4b98d06", "d218db09-4505-4f72-b670-0683ee7d8036").unwrap(),
        );
        let s = super::Store {
            firmware: super::Firmware::Uefi,
            system: d,
            os: p2,
            locale: "en-US".into(),
            description: "Windows 11".into(),
            loader_guid: "{11111111-2222-3333-4444-555555555555}".into(),
            resume_guid: "{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}".into(),
            recovery_enabled: true,
        };
        std::fs::write(p, super::build(&s).unwrap()).unwrap();
    }
}
