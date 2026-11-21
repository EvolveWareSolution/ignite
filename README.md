# Ignite

Forge bootable USB drives from ISO images: a graphical, safer `dd`
(Rust + [iced](https://iced.rs)).

## Features

- Write ISO/IMG to a whole disk (4 MiB blocks, periodic fsync)
- Progress bar with speed and ETA, log window, cancel button
- Drag an image onto the window, or use Browse
- Two modes: the full window, and a small floating "compact" window
  (always on top where the desktop allows it) that shares all state with it.
  Switch with the Compact / Expand button, or start small with `ignite --compact`
  (also in the right-click menu of the launcher: "Compact floating window")
- The window title shows the progress ("Ignite - 42%") while a job runs
- Optional SHA256 check of the image BEFORE anything is erased
  (paste the hash or the whole `sha256sum` line)
- Optional read-back verification after writing
- Restore drive: wipe it and recreate one normal FAT32 partition
- Power off the drive when finished (udisks), safe to unplug
- The drive list refreshes by itself when you plug or unplug a USB drive

## Windows page (Windows installer media)

The **Windows** button in the header opens the same window with Windows-specific
cards (press **Standard** to go back). It does not copy the ISO byte by byte, it formats the drive and copies
the files.

- Write mode: **ISO mode** (extract files, recommended) or **DD mode** (raw copy)
- Partition scheme **GPT / MBR**, target **UEFI / BIOS (CSM) / BIOS + UEFI**
- File system **FAT32 / NTFS / exFAT**, volume label
- `install.wim` over 4 GiB on FAT32 is split automatically (`wimlib-imagex split`)
- NTFS/exFAT with UEFI: a 1 MiB UEFI:NTFS partition is added
  (`data/uefi-ntfs.img`, third-party GPL binary, Secure Boot signed; see `data/NOTICE.md`)
- Legacy BIOS: GRUB (`grub-install`) hands over to `bootmgr`
- Windows User Experience options (written to `Autounattend.xml` inside
  `boot.wim`, or to `sources\$OEM$\$$\Panther\unattend.xml`):
  remove the 4 GB RAM / Secure Boot / TPM 2.0 requirement, remove the online
  account requirement, create a local account, same regional options as this
  user (language + time zone), skip privacy questions, disable BitLocker
  auto-encryption, quality of life (no Teams / Outlook / Copilot / OneDrive),
  silent installation (erases the first disk of the target PC, with a
  safety net), "Windows UEFI CA 2023" signed boot loaders
- SHA256 check before erasing, optional file-by-file verification afterwards

Extra tools needed only for this page (Windows To Go also uses `sfdisk`, from util-linux): `wimlib-imagex` (wimlib-utils / wimtools),
`mkfs.ntfs` (ntfs-3g / ntfsprogs), `mkfs.exfat` (exfatprogs), `grub-install`
(grub2-pc / grub-pc-bin, BIOS targets only). `install.sh` installs them on a
best-effort basis; the page names the missing one before anything is erased.

Also included:

- Windows 11 24H2+ `setup.exe` wrapper (third-party GPL wrapper, see `data/NOTICE.md`, so the bypass options
  also apply to in-place upgrades started from the drive)
- UEFI:Validation: the drive checks its own files (MD5) at every UEFI boot
- Download official ISOs straight from Microsoft (needs `curl`). Microsoft changes this service often; it has not been tested
  against Microsoft's live servers. The "Open Microsoft's page" button is the fallback.

### Windows To Go (run Windows from the drive)

Choose **Windows To Go** in "Write mode". Windows does this with its own
`bcdboot.exe` and `dism.exe`; Ignite does the same steps without Windows:

- a 300 MB FAT32 boot partition and an NTFS partition (GPT+UEFI, or MBR with
  BIOS and/or UEFI); the drive must be at least 16 GB
- `wimlib-imagex apply` writes the chosen edition straight onto the NTFS volume
  (keeps security descriptors and streams)
- the boot files are copied from `Windows\Boot` the way bcdboot does it, and the
  BCD stores (`EFI\Microsoft\Boot\BCD` for UEFI, `Boot\BCD` for BIOS) are written
  by Ignite itself: `hive.rs` is a registry hive writer, `bcd.rs` builds the
  objects and elements a Windows-written store has
- `Windows\Panther\unattend.xml` applies the user experience options and keeps
  the PC's own disks offline (SAN policy)
- recovery environment is disabled

How far this is verified: the BCD files are read correctly by an independent
parser (hivex) and every value shared with a store written by Windows is
byte-identical; partition GUIDs/offsets in the BCD match what the kernel and
`blkid` report; applying a WIM to a real NTFS partition works. **Booting a real
Windows To Go drive has not been tried** (no Windows machine was available), so
treat this mode as experimental and report what you see. Details of the
two points most likely to need a fix are in `src/bcd.rs` (device element) and
`src/wtg.rs` (boot files).

Code: `winopts.rs` (options, validation, arguments, regional detection),
`unattend.rs` (answer file), `winworker.rs` (the privileged work),
`download.rs` (Microsoft ISO download), `md5.rs` (checksum list),
`hive.rs` + `bcd.rs` (BCD store writer), `wtg.rs` (Windows To Go). Test with a
loop device and an extracted tree: see `loop_device_end_to_end` in `winworker.rs`.

## Install (Fedora, Arch, Debian/Ubuntu, openSUSE)

```
./install.sh
```

The script detects your distribution family (from `/etc/os-release`), installs
what the program needs, builds it, and installs it with a menu entry
("Ignite") and icons.

| Family | Detected for | Package manager |
|---|---|---|
| Fedora | Fedora, Nobara, RHEL, Rocky, Alma, CentOS | `dnf` |
| Arch | Arch, Manjaro, EndeavourOS, CachyOS, ... | `pacman` |
| Debian | Debian, Ubuntu, Linux Mint, Pop!_OS, Kali, ... | `apt` |
| SUSE | openSUSE Tumbleweed / Leap / Slowroll, SLE | `zypper` |

Other systems (Alpine, Void, Gentoo, ...) are not automated: the script prints what
to install, then run `./install.sh --no-deps`.

Options:

```
./install.sh --no-deps       # skip the package-manager step
PREFIX=/usr ./install.sh     # install somewhere other than /usr/local
DRY_RUN=1 ./install.sh       # only print what would be done
DISTRO=arch ./install.sh     # force fedora | arch | debian | suse
./uninstall.sh               # remove the program (installed packages are kept)
./uninstall.sh --legacy      # also remove the older names (bootforge, gdd)
./uninstall.sh --clean-build # also delete the build folder (target/)
DRY_RUN=1 ./uninstall.sh     # show what would be removed
PREFIX=/usr ./uninstall.sh   # if you installed with that PREFIX
```

### Rust version

Cargo 1.85 or newer is required. The installer handles the usual traps:

- An old `cargo` from rustup in `~/.cargo/bin` shadowing the newer system one:
  the system one is used automatically.
- Debian/Ubuntu releases that ship an older Rust: if a newer
  `rustc-1.N`/`cargo-1.N` pair (N >= 85) exists in your repositories it is
  installed and used; otherwise the script stops and tells you to use rustup
  (https://rustup.rs).
- openSUSE Leap releases that ship an older Rust: the script stops and suggests
  `sudo zypper install rustup && rustup default stable` (Tumbleweed is recent
  enough out of the box).
- Arch with rustup installed: the `rust` package is skipped (the two conflict);
  make sure a toolchain is set with `rustup default stable`.

### Manual build

Packages (Fedora names; use your family's equivalents): Rust + Cargo, a C
compiler, pkg-config, fontconfig / libxkbcommon / wayland development files,
Vulkan loader + Mesa, xdg-desktop-portal, polkit (pkexec), udisks2, parted,
dosfstools, util-linux.

```
cargo build --release
./target/release/ignite
```

(`parted` and `dosfstools` are only needed for "Restore drive".)

## Architecture

### Files

```
ignite/
|-- Cargo.toml        dependencies: iced (GUI), rfd (file dialog)
|-- install.sh        packages + build + install (dnf, pacman, apt, zypper)
|-- uninstall.sh
|-- ignite.desktop    menu entry, plus the "Compact floating window" action
|-- data/icons/       master icon and hicolor PNGs (512 ... 32)
`-- src/
    |-- main.rs       entry point, iced UI: state, update loop, full + compact views
    |-- job.rs        everything that is not GUI: worker protocol, process
    |                 handling, progress/ETA maths, argument building, helpers
    |-- disks.rs      drive discovery through lsblk, system-mount protection
    |-- worker.rs     the privileged worker: checks, unmount, write, verify,
    |                 restore, eject
    `-- sha256.rs     SHA-256 (std only)
```

### Two processes

The window never touches a disk. It starts the same executable again as root
and talks to it over pipes:

```
 normal user                                   root (through pkexec)
+--------------------------+   arguments   +-----------------------------+
| ignite  (iced window)    | ------------> | ignite --worker ...         |
|  main.rs: state/update   |               |  worker.rs                  |
|  job.rs:  shared queue <-+-- stdout ---- |  STAGE / PROGRESS / DONE /  |
|                          | -- "cancel" ->|  ERROR lines                |
+--------------------------+    (stdin)    +-----------------------------+
```

- `pkexec` shows the polkit password prompt; only the worker runs as root.
- Worker output is one line per message: `STAGE text`, `PROGRESS phase done
  total` (phase = hash, write or verify), `DONE`, `ERROR text`.
- Reader threads (`job::spawn`) push parsed messages into a shared queue; the
  UI drains it every 100 ms (an iced timer subscription). Closing the pipe or
  sending `cancel` aborts the worker.
- A second thread rescans drives every 2 s (not while a job runs) and publishes
  the list only when it changed.

### What happens when you press Ignite

1. UI checks: the image exists, a drive is selected, the checksum field (if
   any) is 64 hex characters. Then an inline confirmation appears.
2. `job::spawn_worker` starts `pkexec ignite --worker --image ... --device ...`.
3. The worker re-validates everything as root (see below).
4. Optional: SHA-256 of the image is computed and compared BEFORE anything is
   erased; a mismatch stops the job with nothing written.
5. All partitions of the drive are unmounted (udisks first, then umount).
6. The image is copied in 4 MiB blocks with an fsync every 32 MiB, so the
   progress is real and write errors show up immediately.
7. Optional: caches are dropped and the drive is read back and compared with
   the image; the first differing byte is reported.
8. Optional: the drive is powered off through udisks. The worker prints DONE.
9. The UI turns the stream of PROGRESS lines into the bar, speed and ETA
   (`job::Meter`), and shows a green or red result.

"Restore drive" follows the same path but runs wipefs, writes an MBR with one
FAT32 partition (parted) and formats it (mkfs.vfat).

### Safety checks (in the worker, as root)

The GUI is never trusted. The worker refuses unless all of these hold:

- the device is under `/dev` and is a block device,
- lsblk reports TYPE `disk` (a partition is refused),
- it is removable/USB, unless "show non-removable disks" was ticked,
- none of its partitions is mounted at `/`, `/boot`, `/home`, `/usr`, `/var`,
  `/etc`, `/opt` or `/srv`,
- the image is not empty and fits on the device,
- every partition could be unmounted.

### Full and compact modes

Both views read the same `Ignite` state (image path, checksum, drive, options,
running job), so you can switch in the middle of a write. Switching asks iced
for the window id (`window::get_latest`), then resizes the window and changes
its level (`AlwaysOnTop` for compact). Not every Wayland compositor honours
"always on top"; then use the window menu's "Always on top" entry.

## Test safely first

Use a spare USB stick. Check the name with `lsblk` before pressing Ignite.
Unit tests (no hardware, no window needed; they cover the protocol, process
handling, cancellation, progress maths, checksum, lsblk parsing and the
system-mount rule; the window code in `main.rs` is not covered by unit tests):

```
cargo test
```

Worker only (no GUI), as root:

```
sudo ./target/release/ignite --worker --image image.iso --device /dev/sdX --verify
sudo ./target/release/ignite --worker --restore --device /dev/sdX
```

Messages printed: STAGE, PROGRESS, DONE or ERROR.

## Author and license

Made by [EvolveWare](https://github.com/EvolveWareSolution) - https://github.com/EvolveWareSolution/ignite

License: GPL-3.0-or-later (the bundled third-party binaries are GPL too; see `data/NOTICE.md`).
