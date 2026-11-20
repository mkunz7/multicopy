#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod drives;
mod eject;
mod transfer;
mod transfer_log;

use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering::Relaxed, mpsc},
    time::{Duration, Instant},
};
use transfer::{Job, Location, Progress};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const THEME_KEY: &str = "theme";

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Theme {
    Light,
    #[default]
    Dark,
}

impl Theme {
    fn stored(storage: Option<&dyn eframe::Storage>) -> Self {
        match storage
            .and_then(|storage| storage.get_string(THEME_KEY))
            .as_deref()
        {
            Some("light") => Self::Light,
            _ => Self::Dark,
        }
    }

    fn apply(self, ctx: &egui::Context) {
        ctx.set_theme(match self {
            Self::Light => egui::Theme::Light,
            Self::Dark => egui::Theme::Dark,
        });
        let mut style = (*ctx.style()).clone();
        let mut visuals = match self {
            Self::Light => egui::Visuals::light(),
            Self::Dark => egui::Visuals::dark(),
        };
        match self {
            Self::Light => {
                visuals.panel_fill = Color32::from_rgb(248, 249, 252);
                visuals.window_fill = Color32::WHITE;
                visuals.extreme_bg_color = Color32::from_rgb(235, 238, 244);
                visuals.window_stroke.color = Color32::from_rgb(202, 208, 220);
            }
            Self::Dark => {
                visuals.panel_fill = Color32::from_rgb(29, 29, 35);
                visuals.window_fill = Color32::from_rgb(38, 38, 46);
                visuals.extreme_bg_color = Color32::from_rgb(23, 23, 28);
                visuals.window_stroke.color = Color32::from_rgb(57, 57, 66);
                visuals.override_text_color = Some(Color32::from_rgb(241, 241, 246));
                visuals.widgets.active.bg_fill = Color32::from_rgb(52, 69, 105);
                visuals.widgets.hovered.bg_fill = Color32::from_rgb(60, 61, 74);
                visuals.widgets.inactive.bg_fill = Color32::from_rgb(48, 48, 58);
            }
        }
        style.visuals = visuals;
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        ctx.set_style(style);
    }
}

fn accent(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(106, 159, 255)
    } else {
        Color32::from_rgb(37, 87, 181)
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "MultiCopy",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1120.0, 740.0])
                .with_min_inner_size([850.0, 560.0]),
            ..Default::default()
        },
        Box::new(|cc| {
            let theme = Theme::stored(cc.storage);
            theme.apply(&cc.egui_ctx);
            Ok(Box::new(App {
                theme,
                ..App::new()
            }))
        }),
    )
}

struct Source {
    path: PathBuf,
    folder: String,
}

enum Status {
    Queued,
    Active {
        result: mpsc::Receiver<std::io::Result<()>>,
        since: Instant,
    },
    Complete(Duration),
    Canceled,
    Failed(String),
}

struct Transfer {
    job: Job,
    progress: Arc<Progress>,
    status: Status,
}

impl Transfer {
    fn pending(&self) -> bool {
        matches!(self.status, Status::Active { .. } | Status::Queued)
    }

    fn cancel(&mut self) {
        match self.status {
            Status::Queued => self.status = Status::Canceled,
            Status::Active { .. } => self.progress.canceled.store(true, Relaxed),
            _ => {}
        }
    }

    fn retry(&mut self) {
        self.job.detect_duplicates = true;
        self.progress = Arc::new(Progress::default());
        self.status = Status::Queued;
    }
}

struct App {
    drives: Vec<drives::Drive>,
    sources: Vec<Source>,
    destinations: Vec<PathBuf>,
    transfers: Vec<Transfer>,
    used_sources: Vec<Location>,
    ejecting: Option<EjectOperation>,
    eject_report: Option<eject::Report>,
    show_drives: bool,
    theme: Theme,
    show_about: bool,
    verify: bool,
    detect_duplicates: bool,
    error: Option<String>,
    confirm_close: bool,
    exiting: bool,
}

struct EjectOperation {
    result: mpsc::Receiver<eject::Report>,
    draft_disks: HashMap<PathBuf, u32>,
}

impl App {
    fn new() -> Self {
        Self {
            drives: drives::list(),
            sources: Vec::new(),
            destinations: Vec::new(),
            transfers: Vec::new(),
            used_sources: Vec::new(),
            ejecting: None,
            eject_report: None,
            show_drives: true,
            theme: Theme::Dark,
            show_about: false,
            verify: true,
            detect_duplicates: true,
            error: None,
            confirm_close: false,
            exiting: false,
        }
    }

    fn add_source(&mut self, path: PathBuf) {
        if self.sources.iter().any(|source| source.path == path) {
            return;
        }
        let base = if drives::is_drive_root(&path) {
            drive_folder_name(&path)
        } else {
            path.file_stem()
                .filter(|_| path.is_file())
                .or_else(|| path.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let base = if transfer::valid_folder(&base) {
            base
        } else {
            "Offload".into()
        };
        let mut folder = base.clone();
        let mut suffix = 2;
        while self
            .sources
            .iter()
            .any(|item| item.folder.eq_ignore_ascii_case(&folder))
        {
            folder = format!("{base}-{suffix}");
            if !transfer::valid_folder(&folder) {
                folder = format!("Offload-{suffix}");
            }
            suffix += 1;
        }
        self.sources.push(Source { path, folder });
    }

    fn queue(&mut self) -> std::io::Result<()> {
        let mut names = HashSet::new();
        if self.sources.iter().any(|source| {
            !transfer::valid_folder(&source.folder) || !names.insert(source.folder.to_lowercase())
        }) {
            return Err(std::io::Error::other(
                "Enter a valid, distinct backup folder name for each source.",
            ));
        }
        let destinations = self
            .destinations
            .iter()
            .map(|path| Location::new(path))
            .collect::<std::io::Result<Vec<_>>>()?;
        let sources = self
            .sources
            .iter()
            .map(|source| {
                Ok(Job {
                    source: Location::new(&source.path)?,
                    destinations: destinations.clone(),
                    folder: source.folder.clone(),
                    verify: self.verify,
                    detect_duplicates: self.detect_duplicates,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        for job in sources {
            if !self
                .used_sources
                .iter()
                .any(|source| source.volume.id == job.source.volume.id)
            {
                self.used_sources.push(job.source.clone());
            }
            self.transfers.push(Transfer {
                job,
                progress: Arc::new(Progress::default()),
                status: Status::Queued,
            });
        }
        self.sources.clear();
        Ok(())
    }

    fn start_eject(&mut self, ctx: &egui::Context, sources: bool) {
        let source_paths = self.sources.iter().map(|source| &source.path);
        let draft_volumes: HashMap<_, _> = source_paths
            .clone()
            .chain(self.destinations.iter())
            .filter_map(|path| {
                Location::new(path)
                    .ok()
                    .map(|location| (path.clone(), location.volume))
            })
            .collect();
        let paths: Vec<_> = if sources {
            source_paths.collect()
        } else {
            self.destinations.iter().collect()
        };
        let mut requests: Vec<_> = paths
            .into_iter()
            .map(|path| eject::Request {
                volume_id: draft_volumes
                    .get(path)
                    .filter(|volume| volume.disk.is_some())
                    .map(|volume| volume.id.clone())
                    .unwrap_or_default(),
                label: display_path(path),
            })
            .collect();
        if sources {
            for source in &self.used_sources {
                if requests
                    .iter()
                    .all(|request| request.volume_id != source.volume.id)
                {
                    requests.push(eject::Request {
                        volume_id: source.volume.id.clone(),
                        label: display_path(&source.path),
                    });
                }
            }
        }
        let draft_disks = draft_volumes
            .into_iter()
            .filter_map(|(path, volume)| volume.disk.map(|disk| (path, disk)))
            .collect();
        let (sender, result) = mpsc::channel();
        let ctx = ctx.clone();
        self.eject_report = None;
        match std::thread::Builder::new()
            .name("eject-drives".into())
            .spawn(move || {
                let _ = sender.send(eject::run(requests));
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.ejecting = Some(EjectOperation {
                    result,
                    draft_disks,
                })
            }
            Err(error) => {
                self.eject_report = Some(eject::Report {
                    skipped: vec![error.to_string()],
                    ..Default::default()
                })
            }
        }
    }

    fn finish_eject(&mut self) {
        let Some(operation) = &self.ejecting else {
            return;
        };
        let report = match operation.result.try_recv() {
            Ok(report) => report,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => eject::Report {
                skipped: vec!["The eject operation ended unexpectedly.".into()],
                ..Default::default()
            },
        };
        let was_ejected = |path: &Path| {
            operation
                .draft_disks
                .get(path)
                .is_some_and(|disk| report.ejected.contains(disk))
        };
        self.sources.retain(|source| !was_ejected(&source.path));
        self.destinations.retain(|path| !was_ejected(path));
        self.used_sources.retain(|source| {
            source
                .volume
                .disk
                .is_none_or(|disk| !report.ejected.contains(&disk))
        });
        self.drives = drives::list();
        self.eject_report = Some(report);
        self.ejecting = None;
    }

    fn advance(&mut self, ctx: &egui::Context) {
        for transfer in &mut self.transfers {
            if let Status::Active { result, since } = &transfer.status {
                let status = match result.try_recv() {
                    Ok(Ok(())) => Status::Complete(since.elapsed()),
                    Ok(Err(error))
                        if error.kind() == std::io::ErrorKind::Interrupted
                            && transfer.progress.canceled.load(Relaxed) =>
                    {
                        Status::Canceled
                    }
                    Ok(Err(error)) => Status::Failed(error.to_string()),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Status::Failed("Transfer worker stopped unexpectedly.".into())
                    }
                    Err(mpsc::TryRecvError::Empty) => continue,
                };
                transfer.status = status;
            }
        }

        let mut active: HashSet<_> = self
            .transfers
            .iter()
            .filter_map(|item| {
                matches!(item.status, Status::Active { .. })
                    .then(|| item.job.source.volume.lane.clone())
            })
            .collect();
        let mut outputs: Vec<_> = self
            .transfers
            .iter()
            .filter(|item| matches!(item.status, Status::Active { .. }))
            .flat_map(|item| item.job.output_roots())
            .collect();

        for transfer in &mut self.transfers {
            if !matches!(transfer.status, Status::Queued)
                || !active.insert(transfer.job.source.volume.lane.clone())
            {
                continue;
            }
            let roots: Vec<_> = transfer.job.output_roots().collect();
            if roots.iter().any(|root| {
                outputs
                    .iter()
                    .any(|active| transfer::paths_overlap(root, active))
            }) {
                continue;
            }
            let (sender, result) = mpsc::channel();
            let job = transfer.job.clone();
            let progress = Arc::clone(&transfer.progress);
            let ctx = ctx.clone();
            match std::thread::Builder::new()
                .name("media-offload".into())
                .spawn(move || {
                    let _ = sender.send(transfer::run(&job, &progress));
                    ctx.request_repaint();
                }) {
                Ok(_) => {
                    outputs.extend(roots);
                    transfer.status = Status::Active {
                        result,
                        since: Instant::now(),
                    }
                }
                Err(error) => transfer.status = Status::Failed(error.to_string()),
            }
        }

        if self.transfers.iter().any(Transfer::pending) {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn source_panel(&mut self, ui: &mut egui::Ui) {
        panel_heading(ui, "Sources");
        let mut removed = None;
        let mut replacement = None;
        for (index, source) in self.sources.iter_mut().enumerate() {
            card(ui).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if drives::is_drive_root(&source.path) {
                    replacement = location_folder_menu(ui, &source.path, "Source folder…")
                        .map(|path| (index, path))
                        .or(replacement.take());
                }
                ui.vertical_centered(|ui| {
                    location_icon(ui, drives::is_drive_root(&source.path));
                    path_label(ui, &source.path);
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new("Backup folder")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.push_id(index, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut source.folder)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    if ui.small_button("Remove").clicked() {
                        removed = Some(index);
                    }
                });
            });
        }
        if let Some(index) = removed {
            self.sources.remove(index);
        } else if let Some((index, path)) = replacement {
            if self.sources[index].path != path {
                self.sources.remove(index);
                let previous_count = self.sources.len();
                self.add_source(path);
                let added = self.sources.len() - previous_count;
                self.sources[index..].rotate_right(added);
            }
        }
        card(ui).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.menu_button("+  Add source", |ui| {
                    if ui.button("Browse for folders…").clicked() {
                        ui.close_menu();
                        for path in rfd::FileDialog::new().pick_folders().unwrap_or_default() {
                            self.add_source(path);
                        }
                    }
                    if ui.button("Browse for files…").clicked() {
                        ui.close_menu();
                        for path in rfd::FileDialog::new().pick_files().unwrap_or_default() {
                            self.add_source(path);
                        }
                    }
                    ui.separator();
                    if let Some(path) = drive_options(ui, &self.drives) {
                        self.add_source(path);
                    }
                });
            });
        });
    }

    fn add_destination(&mut self, path: PathBuf) {
        if !self.destinations.contains(&path) {
            self.destinations.push(path);
        }
    }

    fn destination_panel(&mut self, ui: &mut egui::Ui) {
        panel_heading(ui, "Destinations");
        let mut removed = None;
        let mut replacement = None;
        for (index, path) in self.destinations.iter().enumerate() {
            card(ui).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if drives::is_drive_root(path) {
                    replacement = location_folder_menu(ui, path, "Destination folder…")
                        .map(|path| (index, path))
                        .or(replacement.take());
                }
                ui.vertical_centered(|ui| {
                    location_icon(ui, drives::is_drive_root(path));
                    path_label(ui, path);
                    if ui.small_button("Remove").clicked() {
                        removed = Some(index);
                    }
                });
            });
        }
        if let Some(index) = removed {
            self.destinations.remove(index);
        } else if let Some((index, path)) = replacement {
            if self.destinations[index] != path {
                if self.destinations.contains(&path) {
                    self.destinations.remove(index);
                } else {
                    self.destinations[index] = path;
                }
            }
        }
        card(ui).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.menu_button("+  Add destination", |ui| {
                    if ui.button("Browse for folders…").clicked() {
                        ui.close_menu();
                        for path in rfd::FileDialog::new().pick_folders().unwrap_or_default() {
                            self.add_destination(path);
                        }
                    }
                    ui.separator();
                    if let Some(path) = drive_options(ui, &self.drives) {
                        self.add_destination(path);
                    }
                });
            });
        });
    }

    fn drive_view(&mut self, ui: &mut egui::Ui) {
        if self.drives.is_empty() {
            ui.label(
                RichText::new("No drives detected. Connect a drive and refresh.")
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }

        let mut selected = None;
        ui.horizontal_wrapped(|ui| {
            for drive in &self.drives {
                let id = ui.make_persistent_id(&drive.path);
                let hovering = ui.ctx().read_response(id).is_some_and(|response| {
                    ui.input(|input| {
                        input
                            .pointer
                            .hover_pos()
                            .is_some_and(|pos| response.rect.contains(pos))
                    })
                });
                card(ui).show(ui, |ui| {
                    ui.set_width(124.0);
                    ui.vertical(|ui| {
                        let color = if hovering {
                            ui.visuals().strong_text_color()
                        } else {
                            ui.visuals().weak_text_color()
                        };
                        if drive.free.is_some() {
                            card_menu(ui, color, |ui| {
                                for (role, source) in
                                    [("Source folder…", true), ("Destination folder…", false)]
                                {
                                    if ui.button(role).clicked() {
                                        ui.close_menu();
                                        selected = pick_folder(&drive.path, role)
                                            .map(|path| (path, source));
                                    }
                                }
                            });
                        } else {
                            ui.add_space(24.0);
                        }
                        let body = |ui: &mut egui::Ui| {
                            ui.vertical_centered(|ui| {
                                location_icon(ui, true);
                                ui.strong(display_path(&drive.path));
                                if !drive.label.is_empty() {
                                    ui.add(egui::Label::new(&drive.label).truncate())
                                        .on_hover_text(&drive.label);
                                }
                                ui.label(
                                    RichText::new(drive.free.map_or_else(
                                        || "Unavailable".into(),
                                        |bytes| format!("{} free", size(bytes)),
                                    ))
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                                );
                            });
                        };
                        if drive.free.is_some() {
                            ui.dnd_drag_source(id, drive.path.clone(), body);
                        } else {
                            body(ui);
                        }
                    });
                });
            }
        });
        if let Some((path, source)) = selected {
            if source {
                self.add_source(path);
            } else {
                self.add_destination(path);
            }
        }
    }

    fn transfer_controls(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.error {
            ui.colored_label(Color32::LIGHT_RED, error);
        }
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.verify, "Verify transfer")
                .on_hover_text("Compare each destination with the source using XXH64 and write a transfer log.");
            ui.checkbox(&mut self.detect_duplicates, "Duplicate detection")
                .on_hover_text("Reuse backup folders and skip existing files that match byte for byte.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = !self.exiting
                    && self.ejecting.is_none()
                    && !self.sources.is_empty()
                    && !self.destinations.is_empty();
                let count = self.sources.len();
                let label = if count <= 1 {
                    "Queue transfer".into()
                } else {
                    format!("Queue {count} transfers")
                };
                let fill = if enabled {
                    Color32::from_rgb(47, 103, 209)
                } else {
                    ui.visuals().window_fill
                };
                let text = RichText::new(label).strong().color(if enabled {
                    Color32::WHITE
                } else {
                    ui.visuals().text_color()
                });
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(text)
                            .fill(fill)
                            .min_size(egui::vec2(132.0, 32.0)),
                    )
                    .clicked()
                {
                    match self.queue() {
                        Ok(()) => {
                            self.error = None;
                            self.show_drives = false;
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
            });
        });
    }

    fn queue_view(&mut self, ui: &mut egui::Ui) {
        let active = self
            .transfers
            .iter()
            .filter(|item| matches!(item.status, Status::Active { .. }))
            .count();
        let waiting = self
            .transfers
            .iter()
            .filter(|item| matches!(item.status, Status::Queued))
            .count();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Transfer queue").size(15.0).strong());
            ui.label(
                RichText::new(format!("{active} active  ·  {waiting} waiting"))
                    .color(ui.visuals().weak_text_color()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.transfers.iter().any(|item| !item.pending()),
                        egui::Button::new("Clear finished"),
                    )
                    .clicked()
                {
                    self.transfers.retain(Transfer::pending);
                }
            });
        });
        ui.add_space(10.0);
        if self.transfers.is_empty() {
            card(ui).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.strong("No transfers queued");
                ui.label(RichText::new("Jobs run in order on each source disk. Different source disks can copy at the same time.").color(ui.visuals().weak_text_color()));
            });
        }
        for transfer in &mut self.transfers {
            show_transfer(ui, transfer, !self.exiting && self.ejecting.is_none());
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let theme = match self.theme {
            Theme::Light => "light",
            Theme::Dark => "dark",
        };
        storage.set_string(THEME_KEY, theme.to_owned());
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.finish_eject();
        self.advance(ctx);
        let pending = self.transfers.iter().any(Transfer::pending);
        let ejecting = self.ejecting.is_some();
        if ejecting && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if ctx.input(|input| input.viewport().close_requested()) && pending {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = !self.exiting;
        }
        if self.exiting && !pending {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let visuals = ctx.style().visuals.clone();
        let chrome = visuals.extreme_bg_color;
        let workspace = visuals.panel_fill;
        let border = visuals.window_stroke.color;

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(chrome)
                    .inner_margin(egui::Margin::symmetric(18, 9)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("MultiCopy").size(17.0).strong());
                    ui.add_space(14.0);
                    ui.selectable_value(&mut self.show_drives, true, "Drives");
                    ui.selectable_value(&mut self.show_drives, false, "Transfers");
                    ui.menu_button("Settings", |ui| {
                        ui.strong("Appearance");
                        let previous_theme = self.theme;
                        ui.radio_value(&mut self.theme, Theme::Light, "Light");
                        ui.radio_value(&mut self.theme, Theme::Dark, "Dark");
                        if self.theme != previous_theme {
                            self.theme.apply(ctx);
                        }
                        ui.separator();
                        if ui.button("About MultiCopy").clicked() {
                            self.show_about = true;
                            ui.close_menu();
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!ejecting, egui::Button::new("Refresh drives"))
                            .clicked()
                        {
                            self.drives = drives::list();
                        }
                        let can_eject = !pending && !ejecting && !self.exiting;
                        let has_sources = !self.sources.is_empty() || !self.used_sources.is_empty();
                        let has_destinations = !self.destinations.is_empty();
                        for (label, sources, has_drives) in [
                            ("Eject all destination drives", false, has_destinations),
                            ("Eject all source drives", true, has_sources),
                        ] {
                            let button =
                                ui.add_enabled(can_eject && has_drives, egui::Button::new(label));
                            let button = if pending {
                                button.on_disabled_hover_text(
                                    "Wait for queued and active transfers to finish.",
                                )
                            } else {
                                button
                            };
                            if button.clicked() {
                                self.start_eject(ctx, sources);
                            }
                        }
                    });
                });
                if self.ejecting.is_some() {
                    ui.label("Waiting for Windows to eject drives…");
                }
                if let Some(report) = &self.eject_report {
                    let summary = format!(
                        "{} ejected · {} skipped",
                        report.ejected.len(),
                        report.skipped.len()
                    );
                    let dismissed = ui
                        .horizontal(|ui| {
                            ui.label(summary);
                            if !report.skipped.is_empty() {
                                ui.menu_button("Details", |ui| {
                                    egui::ScrollArea::vertical()
                                        .max_height(200.0)
                                        .show(ui, |ui| {
                                            for reason in &report.skipped {
                                                ui.label(reason);
                                            }
                                        });
                                });
                            }
                            ui.small_button("Dismiss").clicked()
                        })
                        .inner;
                    if dismissed {
                        self.eject_report = None;
                    }
                }
            });

        egui::TopBottomPanel::bottom("version")
            .frame(
                egui::Frame::new()
                    .fill(chrome)
                    .inner_margin(egui::Margin::symmetric(14, 5)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("Version {VERSION}"))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            });

        for (side, id) in [
            (egui::panel::Side::Left, "sources"),
            (egui::panel::Side::Right, "destinations"),
        ] {
            egui::SidePanel::new(side, id)
                .resizable(false)
                .exact_width(203.0)
                .frame(
                    egui::Frame::new()
                        .fill(chrome)
                        .inner_margin(egui::Margin::same(14))
                        .stroke(egui::Stroke::new(1.0_f32, border)),
                )
                .show(ctx, |ui| {
                    if ejecting {
                        ui.disable();
                    }
                    let drop_rect = ui.available_rect_before_wrap();
                    egui::ScrollArea::vertical()
                        .id_salt(id)
                        .auto_shrink([false; 2])
                        .show(ui, |ui| match side {
                            egui::panel::Side::Left => self.source_panel(ui),
                            egui::panel::Side::Right => self.destination_panel(ui),
                        });
                    let target =
                        ui.interact(drop_rect, ui.id().with("drive-drop"), egui::Sense::hover());
                    if target.dnd_hover_payload::<PathBuf>().is_some() {
                        ui.painter().rect_stroke(
                            drop_rect,
                            8,
                            egui::Stroke::new(2.0_f32, accent(ui)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if !ejecting && let Some(path) = target.dnd_release_payload::<PathBuf>() {
                        match side {
                            egui::panel::Side::Left => self.add_source((*path).clone()),
                            egui::panel::Side::Right => self.add_destination((*path).clone()),
                        }
                    }
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(workspace))
            .show(ctx, |ui| {
                egui::TopBottomPanel::bottom("transfer-controls")
                    .frame(
                        egui::Frame::new()
                            .fill(chrome)
                            .inner_margin(egui::Margin::symmetric(16, 10))
                            .stroke(egui::Stroke::new(1.0_f32, border)),
                    )
                    .show_inside(ui, |ui| self.transfer_controls(ui));

                egui::Frame::new()
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("workspace")
                            .auto_shrink([false; 2])
                            .show(ui, |ui| {
                                if self.show_drives {
                                    panel_heading(ui, "Available drives");
                                    ui.label(
                                        RichText::new("Drag a drive to Sources or Destinations.")
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                    ui.add_enabled_ui(!ejecting, |ui| self.drive_view(ui));
                                } else {
                                    self.queue_view(ui);
                                }
                            });
                    });
            });
        if self.confirm_close {
            egui::Window::new("Transfers in progress")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Cancel pending transfers and close MultiCopy?");
                    ui.horizontal(|ui| {
                        if ui.button("Keep working").clicked() {
                            self.confirm_close = false;
                        }
                        if ui.button("Cancel transfers and close").clicked() {
                            self.confirm_close = false;
                            self.exiting = true;
                            for transfer in &mut self.transfers {
                                transfer.cancel();
                            }
                        }
                    });
                });
        }
        if self.show_about {
            egui::Window::new("About MultiCopy")
                .open(&mut self.show_about)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.heading(format!("MultiCopy {VERSION}"));
                    ui.label(transfer_log::os_version());
                    ui.separator();
                    ui.label("Created with ChatGPT");
                });
        }
    }
}

fn show_transfer(ui: &mut egui::Ui, item: &mut Transfer, can_retry: bool) {
    card(ui).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        let (label, color) = match &item.status {
            Status::Queued => ("Waiting", ui.visuals().weak_text_color()),
            Status::Active { .. } if item.progress.canceled.load(Relaxed) => {
                ("Canceling", Color32::LIGHT_YELLOW)
            }
            Status::Active { .. } => ("Running", accent(ui)),
            Status::Complete(_) => ("Complete", accent(ui)),
            Status::Canceled => ("Canceled", ui.visuals().weak_text_color()),
            Status::Failed(_) => ("Failed", Color32::LIGHT_RED),
        };
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&item.job.folder).size(18.0).strong()).truncate(),
            )
            .on_hover_text(&item.job.folder);
            ui.label(RichText::new(label).color(color).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if item.pending()
                    && !item.progress.canceled.load(Relaxed)
                    && ui.button("Cancel").clicked()
                {
                    item.cancel();
                }
                if matches!(item.status, Status::Failed(_) | Status::Canceled)
                    && ui
                        .add_enabled(can_retry, egui::Button::new("Retry"))
                        .on_hover_text("Retry with duplicate detection enabled.")
                        .clicked()
                {
                    item.retry();
                    ui.ctx().request_repaint();
                }
            });
        });
        let source = display_path(&item.job.source.path);
        ui.add(
            egui::Label::new(
                RichText::new(format!("From  {source}")).color(ui.visuals().weak_text_color()),
            )
            .truncate(),
        )
        .on_hover_text(source);

        let outputs: Vec<_> = item
            .job
            .destinations
            .iter()
            .map(|destination| destination.path.join(&item.job.folder))
            .collect();
        let summary = outputs
            .iter()
            .map(|path| display_path(path))
            .collect::<Vec<_>>()
            .join("  ·  ");
        ui.add(
            egui::Label::new(
                RichText::new(format!("To      {summary}")).color(ui.visuals().weak_text_color()),
            )
            .truncate(),
        )
        .on_hover_text(summary);

        let bytes = item.progress.read.load(Relaxed);
        let total = item.progress.total.load(Relaxed);
        let files = item.progress.committed.load(Relaxed);
        let reused = item.progress.reused.load(Relaxed);
        match &item.status {
            Status::Active { since, .. } => {
                let fraction = if total == 0 {
                    0.0
                } else {
                    bytes as f32 / total as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .fill(accent(ui))
                        .text(format!("{} / {} read", size(bytes), size(total))),
                );
                let rate = bytes as f64 / since.elapsed().as_secs_f64().max(0.1);
                ui.label(format!(
                    "{}   ·   {}/s avg",
                    item.progress.detail.lock().unwrap(),
                    size(rate as u64)
                ));
            }
            Status::Complete(elapsed) => {
                let checked = if reused > 0 {
                    "completed"
                } else if item.job.verify {
                    "verified"
                } else {
                    "copied"
                };
                ui.label(format!(
                    "{files} files {checked}  ·  {}  ·  {:.1}s",
                    size(total),
                    elapsed.as_secs_f64()
                ));
            }
            Status::Failed(error) => {
                ui.colored_label(Color32::LIGHT_RED, error);
                ui.label(format!(
                    "{files} files completed on all destinations before failure."
                ));
            }
            Status::Canceled if files > 0 => {
                ui.label(format!("{files} files completed before cancellation."));
            }
            _ => {}
        }
        if reused > 0 {
            ui.label(format!(
                "{reused} existing destination copies matched and skipped"
            ));
        }

        if matches!(item.status, Status::Complete(_)) {
            ui.menu_button("Open backup", |ui| {
                for path in outputs {
                    if ui.button(display_path(&path)).clicked() {
                        open_backup(&path);
                        ui.close_menu();
                    }
                }
            });
        }
    });
}

fn open_backup(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe")
        .arg(display_path(path))
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(path)
        .spawn();
}

fn drive_folder_name(path: &Path) -> String {
    #[cfg(windows)]
    return format!(
        "Card-{}",
        path.display().to_string().trim_end_matches([':', '\\'])
    );

    #[cfg(target_os = "macos")]
    path.file_name().map_or_else(
        || "Startup disk".into(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn pick_folder(drive: &Path, title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title(title)
        .set_directory(drive)
        .pick_folder()
}

fn location_folder_menu(ui: &mut egui::Ui, drive: &Path, label: &str) -> Option<PathBuf> {
    card_menu(ui, ui.visuals().weak_text_color(), |ui| {
        if ui.button(label).clicked() {
            ui.close_menu();
            pick_folder(drive, label)
        } else {
            None
        }
    })
    .flatten()
}

fn card_menu<R>(
    ui: &mut egui::Ui,
    color: Color32,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 24.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            let menu = ui.menu_button("   ", contents);
            let center = menu.response.rect.center();
            let color = if menu.response.hovered() {
                ui.visuals().strong_text_color()
            } else {
                color
            };
            for offset in [-4.0, 0.0, 4.0] {
                ui.painter().line_segment(
                    [
                        center + egui::vec2(-6.0, offset),
                        center + egui::vec2(6.0, offset),
                    ],
                    egui::Stroke::new(1.5_f32, color),
                );
            }
            menu.response.on_hover_text("Drive folders");
            menu.inner
        },
    )
    .inner
}

fn panel_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(15.0).strong());
    ui.add_space(6.0);
}

fn path_label(ui: &mut egui::Ui, path: &Path) {
    let full = display_path(path);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| full.as_str().into());
    ui.add(egui::Label::new(RichText::new(name).strong()).truncate())
        .on_hover_text(&full);
    if path.file_name().is_some() {
        ui.add(
            egui::Label::new(
                RichText::new(&full)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            )
            .truncate(),
        )
        .on_hover_text(full);
    }
}

fn location_icon(ui: &mut egui::Ui, drive: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(62.0, 42.0), egui::Sense::hover());
    let p = |x, y| rect.min + egui::vec2(x, y);
    let painter = ui.painter();
    if drive {
        painter.rect_filled(
            egui::Rect::from_min_max(p(3.0, 7.0), p(59.0, 36.0)),
            6,
            Color32::from_rgb(145, 156, 175),
        );
        painter.rect_filled(
            egui::Rect::from_min_max(p(6.0, 22.0), p(56.0, 33.0)),
            3,
            Color32::from_rgb(57, 65, 79),
        );
        painter.line_segment(
            [p(13.0, 27.5), p(30.0, 27.5)],
            egui::Stroke::new(2.0_f32, Color32::from_rgb(116, 128, 148)),
        );
        painter.circle_filled(p(49.0, 27.5), 2.0, accent(ui));
    } else {
        painter.add(egui::Shape::convex_polygon(
            vec![
                p(4.0, 9.0),
                p(24.0, 9.0),
                p(31.0, 16.0),
                p(56.0, 16.0),
                p(56.0, 35.0),
                p(4.0, 35.0),
            ],
            Color32::from_rgb(170, 122, 51),
            egui::Stroke::NONE,
        ));
        painter.rect_filled(
            egui::Rect::from_min_max(p(4.0, 17.0), p(59.0, 37.0)),
            4,
            Color32::from_rgb(224, 177, 91),
        );
    }
}

fn drive_options(ui: &mut egui::Ui, drives: &[drives::Drive]) -> Option<PathBuf> {
    if drives.is_empty() {
        ui.label("No drives detected");
    }
    for drive in drives {
        let free = drive.free.map_or_else(
            || "unavailable".into(),
            |bytes| format!("{} free", size(bytes)),
        );
        let label = format!("{} {}  ·  {free}", drive.path.display(), drive.label);
        if ui
            .add_enabled(drive.free.is_some(), egui::Button::new(label))
            .clicked()
        {
            ui.close_menu();
            return Some(drive.path.clone());
        }
    }
    None
}

fn card(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(ui.visuals().window_fill)
        .stroke(ui.visuals().window_stroke)
        .corner_radius(8)
        .inner_margin(egui::Margin::same(12))
}

fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).into()
    }
}

fn size(bytes: u64) -> String {
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_runs_distinct_lanes_and_serializes_shared_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("camera");
        let destination = tmp.path().join("backup");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source.join("clip"), b"video").unwrap();
        let source = Location::new(&source).unwrap();
        let destination = Location::new(&destination).unwrap();
        let mut app = App::new();

        for (folder, lane) in [
            ("first", "disk A"),
            ("second", "disk A"),
            ("third", "disk B"),
            ("first", "disk C"),
        ] {
            let mut source = source.clone();
            source.volume.lane = lane.into();
            app.transfers.push(Transfer {
                job: Job {
                    source,
                    destinations: vec![destination.clone()],
                    folder: folder.into(),
                    verify: false,
                    detect_duplicates: true,
                },
                progress: Arc::new(Progress::default()),
                status: Status::Queued,
            });
        }

        let ctx = egui::Context::default();
        app.advance(&ctx);
        assert!(matches!(app.transfers[0].status, Status::Active { .. }));
        assert!(matches!(app.transfers[1].status, Status::Queued));
        assert!(matches!(app.transfers[2].status, Status::Active { .. }));
        assert!(matches!(app.transfers[3].status, Status::Queued));

        let deadline = Instant::now() + Duration::from_secs(10);
        while app.transfers.iter().any(Transfer::pending) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.advance(&ctx);
        }
        assert!(
            app.transfers
                .iter()
                .all(|item| matches!(item.status, Status::Complete(_)))
        );
        assert_eq!(app.transfers[3].progress.reused.load(Relaxed), 1);
    }
}
