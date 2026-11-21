//! A small writer for Windows registry hive files ("regf"), std only.
//!
//! A BCD store is an ordinary registry hive, and the Windows boot manager reads
//! it with its own tiny parser. So the hive must look like the ones Windows
//! writes: version 1.3, `lf` subkey lists sorted case-insensitively, ASCII key
//! names, security (`sk`) records, 4 KiB `hbin` pages and a correct header
//! checksum. This module builds such a file from a tree of keys and values.
//!
//! Only what a BCD needs is supported: no class names, no big-data blocks
//! (values up to 16344 bytes), no volatile keys.

pub const REG_SZ: u32 = 1;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;
pub const REG_MULTI_SZ: u32 = 7;

const NONE: u32 = 0xFFFF_FFFF;
const HBIN: usize = 0x1000;
const MAX_VALUE: usize = 16344;

/// Security descriptors Windows puts on a BCD hive (copied from a hive written
/// by Windows). They differ only in the access mask of the Administrators ACE.
const SD_ROOT: [u8; 116] = [
    0x01, 0x00, 0x04, 0x80, 0x48, 0x00, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x02, 0x00, 0x34, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00,
    0x3f, 0x00, 0x0f, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00,
    0x20, 0x02, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x3f, 0x00, 0x0f, 0x00, 0x01, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x05, 0x12, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x15, 0x00, 0x00, 0x00, 0x30, 0xff, 0xf4, 0xfc, 0x2b, 0x73, 0x06, 0x06, 0x94, 0xe7, 0x1c, 0x02,
    0x01, 0x02, 0x00, 0x00,
];

fn sd_child() -> [u8; 116] {
    let mut sd = SD_ROOT;
    // Administrators ACE access mask: 0x000f003f -> 0x00060019
    sd[32] = 0x19;
    sd[33] = 0x00;
    sd[34] = 0x06;
    sd[35] = 0x00;
    sd
}

#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub name: String,
    pub kind: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Key {
    pub name: String,
    pub values: Vec<Value>,
    pub children: Vec<Key>,
}

impl Key {
    pub fn new(name: &str) -> Key {
        Key {
            name: name.to_string(),
            ..Key::default()
        }
    }

    /// The child with this name (case-insensitive), created if missing.
    pub fn key(&mut self, name: &str) -> &mut Key {
        let i = match self
            .children
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
        {
            Some(i) => i,
            None => {
                self.children.push(Key::new(name));
                self.children.len() - 1
            }
        };
        &mut self.children[i]
    }

    #[cfg(test)]
    pub fn find(&self, name: &str) -> Option<&Key> {
        self.children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn set(&mut self, name: &str, kind: u32, data: Vec<u8>) {
        if let Some(v) = self.values.iter_mut().find(|v| v.name == name) {
            v.kind = kind;
            v.data = data;
        } else {
            self.values.push(Value {
                name: name.to_string(),
                kind,
                data,
            });
        }
    }

    #[cfg(test)]
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|v| v.name == name)
    }

    fn count(&self) -> usize {
        1 + self.children.iter().map(Key::count).sum::<usize>()
    }
}

/// UTF-16LE with a terminating NUL, as REG_SZ wants.
pub fn utf16z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    v.extend_from_slice(&[0, 0]);
    v
}

/// REG_MULTI_SZ: each string NUL terminated, plus a final empty string.
pub fn multi_sz(items: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    for s in items {
        v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        v.extend_from_slice(&[0, 0]);
    }
    v.extend_from_slice(&[0, 0]);
    v
}

fn upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

fn is_ascii_name(s: &str) -> bool {
    s.is_ascii() && !s.is_empty()
}

/// Builds the file one cell at a time. Offsets are relative to the start of
/// the first hbin (file offset 0x1000), as in the format.
struct Builder {
    data: Vec<u8>,
    /// End (exclusive) of the hbin currently being filled.
    hbin_end: usize,
    hbins: Vec<(usize, usize)>,
    stamp: u64,
}

impl Builder {
    fn new(stamp: u64) -> Builder {
        let mut b = Builder {
            data: Vec::new(),
            hbin_end: 0,
            hbins: Vec::new(),
            stamp,
        };
        b.start_hbin(HBIN);
        b
    }

    fn start_hbin(&mut self, size: usize) {
        let start = self.data.len();
        self.hbins.push((start, size));
        self.data.resize(start + 0x20, 0);
        self.hbin_end = start + size;
    }

    /// Closes the current hbin with a free cell covering what is left.
    fn close_hbin(&mut self) {
        let left = self.hbin_end - self.data.len();
        if left >= 8 {
            let at = self.data.len();
            self.data.resize(self.hbin_end, 0);
            self.data[at..at + 4].copy_from_slice(&(left as i32).to_le_bytes());
        }
    }

    /// Reserves a cell able to hold `len` bytes and returns its offset.
    fn alloc(&mut self, len: usize) -> usize {
        let size = align8(len + 4);
        if self.data.len() + size > self.hbin_end {
            self.close_hbin();
            let need = ((size + 0x20) + HBIN - 1) / HBIN * HBIN;
            self.start_hbin(need.max(HBIN));
        }
        let off = self.data.len();
        self.data.resize(off + size, 0);
        self.data[off..off + 4].copy_from_slice(&(-(size as i32)).to_le_bytes());
        off
    }

    fn body(&mut self, off: usize) -> &mut [u8] {
        let size = (-i32::from_le_bytes(self.data[off..off + 4].try_into().unwrap())) as usize;
        &mut self.data[off + 4..off + size]
    }

    fn cell(&mut self, content: &[u8]) -> usize {
        let off = self.alloc(content.len());
        self.body(off)[..content.len()].copy_from_slice(content);
        off
    }

    fn sk(&mut self, refcount: u32, sd: &[u8]) -> usize {
        let off = self.alloc(20 + sd.len());
        // prev/next are fixed up by the caller once both exist
        let b = self.body(off);
        b[0..2].copy_from_slice(b"sk");
        b[12..16].copy_from_slice(&refcount.to_le_bytes());
        b[16..20].copy_from_slice(&(sd.len() as u32).to_le_bytes());
        b[20..20 + sd.len()].copy_from_slice(sd);
        off
    }

    fn put32(&mut self, off: usize, at: usize, v: u32) {
        self.body(off)[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn vk(&mut self, v: &Value) -> Result<usize, String> {
        if v.data.len() > MAX_VALUE {
            return Err(format!("value {} is too large", v.name));
        }
        if !v.name.is_empty() && !is_ascii_name(&v.name) {
            return Err(format!("value name {} is not ASCII", v.name));
        }
        let inline = v.data.len() <= 4 && !v.data.is_empty();
        let data_off = if inline || v.data.is_empty() {
            0
        } else {
            self.cell(&v.data) as u32
        };
        let off = self.alloc(0x14 + v.name.len());
        let stamp_len = if v.data.is_empty() {
            0
        } else if inline {
            v.data.len() as u32 | 0x8000_0000
        } else {
            v.data.len() as u32
        };
        let inline_word = if inline {
            let mut w = [0u8; 4];
            w[..v.data.len()].copy_from_slice(&v.data);
            u32::from_le_bytes(w)
        } else {
            data_off
        };
        let b = self.body(off);
        b[0..2].copy_from_slice(b"vk");
        b[2..4].copy_from_slice(&(v.name.len() as u16).to_le_bytes());
        b[4..8].copy_from_slice(&stamp_len.to_le_bytes());
        b[8..12].copy_from_slice(&inline_word.to_le_bytes());
        b[12..16].copy_from_slice(&v.kind.to_le_bytes());
        let flags: u16 = if v.name.is_empty() { 0 } else { 1 };
        b[16..18].copy_from_slice(&flags.to_le_bytes());
        b[0x14..0x14 + v.name.len()].copy_from_slice(v.name.as_bytes());
        Ok(off)
    }

    /// Writes a key and everything below it. `nk` is the already reserved
    /// cell of this key.
    #[allow(clippy::too_many_arguments)]
    fn key(
        &mut self,
        k: &Key,
        nk: usize,
        parent: u32,
        sk: usize,
        root: bool,
        sk_child: usize,
    ) -> Result<(), String> {
        if !is_ascii_name(&k.name) {
            return Err(format!("key name '{}' must be ASCII and not empty", k.name));
        }
        // values
        let mut vks = Vec::new();
        for v in &k.values {
            vks.push(self.vk(v)? as u32);
        }
        let values_list = if vks.is_empty() {
            NONE
        } else {
            let off = self.alloc(vks.len() * 4);
            let b = self.body(off);
            for (i, o) in vks.iter().enumerate() {
                b[i * 4..i * 4 + 4].copy_from_slice(&o.to_le_bytes());
            }
            off as u32
        };

        // children, sorted the way Windows searches them
        let mut order: Vec<&Key> = k.children.iter().collect();
        order.sort_by_key(|c| upper(&c.name));
        for w in order.windows(2) {
            if upper(&w[0].name) == upper(&w[1].name) {
                return Err(format!("duplicate key {}", w[0].name));
            }
        }
        let mut entries = Vec::new();
        for c in &order {
            let cnk = self.alloc(0x4C + c.name.len());
            self.key(c, cnk, nk as u32, sk_child, false, sk_child)?;
            entries.push((cnk as u32, hash4(&c.name)));
        }
        let list = if entries.is_empty() {
            NONE
        } else {
            let off = self.alloc(4 + entries.len() * 8);
            let b = self.body(off);
            b[0..2].copy_from_slice(b"lf");
            b[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
            for (i, (o, h)) in entries.iter().enumerate() {
                b[4 + i * 8..8 + i * 8].copy_from_slice(&o.to_le_bytes());
                b[8 + i * 8..12 + i * 8].copy_from_slice(&h.to_le_bytes());
            }
            off as u32
        };

        let max_sub = k.children.iter().map(|c| c.name.len() * 2).max().unwrap_or(0) as u32;
        let max_vname = k.values.iter().map(|v| v.name.len() * 2).max().unwrap_or(0) as u32;
        let max_vdata = k.values.iter().map(|v| v.data.len()).max().unwrap_or(0) as u32;
        let stamp = self.stamp;
        let b = self.body(nk);
        b[0..2].copy_from_slice(b"nk");
        // 0x2c: root entry | no delete | compressed (ASCII) name; 0x20: ordinary
        let flags: u16 = if root { 0x2C } else { 0x20 };
        b[2..4].copy_from_slice(&flags.to_le_bytes());
        b[4..12].copy_from_slice(&stamp.to_le_bytes());
        b[12..16].copy_from_slice(&(if root { 2u32 } else { 0 }).to_le_bytes());
        b[16..20].copy_from_slice(&parent.to_le_bytes());
        b[20..24].copy_from_slice(&(k.children.len() as u32).to_le_bytes());
        b[24..28].copy_from_slice(&0u32.to_le_bytes());
        b[28..32].copy_from_slice(&list.to_le_bytes());
        b[32..36].copy_from_slice(&NONE.to_le_bytes());
        b[36..40].copy_from_slice(&(k.values.len() as u32).to_le_bytes());
        b[40..44].copy_from_slice(&values_list.to_le_bytes());
        b[44..48].copy_from_slice(&(sk as u32).to_le_bytes());
        b[48..52].copy_from_slice(&NONE.to_le_bytes());
        b[52..56].copy_from_slice(&max_sub.to_le_bytes());
        b[56..60].copy_from_slice(&0u32.to_le_bytes());
        b[60..64].copy_from_slice(&max_vname.to_le_bytes());
        b[64..68].copy_from_slice(&max_vdata.to_le_bytes());
        b[68..72].copy_from_slice(&0u32.to_le_bytes());
        b[72..74].copy_from_slice(&(k.name.len() as u16).to_le_bytes());
        b[74..76].copy_from_slice(&0u16.to_le_bytes());
        b[76..76 + k.name.len()].copy_from_slice(k.name.as_bytes());
        Ok(())
    }
}

/// The `lf` hash: the first four characters of the name.
fn hash4(name: &str) -> u32 {
    let mut w = [0u8; 4];
    for (i, c) in name.bytes().take(4).enumerate() {
        w[i] = c;
    }
    u32::from_le_bytes(w)
}

/// Windows FILETIME for "now".
fn filetime_now() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs + 11_644_473_600) * 10_000_000
}

/// Serializes `root` as a registry hive file.
pub fn write(root: &Key, file_name: &str) -> Result<Vec<u8>, String> {
    let stamp = filetime_now();
    let mut b = Builder::new(stamp);

    // Root key first (offset 0x20, like Windows), then the two security cells.
    let root_nk = b.alloc(0x4C + root.name.len());
    let total = root.count();
    let sk_root = b.sk(1, &SD_ROOT);
    let sk_kids = b.sk(total as u32 - 1, &sd_child());
    // circular list of security records
    b.put32(sk_root, 4, sk_kids as u32);
    b.put32(sk_root, 8, sk_kids as u32);
    b.put32(sk_kids, 4, sk_root as u32);
    b.put32(sk_kids, 8, sk_root as u32);

    b.key(root, root_nk, NONE, sk_root, true, sk_kids)?;
    b.close_hbin();

    // hbin headers
    let hbins = b.hbins.clone();
    for (start, size) in &hbins {
        let h = &mut b.data[*start..*start + 0x20];
        h[0..4].copy_from_slice(b"hbin");
        h[4..8].copy_from_slice(&(*start as u32).to_le_bytes());
        h[8..12].copy_from_slice(&(*size as u32).to_le_bytes());
        h[20..28].copy_from_slice(&stamp.to_le_bytes());
    }

    let mut out = vec![0u8; 0x1000];
    out[0..4].copy_from_slice(b"regf");
    out[4..8].copy_from_slice(&1u32.to_le_bytes());
    out[8..12].copy_from_slice(&1u32.to_le_bytes());
    out[12..20].copy_from_slice(&stamp.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes()); // major
    out[24..28].copy_from_slice(&3u32.to_le_bytes()); // minor
    out[28..32].copy_from_slice(&0u32.to_le_bytes()); // type: primary
    out[32..36].copy_from_slice(&1u32.to_le_bytes()); // format: direct memory load
    out[36..40].copy_from_slice(&(root_nk as u32).to_le_bytes());
    out[40..44].copy_from_slice(&(b.data.len() as u32).to_le_bytes());
    out[44..48].copy_from_slice(&1u32.to_le_bytes()); // clustering factor
    for (i, u) in file_name.encode_utf16().take(31).enumerate() {
        out[0x30 + i * 2..0x32 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    let mut sum = 0u32;
    for i in 0..127 {
        sum ^= u32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
    }
    if sum == 0xFFFF_FFFF {
        sum = 0xFFFF_FFFE;
    } else if sum == 0 {
        sum = 1;
    }
    out[0x1FC..0x200].copy_from_slice(&sum.to_le_bytes());
    out.extend_from_slice(&b.data);
    Ok(out)
}

/// Reads a hive back into a tree (used by the tests, which also compare with
/// hivex and with a hive written by Windows).
#[cfg(test)]
pub fn read(d: &[u8]) -> Result<Key, String> {
    fn u32_at(d: &[u8], o: usize) -> Result<u32, String> {
        d.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| format!("read past the end at {:#x}", o))
    }
    fn u16_at(d: &[u8], o: usize) -> Result<u16, String> {
        d.get(o..o + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| format!("read past the end at {:#x}", o))
    }
    // cell body start (after the size field) and body length
    fn cell(d: &[u8], off: u32) -> Result<(usize, usize), String> {
        let at = 0x1000 + off as usize;
        let size = u32_at(d, at)? as i32;
        if size >= 0 {
            return Err(format!("cell {:#x} is free", off));
        }
        let n = (-size) as usize;
        if n % 8 != 0 || at + n > d.len() {
            return Err(format!("cell {:#x} has a bad size {}", off, n));
        }
        Ok((at + 4, n - 4))
    }
    fn key(d: &[u8], off: u32, depth: usize) -> Result<Key, String> {
        if depth > 16 {
            return Err("too deep".into());
        }
        let (at, len) = cell(d, off)?;
        if &d[at..at + 2] != b"nk" {
            return Err(format!("{:#x} is not an nk", off));
        }
        let name_len = u16_at(d, at + 72)? as usize;
        if 76 + name_len > len {
            return Err("key name too long".into());
        }
        let name = String::from_utf8_lossy(&d[at + 76..at + 76 + name_len]).to_string();
        let mut k = Key::new(&name);
        let nvalues = u32_at(d, at + 36)?;
        let vlist = u32_at(d, at + 40)?;
        if nvalues > 0 {
            let (la, ll) = cell(d, vlist)?;
            if (nvalues as usize) * 4 > ll {
                return Err("value list too short".into());
            }
            for i in 0..nvalues as usize {
                let vo = u32_at(d, la + i * 4)?;
                let (va, _) = cell(d, vo)?;
                if &d[va..va + 2] != b"vk" {
                    return Err("not a vk".into());
                }
                let nl = u16_at(d, va + 2)? as usize;
                let dl = u32_at(d, va + 4)?;
                let doff = u32_at(d, va + 8)?;
                let kind = u32_at(d, va + 12)?;
                let vname = String::from_utf8_lossy(&d[va + 0x14..va + 0x14 + nl]).to_string();
                let data = if dl & 0x8000_0000 != 0 {
                    let n = (dl & 0x7FFF_FFFF) as usize;
                    d[va + 8..va + 8 + n].to_vec()
                } else if dl == 0 {
                    Vec::new()
                } else {
                    let (da, dlen) = cell(d, doff)?;
                    if dl as usize > dlen {
                        return Err("value data too short".into());
                    }
                    d[da..da + dl as usize].to_vec()
                };
                k.values.push(Value {
                    name: vname,
                    kind,
                    data,
                });
            }
        }
        let nsub = u32_at(d, at + 20)?;
        let slist = u32_at(d, at + 28)?;
        if nsub > 0 {
            let (la, ll) = cell(d, slist)?;
            if &d[la..la + 2] != b"lf" && &d[la..la + 2] != b"lh" {
                return Err("unsupported subkey list".into());
            }
            let n = u16_at(d, la + 2)? as usize;
            if n != nsub as usize || 4 + n * 8 > ll {
                return Err("bad subkey list".into());
            }
            let mut last = String::new();
            for i in 0..n {
                let c = key(d, u32_at(d, la + 4 + i * 8)?, depth + 1)?;
                let up = upper(&c.name);
                if up <= last {
                    return Err(format!("subkeys not sorted at {}", c.name));
                }
                last = up;
                k.children.push(c);
            }
        }
        Ok(k)
    }

    if d.len() < 0x1020 || &d[0..4] != b"regf" {
        return Err("not a hive".into());
    }
    let mut sum = 0u32;
    for i in 0..127 {
        sum ^= u32_at(d, i * 4)?;
    }
    if sum == 0xFFFF_FFFF {
        sum = 0xFFFF_FFFE;
    } else if sum == 0 {
        sum = 1;
    }
    if sum != u32_at(d, 0x1FC)? {
        return Err("bad header checksum".into());
    }
    let total = u32_at(d, 40)? as usize;
    if 0x1000 + total != d.len() {
        return Err("hbin length does not match the file".into());
    }
    // every hbin must be well formed and the cells must tile it exactly
    let mut pos = 0usize;
    while pos < total {
        let h = 0x1000 + pos;
        if &d[h..h + 4] != b"hbin" || u32_at(d, h + 4)? as usize != pos {
            return Err(format!("bad hbin at {:#x}", pos));
        }
        let size = u32_at(d, h + 8)? as usize;
        if size == 0 || size % HBIN != 0 || pos + size > total {
            return Err(format!("bad hbin size at {:#x}", pos));
        }
        let mut c = pos + 0x20;
        while c < pos + size {
            let n = (u32_at(d, 0x1000 + c)? as i32).unsigned_abs() as usize;
            if n < 8 || n % 8 != 0 || c + n > pos + size {
                return Err(format!("cell at {:#x} breaks the hbin", c));
            }
            c += n;
        }
        pos += size;
    }
    key(d, u32_at(d, 36)?, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Key {
        let mut root = Key::new("BCD00000000");
        let d = root.key("Description");
        d.set("KeyName", REG_SZ, utf16z("BCD00000000"));
        d.set("System", REG_DWORD, 1u32.to_le_bytes().to_vec());
        let o = root.key("Objects");
        for i in 0..40 {
            let g = o.key(&format!("{{{:08x}-0000-0000-0000-000000000000}}", i * 7919));
            g.key("Description")
                .set("Type", REG_DWORD, 0x10200003u32.to_le_bytes().to_vec());
            let e = g.key("Elements");
            e.key("12000004")
                .set("Element", REG_SZ, utf16z("Windows Boot Manager"));
            e.key("14000006")
                .set("Element", REG_MULTI_SZ, multi_sz(&["{a}", "{b}"]));
            e.key("25000004")
                .set("Element", REG_BINARY, vec![0x1e, 0, 0, 0, 0, 0, 0, 0]);
            e.key("16000020").set("Element", REG_BINARY, vec![1]);
        }
        root
    }

    #[test]
    fn round_trip() {
        let k = sample();
        let bytes = write(&k, "BCD").unwrap();
        let back = read(&bytes).unwrap();
        // children come back sorted, so compare after sorting the original
        fn sorted(k: &Key) -> Key {
            let mut c = k.clone();
            c.children = c.children.iter().map(sorted).collect();
            c.children.sort_by_key(|x| upper(&x.name));
            c
        }
        assert_eq!(sorted(&k), back);
    }

    #[test]
    fn file_is_page_aligned_and_checksummed() {
        let bytes = write(&sample(), "BCD").unwrap();
        assert_eq!(bytes.len() % 0x1000, 0);
        assert_eq!(&bytes[0..4], b"regf");
        // read() validates checksum, hbin tiling and cell sizes
        read(&bytes).unwrap();
    }

    #[test]
    fn large_value_gets_its_own_hbin() {
        let mut root = Key::new("R");
        root.key("A").set("Big", REG_BINARY, vec![7u8; 9000]);
        let bytes = write(&root, "x").unwrap();
        let back = read(&bytes).unwrap();
        assert_eq!(back.find("A").unwrap().value("Big").unwrap().data.len(), 9000);
    }

    #[test]
    fn rejects_bad_names() {
        let mut root = Key::new("R");
        root.key("Ünï");
        assert!(write(&root, "x").is_err());
    }

    #[test]
    fn short_values_are_stored_inline() {
        let mut root = Key::new("R");
        root.set("D", REG_DWORD, 0xdeadbeefu32.to_le_bytes().to_vec());
        root.set("B", REG_BINARY, vec![1, 2]);
        let back = read(&write(&root, "x").unwrap()).unwrap();
        assert_eq!(back.value("D").unwrap().data, 0xdeadbeefu32.to_le_bytes());
        assert_eq!(back.value("B").unwrap().data, vec![1, 2]);
    }
}
