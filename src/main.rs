//! Ignite - forge bootable USB drives from ISO images (a graphical, safer `dd`).
//!
//! The window runs as a normal user (iced). When you press "Ignite" or
//! "Restore", the same executable is started again through `pkexec` with
//! `--worker`, so only the small routines in worker.rs run as root.

mod disks;
mod download;
mod bcd;
mod hive;
mod job;
mod sha256;
mod md5;
mod unattend;
mod winopts;
mod winworker;
mod wtg;
mod worker;

use disks::Disk;
use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    button, checkbox, column, container, horizontal_space, image, pick_list, progress_bar, row,
    scrollable, text, text_input,
};
use iced::{
    event, window, Background, Border, Color, Element, Event, Font, Length, Size, Subscription,
    Task, Theme,
};
use job::{Meter, Msg, Shared, WriteOptions};
use winopts::{Fs, Regional, Scheme, Target, WinOptions, WriteMode};
use std::io::Write;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const LOGO: &[u8] = include_bytes!("../data/icons/hicolor/128x128/apps/ignite.png");
const WINDOW_ICON: &[u8] = include_bytes!("../data/icons/hicolor/256x256/apps/ignite.png");

fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--worker" {
        std::process::exit(worker::run(&args[2..]));
    }

    // `ignite --compact` starts in the small floating window.
    let compact = args.iter().any(|a| a == "--compact");

    iced::application(Ignite::title, Ignite::update, Ignite::view)
        .subscription(Ignite::subscription)
        .theme(Ignite::theme)
        .window(window_settings(compact))
        .antialiasing(true)
        .run_with(move || Ignite::new(compact))
}

const NORMAL_SIZE: Size = Size {
    width: 780.0,
    height: 920.0,
};
const COMPACT_SIZE: Size = Size {
    width: 440.0,
    height: 330.0,
};

fn window_settings(compact: bool) -> window::Settings {
    #[allow(unused_mut)]
    let mut settings = window::Settings {
        size: if compact { COMPACT_SIZE } else { NORMAL_SIZE },
        min_size: Some(Size::new(360.0, 260.0)),
        // The small window floats above other windows (X11; many Wayland
        // compositors ignore this, then use the title-bar menu "Always on top").
        level: if compact {
            window::Level::AlwaysOnTop
        } else {
            window::Level::Normal
        },
        position: window::Position::Centered,
        icon: window::icon::from_file_data(WINDOW_ICON, None).ok(),
        ..window::Settings::default()
    };
    // Lets Wayland match the window to ignite.desktop (menu icon, taskbar).
    #[cfg(target_os = "linux")]
    {
        settings.platform_specific.application_id = "ignite".to_string();
    }
    settings
}

// ------------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Write,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Page {
    Standard,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WinFlag {
    BypassHw,
    NoOnlineAccount,
    LocalUser,
    Regional,
    NoDataCollection,
    NoBitlocker,
    Qol,
    Silent,
    ModernSb,
    Validate,
}

enum ImageInfo {
    Empty,
    Missing,
    Ready { name: String, size: u64 },
}

enum Outcome {
    Success,
    Failed(String),
}

struct Ignite {
    shared: Arc<Shared>,
    theme: Theme,
    logo: image::Handle,

    image_path: String,
    image_info: ImageInfo,
    sha_input: String,
    disks: Vec<Disk>,
    selected: Option<Disk>,
    show_all: bool,
    verify: bool,
    eject: bool,
    compact: bool,

    page: Page,
    win_mode: WriteMode,
    win: WinOptions,
    edition_text: String,
    regional: Option<Regional>,

    dl_version: &'static str,
    dl_arch: &'static str,
    dl_fetched: Option<download::Fetched>,
    dl_lang: Option<download::Lang>,
    dl_fetching: bool,
    dl_downloading: bool,
    dl_cancel: Arc<std::sync::atomic::AtomicBool>,

    confirm: Option<Action>,
    notice: Option<String>,

    running: bool,
    stdin: Option<ChildStdin>,
    outcome: Option<Outcome>,
    meter: Meter,
    fraction: f32,
    bar_text: String,
    status: String,
    log: Vec<String>,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    ImageInput(String),
    Browse,
    ImagePicked(Option<PathBuf>),
    FileDropped(PathBuf),
    ShaInput(String),
    DiskSelected(Disk),
    Refresh,
    ShowAll(bool),
    Verify(bool),
    Eject(bool),
    AskWrite,
    AskRestore,
    ConfirmYes,
    ConfirmNo,
    Cancel,
    ToggleCompact,
    SetPage(Page),
    WinMode(WriteMode),
    WinScheme(Scheme),
    WinTarget(Target),
    WinFs(Fs),
    WinLabel(String),
    WinUser(String),
    WinEdition(String),
    WinFlag(WinFlag, bool),
    DlVersion(&'static str),
    DlArch(&'static str),
    DlFetch,
    DlLang(download::Lang),
    DlStart,
    DlCancel,
    DlOpenPage,
    OpenAuthor,
    /// The window id arrives asynchronously; the resize happens then.
    ApplyMode(Option<window::Id>),
}

async fn pick_image() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Select an image")
        .add_filter("Disk images", &["iso", "img", "raw"])
        .add_filter("All files", &["*"])
        .pick_file()
        .await
        .map(|f| f.path().to_path_buf())
}

impl Ignite {
    fn new(compact: bool) -> (Self, Task<Message>) {
        let shared = Shared::new();
        job::start_disk_poller(shared.clone());

        let theme = Theme::custom(
            "Ignite".to_string(),
            iced::theme::Palette {
                background: rgb(15, 16, 20),
                text: rgb(232, 234, 240),
                primary: rgb(249, 115, 22),
                success: rgb(34, 197, 94),
                danger: rgb(239, 68, 68),
                ..iced::theme::Palette::DARK
            },
        );

        let mut app = Ignite {
            shared,
            theme,
            logo: image::Handle::from_bytes(&LOGO[..]),
            image_path: String::new(),
            image_info: ImageInfo::Empty,
            sha_input: String::new(),
            disks: Vec::new(),
            selected: None,
            show_all: false,
            verify: true,
            eject: true,
            compact,
            page: Page::Standard,
            win_mode: WriteMode::Iso,
            win: WinOptions::default(),
            edition_text: "1".to_string(),
            regional: winopts::detect_regional(),
            dl_version: download::VERSIONS[0].0,
            dl_arch: download::ARCHS[0],
            dl_fetched: None,
            dl_lang: None,
            dl_fetching: false,
            dl_downloading: false,
            dl_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            confirm: None,
            notice: None,
            running: false,
            stdin: None,
            outcome: None,
            meter: Meter::new(),
            fraction: 0.0,
            bar_text: "Idle".to_string(),
            status: "Choose an image and a drive.".to_string(),
            log: Vec::new(),
        };
        app.reload_disks();
        (app, Task::none())
    }

    fn theme(&self) -> Theme {
        self.theme.clone()
    }

    /// The title shows the progress while a job runs, handy in the small window.
    fn title(&self) -> String {
        let name = if self.page == Page::Windows {
            "Ignite - Windows"
        } else {
            "Ignite"
        };
        if self.running && self.fraction > 0.0 {
            format!("{} - {:.0}%", name, self.fraction * 100.0)
        } else {
            name.to_string()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
            event::listen_with(|event, _status, _window| match event {
                Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
                _ => None,
            }),
        ])
    }

    // ------------------------------------------------------------- helpers

    fn log_line(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    fn set_image(&mut self, path: String) {
        let path = path.trim().to_string();
        self.image_info = if path.is_empty() {
            ImageInfo::Empty
        } else {
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => ImageInfo::Ready {
                    name: std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone()),
                    size: m.len(),
                },
                _ => ImageInfo::Missing,
            }
        };
        self.image_path = path;
        self.notice = None;
    }

    fn apply_disks(&mut self, list: Vec<Disk>) {
        self.disks = list;
        self.selected = self
            .selected
            .take()
            .and_then(|s| self.disks.iter().find(|d| d.path == s.path).cloned());
    }

    fn reload_disks(&mut self) {
        let list = disks::list_disks(self.show_all);
        self.apply_disks(list);
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Stage(t) => {
                self.log_line(t.clone());
                self.status = t;
            }
            Msg::Progress { phase, done, total } => {
                let r = self.meter.update(&phase, done, total);
                self.fraction = r.fraction;
                self.bar_text = r.describe();
            }
            Msg::Done => {
                self.outcome = Some(Outcome::Success);
                self.fraction = 1.0;
                self.bar_text = "Finished".to_string();
                self.status = "Finished successfully. You can remove the drive safely.".into();
                self.log_line("DONE: finished successfully.");
            }
            Msg::Error(e) if self.dl_downloading => {
                self.dl_downloading = false;
                self.shared.busy.store(false, Ordering::SeqCst);
                self.fraction = 0.0;
                self.bar_text = "Stopped".to_string();
                self.log_line(format!("ERROR: {}", e));
                self.status = format!("Download failed: {}", e);
            }
            Msg::Error(e) => {
                self.log_line(format!("ERROR: {}", e));
                self.status = format!("Error: {}", e);
                self.outcome = Some(Outcome::Failed(e));
            }
            Msg::Log(l) => self.log_line(l),
            Msg::Langs(r) => {
                self.dl_fetching = false;
                match r {
                    Ok(f) => {
                        self.dl_lang = download::pick_default_language(&f.langs)
                            .map(|i| f.langs[i].clone());
                        self.status = format!("{} languages available.", f.langs.len());
                        self.dl_fetched = Some(f);
                    }
                    Err(e) => {
                        self.dl_fetched = None;
                        self.dl_lang = None;
                        self.log_line(format!("ERROR: {}", e));
                        self.status = format!("Could not get the language list: {}", e);
                    }
                }
            }
            Msg::Downloaded(path) => {
                self.dl_downloading = false;
                self.shared.busy.store(false, Ordering::SeqCst);
                self.fraction = 0.0;
                self.bar_text = "Idle".to_string();
                self.log_line(format!("Downloaded: {}", path));
                self.status = format!("Downloaded {}. It is now selected as the image.", path);
                self.set_image(path);
            }
            Msg::Exit(code) => {
                self.running = false;
                self.stdin = None;
                self.shared.busy.store(false, Ordering::SeqCst);
                if self.outcome.is_none() {
                    let text = match code {
                        Some(126) => "Authentication was cancelled or not authorized.",
                        Some(127) => "pkexec could not run the worker (is polkit installed?).",
                        _ => "The worker stopped unexpectedly.",
                    };
                    self.log_line(text);
                    self.status = text.to_string();
                    self.outcome = Some(Outcome::Failed(text.to_string()));
                }
                if !matches!(self.outcome, Some(Outcome::Success)) {
                    self.fraction = 0.0;
                    self.bar_text = "Stopped".to_string();
                }
                self.reload_disks();
            }
        }
    }

    fn validate_write(&self) -> Result<Option<String>, String> {
        if !matches!(self.image_info, ImageInfo::Ready { .. }) {
            return Err("Select an existing image file first.".into());
        }
        if self.selected.is_none() {
            return Err("Select a drive first.".into());
        }
        let sha = job::extract_sha256(&self.sha_input)?;
        if self.windows_iso_mode() {
            self.win_options()?;
        }
        Ok(sha)
    }

    fn windows_iso_mode(&self) -> bool {
        self.page == Page::Windows && self.win_mode != WriteMode::Dd
    }

    /// The Windows options as typed, checked and cleaned up.
    fn win_options(&self) -> Result<WinOptions, String> {
        let mut o = self.win.clone();
        o.wtg = self.win_mode == WriteMode::Wtg;
        o.edition = if o.silent || o.wtg {
            self.edition_text
                .trim()
                .parse()
                .map_err(|_| "The image index must be a number (1 = first edition).".to_string())?
        } else {
            1
        };
        o.normalized()
    }

    fn start(&mut self, args: Vec<String>, info: Vec<String>) {
        match job::spawn_worker(&args, self.shared.clone()) {
            Ok(stdin) => {
                self.stdin = Some(stdin);
                self.running = true;
                self.shared.busy.store(true, Ordering::SeqCst);
                self.outcome = None;
                self.meter = Meter::new();
                self.fraction = 0.0;
                self.bar_text = "Waiting for authentication...".to_string();
                self.status = "Waiting for administrator authentication...".to_string();
                for l in info {
                    self.log_line(l);
                }
            }
            Err(e) => {
                let text = format!("Cannot start pkexec: {}", e);
                self.log_line(text.clone());
                self.status = text.clone();
                self.outcome = Some(Outcome::Failed(text));
            }
        }
    }

    // -------------------------------------------------------------- update

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                for m in self.shared.drain() {
                    self.handle(m);
                }
                if let Some(list) = self.shared.take_disks() {
                    if !self.running {
                        self.apply_disks(list);
                    }
                }
            }
            Message::ImageInput(s) => {
                if !self.running {
                    self.set_image(s);
                }
            }
            Message::Browse => {
                if !self.running {
                    return Task::perform(pick_image(), Message::ImagePicked);
                }
            }
            Message::ImagePicked(path) => {
                if let Some(p) = path {
                    self.set_image(p.to_string_lossy().into_owned());
                }
            }
            Message::FileDropped(p) => {
                if !self.running {
                    self.set_image(p.to_string_lossy().into_owned());
                }
            }
            Message::ShaInput(s) => {
                if !self.running {
                    self.sha_input = s;
                    self.notice = None;
                }
            }
            Message::DiskSelected(d) => {
                if !self.running {
                    self.selected = Some(d);
                    self.notice = None;
                }
            }
            Message::Refresh => {
                if !self.running {
                    self.reload_disks();
                }
            }
            Message::ShowAll(v) => {
                if !self.running {
                    self.show_all = v;
                    self.shared.show_all.store(v, Ordering::SeqCst);
                    self.reload_disks();
                }
            }
            Message::Verify(v) => self.verify = v,
            Message::Eject(v) => self.eject = v,
            Message::AskWrite => {
                self.notice = None;
                match self.validate_write() {
                    Ok(_) => self.confirm = Some(Action::Write),
                    Err(e) => self.notice = Some(e),
                }
            }
            Message::AskRestore => {
                self.notice = None;
                if self.selected.is_some() {
                    self.confirm = Some(Action::Restore);
                } else {
                    self.notice = Some("Select a drive first.".into());
                }
            }
            Message::ConfirmNo => self.confirm = None,
            Message::ConfirmYes => {
                let action = self.confirm.take();
                let Some(disk) = self.selected.clone() else {
                    return Task::none();
                };
                match action {
                    Some(Action::Write) => match self.validate_write() {
                        Ok(sha) => {
                            let mut args = job::write_args(&WriteOptions {
                                image: &self.image_path,
                                device: &disk.path,
                                verify: self.verify,
                                eject: self.eject,
                                any_disk: self.show_all,
                                sha256: sha.as_deref(),
                            });
                            let mut info = vec![
                                format!("Image : {}", self.image_path),
                                format!("Target: {}", disk.label()),
                            ];
                            if self.windows_iso_mode() {
                                match self.win_options() {
                                    Ok(o) => {
                                        args.extend(winopts::win_args(&o, self.regional.as_ref()));
                                        info.push(format!(
                                            "Windows media: {} / {} / {} / label {}",
                                            o.scheme, o.target, o.fs, o.label
                                        ));
                                    }
                                    Err(e) => {
                                        self.notice = Some(e);
                                        return Task::none();
                                    }
                                }
                            }
                            if sha.is_some() {
                                info.push(
                                    "SHA256 will be checked before anything is erased.".into(),
                                );
                            }
                            self.start(args, info);
                        }
                        Err(e) => self.notice = Some(e),
                    },
                    Some(Action::Restore) => {
                        let args = job::restore_args(&disk.path, self.eject, self.show_all);
                        self.start(args, vec![format!("Restoring: {}", disk.label())]);
                    }
                    None => {}
                }
            }
            Message::Cancel => {
                if let Some(s) = self.stdin.as_mut() {
                    let _ = s.write_all(b"cancel\n");
                    let _ = s.flush();
                    self.status = "Cancelling...".to_string();
                }
            }
            Message::SetPage(p) => {
                if !self.running {
                    self.page = p;
                    self.notice = None;
                    self.confirm = None;
                }
            }
            Message::WinMode(m) => {
                if !self.running {
                    self.win_mode = m;
                }
            }
            Message::WinScheme(sc) => {
                if !self.running {
                    self.win.scheme = sc;
                    if !sc.targets().contains(&self.win.target) {
                        self.win.target = sc.targets()[0];
                    }
                }
            }
            Message::WinTarget(t) => {
                if !self.running {
                    self.win.target = t;
                }
            }
            Message::WinFs(f) => {
                if !self.running {
                    self.win.fs = f;
                }
            }
            Message::WinLabel(l) => {
                if !self.running {
                    self.win.label = l;
                    self.notice = None;
                }
            }
            Message::WinUser(u) => {
                if !self.running {
                    self.win.username = u;
                    self.notice = None;
                }
            }
            Message::WinEdition(e) => {
                if !self.running {
                    self.edition_text = e;
                }
            }
            Message::WinFlag(flag, v) => {
                if !self.running {
                    let w = &mut self.win;
                    match flag {
                        WinFlag::BypassHw => w.bypass_hw = v,
                        WinFlag::NoOnlineAccount => w.no_online_account = v,
                        WinFlag::LocalUser => w.local_user = v,
                        WinFlag::Regional => w.regional = v,
                        WinFlag::NoDataCollection => w.no_data_collection = v,
                        WinFlag::NoBitlocker => w.no_bitlocker = v,
                        WinFlag::Qol => w.qol = v,
                        WinFlag::Silent => w.silent = v,
                        WinFlag::ModernSb => w.modern_secure_boot = v,
                        WinFlag::Validate => w.uefi_validation = v,
                    }
                }
            }
            Message::DlVersion(v) => {
                if !self.dl_fetching && !self.dl_downloading {
                    self.dl_version = v;
                    self.dl_fetched = None;
                    self.dl_lang = None;
                }
            }
            Message::DlArch(a) => {
                if !self.dl_downloading {
                    self.dl_arch = a;
                }
            }
            Message::DlLang(l) => self.dl_lang = Some(l),
            Message::DlFetch => {
                if !self.running && !self.dl_fetching && !self.dl_downloading {
                    let idx = download::VERSIONS
                        .iter()
                        .position(|(n, _)| *n == self.dl_version)
                        .unwrap_or(0);
                    self.dl_fetching = true;
                    self.dl_cancel.store(false, Ordering::SeqCst);
                    self.status = "Asking Microsoft for the available languages...".into();
                    let shared = self.shared.clone();
                    let cancel = self.dl_cancel.clone();
                    std::thread::spawn(move || {
                        shared.push(Msg::Langs(download::fetch_languages(idx, &cancel)));
                    });
                }
            }
            Message::DlStart => {
                if self.running || self.dl_downloading || self.dl_fetching {
                    return Task::none();
                }
                let (Some(f), Some(lang)) = (self.dl_fetched.clone(), self.dl_lang.clone()) else {
                    self.notice = Some("Fetch the languages and pick one first.".into());
                    return Task::none();
                };
                self.dl_downloading = true;
                self.notice = None;
                self.dl_cancel.store(false, Ordering::SeqCst);
                self.shared.busy.store(true, Ordering::SeqCst);
                self.outcome = None;
                self.meter = Meter::new();
                self.fraction = 0.0;
                self.bar_text = "Asking Microsoft for the download link...".into();
                self.status = self.bar_text.clone();
                let shared = self.shared.clone();
                let cancel = self.dl_cancel.clone();
                let arch = self.dl_arch;
                std::thread::spawn(move || {
                    match download::get_link(&f, &lang, arch, &cancel) {
                        Ok(url) => download::start_download(
                            url,
                            download::download_dir(),
                            shared,
                            cancel,
                        ),
                        Err(e) => shared.push(Msg::Error(e)),
                    }
                });
            }
            Message::DlCancel => {
                self.dl_cancel.store(true, Ordering::SeqCst);
                self.status = "Cancelling...".into();
            }
            Message::DlOpenPage => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(download::DOWNLOAD_PAGE)
                    .spawn();
            }
            Message::OpenAuthor => {
                let _ = std::process::Command::new("xdg-open")
                    .arg(AUTHOR_URL)
                    .spawn();
            }
            Message::ToggleCompact => {
                self.compact = !self.compact;
                return window::get_latest().map(Message::ApplyMode);
            }
            Message::ApplyMode(id) => {
                if let Some(id) = id {
                    let (size, level) = if self.compact {
                        (COMPACT_SIZE, window::Level::AlwaysOnTop)
                    } else {
                        (NORMAL_SIZE, window::Level::Normal)
                    };
                    return Task::batch([
                        window::resize(id, size),
                        window::change_level(id, level),
                    ]);
                }
            }
        }
        Task::none()
    }

    // ---------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Message> {
        if self.compact {
            self.view_compact()
        } else {
            self.view_full()
        }
    }

    fn view_full(&self) -> Element<'_, Message> {
        let busy = self.running;

        // ---- header
        let header = row![
            image(self.logo.clone()).width(56).height(56),
            column![
                text("Ignite").size(34),
                text(if self.page == Page::Windows {
                    "Windows installer media (ISO mode)"
                } else {
                    "Forge bootable USB drives from ISO images"
                })
                .size(14)
                .color(muted()),
            ]
            .spacing(2),
            horizontal_space(),
            button(
                text(if self.page == Page::Windows {
                    "Standard"
                } else {
                    "Windows"
                })
                .size(14)
            )
            .padding([8, 16])
            .style(rounded(if self.page == Page::Windows {
                button::secondary
            } else {
                button::primary
            }))
            .on_press_maybe((!busy).then_some(Message::SetPage(if self.page == Page::Windows {
                Page::Standard
            } else {
                Page::Windows
            }))),
            button(text("Compact").size(14))
                .padding([8, 16])
                .style(rounded(button::secondary))
                .on_press(Message::ToggleCompact),
        ]
        .spacing(16)
        .align_y(Vertical::Center);

        // ---- 1. image
        let mut path_input = text_input(
            "Path to an ISO / IMG file, or drop one onto this window",
            &self.image_path,
        )
        .padding(12)
        .size(15)
        .style(input_style);
        if !busy {
            path_input = path_input.on_input(Message::ImageInput);
        }
        let browse = button(text("Browse").size(15))
            .padding([12, 22])
            .style(rounded(button::secondary))
            .on_press_maybe((!busy).then_some(Message::Browse));

        let info_line: Element<'_, Message> = match &self.image_info {
            ImageInfo::Empty => text("No image selected").size(13).color(muted()).into(),
            ImageInfo::Missing => text("File not found")
                .size(13)
                .color(rgb(239, 68, 68))
                .into(),
            ImageInfo::Ready { name, size } => {
                text(format!("{}  -  {}", name, disks::human_size(*size)))
                    .size(13)
                    .color(rgb(34, 197, 94))
                    .into()
            }
        };

        let mut sha_input = text_input(
            "SHA256 (optional): paste the hash or the whole line from sha256sum",
            &self.sha_input,
        )
        .padding(12)
        .size(14)
        .style(input_style);
        if !busy {
            sha_input = sha_input.on_input(Message::ShaInput);
        }

        let image_card = card(
            "IMAGE",
            column![row![path_input, browse].spacing(10), info_line, sha_input,].spacing(10),
        );

        // ---- 2. drive
        let drives = pick_list(
            self.disks.as_slice(),
            self.selected.as_ref(),
            Message::DiskSelected,
        )
        .placeholder(if self.disks.is_empty() {
            "No removable drive found - insert a USB drive"
        } else {
            "Select a drive"
        })
        .width(Length::Fill)
        .padding(12)
        .style(picker_style);
        let refresh = button(text("Refresh").size(15))
            .padding([12, 22])
            .style(rounded(button::secondary))
            .on_press_maybe((!busy).then_some(Message::Refresh));

        let drive_card = card(
            "DRIVE",
            column![
                row![drives, refresh].spacing(10),
                check(
                    "Also show non-removable disks (dangerous; system disk stays protected)",
                    self.show_all,
                    busy,
                    Message::ShowAll
                ),
                text("Everything on the selected drive will be erased.")
                    .size(13)
                    .color(muted()),
            ]
            .spacing(10),
        );

        // ---- 3. options
        let options_card = card(
            "OPTIONS",
            column![
                check(
                    "Verify after writing (reads everything back)",
                    self.verify,
                    busy,
                    Message::Verify
                ),
                check(
                    "Power off the drive when finished (safe to unplug)",
                    self.eject,
                    busy,
                    Message::Eject
                ),
            ]
            .spacing(10),
        );

        // ---- actions (or the confirmation panel)
        let actions: Element<'_, Message> = match (self.confirm, &self.selected) {
            (Some(action), Some(disk)) => {
                let (title, detail, yes) = match action {
                    Action::Write if self.windows_iso_mode() && self.win.silent => (
                        "Erase this drive and enable SILENT INSTALL?",
                        format!(
                            "ALL DATA on {} will be destroyed. The drive will install Windows by itself, WITHOUT asking, and ERASE THE FIRST DISK of any PC that boots from it.",
                            disk.label()
                        ),
                        "Erase and write",
                    ),
                    Action::Write if self.page == Page::Windows && self.win_mode == WriteMode::Wtg => (
                        "Erase this drive and install Windows To Go?",
                        format!(
                            "ALL DATA on {} will be destroyed. The chosen Windows edition is written to the drive so a PC can run Windows from it.",
                            disk.label()
                        ),
                        "Erase and write",
                    ),
                    Action::Write => (
                        "Erase this drive?",
                        format!(
                            "ALL DATA on {} will be destroyed and replaced by {}.",
                            disk.label(),
                            match &self.image_info {
                                ImageInfo::Ready { name, .. } => name.as_str(),
                                _ => "the image",
                            }
                        ),
                        "Erase and write",
                    ),
                    Action::Restore => (
                        "Restore this drive?",
                        format!(
                            "ALL DATA on {} will be destroyed. It will become one normal FAT32 partition.",
                            disk.label()
                        ),
                        "Erase and restore",
                    ),
                };
                container(
                    column![
                        text(title).size(20).color(rgb(239, 68, 68)),
                        text(detail).size(14),
                        row![
                            button(text("Cancel").size(15).align_x(Horizontal::Center))
                                .width(Length::Fill)
                                .padding(12)
                                .style(rounded(button::secondary))
                                .on_press(Message::ConfirmNo),
                            button(text(yes).size(15).align_x(Horizontal::Center))
                                .width(Length::Fill)
                                .padding(12)
                                .style(rounded(button::danger))
                                .on_press(Message::ConfirmYes),
                        ]
                        .spacing(10),
                    ]
                    .spacing(12),
                )
                .padding(18)
                .width(Length::Fill)
                .style(banner(rgb(239, 68, 68)))
                .into()
            }
            _ => {
                let ignite = button(
                    text("Ignite")
                        .size(20)
                        .width(Length::Fill)
                        .align_x(Horizontal::Center),
                )
                .width(Length::FillPortion(3))
                .padding(16)
                .style(rounded(button::primary))
                .on_press_maybe((!busy).then_some(Message::AskWrite));
                let restore = button(
                    text("Restore drive")
                        .size(15)
                        .width(Length::Fill)
                        .align_x(Horizontal::Center),
                )
                .width(Length::FillPortion(1))
                .padding(16)
                .style(rounded(button::secondary))
                .on_press_maybe((!busy).then_some(Message::AskRestore));
                let mut r = row![ignite, restore].spacing(10);
                if busy {
                    r = r.push(
                        button(text("Cancel").size(15).align_x(Horizontal::Center))
                            .padding(16)
                            .style(rounded(button::danger))
                            .on_press(Message::Cancel),
                    );
                }
                r.into()
            }
        };

        // ---- progress / status / log
        let bar = progress_bar(0.0..=1.0, self.fraction)
            .height(12)
            .style(bar_style);

        let banner_view: Option<Element<'_, Message>> = match &self.outcome {
            Some(Outcome::Success) => Some(
                container(text(self.status.as_str()).size(14))
                    .padding(12)
                    .width(Length::Fill)
                    .style(banner(rgb(34, 197, 94)))
                    .into(),
            ),
            Some(Outcome::Failed(reason)) => Some(
                container(text(format!("Failed: {}", reason)).size(14))
                    .padding(12)
                    .width(Length::Fill)
                    .style(banner(rgb(239, 68, 68)))
                    .into(),
            ),
            None => None,
        };

        let log_lines = column(self.log.iter().rev().take(200).map(|l| {
            Element::from(
                text(l.as_str())
                    .size(12)
                    .font(Font::MONOSPACE)
                    .color(muted()),
            )
        }))
        .spacing(2);
        let log_view = container(scrollable(
            container(log_lines).padding(10).width(Length::Fill),
        ))
        .height(150)
        .width(Length::Fill)
        .style(log_style);

        let mut progress_col = column![
            row![text(self.bar_text.as_str()).size(14), horizontal_space(),],
            bar,
        ]
        .spacing(10);
        if let Some(b) = banner_view {
            progress_col = progress_col.push(b);
        } else {
            progress_col = progress_col.push(text(self.status.as_str()).size(13).color(muted()));
        }
        progress_col = progress_col.push(log_view);

        let mut content = column![header, image_card, drive_card].spacing(16);
        if self.page == Page::Windows {
            content = content.push(self.windows_cards(busy));
        }
        content = content.push(options_card);
        if let Some(n) = &self.notice {
            content = content.push(text(n.as_str()).size(14).color(rgb(239, 68, 68)));
        }
        content = content
            .push(actions)
            .push(card("PROGRESS", progress_col))
            .push(
                text("Root access is requested only for the copy itself (through polkit).")
                    .size(12)
                    .color(muted()),
            )
            .push(
                button(text("Built by EvolveWare").size(12).color(muted()))
                    .padding(0)
                    .style(button::text)
                    .on_press(Message::OpenAuthor),
            );

        scrollable(
            container(container(content).max_width(760).padding(24))
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        )
        .into()
    }

    /// Download an official Windows ISO from Microsoft.
    fn download_card(&self, busy: bool) -> Element<'_, Message> {
        let versions: Vec<&'static str> = download::VERSIONS.iter().map(|v| v.0).collect();
        let idle = !busy && !self.dl_fetching && !self.dl_downloading;
        let mut top = row![
            pick_list(versions, Some(self.dl_version), Message::DlVersion)
                .padding(10)
                .style(picker_style),
            pick_list(&download::ARCHS[..], Some(self.dl_arch), Message::DlArch)
                .padding(10)
                .style(picker_style),
            button(text(if self.dl_fetching { "Asking Microsoft..." } else { "Get languages" }).size(15))
                .padding([10, 18])
                .style(rounded(button::secondary))
                .on_press_maybe(idle.then_some(Message::DlFetch)),
        ]
        .spacing(10)
        .align_y(Vertical::Center);
        if let Some(f) = &self.dl_fetched {
            top = top.push(
                pick_list(f.langs.as_slice(), self.dl_lang.clone(), Message::DlLang)
                    .padding(10)
                    .width(Length::Fill)
                    .style(picker_style),
            );
        }
        let mut actions = row![].spacing(10);
        actions = actions.push(
            button(text("Download ISO").size(15))
                .padding([10, 18])
                .style(rounded(button::primary))
                .on_press_maybe((idle && self.dl_lang.is_some()).then_some(Message::DlStart)),
        );
        if self.dl_downloading {
            actions = actions.push(
                button(text("Cancel download").size(15))
                    .padding([10, 18])
                    .style(rounded(button::danger))
                    .on_press(Message::DlCancel),
            );
        }
        actions = actions.push(
            button(text("Open Microsoft's page in the browser").size(13))
                .padding([10, 14])
                .style(rounded(button::secondary))
                .on_press(Message::DlOpenPage),
        );
        card(
            "DOWNLOAD A WINDOWS ISO (optional)",
            column![
                top,
                actions,
                text(format!(
                    "Official ISOs straight from Microsoft's servers, saved to {} and selected as the image. Needs curl. Microsoft changes its download service often; if this fails, use the browser page.",
                    download::download_dir().display()
                ))
                .size(12)
                .color(muted()),
            ]
            .spacing(10),
        )
    }

    /// The extra cards of the Windows page.
    fn windows_cards(&self, busy: bool) -> Element<'_, Message> {
        fn picker_row<'a>(label: &'static str, p: Element<'a, Message>) -> Element<'a, Message> {
            row![text(label).size(14).width(150), p]
                .spacing(10)
                .align_y(Vertical::Center)
                .into()
        }

        let mode = pick_list(
            &WriteMode::ALL[..],
            Some(self.win_mode),
            Message::WinMode,
        )
        .width(Length::Fill)
        .padding(10)
        .style(picker_style);

        let iso = self.win_mode != WriteMode::Dd;
        let wtg = self.win_mode == WriteMode::Wtg;
        let mut drive_col = column![picker_row("Write mode", mode.into())].spacing(10);

        if iso {
            let scheme = pick_list(&Scheme::ALL[..], Some(self.win.scheme), Message::WinScheme)
                .width(Length::Fill)
                .padding(10)
                .style(picker_style);
            let target = pick_list(
                self.win.scheme.targets(),
                Some(self.win.target),
                Message::WinTarget,
            )
            .width(Length::Fill)
            .padding(10)
            .style(picker_style);
            let fs = pick_list(&Fs::ALL[..], Some(self.win.fs), Message::WinFs)
                .width(Length::Fill)
                .padding(10)
                .style(picker_style);
            let mut label = text_input(winopts::DEFAULT_LABEL, &self.win.label)
                .padding(10)
                .size(14)
                .style(input_style);
            if !busy {
                label = label.on_input(Message::WinLabel);
            }
            drive_col = drive_col
                .push(picker_row("Partition scheme", scheme.into()))
                .push(picker_row("Target system", target.into()));
            if !wtg {
                drive_col = drive_col.push(picker_row("File system", fs.into()));
            }
            drive_col = drive_col.push(picker_row("Volume label", label.into()));
            let mut hint = String::new();
            if wtg {
                hint.push_str("Windows runs from the drive: a 300 MB FAT32 boot partition plus an NTFS partition holding the chosen edition of the image (needs a fast USB 3 drive of at least 16 GB). ");
                if self.win.target.wants_bios() {
                    hint.push_str("Legacy BIOS boot uses GRUB, which hands over to the Windows boot manager.");
                }
                drive_col = drive_col.push(text(hint).size(12).color(muted()));
                hint = String::new();
            }
            let wtg_skip = wtg;
            if !wtg_skip && self.win.fs != Fs::Fat32 && self.win.target.wants_uefi() {
                hint.push_str("UEFI cannot read NTFS/exFAT by itself: a small UEFI:NTFS partition is added (Secure Boot may need third-party certificates enabled). ");
            }
            if !wtg_skip && self.win.fs == Fs::Fat32 {
                hint.push_str("FAT32 boots everywhere; install.wim over 4 GiB is split automatically. ");
            }
            if !wtg_skip && self.win.target.wants_bios() {
                hint.push_str("Legacy BIOS boot uses GRUB, which hands over to the Windows boot manager.");
            }
            if !hint.is_empty() {
                drive_col = drive_col.push(text(hint).size(12).color(muted()));
            }
        } else {
            drive_col = drive_col.push(
                text("The image is copied byte by byte. Windows ISOs are not hybrid, so this will normally not boot; use ISO mode for Windows.")
                    .size(12)
                    .color(muted()),
            );
        }
        let drive_card = card("WINDOWS DRIVE", drive_col);
        let dl_card = self.download_card(busy);
        if !iso {
            return column![dl_card, drive_card].spacing(16).into();
        }

        let w = &self.win;
        let regional_label = match &self.regional {
            Some(r) => format!("Use the same regional options as this user ({})", r),
            None => "Use the same regional options as this user (not detected)".to_string(),
        };
        let mut ux = column![].spacing(10);
        if !wtg {
            ux = ux.push(wcheck(
                "Remove requirement for 4GB+ RAM, Secure Boot and TPM 2.0",
                w.bypass_hw,
                busy,
                WinFlag::BypassHw,
            ));
        }
        ux = ux
            .push(wcheck(
                "Remove requirement for an online Microsoft account",
                w.no_online_account,
                busy,
                WinFlag::NoOnlineAccount,
            ))
            .push(wcheck(
                "Create a local account with username",
                w.local_user,
                busy,
                WinFlag::LocalUser,
            ));
        if wtg {
            let mut ed = text_input("1", &self.edition_text)
                .padding(10)
                .size(14)
                .width(80)
                .style(input_style);
            if !busy {
                ed = ed.on_input(Message::WinEdition);
            }
            ux = ux.push(
                row![
                    text("Edition to put on the drive (image index in install.wim)").size(14),
                    ed
                ]
                .spacing(10)
                .align_y(Vertical::Center),
            );
            ux = ux.push(
                text("The PC's own disks are kept offline (SAN policy) so this Windows cannot touch them.")
                    .size(12)
                    .color(muted()),
            );
        }
        if w.local_user {
            let mut name = text_input("User name", &w.username)
                .padding(10)
                .size(14)
                .style(input_style);
            if !busy {
                name = name.on_input(Message::WinUser);
            }
            ux = ux.push(row![horizontal_space().width(30), name]);
            ux = ux.push(
                text("The account starts with an empty password; Windows asks for a new one at the first logon.")
                    .size(12)
                    .color(muted()),
            );
        }
        let regional_on = w.regional && self.regional.is_some();
        ux = ux
            .push(wcheck_enabled(
                regional_label,
                regional_on,
                busy || self.regional.is_none(),
                WinFlag::Regional,
            ))
            .push(wcheck(
                "Disable data collection (skip privacy questions)",
                w.no_data_collection,
                busy,
                WinFlag::NoDataCollection,
            ))
            .push(wcheck(
                "Disable BitLocker automatic device encryption",
                w.no_bitlocker,
                busy,
                WinFlag::NoBitlocker,
            ))
            .push(wcheck(
                "Quality of life: no Teams, Outlook, Copilot, OneDrive and similar nags",
                w.qol,
                busy,
                WinFlag::Qol,
            ))
            .push(wcheck(
                "Use 'Windows UEFI CA 2023' signed boot loaders (needs PCs that trust that certificate)",
                w.modern_secure_boot,
                busy,
                WinFlag::ModernSb,
            ))
            ;
        if !wtg {
            ux = ux
                .push(wcheck(
                    "UEFI:Validation: the drive checks its own files (MD5) at every UEFI boot",
                    w.uefi_validation,
                    busy,
                    WinFlag::Validate,
                ))
                .push(wcheck(
                    "Silent installation: install on the first disk without asking (ERASES it)",
                    w.silent,
                    busy,
                    WinFlag::Silent,
                ));
        }
        if w.silent && !wtg {
            let mut ed = text_input("1", &self.edition_text)
                .padding(10)
                .size(14)
                .width(80)
                .style(input_style);
            if !busy {
                ed = ed.on_input(Message::WinEdition);
            }
            ux = ux.push(
                row![
                    horizontal_space().width(30),
                    text("Edition (image index in install.wim)").size(14),
                    ed
                ]
                .spacing(10)
                .align_y(Vertical::Center),
            );
            ux = ux.push(
                container(
                    text("DANGER: a PC that boots this drive will wipe its first disk and install Windows with no questions. If it sees more or fewer disks than one internal disk plus this USB drive, Setup falls back to its normal screens.")
                        .size(13),
                )
                .padding(12)
                .width(Length::Fill)
                .style(banner(rgb(239, 68, 68))),
            );
        }
        let ux_card = card("WINDOWS USER EXPERIENCE", ux);
        column![dl_card, drive_card, ux_card].spacing(16).into()
    }

    /// Small floating window: image, drive, one button, one progress line.
    /// It shares all state with the full window (path, checksum, options, job).
    fn view_compact(&self) -> Element<'_, Message> {
        let busy = self.running;

        let header = row![
            image(self.logo.clone()).width(28).height(28),
            text(if self.page == Page::Windows {
                "Ignite - Windows"
            } else {
                "Ignite"
            })
            .size(20),
            horizontal_space(),
            button(text("Expand").size(13))
                .padding([6, 12])
                .style(rounded(button::secondary))
                .on_press(Message::ToggleCompact),
        ]
        .spacing(10)
        .align_y(Vertical::Center);

        let image_label: Element<'_, Message> = match &self.image_info {
            ImageInfo::Empty => text("Drop an image here, or press Browse")
                .size(13)
                .color(muted())
                .into(),
            ImageInfo::Missing => text("File not found")
                .size(13)
                .color(rgb(239, 68, 68))
                .into(),
            ImageInfo::Ready { name, size } => text(format!(
                "{}  ({})",
                job::ellipsize(name, 26),
                disks::human_size(*size)
            ))
            .size(13)
            .color(rgb(34, 197, 94))
            .into(),
        };
        let browse = button(text("Browse").size(13))
            .padding([6, 14])
            .style(rounded(button::secondary))
            .on_press_maybe((!busy).then_some(Message::Browse));
        let image_row = row![container(image_label).width(Length::Fill), browse]
            .spacing(8)
            .align_y(Vertical::Center);

        let drives = pick_list(
            self.disks.as_slice(),
            self.selected.as_ref(),
            Message::DiskSelected,
        )
        .placeholder(if self.disks.is_empty() {
            "No removable drive found"
        } else {
            "Select a drive"
        })
        .width(Length::Fill)
        .padding(8)
        .text_size(13)
        .style(picker_style);
        let rescan = button(text("Rescan").size(13))
            .padding([6, 12])
            .style(rounded(button::secondary))
            .on_press_maybe((!busy).then_some(Message::Refresh));
        let drive_row = row![drives, rescan].spacing(8).align_y(Vertical::Center);

        let options = row![
            check("Verify", self.verify, busy, Message::Verify),
            check("Power off when done", self.eject, busy, Message::Eject),
        ]
        .spacing(18);

        let actions: Element<'_, Message> = match (self.confirm, &self.selected) {
            (Some(action), Some(disk)) => {
                let (title, yes) = match action {
                    Action::Write => ("Erase and write", "Erase"),
                    Action::Restore => ("Erase and restore", "Restore"),
                };
                container(
                    column![
                        text(format!("{} {}?", title, disk.path))
                            .size(14)
                            .color(rgb(239, 68, 68)),
                        text(format!(
                            "{} - ALL DATA will be lost",
                            disks::human_size(disk.size)
                        ))
                        .size(12)
                        .color(muted()),
                        row![
                            button(text("Cancel").size(13).align_x(Horizontal::Center))
                                .width(Length::Fill)
                                .padding(8)
                                .style(rounded(button::secondary))
                                .on_press(Message::ConfirmNo),
                            button(text(yes).size(13).align_x(Horizontal::Center))
                                .width(Length::Fill)
                                .padding(8)
                                .style(rounded(button::danger))
                                .on_press(Message::ConfirmYes),
                        ]
                        .spacing(8),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .width(Length::Fill)
                .style(banner(rgb(239, 68, 68)))
                .into()
            }
            _ => {
                let mut r = row![button(
                    text("Ignite")
                        .size(16)
                        .width(Length::Fill)
                        .align_x(Horizontal::Center),
                )
                .width(Length::Fill)
                .padding(10)
                .style(rounded(button::primary))
                .on_press_maybe((!busy).then_some(Message::AskWrite))]
                .spacing(8);
                if busy {
                    r = r.push(
                        button(text("Cancel").size(13))
                            .padding(10)
                            .style(rounded(button::danger))
                            .on_press(Message::Cancel),
                    );
                }
                r.into()
            }
        };

        let bar = progress_bar(0.0..=1.0, self.fraction)
            .height(8)
            .style(bar_style);
        let line: Element<'_, Message> = match (&self.outcome, &self.notice) {
            (_, Some(n)) => text(n.as_str()).size(12).color(rgb(239, 68, 68)).into(),
            (Some(Outcome::Success), _) => text("Done. You can remove the drive.")
                .size(12)
                .color(rgb(34, 197, 94))
                .into(),
            (Some(Outcome::Failed(reason)), _) => {
                text(format!("Failed: {}", job::ellipsize(reason, 60)))
                    .size(12)
                    .color(rgb(239, 68, 68))
                    .into()
            }
            (None, None) => text(job::ellipsize(&self.status, 60))
                .size(12)
                .color(muted())
                .into(),
        };
        let progress = column![text(self.bar_text.as_str()).size(12), bar, line].spacing(6);

        scrollable(
            column![header, image_row, drive_row, options, actions, progress]
                .spacing(10)
                .padding(14),
        )
        .into()
    }
}

// ------------------------------------------------------------------ styling

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

/// The project page, opened by the "Built by EvolveWare" link.
const AUTHOR_URL: &str = "https://github.com/EvolveWareSolution/ignite";

fn muted() -> Color {
    rgb(150, 156, 170)
}

fn card<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let body: Element<'a, Message> = body.into();
    container(column![text(title).size(12).color(muted()), body].spacing(12))
        .padding(18)
        .width(Length::Fill)
        .style(card_style)
        .into()
}

fn check<'a>(
    label: &'a str,
    value: bool,
    busy: bool,
    on_toggle: fn(bool) -> Message,
) -> Element<'a, Message> {
    let mut c = checkbox(label, value).size(20).text_size(15);
    if !busy {
        c = c.on_toggle(on_toggle);
    }
    c.into()
}

fn wcheck<'a>(label: &'a str, value: bool, busy: bool, flag: WinFlag) -> Element<'a, Message> {
    wcheck_enabled(label, value, busy, flag)
}

fn wcheck_enabled<'a>(
    label: impl Into<String>,
    value: bool,
    disabled: bool,
    flag: WinFlag,
) -> Element<'a, Message> {
    let mut c = checkbox(label, value).size(20).text_size(15);
    if !disabled {
        c = c.on_toggle(move |v| Message::WinFlag(flag, v));
    }
    c.into()
}

fn card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rgb(23, 26, 33))),
        border: Border {
            color: rgb(38, 43, 54),
            width: 1.0,
            radius: 14.0.into(),
        },
        ..container::Style::default()
    }
}

fn log_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rgb(14, 16, 20))),
        border: Border {
            color: rgb(38, 43, 54),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..container::Style::default()
    }
}

fn banner(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(Color { a: 0.12, ..color })),
        border: Border {
            color: Color { a: 0.55, ..color },
            width: 1.0,
            radius: 12.0.into(),
        },
        ..container::Style::default()
    }
}

fn bar_style(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(rgb(38, 43, 54)),
        bar: Background::Color(rgb(249, 115, 22)),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
    }
}

fn rounded(
    base: fn(&Theme, button::Status) -> button::Style,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = base(theme, status);
        style.border.radius = 10.0.into();
        style
    }
}

fn input_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = text_input::default(theme, status);
    style.border.radius = 10.0.into();
    style
}

fn picker_style(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let mut style = pick_list::default(theme, status);
    style.border.radius = 10.0.into();
    style
}
