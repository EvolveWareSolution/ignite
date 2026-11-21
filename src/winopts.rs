//! Windows mode: the options shared by the window and the privileged worker.
//!
//! Nothing here touches a disk or needs a GUI, so everything can be unit-tested:
//!   - the option types (partition scheme, target system, file system, ...),
//!   - validation / normalisation (volume label, local account name, ...),
//!   - the command-line form used to hand the options to the worker,
//!   - detection of "this user's" regional options (language, time zone).

use std::fmt;

// ------------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Extract the ISO onto a freshly partitioned drive (the usual way to make Windows media).
    #[default]
    Iso,
    /// Raw copy of the image, exactly like the standard Ignite page.
    Dd,
    /// Run Windows from the drive (Windows To Go).
    Wtg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scheme {
    Mbr,
    #[default]
    Gpt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Target {
    #[default]
    Uefi,
    /// Legacy BIOS (or UEFI with the CSM): MBR only.
    Bios,
    /// MBR drive that boots on both.
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Fs {
    #[default]
    Fat32,
    Ntfs,
    Exfat,
}

impl WriteMode {
    pub const ALL: [WriteMode; 3] = [WriteMode::Iso, WriteMode::Wtg, WriteMode::Dd];
}
impl Scheme {
    pub const ALL: [Scheme; 2] = [Scheme::Gpt, Scheme::Mbr];
    /// Target systems that make sense for this partition scheme.
    pub fn targets(self) -> Vec<Target> {
        match self {
            Scheme::Gpt => vec![Target::Uefi],
            Scheme::Mbr => vec![Target::Both, Target::Bios, Target::Uefi],
        }
    }
}
impl Fs {
    pub const ALL: [Fs; 3] = [Fs::Fat32, Fs::Ntfs, Fs::Exfat];
}

impl fmt::Display for WriteMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WriteMode::Iso => "ISO mode (extract the files, recommended)",
            WriteMode::Wtg => "Windows To Go (run Windows from the drive)",
            WriteMode::Dd => "DD mode (raw copy of the image)",
        })
    }
}
impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scheme::Gpt => "GPT",
            Scheme::Mbr => "MBR",
        })
    }
}
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::Uefi => "UEFI (non-CSM)",
            Target::Bios => "BIOS (or UEFI-CSM)",
            Target::Both => "BIOS + UEFI",
        })
    }
}
impl fmt::Display for Fs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Fs::Fat32 => "FAT32",
            Fs::Ntfs => "NTFS",
            Fs::Exfat => "exFAT",
        })
    }
}

impl Scheme {
    fn arg(self) -> &'static str {
        match self {
            Scheme::Mbr => "mbr",
            Scheme::Gpt => "gpt",
        }
    }
    fn parse(s: &str) -> Result<Scheme, String> {
        match s {
            "mbr" => Ok(Scheme::Mbr),
            "gpt" => Ok(Scheme::Gpt),
            _ => Err(format!("unknown partition scheme '{}'", s)),
        }
    }
}
impl Target {
    fn arg(self) -> &'static str {
        match self {
            Target::Uefi => "uefi",
            Target::Bios => "bios",
            Target::Both => "both",
        }
    }
    fn parse(s: &str) -> Result<Target, String> {
        match s {
            "uefi" => Ok(Target::Uefi),
            "bios" => Ok(Target::Bios),
            "both" => Ok(Target::Both),
            _ => Err(format!("unknown target system '{}'", s)),
        }
    }
    pub fn wants_bios(self) -> bool {
        matches!(self, Target::Bios | Target::Both)
    }
    pub fn wants_uefi(self) -> bool {
        matches!(self, Target::Uefi | Target::Both)
    }
}
impl Fs {
    fn arg(self) -> &'static str {
        match self {
            Fs::Fat32 => "fat32",
            Fs::Ntfs => "ntfs",
            Fs::Exfat => "exfat",
        }
    }
    fn parse(s: &str) -> Result<Fs, String> {
        match s {
            "fat32" => Ok(Fs::Fat32),
            "ntfs" => Ok(Fs::Ntfs),
            "exfat" => Ok(Fs::Exfat),
            _ => Err(format!("unknown file system '{}'", s)),
        }
    }
    /// Longest volume label the file system allows.
    pub fn max_label(self) -> usize {
        match self {
            Fs::Fat32 => 11,
            Fs::Ntfs => 32,
            Fs::Exfat => 15,
        }
    }
}

// ----------------------------------------------------------------- options

pub const DEFAULT_LABEL: &str = "IGNITE_WIN";
const USERNAME_INVALID_CHARS: &str = "/\\[]:;|=,+*?<>%@&\"";
/// Names Windows reserves (reserved or localised built-in account names).
const RESERVED_ACCOUNTS: [&str; 15] = [
    "Administrator",
    "Järjestelmänvalvoja",
    "Administrateur",
    "Rendszergazda",
    "Administrador",
    "Администратор",
    "Administratör",
    "Guest",
    "DefaultAccount",
    "WDAGUtilityAccount",
    "HelpAssistant",
    "KRBTGT",
    "Local",
    "NONE",
    "SYSTEM",
];

/// Everything the "Windows" page lets you choose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinOptions {
    pub scheme: Scheme,
    pub target: Target,
    pub fs: Fs,
    pub label: String,

    /// Remove the requirement for 4 GB+ RAM, Secure Boot and TPM 2.0.
    pub bypass_hw: bool,
    /// Remove the requirement for an online Microsoft account.
    pub no_online_account: bool,
    /// Create a local account with this user name.
    pub local_user: bool,
    pub username: String,
    /// Use the same regional options (language, keyboard, time zone) as this user.
    pub regional: bool,
    /// Skip the privacy questions / disable data collection.
    pub no_data_collection: bool,
    /// Disable BitLocker automatic device encryption.
    pub no_bitlocker: bool,
    /// Quality of life: no Teams, Outlook, Copilot, OneDrive and similar nags.
    pub qol: bool,
    /// Fully automatic install on the first disk. DANGEROUS: erases that disk.
    pub silent: bool,
    /// Image index installed by the silent installation (1 = first edition).
    pub edition: u32,
    /// Boot with the "Windows UEFI CA 2023" signed boot loaders.
    pub modern_secure_boot: bool,
    /// UEFI:Validation: the drive checks its own files (MD5) every time it
    /// boots in UEFI mode, before Windows Setup starts.
    pub uefi_validation: bool,
    /// Windows To Go: apply the edition to the drive instead of making installer media.
    pub wtg: bool,
}

impl Default for WinOptions {
    fn default() -> Self {
        WinOptions {
            scheme: Scheme::Gpt,
            target: Target::Uefi,
            fs: Fs::Fat32,
            label: DEFAULT_LABEL.to_string(),
            bypass_hw: true,
            no_online_account: true,
            local_user: false,
            username: String::new(),
            regional: false,
            no_data_collection: true,
            no_bitlocker: false,
            qol: false,
            silent: false,
            edition: 1,
            modern_secure_boot: false,
            uefi_validation: false,
            wtg: false,
        }
    }
}

/// Characters a volume label may not contain on the chosen file system.
fn label_invalid(fs: Fs) -> &'static str {
    match fs {
        Fs::Fat32 => "\"*+,/:;<=>?[\\]|",
        Fs::Ntfs | Fs::Exfat => "\"*/:<>?\\|",
    }
}

pub fn sanitize_label(fs: Fs, label: &str) -> Result<String, String> {
    let t = label.trim();
    if t.is_empty() {
        return Ok(DEFAULT_LABEL.to_string());
    }
    if let Some(c) = t
        .chars()
        .find(|c| c.is_control() || label_invalid(fs).contains(*c))
    {
        return Err(format!(
            "The volume label cannot contain '{}' on {}.",
            c.escape_default(),
            fs
        ));
    }
    if t.chars().count() > fs.max_label() {
        return Err(format!(
            "The volume label can have at most {} characters on {}.",
            fs.max_label(),
            fs
        ));
    }
    // FAT stores labels in upper case; do it here so the user sees the result.
    Ok(if fs == Fs::Fat32 {
        t.to_uppercase()
    } else {
        t.to_string()
    })
}

pub fn validate_username(name: &str) -> Result<String, String> {
    let t = name.trim();
    if t.is_empty() {
        return Err("Enter a name for the local account.".into());
    }
    if t.chars().count() > 20 {
        return Err("The local account name can have at most 20 characters.".into());
    }
    if t.ends_with('.') {
        return Err("The local account name cannot end with a dot.".into());
    }
    if let Some(c) = t
        .chars()
        .find(|c| c.is_control() || USERNAME_INVALID_CHARS.contains(*c))
    {
        return Err(format!(
            "The local account name cannot contain '{}'.",
            c.escape_default()
        ));
    }
    if RESERVED_ACCOUNTS
        .iter()
        .any(|r| r.to_lowercase() == t.to_lowercase())
    {
        return Err(format!("'{}' is a reserved Windows account name.", t));
    }
    Ok(t.to_string())
}

impl WinOptions {
    /// Check the combination of options and return a cleaned-up copy
    /// (trimmed label / user name). Used by the window before it starts a job
    /// and again by the worker, which never trusts the window.
    pub fn normalized(&self) -> Result<WinOptions, String> {
        let mut o = self.clone();
        if o.scheme == Scheme::Gpt && o.target != Target::Uefi {
            return Err("A GPT drive can only boot in UEFI mode.".into());
        }
        if o.wtg {
            o.fs = Fs::Ntfs;
        }
        o.label = sanitize_label(o.fs, &o.label)?;
        if o.local_user {
            o.username = validate_username(&o.username)?;
        } else {
            o.username.clear();
        }
        if o.edition == 0 || o.edition > 99 {
            return Err("The image index must be between 1 and 99.".into());
        }
        if !o.silent && !o.wtg {
            o.edition = 1;
        }
        if o.wtg {
            // Windows To Go: the OS lives on NTFS, next to a FAT32 boot partition;
            // there is no Setup, so the Setup-only options do not apply.
            o.silent = false;
            o.bypass_hw = false;
            o.uefi_validation = false;
        }
        Ok(o)
    }

    /// Does this configuration need a second, small partition after the data
    /// partition for the UEFI:NTFS boot loader? (NTFS and exFAT are not
    /// readable by UEFI firmware, so a tiny FAT partition with a driver is
    /// added.)
    pub fn needs_uefi_ntfs(&self) -> bool {
        self.fs != Fs::Fat32 && self.target.wants_uefi()
    }
}

// -------------------------------------------------- command line (worker)

/// Regional options detected on this computer.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Regional {
    /// BCP-47 language tag, e.g. "fa-IR".
    pub locale: String,
    /// Windows time zone id, e.g. "Iran Standard Time".
    pub timezone: Option<String>,
}

pub fn win_args(o: &WinOptions, regional: Option<&Regional>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--windows".into(),
        "--scheme".into(),
        o.scheme.arg().into(),
        "--target".into(),
        o.target.arg().into(),
        "--fs".into(),
        o.fs.arg().into(),
        "--label".into(),
        o.label.clone(),
    ];
    let mut flag = |on: bool, name: &str| {
        if on {
            a.push(name.to_string());
        }
    };
    flag(o.bypass_hw, "--bypass-hw");
    flag(o.no_online_account, "--no-msa");
    flag(o.no_data_collection, "--no-telemetry");
    flag(o.no_bitlocker, "--no-bitlocker");
    flag(o.qol, "--qol");
    flag(o.silent, "--silent");
    flag(o.modern_secure_boot, "--modern-sb");
    flag(o.uefi_validation, "--validate");
    flag(o.wtg, "--wtg");
    if o.silent || o.wtg {
        a.push("--edition".into());
        a.push(o.edition.to_string());
    }
    if o.local_user {
        a.push("--user".into());
        a.push(o.username.clone());
    }
    if o.regional {
        if let Some(r) = regional {
            a.push("--locale".into());
            a.push(r.locale.clone());
            if let Some(tz) = &r.timezone {
                a.push("--timezone".into());
                a.push(tz.clone());
            }
        }
    }
    a
}

/// Worker side: consume one Windows option from the argument iterator.
/// Returns Ok(true) when `arg` was one of ours.
pub fn parse_arg<'a>(
    o: &mut WinOptions,
    regional: &mut Option<Regional>,
    arg: &str,
    it: &mut impl Iterator<Item = &'a String>,
) -> Result<bool, String> {
    let mut value = |name: &str| -> Result<String, String> {
        it.next()
            .cloned()
            .ok_or_else(|| format!("{} needs a value", name))
    };
    match arg {
        "--scheme" => o.scheme = Scheme::parse(&value(arg)?)?,
        "--target" => o.target = Target::parse(&value(arg)?)?,
        "--fs" => o.fs = Fs::parse(&value(arg)?)?,
        "--label" => o.label = value(arg)?,
        "--bypass-hw" => o.bypass_hw = true,
        "--no-msa" => o.no_online_account = true,
        "--no-telemetry" => o.no_data_collection = true,
        "--no-bitlocker" => o.no_bitlocker = true,
        "--qol" => o.qol = true,
        "--silent" => o.silent = true,
        "--modern-sb" => o.modern_secure_boot = true,
        "--validate" => o.uefi_validation = true,
        "--wtg" => o.wtg = true,
        "--edition" => {
            o.edition = value(arg)?
                .parse()
                .map_err(|_| "--edition must be a number".to_string())?
        }
        "--user" => {
            o.local_user = true;
            o.username = value(arg)?;
        }
        "--locale" => {
            o.regional = true;
            regional.get_or_insert_with(Regional::default).locale = value(arg)?;
        }
        "--timezone" => {
            regional.get_or_insert_with(Regional::default).timezone = Some(value(arg)?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A job that was started without any Windows flag must not inherit the
/// "on by default" values of `WinOptions::default()`: the worker starts from
/// this all-off baseline and only turns on what the arguments ask for.
pub fn all_off() -> WinOptions {
    WinOptions {
        bypass_hw: false,
        no_online_account: false,
        no_data_collection: false,
        ..WinOptions::default()
    }
}

// ----------------------------------------------------- regional detection

/// "fa_IR.UTF-8" -> "fa-IR", "en_US@euro" -> "en-US". None for C / POSIX.
pub fn locale_to_tag(raw: &str) -> Option<String> {
    let base = raw.split(['.', '@']).next()?.trim();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    let mut parts = base.split('_');
    let lang = parts.next()?;
    if lang.len() < 2 || lang.len() > 3 || !lang.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let lang = lang.to_ascii_lowercase();
    match parts.next() {
        Some(region) if region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()) => {
            Some(format!("{}-{}", lang, region.to_ascii_uppercase()))
        }
        Some(_) | None => Some(lang),
    }
}

/// Windows time zone id for an IANA name (the common ones; the table follows
/// the Unicode CLDR "windowsZones" mapping).
pub fn iana_to_windows(iana: &str) -> Option<&'static str> {
    const TABLE: &[(&str, &str)] = &[
        ("UTC", "UTC"),
        ("Etc/UTC", "UTC"),
        ("Etc/GMT", "UTC"),
        ("Europe/London", "GMT Standard Time"),
        ("Europe/Dublin", "GMT Standard Time"),
        ("Europe/Lisbon", "GMT Standard Time"),
        ("Europe/Berlin", "W. Europe Standard Time"),
        ("Europe/Amsterdam", "W. Europe Standard Time"),
        ("Europe/Rome", "W. Europe Standard Time"),
        ("Europe/Vienna", "W. Europe Standard Time"),
        ("Europe/Zurich", "W. Europe Standard Time"),
        ("Europe/Stockholm", "W. Europe Standard Time"),
        ("Europe/Oslo", "W. Europe Standard Time"),
        ("Europe/Paris", "Romance Standard Time"),
        ("Europe/Madrid", "Romance Standard Time"),
        ("Europe/Brussels", "Romance Standard Time"),
        ("Europe/Copenhagen", "Romance Standard Time"),
        ("Europe/Budapest", "Central Europe Standard Time"),
        ("Europe/Prague", "Central Europe Standard Time"),
        ("Europe/Belgrade", "Central Europe Standard Time"),
        ("Europe/Warsaw", "Central European Standard Time"),
        ("Europe/Zagreb", "Central European Standard Time"),
        ("Europe/Sarajevo", "Central European Standard Time"),
        ("Europe/Athens", "GTB Standard Time"),
        ("Europe/Bucharest", "GTB Standard Time"),
        ("Europe/Helsinki", "FLE Standard Time"),
        ("Europe/Kiev", "FLE Standard Time"),
        ("Europe/Kyiv", "FLE Standard Time"),
        ("Europe/Riga", "FLE Standard Time"),
        ("Europe/Sofia", "FLE Standard Time"),
        ("Europe/Tallinn", "FLE Standard Time"),
        ("Europe/Vilnius", "FLE Standard Time"),
        ("Europe/Istanbul", "Turkey Standard Time"),
        ("Europe/Moscow", "Russian Standard Time"),
        ("Europe/Minsk", "Belarus Standard Time"),
        ("Asia/Tehran", "Iran Standard Time"),
        ("Asia/Dubai", "Arabian Standard Time"),
        ("Asia/Baghdad", "Arabic Standard Time"),
        ("Asia/Riyadh", "Arab Standard Time"),
        ("Asia/Kuwait", "Arab Standard Time"),
        ("Asia/Qatar", "Arab Standard Time"),
        ("Asia/Baku", "Azerbaijan Standard Time"),
        ("Asia/Tbilisi", "Georgian Standard Time"),
        ("Asia/Yerevan", "Caucasus Standard Time"),
        ("Asia/Kabul", "Afghanistan Standard Time"),
        ("Asia/Karachi", "Pakistan Standard Time"),
        ("Asia/Tashkent", "West Asia Standard Time"),
        ("Asia/Almaty", "Central Asia Standard Time"),
        ("Asia/Kolkata", "India Standard Time"),
        ("Asia/Calcutta", "India Standard Time"),
        ("Asia/Colombo", "Sri Lanka Standard Time"),
        ("Asia/Kathmandu", "Nepal Standard Time"),
        ("Asia/Dhaka", "Bangladesh Standard Time"),
        ("Asia/Bangkok", "SE Asia Standard Time"),
        ("Asia/Ho_Chi_Minh", "SE Asia Standard Time"),
        ("Asia/Jakarta", "SE Asia Standard Time"),
        ("Asia/Shanghai", "China Standard Time"),
        ("Asia/Hong_Kong", "China Standard Time"),
        ("Asia/Singapore", "Singapore Standard Time"),
        ("Asia/Kuala_Lumpur", "Singapore Standard Time"),
        ("Asia/Manila", "Singapore Standard Time"),
        ("Asia/Taipei", "Taipei Standard Time"),
        ("Asia/Tokyo", "Tokyo Standard Time"),
        ("Asia/Seoul", "Korea Standard Time"),
        ("Asia/Jerusalem", "Israel Standard Time"),
        ("Asia/Beirut", "Middle East Standard Time"),
        ("Asia/Amman", "Jordan Standard Time"),
        ("Asia/Damascus", "Syria Standard Time"),
        ("Africa/Cairo", "Egypt Standard Time"),
        ("Africa/Johannesburg", "South Africa Standard Time"),
        ("Africa/Lagos", "W. Central Africa Standard Time"),
        ("Africa/Algiers", "W. Central Africa Standard Time"),
        ("Africa/Tunis", "W. Central Africa Standard Time"),
        ("Africa/Nairobi", "E. Africa Standard Time"),
        ("Africa/Addis_Ababa", "E. Africa Standard Time"),
        ("Africa/Casablanca", "Morocco Standard Time"),
        ("America/New_York", "Eastern Standard Time"),
        ("America/Toronto", "Eastern Standard Time"),
        ("America/Chicago", "Central Standard Time"),
        ("America/Denver", "Mountain Standard Time"),
        ("America/Phoenix", "US Mountain Standard Time"),
        ("America/Los_Angeles", "Pacific Standard Time"),
        ("America/Vancouver", "Pacific Standard Time"),
        ("America/Anchorage", "Alaskan Standard Time"),
        ("America/Halifax", "Atlantic Standard Time"),
        ("America/Mexico_City", "Central Standard Time (Mexico)"),
        ("America/Sao_Paulo", "E. South America Standard Time"),
        ("America/Argentina/Buenos_Aires", "Argentina Standard Time"),
        ("America/Bogota", "SA Pacific Standard Time"),
        ("America/Lima", "SA Pacific Standard Time"),
        ("America/Santiago", "Pacific SA Standard Time"),
        ("America/Caracas", "Venezuela Standard Time"),
        ("Pacific/Honolulu", "Hawaiian Standard Time"),
        ("Australia/Sydney", "AUS Eastern Standard Time"),
        ("Australia/Melbourne", "AUS Eastern Standard Time"),
        ("Australia/Brisbane", "E. Australia Standard Time"),
        ("Australia/Perth", "W. Australia Standard Time"),
        ("Australia/Adelaide", "Cen. Australia Standard Time"),
        ("Pacific/Auckland", "New Zealand Standard Time"),
    ];
    TABLE.iter().find(|(i, _)| *i == iana).map(|(_, w)| *w)
}

fn system_timezone() -> Option<String> {
    if let Ok(p) = std::fs::read_link("/etc/localtime") {
        let s = p.to_string_lossy().into_owned();
        if let Some(i) = s.find("zoneinfo/") {
            return Some(s[i + "zoneinfo/".len()..].to_string());
        }
    }
    std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Look at this user's environment (language) and this computer (time zone).
pub fn detect_regional() -> Option<Regional> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")?;
    // LANGUAGE can be a colon separated list.
    let locale = locale_to_tag(raw.split(':').next()?)?;
    let timezone = system_timezone()
        .and_then(|z| iana_to_windows(&z))
        .map(|s| s.to_string());
    Some(Regional { locale, timezone })
}

impl fmt::Display for Regional {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.timezone {
            Some(tz) => write!(f, "{}, {}", self.locale, tz),
            None => write!(f, "{}", self.locale),
        }
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_needs_uefi() {
        let mut o = WinOptions::default();
        o.target = Target::Both;
        assert!(o.normalized().is_err());
        o.scheme = Scheme::Mbr;
        assert!(o.normalized().is_ok());
        assert_eq!(Scheme::Gpt.targets(), vec![Target::Uefi]);
        assert!(Scheme::Mbr.targets().contains(&Target::Bios));
    }

    #[test]
    fn labels() {
        assert_eq!(sanitize_label(Fs::Fat32, "  win 11 ").unwrap(), "WIN 11");
        assert_eq!(sanitize_label(Fs::Ntfs, "Win 11").unwrap(), "Win 11");
        assert_eq!(sanitize_label(Fs::Ntfs, "  ").unwrap(), DEFAULT_LABEL);
        assert!(sanitize_label(Fs::Fat32, "TWELVE_CHARS").is_err());
        assert!(sanitize_label(Fs::Exfat, "SIXTEEN_CHARS_AB").is_err());
        assert!(sanitize_label(Fs::Ntfs, "a/b").is_err());
        assert!(sanitize_label(Fs::Fat32, "a+b").is_err());
        assert!(sanitize_label(Fs::Ntfs, "a+b").is_ok());
    }

    #[test]
    fn usernames() {
        assert_eq!(validate_username("  Ali ").unwrap(), "Ali");
        assert!(validate_username("").is_err());
        assert!(validate_username("administrator").is_err());
        assert!(validate_username("Администратор").is_err());
        assert!(validate_username("a:b").is_err());
        assert!(validate_username("a&b").is_err());
        assert!(validate_username("dot.").is_err());
        assert!(validate_username(&"x".repeat(21)).is_err());
        assert!(validate_username("علی").is_ok());
    }

    #[test]
    fn normalization() {
        let mut o = WinOptions::default();
        o.local_user = true;
        o.username = " Sara ".into();
        o.edition = 3; // ignored unless silent
        let n = o.normalized().unwrap();
        assert_eq!(n.username, "Sara");
        assert_eq!(n.edition, 1);
        o.silent = true;
        assert_eq!(o.normalized().unwrap().edition, 3);
        o.edition = 0;
        assert!(o.normalized().is_err());
        o.local_user = false;
        o.edition = 2;
        o.username = "ignored".into();
        assert_eq!(o.normalized().unwrap().username, "");
    }

    #[test]
    fn uefi_ntfs_rule() {
        let mut o = WinOptions::default();
        assert!(!o.needs_uefi_ntfs()); // FAT32 boots by itself
        o.fs = Fs::Ntfs;
        assert!(o.needs_uefi_ntfs());
        o.scheme = Scheme::Mbr;
        o.target = Target::Bios; // BIOS only: no UEFI loader needed
        assert!(!o.needs_uefi_ntfs());
        o.target = Target::Both;
        assert!(o.needs_uefi_ntfs());
    }

    fn roundtrip(o: &WinOptions, r: Option<&Regional>) -> (WinOptions, Option<Regional>) {
        let args = win_args(o, r);
        assert_eq!(args[0], "--windows");
        let mut parsed = all_off();
        let mut reg = None;
        let rest: Vec<String> = args[1..].to_vec();
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            assert!(
                parse_arg(&mut parsed, &mut reg, a, &mut it).unwrap(),
                "{}",
                a
            );
        }
        (parsed, reg)
    }

    #[test]
    fn arguments_round_trip() {
        let mut o = WinOptions {
            scheme: Scheme::Mbr,
            target: Target::Both,
            fs: Fs::Ntfs,
            label: "MY WIN".into(),
            bypass_hw: true,
            no_online_account: false,
            local_user: true,
            username: "Ali".into(),
            regional: true,
            no_data_collection: false,
            no_bitlocker: true,
            qol: true,
            silent: true,
            edition: 2,
            modern_secure_boot: true,
            uefi_validation: true,
            wtg: false,
        };
        let reg = Regional {
            locale: "fa-IR".into(),
            timezone: Some("Iran Standard Time".into()),
        };
        let (p, r) = roundtrip(&o, Some(&reg));
        assert_eq!(p, o);
        assert_eq!(r, Some(reg.clone()));

        // not silent: no edition argument; regional off: no locale arguments
        o.silent = false;
        o.regional = false;
        o.edition = 1;
        let args = win_args(&o, Some(&reg));
        assert!(!args.contains(&"--edition".to_string()));
        assert!(!args.contains(&"--locale".to_string()));
        let (p, r) = roundtrip(&o, Some(&reg));
        assert_eq!(p, o);
        assert_eq!(r, None);
    }

    #[test]
    fn parser_rejects_bad_values() {
        let mut o = all_off();
        let mut r = None;
        let bad = vec!["gpx".to_string()];
        assert!(parse_arg(&mut o, &mut r, "--scheme", &mut bad.iter()).is_err());
        let none: Vec<String> = vec![];
        assert!(parse_arg(&mut o, &mut r, "--fs", &mut none.iter()).is_err());
        assert!(!parse_arg(&mut o, &mut r, "--bogus", &mut none.iter()).unwrap());
        let nan = vec!["x".to_string()];
        assert!(parse_arg(&mut o, &mut r, "--edition", &mut nan.iter()).is_err());
    }

    #[test]
    fn locale_tags() {
        assert_eq!(locale_to_tag("fa_IR.UTF-8").as_deref(), Some("fa-IR"));
        assert_eq!(locale_to_tag("en_US").as_deref(), Some("en-US"));
        assert_eq!(locale_to_tag("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(locale_to_tag("fr").as_deref(), Some("fr"));
        assert_eq!(locale_to_tag("C"), None);
        assert_eq!(locale_to_tag("POSIX"), None);
        assert_eq!(locale_to_tag("C.UTF-8"), None);
        assert_eq!(locale_to_tag(""), None);
        assert_eq!(locale_to_tag("12_34"), None);
    }

    #[test]
    fn time_zones() {
        assert_eq!(iana_to_windows("Asia/Tehran"), Some("Iran Standard Time"));
        assert_eq!(
            iana_to_windows("Europe/Berlin"),
            Some("W. Europe Standard Time")
        );
        assert_eq!(iana_to_windows("Mars/Olympus"), None);
        assert_eq!(
            Regional {
                locale: "fa-IR".into(),
                timezone: Some("Iran Standard Time".into())
            }
            .to_string(),
            "fa-IR, Iran Standard Time"
        );
    }

    #[test]
    fn wtg_arguments_and_normalisation() {
        let mut o = all_off();
        o.wtg = true;
        o.edition = 3;
        o.fs = Fs::Fat32;
        o.silent = true;
        o.bypass_hw = true;
        o.uefi_validation = true;
        let (p, _) = roundtrip(&o, None);
        assert!(p.wtg);
        assert_eq!(p.edition, 3);
        let n = p.normalized().unwrap();
        assert_eq!(n.fs, Fs::Ntfs);
        assert!(!n.silent && !n.bypass_hw && !n.uefi_validation);
        assert_eq!(n.edition, 3);
    }
}
