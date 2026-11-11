//! `standalone` host: a self-contained egui + soft_ratatui window. The embedded FiraCode Nerd
//! Font drives both the soft_ratatui cell rasterizer and egui's text, so the TUI renders
//! identically without a host terminal.
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui; // egui is re-exported by eframe; not a direct dependency
use egui_ratatui::RataguiBackend;
use ratatui::Terminal;
use ratatui::crossterm::event::KeyCode;
use soft_ratatui::{EmbeddedTTF, SoftBackend};

use crate::app::Model;
use crate::view;

static FONT_BYTES: &[u8] = include_bytes!("../res/FiraCodeNerdFontMono-Regular.ttf");

const DEFAULT_BG: egui::Color32 = egui::Color32::from_rgb(0x1E, 0x1E, 0x1E);
const FONT_SIZE: u32 = 18;

type AppTerminal = Terminal<RataguiBackend<EmbeddedTTF>>;

pub fn run(model: Model) -> Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ccsm")
            .with_inner_size([1100.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ccsm",
        native_options,
        Box::new(move |cc| {
            setup_fonts(cc);
            setup_visuals(cc);
            Ok(Box::new(EguiApp::new(model)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Non-text keys only: printable characters (including space) arrive as `egui::Event::Text`,
/// which is what lets the filter accept arbitrary input.
fn map_key(key: egui::Key) -> Option<KeyCode> {
    Some(match key {
        egui::Key::ArrowUp => KeyCode::Up,
        egui::Key::ArrowDown => KeyCode::Down,
        egui::Key::ArrowLeft => KeyCode::Left,
        egui::Key::ArrowRight => KeyCode::Right,
        egui::Key::Home => KeyCode::Home,
        egui::Key::End => KeyCode::End,
        egui::Key::PageUp => KeyCode::PageUp,
        egui::Key::PageDown => KeyCode::PageDown,
        egui::Key::Enter => KeyCode::Enter,
        egui::Key::Escape => KeyCode::Esc,
        egui::Key::Backspace => KeyCode::Backspace,
        egui::Key::Tab => KeyCode::Tab,
        _ => return None,
    })
}

fn setup_fonts(cc: &eframe::CreationContext) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("fira".to_owned(), egui::FontData::from_static(FONT_BYTES).into());
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        fonts.families.entry(family).or_default().insert(0, "fira".to_owned());
    }
    cc.egui_ctx.set_fonts(fonts);
}

fn setup_visuals(cc: &eframe::CreationContext) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = DEFAULT_BG;
    visuals.window_fill = DEFAULT_BG;
    cc.egui_ctx.set_visuals(visuals);
}

struct EguiApp {
    terminal: AppTerminal,
    model:    Model,
    last:     Instant,
}

impl EguiApp {
    fn new(model: Model) -> Self {
        let font = soft_ratatui::rusttype::Font::try_from_bytes(FONT_BYTES).expect("embedded FiraCode font is valid");
        let soft = SoftBackend::<EmbeddedTTF>::new(120, 40, FONT_SIZE, font, None, None);
        let backend = RataguiBackend::new("ccsm", soft);
        let terminal = Terminal::new(backend).expect("ratatui terminal init");
        Self { terminal, model, last: Instant::now() }
    }
}

impl eframe::App for EguiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drain the scan worker and advance the spinner, same as the terminal loop.
        self.model.apply();
        let now = Instant::now();
        self.model.spinner_state.tick(now - self.last);
        self.last = now;

        ui.ctx().input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::Text(text) => text.chars().for_each(|c| self.model.handle(KeyCode::Char(c))),
                    egui::Event::Key { key, pressed: true, .. } => {
                        if let Some(code) = map_key(*key) {
                            self.model.handle(code);
                        }
                    }
                    _ => {}
                }
            }
        });
        if self.model.should_quit {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(DEFAULT_BG).inner_margin(egui::Margin::ZERO))
            .show_inside(ui, |ui| {
                self.terminal.draw(|frame| view::view(frame, &mut self.model)).expect("draw");
                ui.add(self.terminal.backend_mut());
            });

        ui.ctx().request_repaint_after(Duration::from_millis(80));
    }
}
