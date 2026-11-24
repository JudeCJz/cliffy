//! Compact window content for Cliffy.
//!
//! The layout follows `D:\Downloads\base ui.html`: dark flat rows, no in-window
//! header, no status line, and no nested cards. The engine owns capture state
//! and notifications; this module only paints and sends commands.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{mpsc::Sender, Arc};
use std::time::{Duration, Instant};

use egui::{
    pos2, vec2, Align, Align2, Color32, ComboBox, CornerRadius, DragValue, FontFamily, FontId,
    Label, Layout, Pos2, RichText, Sense, Shape, Stroke, StrokeKind, TextStyle, Theme, Ui,
    ViewportCommand,
};

use windows::core::w;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
};

use crate::config::{self, Config, Resolution};
use crate::engine::AppShared;
use crate::hotkeys::HotkeyThread;
use crate::types::Command;

const BG_BASE: Color32 = Color32::from_rgb(5, 5, 5);
const BG_SURFACE: Color32 = Color32::from_rgb(10, 10, 10);
const BG_ELEVATED: Color32 = Color32::from_rgb(18, 18, 18);
const BG_HOVER: Color32 = Color32::from_rgb(26, 26, 26);
const BORDER_SUBTLE: Color32 = Color32::from_rgb(30, 30, 30);
const BORDER_STRONG: Color32 = Color32::from_rgb(46, 46, 46);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(245, 245, 245);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(163, 163, 163);
const TEXT_MUTED: Color32 = Color32::from_rgb(102, 102, 102);
const ACCENT_PRIMARY: Color32 = Color32::from_rgb(239, 68, 68);
const ACCENT_HOVER: Color32 = Color32::from_rgb(220, 38, 38);
const DANGER: Color32 = Color32::from_rgb(239, 68, 68);
const DANGER_DARK: Color32 = Color32::from_rgb(185, 28, 28);
const WHITE: Color32 = Color32::from_rgb(255, 255, 255);

const ROW_H: f32 = 26.0;
const CONTROL_GAP: f32 = 10.0;
const LABEL_COLUMN_W: f32 = 150.0;
const RESOLUTION_CONTROL_W: f32 = 220.0;
const NUMBER_CONTROL_W: f32 = 70.0;
const TOGGLE_W: f32 = 34.0;
const BUTTON_H: f32 = 28.0;
const FOOTER_GAP: f32 = 6.0;
const COMMIT_DELAY: Duration = Duration::from_millis(250);
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Icon {
    Circle,
    Square,
    Pause,
    RefreshCw,
    FolderOpen,
    Check,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResChoice {
    P480,
    P720,
    P1080,
    Custom,
}

impl ResChoice {
    fn from_resolution(res: Resolution) -> Self {
        match res {
            Resolution::P480 => Self::P480,
            Resolution::P720 => Self::P720,
            Resolution::P1080 => Self::P1080,
            Resolution::Native | Resolution::P1440 | Resolution::Custom => Self::Custom,
        }
    }

    fn resolution(self) -> Resolution {
        match self {
            Self::P480 => Resolution::P480,
            Self::P720 => Resolution::P720,
            Self::P1080 => Resolution::P1080,
            Self::Custom => Resolution::Custom,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::Custom => "Custom",
        }
    }
}

pub struct App {
    cfg: Config,
    cmd: Sender<Command>,
    shared: Arc<AppShared>,
    hotkeys: Option<HotkeyThread>,
    dirty: bool,
    last_edit: Option<Instant>,
    retry_at: Option<Instant>,
    refresh_hz: u32,
    capture_exclusion_done: bool,
    hidden: bool,
    restoring: bool,
    startup_hide_pending: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        cmd: Sender<Command>,
        shared: Arc<AppShared>,
        hotkeys: Option<HotkeyThread>,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_theme(Theme::Dark);
        setup_style(&ctx);

        if let Ok(mut guard) = shared.ctx.lock() {
            *guard = Some(ctx.clone());
        }
        shared.repaint();

        Self {
            cfg,
            cmd,
            shared,
            hotkeys,
            dirty: false,
            last_edit: None,
            retry_at: None,
            refresh_hz: config::monitor_refresh_hz().max(5),
            capture_exclusion_done: false,
            hidden: true,
            restoring: false,
            startup_hide_pending: true,
        }
    }

    fn send_command(&self, command: Command) {
        self.shared.toast_sound.store(false, Ordering::Relaxed);
        let _ = self.cmd.send(command);
    }

    fn mark_edited(&mut self) {
        self.dirty = true;
        self.last_edit = Some(Instant::now());
    }

    /// Apply local UI changes after the user pauses briefly.
    fn sync_changes(&mut self, ctx: &egui::Context) {
        if let Some(retry_at) = self.retry_at {
            let now = Instant::now();
            if retry_at > now {
                ctx.request_repaint_after(retry_at - now);
                return;
            }
            self.retry_at = None;
            let due = now.checked_sub(COMMIT_DELAY).unwrap_or(now);
            self.last_edit = Some(self.last_edit.map_or(due, |last| last.max(due)));
        }

        if self.dirty {
            if let Some(last_edit) = self.last_edit {
                let elapsed = last_edit.elapsed();
                if elapsed >= COMMIT_DELAY {
                    self.apply_changes(false);
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint_after(COMMIT_DELAY - elapsed);
                }
            }
        }
    }

    fn sync_viewport(&mut self, ctx: &egui::Context) {
        if self.startup_hide_pending {
            self.startup_hide_pending = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            return;
        }

        if self.shared.hide_requested.swap(false, Ordering::AcqRel) {
            self.hidden = true;
            self.restoring = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            return;
        }

        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && !self.shared.quit_requested.load(Ordering::Acquire) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hidden = true;
            self.restoring = false;
            return;
        }

        if self.shared.show_requested.swap(false, Ordering::AcqRel) {
            self.hidden = false;
            self.restoring = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            ctx.request_repaint();
        }

        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if self.restoring {
            if !minimized {
                self.restoring = false;
            }
            return;
        }
        if minimized && !self.hidden {
            self.hidden = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn apply_capture_exclusion(&mut self) {
        unsafe {
            let hwnd = FindWindowW(None, w!("Cliffy")).unwrap_or_default();
            if !hwnd.is_invalid() {
                self.capture_exclusion_done =
                    SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok();
            }
        }
    }

    fn paint_content(&mut self, ui: &mut Ui) {
        ui.allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
            self.paint_controls(ui);
            divider(ui);
            self.paint_resolution(ui);
            self.paint_clip_length(ui);
            self.paint_delayed_stop(ui);
            self.paint_frame_rate(ui);
            divider(ui);
            self.paint_startup(ui);
            divider(ui);
            self.paint_actions(ui);

            ui.add_space(FOOTER_GAP);
            paint_footer_divider(ui);
            ui.add_space(6.0);
            self.paint_footer(ui);
        });
    }

    fn paint_controls(&mut self, ui: &mut Ui) {
        let (recording, buffering, buffer_seconds) = match self.shared.ui.lock() {
            Ok(snap) => (
                snap.recording_active,
                snap.buffering_active,
                snap.buffer_seconds,
            ),
            Err(_) => (false, false, 0.0),
        };

        let width = (ui.available_width() - 8.0) * 0.5;
        let time = ui.ctx().input(|i| i.time);
        let blink = blink_color(time);

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;

            let record_icon = if recording {
                Icon::Square
            } else {
                Icon::Circle
            };
            let record_label = if recording {
                "Stop Recording"
            } else {
                "Start Recording"
            };
            let record_fill = if recording { blink } else { BG_ELEVATED };
            let record_stroke = if recording { blink } else { BORDER_SUBTLE };
            let record_hover = if recording { blink } else { ACCENT_HOVER };
            let record_clicked = paint_button(
                ui,
                record_label,
                record_icon,
                width,
                BUTTON_H,
                record_fill,
                record_hover,
                record_stroke,
                WHITE,
                recording,
            );
            if record_clicked {
                let command = if recording {
                    Command::StopRecording
                } else {
                    Command::StartRecording
                };
                self.send_command(command);
            }

            let buffer_icon = if buffering {
                Icon::RefreshCw
            } else {
                Icon::Pause
            };
            let target_seconds = self.cfg.replay_seconds.max(1);
            let elapsed_seconds = buffer_seconds.floor().max(0.0).min(target_seconds as f32) as u32;
            let buffer_label = if buffering && elapsed_seconds >= target_seconds {
                format!("Buffer Ready  {}", format_duration(target_seconds))
            } else if buffering {
                format!(
                    "Buffering  {} / {}",
                    format_duration(elapsed_seconds),
                    format_duration(target_seconds)
                )
            } else {
                "Start Buffering".to_owned()
            };
            let buffer_fill = if buffering { blink } else { BG_ELEVATED };
            let buffer_stroke = if buffering { blink } else { BORDER_SUBTLE };
            let buffer_hover = if buffering { blink } else { ACCENT_HOVER };
            let buffer_clicked = paint_button(
                ui,
                &buffer_label,
                buffer_icon,
                width,
                BUTTON_H,
                buffer_fill,
                buffer_hover,
                buffer_stroke,
                WHITE,
                buffering,
            );
            if buffer_clicked {
                let command = if buffering {
                    Command::StopSession
                } else {
                    Command::StartSession
                };
                self.send_command(command);
            }
        });

        if recording || buffering {
            ui.ctx().request_repaint_after(Duration::from_millis(40));
        }
    }

    fn paint_resolution(&mut self, ui: &mut Ui) {
        form_row(ui, "Resolution", RESOLUTION_CONTROL_W, |ui| {
            let mut choice = ResChoice::from_resolution(self.cfg.resolution);
            let before = choice;
            let mut dims_changed = false;
            with_control_visuals(ui, |ui| {
                if choice == ResChoice::Custom {
                    dims_changed |= ui
                        .add_sized(
                            [46.0, 20.0],
                            DragValue::new(&mut self.cfg.custom_width)
                                .range(320u32..=7680)
                                .speed(1.0)
                                .min_decimals(0)
                                .max_decimals(0),
                        )
                        .changed();
                    ui.add_space(6.0);
                    ui.add_sized(
                        [8.0, 20.0],
                        Label::new(RichText::new("x").size(11.0).color(TEXT_MUTED)),
                    );
                    ui.add_space(6.0);
                    dims_changed |= ui
                        .add_sized(
                            [46.0, 20.0],
                            DragValue::new(&mut self.cfg.custom_height)
                                .range(320u32..=7680)
                                .speed(1.0)
                                .min_decimals(0)
                                .max_decimals(0),
                        )
                        .changed();
                    ui.add_space(6.0);
                }

                if choice != ResChoice::Custom {
                    ui.add_space(RESOLUTION_CONTROL_W - 100.0);
                }

                ComboBox::from_id_salt("cliffy_resolution")
                    .width(100.0)
                    .selected_text(choice.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut choice, ResChoice::P480, "480p");
                        ui.selectable_value(&mut choice, ResChoice::P720, "720p");
                        ui.selectable_value(&mut choice, ResChoice::P1080, "1080p");
                        ui.selectable_value(&mut choice, ResChoice::Custom, "Custom");
                    });
            });

            if choice != before || dims_changed {
                self.cfg.resolution = choice.resolution();
                self.mark_edited();
            }
        });
    }

    fn paint_clip_length(&mut self, ui: &mut Ui) {
        form_row(ui, "Clip Length", NUMBER_CONTROL_W, |ui| {
            let changed = with_control_visuals(ui, |ui| {
                ui.add_sized(
                    [NUMBER_CONTROL_W, 20.0],
                    DragValue::new(&mut self.cfg.replay_seconds)
                        .range(5u32..=120)
                        .speed(0.35)
                        .suffix(" s")
                        .min_decimals(0)
                        .max_decimals(0),
                )
                .changed()
            });
            if changed {
                self.mark_edited();
            }
        });
    }

    fn paint_delayed_stop(&mut self, ui: &mut Ui) {
        form_row(ui, "Delayed Stop", NUMBER_CONTROL_W, |ui| {
            let changed = with_control_visuals(ui, |ui| {
                ui.add_sized(
                    [NUMBER_CONTROL_W, 20.0],
                    DragValue::new(&mut self.cfg.delayed_stop_seconds)
                        .range(0u32..=60)
                        .speed(0.25)
                        .suffix(" s")
                        .min_decimals(0)
                        .max_decimals(0),
                )
                .changed()
            });
            if changed {
                self.mark_edited();
            }
        });
    }

    fn paint_frame_rate(&mut self, ui: &mut Ui) {
        form_row(ui, "Frame Rate", NUMBER_CONTROL_W, |ui| {
            let changed = with_control_visuals(ui, |ui| {
                ui.add_sized(
                    [NUMBER_CONTROL_W, 20.0],
                    DragValue::new(&mut self.cfg.fps)
                        .range(5u32..=self.refresh_hz)
                        .speed(0.35)
                        .suffix(" fps")
                        .min_decimals(0)
                        .max_decimals(0),
                )
                .changed()
            });
            if changed {
                self.mark_edited();
            }
        });
    }

    fn paint_startup(&mut self, ui: &mut Ui) {
        let windows_on = self.cfg.autostart && !self.cfg.autostart_only_on_ac;
        let ac_on = self.cfg.autostart && self.cfg.autostart_only_on_ac;
        let clipping_on = self.cfg.buffer_on_start;

        form_row(ui, "Start with Windows", TOGGLE_W, |ui| {
            if toggle(ui, windows_on) {
                self.cfg.autostart = !windows_on;
                self.cfg.autostart_only_on_ac = false;
                self.mark_edited();
            }
        });
        form_row(ui, "Start with Windows (AC only)", TOGGLE_W, |ui| {
            if toggle(ui, ac_on) {
                self.cfg.autostart = !ac_on;
                self.cfg.autostart_only_on_ac = !ac_on;
                self.mark_edited();
            }
        });
        form_row(ui, "Start clipping on launch", TOGGLE_W, |ui| {
            if toggle(ui, clipping_on) {
                self.cfg.buffer_on_start = !clipping_on;
                if self.cfg.buffer_on_start && !self.cfg.autostart {
                    self.cfg.autostart = true;
                    self.cfg.autostart_only_on_ac = false;
                }
                self.mark_edited();
            }
        });
    }

    fn paint_actions(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        ui.horizontal(|ui| {
            if paint_button(
                ui,
                "Open Folder",
                Icon::FolderOpen,
                width,
                BUTTON_H,
                BG_ELEVATED,
                BG_HOVER,
                BORDER_SUBTLE,
                TEXT_PRIMARY,
                false,
            ) {
                if let Err(e) = std::fs::create_dir_all(Path::new(&self.cfg.output_dir)) {
                    self.shared
                        .set_error(format!("Create output folder failed: {e}"));
                } else if let Err(e) = config::open_path(&self.cfg.output_dir) {
                    self.shared.set_error(format!("Open folder failed: {e}"));
                }
            }
        });
    }

    fn paint_footer(&mut self, ui: &mut Ui) {
        let button_w = 124.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if self.dirty {
                ui.label(RichText::new("Unapplied").size(10.0).color(TEXT_MUTED));
            }
            let spacer = (ui.available_width() - button_w).max(0.0);
            ui.add_space(spacer);
            if paint_button(
                ui,
                "Apply Changes",
                Icon::Check,
                button_w,
                BUTTON_H,
                ACCENT_PRIMARY,
                ACCENT_HOVER,
                ACCENT_PRIMARY,
                WHITE,
                false,
            ) {
                self.apply_changes(true);
            }
        });
    }

    fn apply_changes(&mut self, show_splash: bool) {
        for (name, spec) in [
            ("save replay", self.cfg.hotkeys.save_replay.as_str()),
            ("record", self.cfg.hotkeys.toggle_record.as_str()),
            ("buffer", self.cfg.hotkeys.toggle_buffer.as_str()),
            ("settings", self.cfg.hotkeys.show_settings.as_str()),
        ] {
            if let Err(e) = crate::hotkeys::parse_hotkey(spec) {
                self.shared.set_error(format!("Invalid {name} hotkey: {e}"));
                self.dirty = true;
                self.last_edit = None;
                self.retry_at = Some(Instant::now() + RETRY_DELAY);
                return;
            }
        }

        let mut cfg = self.cfg.clone();
        cfg.sanitize();
        cfg.fps = cfg.fps.clamp(5, self.refresh_hz);
        if let Some(hotkeys) = &self.hotkeys {
            hotkeys.update(&cfg.hotkeys);
        }
        if let Err(e) = config::set_autostart(cfg.autostart) {
            self.shared.set_error(e);
        }
        crate::toast::set_enabled(cfg.show_toast);
        crate::toast::set_sound_enabled(cfg.sound_enabled);

        self.send_command(Command::UpdateConfig(Box::new(cfg.clone())));
        self.cfg = cfg;
        self.dirty = false;
        self.last_edit = None;
        self.retry_at = None;
        if show_splash {
            crate::toast::splash("SETTINGS APPLIED", false);
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [
            BG_BASE.r() as f32 / 255.0,
            BG_BASE.g() as f32 / 255.0,
            BG_BASE.b() as f32 / 255.0,
            1.0,
        ]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_changes(ctx);
        self.sync_viewport(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_viewport(&ctx);
        if !self.capture_exclusion_done {
            self.apply_capture_exclusion();
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG_SURFACE)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .show(ui, |ui| self.paint_content(ui));
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.style_mut_of(Theme::Dark, |style| {
        style.animation_time = 0.15;
        style.drag_value_text_style = TextStyle::Button;
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(12.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(11.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(10.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(10.0, FontFamily::Monospace),
        );

        let visuals = &mut style.visuals;
        visuals.dark_mode = true;
        visuals.window_fill = BG_SURFACE;
        visuals.panel_fill = BG_SURFACE;
        visuals.faint_bg_color = BG_ELEVATED;
        visuals.extreme_bg_color = Color32::TRANSPARENT;
        visuals.code_bg_color = BG_ELEVATED;
        visuals.selection.bg_fill = Color32::from_rgba_premultiplied(239, 68, 68, 70);
        visuals.selection.stroke.color = ACCENT_PRIMARY;
        visuals.window_corner_radius = CornerRadius::same(10);
        visuals.menu_corner_radius = CornerRadius::same(10);

        let widgets = &mut visuals.widgets;
        for widget in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            widget.weak_bg_fill = BG_ELEVATED;
            widget.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
            widget.corner_radius = CornerRadius::same(6);
            widget.fg_stroke.color = TEXT_PRIMARY;
            widget.expansion = 0.0;
        }
        widgets.noninteractive.fg_stroke.color = TEXT_SECONDARY;
        widgets.inactive.bg_fill = BORDER_STRONG;
        widgets.inactive.bg_stroke.color = BORDER_SUBTLE;
        widgets.hovered.bg_fill = ACCENT_HOVER;
        widgets.hovered.weak_bg_fill = BG_HOVER;
        widgets.hovered.bg_stroke.color = BORDER_STRONG;
        widgets.active.bg_fill = ACCENT_PRIMARY;
        widgets.active.weak_bg_fill = BG_HOVER;
        widgets.active.bg_stroke.color = ACCENT_PRIMARY;
        widgets.open.bg_fill = BG_ELEVATED;

        let mut scroll = egui::style::ScrollStyle::solid();
        scroll.bar_width = 5.0;
        scroll.bar_inner_margin = 0.0;
        scroll.bar_outer_margin = 0.0;
        scroll.handle_min_length = 12.0;
        scroll.foreground_color = false;
        style.spacing.scroll = scroll;
        style.spacing.item_spacing = vec2(8.0, 6.0);
    });
}

fn with_control_visuals<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.bg_fill = BG_ELEVATED;
        widgets.inactive.weak_bg_fill = BG_ELEVATED;
        widgets.inactive.bg_stroke.color = BORDER_SUBTLE;
        widgets.hovered.bg_fill = BG_HOVER;
        widgets.hovered.weak_bg_fill = BG_HOVER;
        widgets.hovered.bg_stroke.color = BORDER_STRONG;
        widgets.active.bg_fill = BG_HOVER;
        widgets.active.weak_bg_fill = BG_HOVER;
        add_contents(ui)
    })
    .inner
}

fn form_row(ui: &mut Ui, label: &str, control_w: f32, add_control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.spacing_mut().item_spacing.x = 0.0;
        let label_w = LABEL_COLUMN_W.min(ui.available_width());
        let (label_rect, _) = ui.allocate_exact_size(vec2(label_w, ROW_H), Sense::hover());
        ui.painter().text(
            pos2(label_rect.left(), label_rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::new(11.0, FontFamily::Proportional),
            TEXT_SECONDARY,
        );
        let spacer = (ui.available_width() - control_w).max(CONTROL_GAP);
        ui.add_space(spacer);
        add_control(ui);
    });
}

fn format_duration(seconds: u32) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn divider(ui: &mut Ui) {
    ui.add_space(2.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [pos2(rect.min.x, rect.min.y), pos2(rect.max.x, rect.min.y)],
        Stroke::new(1.0, BORDER_SUBTLE),
    );
    ui.add_space(3.0);
}

fn paint_footer_divider(ui: &mut Ui) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [pos2(rect.min.x, rect.min.y), pos2(rect.max.x, rect.min.y)],
        Stroke::new(1.0, BORDER_SUBTLE),
    );
}

fn paint_button(
    ui: &mut Ui,
    label: &str,
    icon: Icon,
    width: f32,
    height: f32,
    fill: Color32,
    hover_fill: Color32,
    border: Color32,
    text: Color32,
    glow: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let painter = ui.painter();
    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fill = if hovered { hover_fill } else { fill };

    if glow {
        let shadow = Color32::from_rgba_premultiplied(239, 68, 68, 55);
        painter.rect_filled(rect.expand(3.0), CornerRadius::same(8), shadow);
    }
    painter.rect_filled(rect, CornerRadius::same(6), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );

    let galley = painter.layout_no_wrap(
        label.to_owned(),
        FontId::new(11.0, FontFamily::Proportional),
        text,
    );
    let icon_size = 14.0;
    let gap = 5.0;
    let total_w = icon_size + gap + galley.size().x;
    let start_x = rect.center().x - total_w * 0.5;
    let icon_rect = egui::Rect::from_min_size(
        pos2(start_x, rect.center().y - icon_size * 0.5),
        vec2(icon_size, icon_size),
    );
    draw_icon(ui, icon, icon_rect, text);
    painter.galley(
        pos2(
            start_x + icon_size + gap,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        text,
    );

    response.clicked()
}

fn toggle(ui: &mut Ui, on: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(TOGGLE_W, 18.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let fill = if on {
        ACCENT_PRIMARY
    } else if response.hovered() {
        BG_HOVER
    } else {
        BG_ELEVATED
    };
    let border = if on { ACCENT_PRIMARY } else { BORDER_STRONG };
    if on {
        let glow = Color32::from_rgba_premultiplied(239, 68, 68, 55);
        painter.rect_filled(rect.expand(3.0), CornerRadius::same(11), glow);
    }
    painter.rect_filled(rect, CornerRadius::same(9), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );

    let knob_x = rect.min.x + if on { 24.0 } else { 8.0 };
    painter.add(Shape::circle_filled(
        pos2(knob_x, rect.center().y),
        6.0,
        if on { WHITE } else { TEXT_MUTED },
    ));

    response.clicked()
}

fn pulse(time: f64) -> f32 {
    ((time / 1.2 * std::f64::consts::PI * 2.0).sin().abs()) as f32
}

fn blink_color(time: f64) -> Color32 {
    let t = pulse(time);
    Color32::from_rgb(
        lerp_byte(DANGER.r(), DANGER_DARK.r(), t),
        lerp_byte(DANGER.g(), DANGER_DARK.g(), t),
        lerp_byte(DANGER.b(), DANGER_DARK.b(), t),
    )
}

fn lerp_byte(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn draw_icon(ui: &Ui, icon: Icon, rect: egui::Rect, color: Color32) {
    let painter = ui.painter();
    let scale = rect.width() / 24.0;
    let stroke = Stroke::new(2.0 * scale, color);
    let p = |x: f32, y: f32| pos2(rect.min.x + x * scale, rect.min.y + y * scale);

    match icon {
        Icon::Circle => {
            painter.add(Shape::circle_stroke(p(12.0, 12.0), 8.0 * scale, stroke));
        }
        Icon::Square => {
            draw_path(
                painter,
                &[
                    p(3.0, 3.0),
                    p(21.0, 3.0),
                    p(21.0, 21.0),
                    p(3.0, 21.0),
                    p(3.0, 3.0),
                ],
                stroke,
            );
        }
        Icon::Pause => {
            draw_path(painter, &[p(8.0, 5.0), p(8.0, 19.0)], stroke);
            draw_path(painter, &[p(16.0, 5.0), p(16.0, 19.0)], stroke);
        }
        Icon::RefreshCw => {
            draw_path(
                painter,
                &[
                    p(21.0, 12.0),
                    p(20.0, 8.0),
                    p(17.0, 5.0),
                    p(13.0, 3.0),
                    p(9.0, 3.0),
                    p(5.0, 5.0),
                    p(3.0, 9.0),
                    p(3.0, 12.0),
                ],
                stroke,
            );
            draw_path(painter, &[p(3.0, 5.0), p(3.0, 12.0), p(10.0, 12.0)], stroke);
            draw_path(
                painter,
                &[
                    p(3.0, 12.0),
                    p(4.0, 16.0),
                    p(7.0, 19.0),
                    p(11.0, 21.0),
                    p(15.0, 21.0),
                    p(19.0, 19.0),
                    p(21.0, 15.0),
                    p(21.0, 12.0),
                ],
                stroke,
            );
            draw_path(
                painter,
                &[p(21.0, 19.0), p(21.0, 12.0), p(14.0, 12.0)],
                stroke,
            );
        }
        Icon::FolderOpen => {
            draw_path(
                painter,
                &[
                    p(3.0, 7.0),
                    p(3.0, 18.0),
                    p(21.0, 18.0),
                    p(21.0, 11.0),
                    p(13.0, 11.0),
                ],
                stroke,
            );
            draw_path(
                painter,
                &[
                    p(7.0, 18.0),
                    p(9.0, 11.0),
                    p(22.0, 11.0),
                    p(20.0, 18.0),
                    p(7.0, 18.0),
                ],
                stroke,
            );
        }
        Icon::Check => {
            draw_path(
                painter,
                &[p(4.0, 12.0), p(10.0, 18.0), p(20.0, 6.0)],
                stroke,
            );
        }
    }
}

fn draw_path(painter: &egui::Painter, points: &[Pos2], stroke: Stroke) {
    for pair in points.windows(2) {
        painter.line_segment([pair[0], pair[1]], stroke);
    }
    let radius = stroke.width * 0.5;
    for point in points {
        painter.add(Shape::circle_filled(*point, radius, stroke.color));
    }
}
