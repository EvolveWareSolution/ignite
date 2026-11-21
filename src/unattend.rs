//! Builds the `autounattend.xml` answer file that implements the Windows
//! "user experience" options (bypasses, local account, regional options, ...).
//!
//! The structure and the registry values are the usual ones for Windows answer
//! files. Pure string building: no disk
//! access, fully unit-tested.

use crate::winopts::{Regional, Target, WinOptions};

pub struct Params<'a> {
    pub opts: &'a WinOptions,
    pub regional: Option<&'a Regional>,
    /// "x86", "amd64" or "arm64" (the processorArchitecture of the media).
    pub arch: &'a str,
    /// UI language of Windows Setup (taken from boot.wim), e.g. "en-US".
    pub ui_language: &'a str,
}

pub struct Unattend {
    pub xml: String,
    /// True when the file has a `windowsPE` pass. Such a file must be placed
    /// in the root of boot.wim (as Autounattend.xml); without it, it goes to
    /// `sources\$OEM$\$$\Panther\unattend.xml` on the media.
    pub winpe: bool,
}

const BYPASS_NAMES: [&str; 3] = ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck"];

/// The label the silent install uses to recognise the install media (see
/// `disk_configuration`).
pub const BOOT_MARKER_LABEL: &str = "IGNITE_BOOT";

/// Is there anything to write into an answer file?
pub fn is_needed(o: &WinOptions, regional: Option<&Regional>) -> bool {
    o.wtg
        || o.bypass_hw
        || o.no_online_account
        || o.local_user
        || o.no_data_collection
        || o.no_bitlocker
        || o.qol
        || o.silent
        || (o.regional && regional.is_some())
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn component(name: &str, arch: &str, body: &str) -> String {
    format!(
        "    <component name=\"{}\" processorArchitecture=\"{}\" language=\"neutral\" \
xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
publicKeyToken=\"31bf3856ad364e35\" versionScope=\"nonSxS\">\n{}    </component>\n",
        name, arch, body
    )
}

/// `<RunSynchronous>` (windowsPE / specialize) from a list of commands.
fn run_synchronous(cmds: &[String]) -> String {
    if cmds.is_empty() {
        return String::new();
    }
    let mut s = String::from("      <RunSynchronous>\n");
    for (i, c) in cmds.iter().enumerate() {
        s.push_str(&format!(
            "        <RunSynchronousCommand wcm:action=\"add\">\n          <Order>{}</Order>\n          <Path>{}</Path>\n        </RunSynchronousCommand>\n",
            i + 1,
            escape(c)
        ));
    }
    s.push_str("      </RunSynchronous>\n");
    s
}

/// `<FirstLogonCommands>` (oobeSystem).
fn first_logon(cmds: &[String]) -> String {
    if cmds.is_empty() {
        return String::new();
    }
    let mut s = String::from("      <FirstLogonCommands>\n");
    for (i, c) in cmds.iter().enumerate() {
        s.push_str(&format!(
            "        <SynchronousCommand wcm:action=\"add\">\n          <Order>{}</Order>\n          <CommandLine>{}</CommandLine>\n        </SynchronousCommand>\n",
            i + 1,
            escape(c)
        ));
    }
    s.push_str("      </FirstLogonCommands>\n");
    s
}

fn reg_add(key: &str, name: &str, value: u32) -> String {
    format!(
        "reg add \"{}\" /v {} /t REG_DWORD /d {} /f",
        key, name, value
    )
}

fn powershell(script: &str) -> String {
    format!(
        "PowerShell -NonInteractive -WindowStyle Hidden -Command \"{}\"",
        script
    )
}

fn remove_appx(pattern: &str) -> [String; 2] {
    [
        powershell(&format!(
            "Get-AppxProvisionedPackage -Online | Where-Object {{$_.PackageName -like '*{}*'}} | Remove-AppxProvisionedPackage -Online",
            pattern
        )),
        powershell(&format!(
            "Get-AppxPackage -AllUsers *{}* | Remove-AppxPackage -AllUsers",
            pattern
        )),
    ]
}

/// Automatic partitioning and image selection for the silent installation.
///
/// Safety net: disk 1 must have a partition 2 that we can
/// relabel `IGNITE_BOOT`. Ignite always creates such a partition on the USB
/// drive for silent installs. If Setup does not see "one internal disk (0) +
/// the install USB (1)", this step fails and Setup shows its normal screens
/// instead of wiping something unexpected.
fn disk_configuration(o: &WinOptions, edition: u32) -> String {
    let uefi = o.target != Target::Bios;
    let mut s = String::new();
    s.push_str("      <DiskConfiguration>\n        <WillShowUI>OnError</WillShowUI>\n");
    if o.no_bitlocker {
        s.push_str(
            "        <DisableEncryptedDiskProvisioning>true</DisableEncryptedDiskProvisioning>\n",
        );
    }
    s.push_str(&format!(
        "        <Disk wcm:action=\"modify\">\n          <DiskID>1</DiskID>\n          <ModifyPartitions>\n            <ModifyPartition wcm:action=\"modify\">\n              <Order>1</Order>\n              <PartitionID>2</PartitionID>\n              <Label>{}</Label>\n            </ModifyPartition>\n          </ModifyPartitions>\n        </Disk>\n",
        BOOT_MARKER_LABEL
    ));
    s.push_str("        <Disk wcm:action=\"add\">\n          <DiskID>0</DiskID>\n          <WillWipeDisk>true</WillWipeDisk>\n          <CreatePartitions>\n");
    let (windows_partition, create, modify) = if uefi {
        (
            3,
            // EFI system partition, Microsoft reserved, Windows. (Setup adds the
            // recovery partition by itself.)
            "            <CreatePartition wcm:action=\"add\">\n              <Order>1</Order>\n              <Type>EFI</Type>\n              <Size>260</Size>\n            </CreatePartition>\n            <CreatePartition wcm:action=\"add\">\n              <Order>2</Order>\n              <Type>MSR</Type>\n              <Size>16</Size>\n            </CreatePartition>\n            <CreatePartition wcm:action=\"add\">\n              <Order>3</Order>\n              <Type>Primary</Type>\n              <Extend>true</Extend>\n            </CreatePartition>\n",
            "            <ModifyPartition wcm:action=\"add\">\n              <Order>1</Order>\n              <PartitionID>1</PartitionID>\n              <Label>EFI</Label>\n              <Format>FAT32</Format>\n            </ModifyPartition>\n            <ModifyPartition wcm:action=\"add\">\n              <Order>2</Order>\n              <PartitionID>3</PartitionID>\n              <Label>Windows</Label>\n              <Letter>C</Letter>\n              <Format>NTFS</Format>\n            </ModifyPartition>\n",
        )
    } else {
        (
            2,
            // Legacy BIOS: a small active system partition, then Windows.
            "            <CreatePartition wcm:action=\"add\">\n              <Order>1</Order>\n              <Type>Primary</Type>\n              <Size>500</Size>\n            </CreatePartition>\n            <CreatePartition wcm:action=\"add\">\n              <Order>2</Order>\n              <Type>Primary</Type>\n              <Extend>true</Extend>\n            </CreatePartition>\n",
            "            <ModifyPartition wcm:action=\"add\">\n              <Order>1</Order>\n              <PartitionID>1</PartitionID>\n              <Label>System</Label>\n              <Format>NTFS</Format>\n              <Active>true</Active>\n            </ModifyPartition>\n            <ModifyPartition wcm:action=\"add\">\n              <Order>2</Order>\n              <PartitionID>2</PartitionID>\n              <Label>Windows</Label>\n              <Letter>C</Letter>\n              <Format>NTFS</Format>\n            </ModifyPartition>\n",
        )
    };
    s.push_str(create);
    s.push_str("          </CreatePartitions>\n          <ModifyPartitions>\n");
    s.push_str(modify);
    s.push_str("          </ModifyPartitions>\n        </Disk>\n      </DiskConfiguration>\n");
    s.push_str(&format!(
        "      <ImageInstall>\n        <OSImage>\n          <WillShowUI>OnError</WillShowUI>\n          <InstallFrom>\n            <MetaData wcm:action=\"add\">\n              <Key>/IMAGE/INDEX</Key>\n              <Value>{}</Value>\n            </MetaData>\n          </InstallFrom>\n          <InstallTo>\n            <DiskID>0</DiskID>\n            <PartitionID>{}</PartitionID>\n          </InstallTo>\n        </OSImage>\n      </ImageInstall>\n",
        edition, windows_partition
    ));
    s
}

/// Settings that change what Windows Setup itself does (pass `windowsPE`).
fn pass_windows_pe(p: &Params) -> String {
    let o = p.opts;
    let mut setup = String::new();
    // Setup insists on a product key entry, even an empty one.
    setup.push_str("      <UserData>\n        <AcceptEula>true</AcceptEula>\n        <ProductKey>\n          <Key />\n        </ProductKey>\n      </UserData>\n");
    if o.silent {
        setup.push_str(&disk_configuration(o, o.edition));
    }
    if o.bypass_hw {
        let cmds: Vec<String> = BYPASS_NAMES
            .iter()
            .map(|n| format!("reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {} /t REG_DWORD /d 1 /f", n))
            .collect();
        setup.push_str(&run_synchronous(&cmds));
    }
    let mut s = component("Microsoft-Windows-Setup", p.arch, &setup);
    if o.silent {
        s.push_str(&component(
            "Microsoft-Windows-International-Core-WinPE",
            p.arch,
            &format!("      <UILanguage>{}</UILanguage>\n", escape(p.ui_language)),
        ));
    }
    format!("  <settings pass=\"windowsPE\">\n{}  </settings>\n", s)
}

/// Commands that run once, early, during the install (pass `specialize`).
fn specialize_commands(o: &WinOptions) -> Vec<String> {
    let mut c: Vec<String> = Vec::new();
    if o.wtg {
        // Windows To Go: keep the PC's own disks offline (SAN policy 4), so a
        // newer Windows on the drive cannot touch or "upgrade" them. The
        // same setting can be applied offline with DISM; setting the value here
        // takes effect from the next boot on.
        c.push(reg_add(
            "HKLM\\System\\CurrentControlSet\\Services\\partmgr\\Parameters",
            "SanPolicy",
            4,
        ));
    }
    if o.no_online_account {
        c.push(
            "reg add \"HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\OOBE\" /v BypassNRO /t REG_DWORD /d 1 /f"
                .into(),
        );
    }
    if o.qol {
        // OneDrive: policy + remove the installer so it is not set up per user.
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Windows\\OneDrive",
            "DisableFileSyncNGSC",
            1,
        ));
        c.push(powershell(
            "Remove-Item -Path $env:SystemRoot\\System32\\OneDriveSetup.exe -Force -Confirm:$false; Remove-Item -Path $env:SystemRoot\\SysWOW64\\OneDriveSetup.exe -Force -Confirm:$false;",
        ));
        // The new Outlook and Teams are provisioned apps: remove them.
        c.extend(remove_appx("Outlook"));
        c.extend(remove_appx("Teams"));
    }
    c
}

/// Commands that run at the first logon, as the new user.
fn first_logon_commands(o: &WinOptions) -> Vec<String> {
    let mut c: Vec<String> = Vec::new();
    if o.local_user {
        // The account has an empty password: make Windows ask for a new one.
        c.push(format!(
            "net user \"{}\" /logonpasswordchg:yes",
            o.username.replace('"', "")
        ));
        c.push("net accounts /maxpwage:unlimited".into());
    }
    if o.qol {
        let hkcu_adv = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
        let hkcu_search = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Search";
        let cloud = "HKLM\\Software\\Policies\\Microsoft\\Windows\\CloudContent";
        // Fast Startup off, Copilot off, search as an icon.
        c.push(reg_add(
            "HKLM\\System\\CurrentControlSet\\Control\\Session Manager\\Power",
            "HiberbootEnabled",
            0,
        ));
        c.push(reg_add(hkcu_adv, "ShowCopilotButton", 0));
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Windows\\WindowsCopilot",
            "TurnOffWindowsCopilot",
            1,
        ));
        c.push(reg_add(hkcu_search, "SearchboxTaskbarMode", 1));
        c.push(reg_add(hkcu_search, "SearchboxTaskbarModeCache", 1));
        // No consumer "suggested apps", ads in the start menu or web search.
        c.push(reg_add(cloud, "DisableWindowsConsumerFeatures", 1));
        c.push(reg_add(
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\ContentDeliveryManager",
            "SystemPaneSuggestionsEnabled",
            0,
        ));
        c.push(reg_add(hkcu_search, "BingSearchEnabled", 0));
        // No manufacturer apps pulled in when new hardware is plugged in.
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Windows\\Device Metadata",
            "PreventDeviceMetadataFromNetwork",
            1,
        ));
        // No News and Interests / feeds.
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Dsh",
            "AllowNewsAndInterests",
            0,
        ));
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Windows\\Windows Feeds",
            "EnableFeeds",
            0,
        ));
        // No Teams auto-install, no Outlook pushed to the taskbar.
        c.push(reg_add(
            "HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Communications",
            "ConfigureChatAutoInstall",
            0,
        ));
        c.push(reg_add(cloud, "DisableCloudOptimizedContent", 1));
        // Skip Edge's first run dialog.
        c.push(reg_add(
            "HKLM\\Software\\Policies\\Microsoft\\Edge",
            "HideFirstRunExperience",
            1,
        ));
    }
    c
}

/// Everything in pass `oobeSystem`.
fn pass_oobe(p: &Params) -> String {
    let o = p.opts;
    let mut out = String::new();

    // ---- Shell-Setup
    let mut shell = String::new();
    let skip_privacy = o.no_data_collection || o.silent;
    let hide_online = o.no_online_account || o.silent;
    if skip_privacy || hide_online || o.silent {
        shell.push_str("      <OOBE>\n");
        if skip_privacy {
            shell.push_str("        <HideEULAPage>true</HideEULAPage>\n");
            // Despite the name, this is only about data collection (3 = off).
            shell.push_str("        <ProtectYourPC>3</ProtectYourPC>\n");
        }
        if hide_online {
            shell.push_str("        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>\n");
        }
        if o.silent {
            shell.push_str("        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>\n");
        }
        shell.push_str("      </OOBE>\n");
    }
    if o.regional {
        if let Some(tz) = p.regional.and_then(|r| r.timezone.as_ref()) {
            shell.push_str(&format!("      <TimeZone>{}</TimeZone>\n", escape(tz)));
        }
    }
    if o.local_user {
        let name = escape(&o.username);
        // An empty password (Windows wants it as the Base64 of UTF-16
        // "Password"); the first logon command asks for a new one.
        shell.push_str(&format!(
            "      <UserAccounts>\n        <LocalAccounts>\n          <LocalAccount wcm:action=\"add\">\n            <Name>{0}</Name>\n            <DisplayName>{0}</DisplayName>\n            <Group>Administrators;Power Users</Group>\n            <Password>\n              <Value>UABhAHMAcwB3AG8AcgBkAA==</Value>\n              <PlainText>false</PlainText>\n            </Password>\n          </LocalAccount>\n        </LocalAccounts>\n      </UserAccounts>\n",
            name
        ));
    }
    shell.push_str(&first_logon(&first_logon_commands(o)));
    if !shell.is_empty() {
        out.push_str(&component("Microsoft-Windows-Shell-Setup", p.arch, &shell));
    }

    // ---- regional options
    if o.regional {
        if let Some(r) = p.regional {
            let l = escape(&r.locale);
            out.push_str(&component(
                "Microsoft-Windows-International-Core",
                p.arch,
                &format!(
                    "      <InputLocale>{0}</InputLocale>\n      <SystemLocale>{0}</SystemLocale>\n      <UserLocale>{0}</UserLocale>\n      <UILanguage>{0}</UILanguage>\n      <UILanguageFallback>en-US</UILanguageFallback>\n",
                    l
                ),
            ));
        }
    }

    // ---- BitLocker / device encryption
    if o.no_bitlocker {
        out.push_str(&component(
            "Microsoft-Windows-SecureStartup-FilterDriver",
            p.arch,
            "      <PreventDeviceEncryption>true</PreventDeviceEncryption>\n",
        ));
        out.push_str(&component(
            "Microsoft-Windows-EnhancedStorage-Adm",
            p.arch,
            "      <TCGSecurityActivationDisabled>1</TCGSecurityActivationDisabled>\n",
        ));
    }

    if out.is_empty() {
        String::new()
    } else {
        format!("  <settings pass=\"oobeSystem\">\n{}  </settings>\n", out)
    }
}

/// Human readable list of what the answer file does (shown in the log).
pub fn summary(o: &WinOptions, regional: Option<&Regional>) -> Vec<String> {
    let mut v = Vec::new();
    if o.silent {
        v.push(format!(
            "Silent installation of image index {} on the first disk",
            o.edition
        ));
    }
    if o.bypass_hw {
        v.push("Bypass the 4 GB RAM, Secure Boot and TPM 2.0 requirements".into());
    }
    if o.no_online_account {
        v.push("Bypass the online Microsoft account requirement".into());
    }
    if o.local_user {
        v.push(format!("Create the local account '{}'", o.username));
    }
    if o.regional {
        if let Some(r) = regional {
            v.push(format!("Use the same regional options as this user ({})", r));
        }
    }
    if o.no_data_collection {
        v.push("Skip the privacy questions (disable data collection)".into());
    }
    if o.no_bitlocker {
        v.push("Disable BitLocker automatic device encryption".into());
    }
    if o.qol {
        v.push("Quality of life: no Teams, Outlook, Copilot, OneDrive nags".into());
    }
    v
}

pub fn build(p: &Params) -> Option<Unattend> {
    let o = p.opts;
    if !is_needed(o, p.regional) {
        return None;
    }
    let winpe = o.bypass_hw || o.silent;
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<unattend xmlns=\"urn:schemas-microsoft-com:unattend\">\n",
    );
    if winpe {
        xml.push_str(&pass_windows_pe(p));
    }
    let spec = specialize_commands(o);
    if !spec.is_empty() {
        xml.push_str(&format!(
            "  <settings pass=\"specialize\">\n{}  </settings>\n",
            component("Microsoft-Windows-Deployment", p.arch, &run_synchronous(&spec))
        ));
    }
    xml.push_str(&pass_oobe(p));
    xml.push_str("</unattend>\n");
    Some(Unattend { xml, winpe })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winopts::all_off;

    fn params<'a>(o: &'a WinOptions, r: Option<&'a Regional>) -> Params<'a> {
        Params {
            opts: o,
            regional: r,
            arch: "amd64",
            ui_language: "en-US",
        }
    }

    #[test]
    fn nothing_selected_builds_nothing() {
        let o = all_off();
        assert!(!is_needed(&o, None));
        assert!(build(&params(&o, None)).is_none());
    }

    #[test]
    fn regional_without_detection_builds_nothing() {
        let mut o = all_off();
        o.regional = true;
        assert!(build(&params(&o, None)).is_none());
    }

    #[test]
    fn bypass_goes_to_windows_pe() {
        let mut o = all_off();
        o.bypass_hw = true;
        let u = build(&params(&o, None)).unwrap();
        assert!(u.winpe);
        for n in BYPASS_NAMES {
            assert!(u.xml.contains(&format!(
                "reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {} /t REG_DWORD /d 1 /f",
                n
            )));
        }
        assert!(u.xml.contains("<settings pass=\"windowsPE\">"));
        assert!(u.xml.contains("<AcceptEula>true</AcceptEula>"));
        assert!(!u.xml.contains("oobeSystem"));
        assert!(!u.xml.contains("specialize"));
    }

    #[test]
    fn online_account_and_privacy() {
        let mut o = all_off();
        o.no_online_account = true;
        o.no_data_collection = true;
        let u = build(&params(&o, None)).unwrap();
        assert!(!u.winpe, "no windowsPE pass: file goes to $OEM$ Panther");
        assert!(u.xml.contains("BypassNRO"));
        assert!(u.xml.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(u.xml.contains("<HideEULAPage>true</HideEULAPage>"));
        assert!(u.xml.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(!u.xml.contains("windowsPE"));
    }

    #[test]
    fn local_account_is_escaped() {
        let mut o = all_off();
        o.local_user = true;
        o.username = "A<b>".into(); // would be refused by validation, but be safe anyway
        let u = build(&params(&o, None)).unwrap();
        assert!(u.xml.contains("<Name>A&lt;b&gt;</Name>"));
        assert!(u.xml.contains("<Group>Administrators;Power Users</Group>"));
        assert!(u.xml.contains("/logonpasswordchg:yes"));
        assert!(u.xml.contains("net accounts /maxpwage:unlimited"));
    }

    #[test]
    fn regional_options() {
        let mut o = all_off();
        o.regional = true;
        let r = Regional {
            locale: "fa-IR".into(),
            timezone: Some("Iran Standard Time".into()),
        };
        let u = build(&params(&o, Some(&r))).unwrap();
        assert!(u.xml.contains("<TimeZone>Iran Standard Time</TimeZone>"));
        assert!(u.xml.contains("<UserLocale>fa-IR</UserLocale>"));
        assert!(u.xml.contains("<InputLocale>fa-IR</InputLocale>"));
        // no timezone known: the element is simply left out
        let r2 = Regional {
            locale: "fa-IR".into(),
            timezone: None,
        };
        let u = build(&params(&o, Some(&r2))).unwrap();
        assert!(!u.xml.contains("TimeZone"));
    }

    #[test]
    fn bitlocker() {
        let mut o = all_off();
        o.no_bitlocker = true;
        let u = build(&params(&o, None)).unwrap();
        assert!(u.xml.contains("<PreventDeviceEncryption>true</PreventDeviceEncryption>"));
        assert!(u.xml.contains("TCGSecurityActivationDisabled"));
        assert!(!u.winpe);
    }

    #[test]
    fn quality_of_life() {
        let mut o = all_off();
        o.qol = true;
        let u = build(&params(&o, None)).unwrap();
        for needle in [
            "DisableFileSyncNGSC",
            "TurnOffWindowsCopilot",
            "*Outlook*",
            "*Teams*",
            "ConfigureChatAutoInstall",
            "DisableWindowsConsumerFeatures",
            "HiberbootEnabled",
        ] {
            assert!(u.xml.contains(needle), "missing {}", needle);
        }
        // '&' in PowerShell/cmd text must be escaped, never raw
        assert!(!u.xml.contains("& "));
    }

    #[test]
    fn silent_uefi_layout() {
        let mut o = all_off();
        o.silent = true;
        o.edition = 2;
        let u = build(&params(&o, None)).unwrap();
        assert!(u.winpe);
        assert!(u.xml.contains("<Type>EFI</Type>"));
        assert!(u.xml.contains("<Type>MSR</Type>"));
        assert!(u.xml.contains("<PartitionID>3</PartitionID>"));
        assert!(u.xml.contains("<Value>2</Value>"));
        assert!(u.xml.contains(&format!("<Label>{}</Label>", BOOT_MARKER_LABEL)));
        assert!(u.xml.contains("<WillWipeDisk>true</WillWipeDisk>"));
        assert!(u.xml.contains("<UILanguage>en-US</UILanguage>"));
        assert!(u.xml.contains("<HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>"));
        // always the "show UI on error" safety net
        assert!(u.xml.matches("<WillShowUI>OnError</WillShowUI>").count() == 2);
    }

    #[test]
    fn silent_bios_layout() {
        let mut o = all_off();
        o.silent = true;
        o.target = Target::Bios;
        let u = build(&params(&o, None)).unwrap();
        assert!(!u.xml.contains("<Type>EFI</Type>"));
        assert!(u.xml.contains("<Active>true</Active>"));
        assert!(u.xml.contains("<PartitionID>2</PartitionID>\n          </InstallTo>"));
    }

    #[test]
    fn silent_with_bitlocker_disables_encrypted_provisioning() {
        let mut o = all_off();
        o.silent = true;
        o.no_bitlocker = true;
        let u = build(&params(&o, None)).unwrap();
        assert!(u.xml.contains("DisableEncryptedDiskProvisioning"));
    }

    #[test]
    fn xml_is_well_formed_for_everything_on() {
        let mut o = WinOptions::default();
        o.scheme = crate::winopts::Scheme::Mbr;
        o.target = Target::Both;
        o.local_user = true;
        o.username = "Ali".into();
        o.regional = true;
        o.no_bitlocker = true;
        o.qol = true;
        o.silent = true;
        let r = Regional {
            locale: "fa-IR".into(),
            timezone: Some("Iran Standard Time".into()),
        };
        let u = build(&params(&o, Some(&r))).unwrap();
        // Count opening/closing tags of the structural elements.
        for tag in ["settings", "component", "FirstLogonCommands", "RunSynchronous", "DiskConfiguration"] {
            let open = u.xml.matches(&format!("<{} ", tag)).count()
                + u.xml.matches(&format!("<{}>", tag)).count();
            let close = u.xml.matches(&format!("</{}>", tag)).count();
            assert_eq!(open, close, "unbalanced <{}>", tag);
        }
        assert!(u.xml.starts_with("<?xml"));
        assert!(u.xml.trim_end().ends_with("</unattend>"));
        // each pass appears at most once (Windows rejects duplicates)
        for pass in ["windowsPE", "specialize", "oobeSystem"] {
            assert!(u.xml.matches(&format!("pass=\"{}\"", pass)).count() <= 1);
        }
        // only one FirstLogonCommands section (Microsoft does not allow two)
        assert_eq!(u.xml.matches("<FirstLogonCommands>").count(), 1);
        assert!(!summary(&o, Some(&r)).is_empty());
    }

    #[test]
    fn escaping() {
        assert_eq!(escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
