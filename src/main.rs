#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use egui::{Align, Color32, ColorImage, ComboBox, Layout, TextEdit, TextureHandle, TextureOptions, Vec2};
use image::{imageops, DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_filled_rect_mut};
use imageproc::rect::Rect;
use qrcode::{Color as QrColor, QrCode};
use rfd::FileDialog;
use sha1::{Digest, Sha1};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Corner {
    Southeast,
    Southwest,
    Northeast,
    Northwest,
    Custom, // X/Y od levého-horního
}

enum JobResult {
    Ok(PathBuf),
    Err(String),
}

#[derive(Clone, Copy)]
enum SaveMode {
    OverlayIntoImage,
    QrOnlySingle,
    QrOnlyBulk,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Png,
    Jpeg,
    Tiff,
}
impl OutputFormat {
    fn ext(self) -> &'static str {
        match self {
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Tiff => "tif",
        }
    }
}

struct AppState {
    // Režimy
    bulk_mode: bool,

    // URL vstup
    url: String,          // single
    bulk_urls: String,    // multi – po řádcích

    // Volby výstupu
    output_path: Option<PathBuf>,   // single QR i overlay
    export_dir: Option<PathBuf>,    // složka pro hromadné
    out_format: OutputFormat,

    // Vstupní obrázek (jen overlay)
    input_path: Option<PathBuf>,
    base_dims: Option<(u32, u32)>,

    // QR parametry
    qr_size_px: u32,
    corner: Corner,
    offset_x: i32,
    offset_y: i32,

    // Vzhled QR
    rounding_percent: u8,       // 0–50 % z velikosti modulu
    module_color: Color32,      // barva „tmavých“ modulů
    background_color: Color32,  // barva pozadí (použije se, když není „Odstranit pozadí“)
    qr_alpha_percent: u8,       // 0–100 %
    cut_white_background: bool, // true => pozadí QR bude plně průhledné

    // Výsledky / status
    last_message: String,
    last_saved_path: Option<PathBuf>,

    // Náhled
    preview: Option<TextureHandle>,
    preview_key: String,
    preview_error: Option<String>,

    // Asynchronní uložení
    is_busy: bool,
    job_rx: Option<Receiver<JobResult>>,

    // Modální okno s výsledkem
    result_modal_open: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            bulk_mode: false,

            url: "".to_owned(),
            bulk_urls: "".to_owned(),

            output_path: None,
            export_dir: None,
            out_format: OutputFormat::Png,

            input_path: None,
            base_dims: None,

            qr_size_px: 160,
            corner: Corner::Southeast,
            offset_x: 10,
            offset_y: 10,

            rounding_percent: 0,
            module_color: Color32::BLACK,
            background_color: Color32::WHITE,
            qr_alpha_percent: 85,
            cut_white_background: true,

            last_message: String::new(),
            last_saved_path: None,

            preview: None,
            preview_key: String::new(),
            preview_error: None,

            is_busy: false,
            job_rx: None,

            result_modal_open: false,
        }
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll výsledků background jobu
        if let Some(rx) = &self.job_rx {
            if let Ok(msg) = rx.try_recv() {
                self.is_busy = false;
                self.job_rx = None;
                match msg {
                    JobResult::Ok(path) => {
                        self.last_saved_path = Some(path.clone());
                        self.last_message = format!("Uloženo: {}", path.display());
                    }
                    JobResult::Err(e) => {
                        self.last_saved_path = None;
                        self.last_message = format!("Chyba: {e}");
                    }
                }
                self.result_modal_open = true;
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.heading("Kjů ár");
                ui.add_space(12.0);
                ui.label("Vlož QR do obrázku nebo hromadně ulož samostatné QR.");
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);

            ui.columns(2, |cols| {
                // === LEVÝ SLOUPEC – ovládání ===
                cols[0].vertical(|ui| {
                    ui.add_enabled_ui(!self.is_busy && !self.result_modal_open, |ui| {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label("Režim:");
                                ui.selectable_value(&mut self.bulk_mode, false, "Jednotlivě");
                                ui.selectable_value(&mut self.bulk_mode, true, "Hromadně (URL po řádcích)");
                            });
                        });

                        ui.group(|ui| {
                            if self.bulk_mode {
                                ui.label("Vlož víc URL – každé na samostatný řádek:");
                                if ui
                                    .add(
                                        egui::TextEdit::multiline(&mut self.bulk_urls)
                                            .hint_text("https://...\nhttps://...\n...")
                                            .desired_rows(6)
                                            .desired_width(f32::INFINITY),
                                    )
                                    .changed()
                                {
                                    self.bump_preview();
                                }
                            } else {
                                ui.label("Odkaz (URL) pro QR kód:");
                                if ui
                                    .add(
                                        TextEdit::singleline(&mut self.url)
                                            .hint_text("https://...")
                                            .clip_text(true)
                                            .desired_width(f32::INFINITY),
                                    )
                                    .changed()
                                {
                                    self.bump_preview();
                                }
                            }
                        });

                        // Soubory / výstup
                        ui.group(|ui| {
                            ui.label("Výstup:");
                            if self.bulk_mode {
                                if ui.button("Zvolit výstupní složku…").clicked() {
                                    if let Some(dir) = FileDialog::new().pick_folder() {
                                        self.export_dir = Some(dir);
                                    }
                                }
                                ui.monospace(format!(
                                    "Složka: {}",
                                    self.export_dir
                                        .as_deref()
                                        .map(shorten)
                                        .unwrap_or_else(|| format!("<automaticky: {}>", default_bulk_dir().display()))
                                ));
                                ui.horizontal(|ui| {
                                    ui.label("Formát:");
                                    ComboBox::from_id_source("fmt")
                                        .selected_text(match self.out_format {
                                            OutputFormat::Png => "PNG (.png)",
                                            OutputFormat::Jpeg => "JPEG (.jpg)",
                                            OutputFormat::Tiff => "TIFF (.tif)",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut self.out_format, OutputFormat::Png, "PNG (.png)");
                                            ui.selectable_value(&mut self.out_format, OutputFormat::Jpeg, "JPEG (.jpg)");
                                            ui.selectable_value(&mut self.out_format, OutputFormat::Tiff, "TIFF (.tif)");
                                        });
                                });
                            } else {
                                if ui.button("Zvolit výstupní soubor…").clicked() {
                                    // návrh názvu: podle vstupu, jinak qr.png
                                    let suggested = if self.input_path.is_some() {
                                        default_out_path(self.input_path.as_ref())
                                    } else {
                                        default_qr_out_path()
                                    };
                                    if let Some(p) = FileDialog::new()
                                        .set_file_name(
                                            suggested
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy(),
                                        )
                                        .save_file()
                                    {
                                        self.output_path = Some(p);
                                    }
                                }
                                ui.monospace(format!(
                                    "Soubor: {}",
                                    self.output_path
                                        .as_deref()
                                        .map(shorten)
                                        .unwrap_or_else(|| {
                                            if self.input_path.is_some() {
                                                "<automaticky: out_<původní>.jpg/png/tif>".to_string()
                                            } else {
                                                "<automaticky: qr.png>".to_string()
                                            }
                                        })
                                ));
                            }
                        });

                        // Vstupní obrázek (jen mimo hromadný režim)
                        ui.add_enabled_ui(!self.bulk_mode, |ui| {
                            ui.group(|ui| {
                                ui.label("Zdrojový obrázek (pro vložení QR):");
                                if ui.button("Vybrat zdrojový obrázek…").clicked() {
                                    if let Some(p) = FileDialog::new()
                                        .add_filter("Obrázky", &["jpg", "jpeg", "png", "tif", "tiff"])
                                        .pick_file()
                                    {
                                        self.input_path = Some(p);
                                        self.refresh_base_dims();
                                        self.bump_preview();
                                    }
                                }
                                ui.monospace(format!(
                                    "Zdroj: {}",
                                    self.input_path
                                        .as_deref()
                                        .map(shorten)
                                        .unwrap_or_else(|| "<není vybráno>".to_string())
                                ));
                            });
                        });

                        ui.group(|ui| {
                            ui.label("QR kód:");

                            // Velikost
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.qr_size_px, 64..=2048)
                                        .text("Velikost")
                                        .suffix(" px")
                                        .step_by(1.0),
                                )
                                .changed()
                            {
                                self.bump_preview();
                            }

                            // Zaoblení rohů (0–50 % modulu)
                            if ui
                                .add(
                                    egui::Slider::new(&mut self.rounding_percent, 0..=50)
                                        .text("Zaoblení rohů")
                                        .suffix(" % modulu")
                                        .step_by(1.0),
                                )
                                .changed()
                            {
                                self.bump_preview();
                            }

                            // Barva modulů
                            ui.horizontal(|ui| {
                                ui.label("Barva modulů:");
                                let mut c = self.module_color;
                                if egui::color_picker::color_edit_button_srgba(
                                    ui,
                                    &mut c,
                                    egui::color_picker::Alpha::Opaque,
                                )
                                .changed()
                                {
                                    self.module_color = c;
                                    self.bump_preview();
                                }
                            });

                            // Barva pozadí (použije se, když není „Odstranit pozadí“)
                            ui.horizontal(|ui| {
                                ui.label("Pozadí QR:");
                                let mut bg = self.background_color;
                                let mut changed = false;
                                ui.add_enabled_ui(!self.cut_white_background, |ui| {
                                    if egui::color_picker::color_edit_button_srgba(
                                        ui,
                                        &mut bg,
                                        egui::color_picker::Alpha::Opaque,
                                    )
                                    .changed()
                                    {
                                        changed = true;
                                    }
                                });
                                if changed {
                                    self.background_color = bg;
                                    self.bump_preview();
                                }
                                if self.cut_white_background {
                                    ui.small(" (nepoužije se při zapnutém „Odstranit pozadí“)");
                                }
                            });

                            // Průhlednost QR – invertované ovládání (→ vpravo = 0 %, vlevo = 100 %)
                            {
                                let mut inv_alpha = 100 - self.qr_alpha_percent;
                                let resp = ui.add(
                                    egui::Slider::new(&mut inv_alpha, 0..=100)
                                        .text("Průhlednost QR")
                                        .suffix(" %")
                                        .step_by(1.0),
                                );
                                if resp.changed() {
                                    self.qr_alpha_percent = 100 - inv_alpha;
                                    self.bump_preview();
                                }
                            }

                            // „Odstranit pozadí“ (pozadí QR)
                            if ui
                                .checkbox(&mut self.cut_white_background, "Odstranit pozadí (průhledné pozadí)")
                                .changed()
                            {
                                self.bump_preview();
                            }

                            ui.separator();

                            // Pozice jen pokud není bulk a máme overlay mód
                            ui.add_enabled_ui(!self.bulk_mode, |ui| {
                                ui.label("Pozice (jen pro vložení do obrázku):");
                                ComboBox::from_id_source("corner")
                                    .selected_text(match self.corner {
                                        Corner::Southeast => "pravý-dolní (SE)",
                                        Corner::Southwest => "levý-dolní (SW)",
                                        Corner::Northeast => "pravý-horní (NE)",
                                        Corner::Northwest => "levý-horní (NW)",
                                        Corner::Custom => "vlastní (X/Y)",
                                    })
                                    .show_ui(ui, |ui| {
                                        let current = self.corner;
                                        if ui.selectable_label(current == Corner::Southeast, "pravý-dolní (SE)").clicked() { self.corner = Corner::Southeast; self.bump_preview(); }
                                        if ui.selectable_label(current == Corner::Southwest, "levý-dolní (SW)").clicked() { self.corner = Corner::Southwest; self.bump_preview(); }
                                        if ui.selectable_label(current == Corner::Northeast, "pravý-horní (NE)").clicked() { self.corner = Corner::Northeast; self.bump_preview(); }
                                        if ui.selectable_label(current == Corner::Northwest, "levý-horní (NW)").clicked() { self.corner = Corner::Northwest; self.bump_preview(); }
                                        if ui.selectable_label(current == Corner::Custom, "vlastní (X/Y)").clicked() { self.corner = Corner::Custom; self.bump_preview(); }
                                    });

                                // Odsazení
                                let (max_w, max_h) = self.base_dims.unwrap_or((4000, 4000));
                                let slider_max_dx = max_w as i32;
                                let slider_max_dy = max_h as i32;

                                match self.corner {
                                    Corner::Custom => {
                                        ui.label("Souřadnice (px) od levého-horního rohu:");
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut self.offset_x, 0..=slider_max_dx)
                                                    .text("X")
                                                    .suffix(" px")
                                                    .step_by(1.0),
                                            )
                                            .changed()
                                        {
                                            self.bump_preview();
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut self.offset_y, 0..=slider_max_dy)
                                                    .text("Y")
                                                    .suffix(" px")
                                                    .step_by(1.0),
                                            )
                                            .changed()
                                        {
                                            self.bump_preview();
                                        }
                                    }
                                    _ => {
                                        ui.label("Odsazení od kraje (px):");
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut self.offset_x, 0..=slider_max_dx)
                                                    .text("dx")
                                                    .suffix(" px")
                                                    .step_by(1.0),
                                            )
                                            .changed()
                                        {
                                            self.bump_preview();
                                        }
                                        if ui
                                            .add(
                                                egui::Slider::new(&mut self.offset_y, 0..=slider_max_dy)
                                                    .text("dy")
                                                    .suffix(" px")
                                                    .step_by(1.0),
                                            )
                                            .changed()
                                        {
                                            self.bump_preview();
                                        }
                                    }
                                }
                            });
                        });

                        // Akce
                        ui.horizontal(|ui| {
                            let green = egui::Color32::from_rgb(16, 163, 74);

                            if !self.bulk_mode {
                                // Uložit do obrázku
                                let overlay_btn = egui::Button::new(
                                    egui::RichText::new("Vložit QR a uložit").color(egui::Color32::WHITE)
                                )
                                .fill(green);
                                let overlay_enabled = self.input_path.is_some();
                                if ui.add_enabled(overlay_enabled, overlay_btn).clicked() {
                                    self.start_job(SaveMode::OverlayIntoImage);
                                }

                                // Uložit jen QR (single)
                                let qr_btn = egui::Button::new(
                                    egui::RichText::new("Uložit jen QR").color(egui::Color32::WHITE)
                                )
                                .fill(egui::Color32::from_rgb(52, 120, 246));
                                if ui.add(qr_btn).clicked() {
                                    self.start_job(SaveMode::QrOnlySingle);
                                }
                            } else {
                                // Hromadné generování QR
                                let bulk_btn = egui::Button::new(
                                    egui::RichText::new("Vygenerovat QR (hromadně)").color(egui::Color32::WHITE)
                                )
                                .fill(egui::Color32::from_rgb(52, 120, 246));
                                if ui.add(bulk_btn).clicked() {
                                    self.start_job(SaveMode::QrOnlyBulk);
                                }
                            }

                            if ui.button("Konec").clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });

                        if !self.last_message.is_empty() {
                            ui.separator();
                            ui.label(&self.last_message);
                        }
                    });

                    if self.is_busy {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new());
                            ui.strong("Zpracovávám…");
                        });
                    }
                });

                // === PRAVÝ SLOUPEC – náhled ===
                cols[1].vertical(|ui| {
                    ui.group(|ui| {
                        ui.label(if self.bulk_mode { "Živý náhled (první URL):" } else { "Živý náhled:" });
                        self.ensure_preview(ctx);
                        if let Some(err) = &self.preview_error {
                            ui.colored_label(egui::Color32::RED, err);
                        }
                        if let Some(tex) = &self.preview {
                            let max = Vec2::new(520.0, 520.0);
                            let size = tex.size_vec2();
                            let scale = (max.x / size.x).min(max.y / size.y).min(1.0);
                            let desired = size * scale;
                            ui.image((tex.id(), desired));
                        } else {
                            ui.monospace("— žádný náhled —");
                        }
                    });
                });
            });

            // === Modální okno s výsledkem ===
            if self.result_modal_open {
                let mut is_open = true;
                let mut close_now = false;

                egui::Window::new(if self.last_saved_path.is_some() { "Hotovo" } else { "Chyba" })
                    .collapsible(false)
                    .resizable(false)
                    .default_size([460.0, 160.0])
                    .min_size([360.0, 120.0])
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut is_open)
                    .show(ctx, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(&self.last_message);
                            if let Some(p) = &self.last_saved_path {
                                ui.add_space(6.0);
                                ui.horizontal_centered(|ui| {
                                    if ui.button("Otevřít výsledek").clicked() {
                                        let _ = open::that(p);
                                    }
                                    if ui.button("Otevřít složku").clicked() {
                                        #[cfg(target_os = "windows")]
                                        {
                                            let _ = std::process::Command::new("explorer")
                                                .args(["/select,", &p.to_string_lossy()])
                                                .spawn();
                                        }
                                        #[cfg(not(target_os = "windows"))]
                                        {
                                            if let Some(parent) = p.parent() {
                                                let _ = open::that(parent);
                                            }
                                        }
                                    }
                                });
                            }
                            ui.add_space(6.0);
                            if ui.button("OK").clicked() {
                                close_now = true;
                            }
                        });
                    });

                self.result_modal_open = is_open && !close_now;

                let painter = ui.painter_at(ui.max_rect());
                painter.rect_filled(ui.max_rect(), 0.0, egui::Color32::from_black_alpha(120));
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([820.0, 560.0])
            .with_title("Kjů ár"),
        ..Default::default()
    };
    eframe::run_native(
        "Kjů ár",
        native_options,
        Box::new(|_| Box::<AppState>::default()),
    )
}

/// Pomocné metody stavu
impl AppState {
    fn bump_preview(&mut self) {
        self.preview_key.clear();
    }

    fn refresh_base_dims(&mut self) {
        self.base_dims = None;
        if let Some(p) = &self.input_path {
            if let Ok((w, h)) = image::image_dimensions(p) {
                self.base_dims = Some((w, h));
            }
        }
    }

    fn ensure_preview(&mut self, ctx: &egui::Context) {
        let key = self.preview_signature();
        if self.preview_key == key {
            return;
        }
        self.preview_key = key.clone();

        match self.render_preview_color_image() {
            Ok(ci) => {
                if let Some(tex) = &mut self.preview {
                    tex.set(ci, TextureOptions::LINEAR);
                } else {
                    self.preview = Some(ctx.load_texture("preview", ci, TextureOptions::LINEAR));
                }
                self.preview_error = None;
            }
            Err(e) => {
                self.preview = None;
                self.preview_error = Some(format!("Náhled nelze vytvořit: {e}"));
            }
        }
    }

    fn preview_signature(&self) -> String {
        let in_tag = if self.bulk_mode {
            "bulk".to_string()
        } else {
            self.input_path
                .as_deref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "qr-only".to_string())
        };
        let mtime = self
            .input_path
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mticks = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let [mr, mg, mb, _] = self.module_color.to_srgba_unmultiplied();
        let [br, bg, bb, _] = self.background_color.to_srgba_unmultiplied();

        format!(
            "{in}|{mt}|{u}|{bulk}|{qr}px|{corner:?}|{ox},{oy}|{alpha}%|cut={cut}|mod={mr},{mg},{mb}|bg={br},{bg},{bb}|round={round}|fmt={fmt}",
            in = in_tag,
            mt = mticks,
            u = if self.bulk_mode { self.bulk_urls.clone() } else { self.url.clone() },
            bulk = self.bulk_mode,
            qr = self.qr_size_px,
            corner = self.corner,
            ox = self.offset_x,
            oy = self.offset_y,
            alpha = self.qr_alpha_percent,
            cut = self.cut_white_background,
            round = self.rounding_percent,
            fmt = self.out_format.ext(),
        )
    }

    /// Náhled:
    /// - bulk: zobrazí QR prvního neprázdného řádku
    /// - single: pokud je vstupní obrázek, ukáže overlay; jinak ukáže samostatný QR
    fn render_preview_color_image(&self) -> anyhow::Result<ColorImage> {
        use anyhow::{anyhow, Context};

        // vyber zdrojový text URL pro náhled
        let preview_url = if self.bulk_mode {
            first_nonempty_line(&self.bulk_urls).ok_or_else(|| anyhow!("Vlož aspoň jednu URL (po řádku)"))?
        } else if self.url.trim().is_empty() {
            return Err(anyhow!("Zadej URL pro QR"));
        } else {
            self.url.trim().to_string()
        };

        let [mr, mg, mb, _] = self.module_color.to_srgba_unmultiplied();
        let bg_opt = if self.cut_white_background {
            None
        } else {
            let [br, bg, bb, _] = self.background_color.to_srgba_unmultiplied();
            Some((br, bg, bb))
        };

        if !self.bulk_mode && self.input_path.is_none() {
            // Samostatný QR náhled (single)
            let qr_img = build_qr_image(
                &preview_url,
                self.qr_size_px,
                (mr, mg, mb),
                bg_opt,
                self.qr_alpha_percent,
                self.rounding_percent,
            )?;
            let [w, h] = [qr_img.width() as usize, qr_img.height() as usize];
            return Ok(ColorImage::from_rgba_unmultiplied([w, h], qr_img.as_raw()));
        }

        if self.bulk_mode {
            // V bulk režimu vždy ukazujeme samostatný QR (podle první URL)
            let qr_img = build_qr_image(
                &preview_url,
                self.qr_size_px,
                (mr, mg, mb),
                bg_opt,
                self.qr_alpha_percent,
                self.rounding_percent,
            )?;
            let [w, h] = [qr_img.width() as usize, qr_img.height() as usize];
            return Ok(ColorImage::from_rgba_unmultiplied([w, h], qr_img.as_raw()));
        }

        // Overlay náhled (single + máme obrázek)
        let in_path = self.input_path.as_ref().unwrap();
        let base = image::open(in_path)
            .with_context(|| format!("Nejde otevřít obrázek: {}", in_path.display()))?
            .to_rgba8();

        let (bw, bh) = base.dimensions();
        let max_w: u32 = 1200;
        let max_h: u32 = 1200;
        let scale = (max_w as f32 / bw as f32)
            .min(max_h as f32 / bh as f32)
            .min(1.0);

        let disp_w = ((bw as f32 * scale).round() as u32).max(1);
        let disp_h = ((bh as f32 * scale).round() as u32).max(1);

        let mut base_small =
            imageops::resize(&base, disp_w, disp_h, imageops::FilterType::Triangle);

        let qr_size_scaled = ((self.qr_size_px as f32 * scale).round() as u32).clamp(1, 4096);
        let qr_img = build_qr_image(
            &preview_url,
            qr_size_scaled,
            (mr, mg, mb),
            bg_opt,
            self.qr_alpha_percent,
            self.rounding_percent,
        )?;

        let (qw, qh) = (qr_img.width(), qr_img.height());
        let dx = ((self.offset_x.max(0) as f32 * scale).round() as u32).min(disp_w - 1);
        let dy = ((self.offset_y.max(0) as f32 * scale).round() as u32).min(disp_h - 1);

        let (x, y) = match self.corner {
            Corner::Northwest => (dx, dy),
            Corner::Northeast => (disp_w.saturating_sub(qw + dx), dy),
            Corner::Southwest => (dx, disp_h.saturating_sub(qh + dy)),
            Corner::Southeast => (disp_w.saturating_sub(qw + dx), disp_h.saturating_sub(qh + dy)),
            Corner::Custom => {
                let ax = dx.min(disp_w.saturating_sub(qw));
                let ay = dy.min(disp_h.saturating_sub(qh));
                (ax, ay)
            }
        };

        imageops::overlay(&mut base_small, &qr_img, x.into(), y.into());

        let [w, h] = [base_small.width() as usize, base_small.height() as usize];
        Ok(ColorImage::from_rgba_unmultiplied([w, h], base_small.as_raw()))
    }

    fn start_job(&mut self, mode: SaveMode) {
        use anyhow::Context;

        if self.is_busy {
            return;
        }

        // společné parametry
        let url = self.url.clone();
        let bulk_urls = self.bulk_urls.clone();
        let in_path = self.input_path.clone();
        let out_path = self.output_path.clone();
        let export_dir = self.export_dir.clone();
        let out_format = self.out_format;

        let size = self.qr_size_px;
        let corner = self.corner;
        let ox = self.offset_x;
        let oy = self.offset_y;

        let alpha = self.qr_alpha_percent;
        let cut_white = self.cut_white_background;
        let [mr, mg, mb, _] = self.module_color.to_srgba_unmultiplied();
        let bg_opt = if cut_white {
            None
        } else {
            let [br, bg, bb, _] = self.background_color.to_srgba_unmultiplied();
            Some((br, bg, bb))
        };
        let rounding = self.rounding_percent;

        let (tx, rx) = channel::<JobResult>();
        self.job_rx = Some(rx);
        self.is_busy = true;

        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<PathBuf> {
                match mode {
                    SaveMode::OverlayIntoImage => {
                        let url = url.trim();
                        if url.is_empty() {
                            anyhow::bail!("URL je prázdná");
                        }
                        let in_path = in_path.as_ref().context("Není vybrán zdrojový obrázek")?;
                        let mut base = image::open(in_path)
                            .with_context(|| format!("Nejde otevřít obrázek: {}", in_path.display()))?
                            .to_rgba8();

                        let qr_img = build_qr_image(url, size, (mr, mg, mb), bg_opt, alpha, rounding)?;

                        let (bw, bh) = base.dimensions();
                        let (qw, qh) = (qr_img.width(), qr_img.height());
                        let (x, y) = match corner {
                            Corner::Northwest => (ox.max(0) as u32, oy.max(0) as u32),
                            Corner::Northeast => (bw.saturating_sub(qw + ox.max(0) as u32), oy.max(0) as u32),
                            Corner::Southwest => (ox.max(0) as u32, bh.saturating_sub(qh + oy.max(0) as u32)),
                            Corner::Southeast => (bw.saturating_sub(qw + ox.max(0) as u32), bh.saturating_sub(qh + oy.max(0) as u32)),
                            Corner::Custom => {
                                let ax = (ox.max(0) as u32).min(bw.saturating_sub(qw));
                                let ay = (oy.max(0) as u32).min(bh.saturating_sub(qh));
                                (ax, ay)
                            }
                        };

                        imageops::overlay(&mut base, &qr_img, x.into(), y.into());

                        let outp = if let Some(p) = &out_path { p.clone() } else { default_out_path(Some(in_path)).to_path_buf() };
                        save_image_rgba(&DynamicImage::ImageRgba8(base), &outp)?;
                        Ok(outp)
                    }
                    SaveMode::QrOnlySingle => {
                        let url = url.trim();
                        if url.is_empty() {
                            anyhow::bail!("URL je prázdná");
                        }
                        let qr_img = build_qr_image(url, size, (mr, mg, mb), bg_opt, alpha, rounding)?;
                        let outp = if let Some(p) = &out_path { p.clone() } else { default_qr_out_path() };
                        save_qr(&qr_img, &outp, out_format, bg_opt)?;
                        Ok(outp)
                    }
                    SaveMode::QrOnlyBulk => {
                        // Rozparsuj URL po řádcích
                        let urls: Vec<String> = bulk_urls
                            .lines()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .collect();

                        if urls.is_empty() {
                            anyhow::bail!("Vlož aspoň jednu URL (po řádku).");
                        }

                        // Výstupní složka
                        let dir = export_dir.unwrap_or_else(default_bulk_dir);
                        fs::create_dir_all(&dir)
                            .with_context(|| format!("Nelze vytvořit složku: {}", dir.display()))?;

                        let mut last = None;
                        let mut ok = 0usize;
                        for (i, u) in urls.iter().enumerate() {
                            let qr_img = build_qr_image(u, size, (mr, mg, mb), bg_opt, alpha, rounding)?;
                            let fname = make_qr_filename(i + 1, u, out_format);
                            let path = dir.join(fname);
                            save_qr(&qr_img, &path, out_format, bg_opt)?;
                            ok += 1;
                            last = Some(path);
                        }

                        let msg_path = last.unwrap_or(dir.clone());
                        println!("Hotovo: {} souborů do {}", ok, dir.display());
                        Ok(msg_path)
                    }
                }
            })();

            let _ = match res {
                Ok(p) => tx.send(JobResult::Ok(p)),
                Err(e) => tx.send(JobResult::Err(e.to_string())),
            };
        });
    }
}

/// Uloží obecný RGBA obrázek podle přípony (png/jpg/tif) – pro overlay.
fn save_image_rgba(img: &DynamicImage, outp: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let ext = outp.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => {
            let mut out = std::fs::File::create(outp)?;
            let rgb = img.to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92);
            encoder
                .encode_image(&DynamicImage::ImageRgb8(rgb))
                .context("JPEG encode selhal")?;
        }
        "png" | "tif" | "tiff" | "" => {
            img.save(outp).context("Uložení obrázku selhalo")?;
        }
        other => anyhow::bail!("Nepodporovaná přípona: .{other} (použij .jpg/.jpeg/.png/.tif/.tiff)"),
    }
    Ok(())
}

/// Uloží samostatný QR (RGBA) ve zvoleném formátu.
/// - PNG/TIFF: zachová alfu.
/// - JPEG: slije alfu na pozadí (bílá pokud `bg_opt=None`, jinak zadaná barva).
fn save_qr(qr: &RgbaImage, outp: &Path, fmt: OutputFormat, bg_opt: Option<(u8, u8, u8)>) -> anyhow::Result<()> {
    use anyhow::Context;
    match fmt {
        OutputFormat::Png | OutputFormat::Tiff => {
            DynamicImage::ImageRgba8(qr.clone()).save(outp).context("Uložení obrázku selhalo")?;
        }
        OutputFormat::Jpeg => {
            let bg = bg_opt.unwrap_or((255, 255, 255));
            let rgb = flatten_rgba_to_rgb(qr, bg);
            let mut out = std::fs::File::create(outp)?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92);
            encoder
                .encode_image(&DynamicImage::ImageRgb8(rgb))
                .context("JPEG encode selhal")?;
        }
    }
    Ok(())
}

/// Vykreslí QR kód s barvou modulů, volitelnou barvou pozadí, průhledností a zaoblením.
/// - `bg_rgb = None` → pozadí QR je plně průhledné (ekvivalent „Odstranit pozadí“)
/// - `rounding_percent` v rozsahu 0–50 (% z velikosti modulu)
fn build_qr_image(
    url: &str,
    size_px: u32,
    mod_rgb: (u8, u8, u8),
    bg_rgb: Option<(u8, u8, u8)>,
    alpha_percent: u8,
    rounding_percent: u8,
) -> anyhow::Result<RgbaImage> {
    use anyhow::Context;

    let code = QrCode::new(url.as_bytes()).context("Neplatné URL pro QR?")?;
    let width_mod = code.width() as u32;
    let quiet_zone_mod: u32 = 4; // doporučené minimum
    let total_mod = width_mod + 2 * quiet_zone_mod;

    // supersampling pro hladké zaoblení
    let ss: u32 = 4;
    let target_ss = size_px.max(total_mod) * ss;
    let module_ss = (target_ss / total_mod).max(1);
    let canvas_ss = module_ss * total_mod;

    let a = ((alpha_percent as u16 * 255) / 100) as u8;
    let mod_rgba = Rgba([mod_rgb.0, mod_rgb.1, mod_rgb.2, a]);
    let bg_rgba = match bg_rgb {
        Some(c) => Rgba([c.0, c.1, c.2, a]),
        None => Rgba([0, 0, 0, 0]),
    };

    let mut img = RgbaImage::from_pixel(canvas_ss, canvas_ss, bg_rgba);

    // přepočet zaoblení na pixely v supersamplovaném prostoru
    let mut r = (module_ss as f32 * (rounding_percent as f32 / 100.0)).round() as i32;
    let half = (module_ss / 2) as i32;
    if r > half {
        r = half; // max 50 % (bez přesahů)
    }

    // vykresli moduly
    for y in 0..width_mod {
        for x in 0..width_mod {
            if code[(x as usize, y as usize)] == QrColor::Dark {
                let x0 = ((x + quiet_zone_mod) * module_ss) as i32;
                let y0 = ((y + quiet_zone_mod) * module_ss) as i32;
                let w = module_ss as i32;
                let h = w;

                if r <= 0 {
                    draw_filled_rect_mut(&mut img, Rect::at(x0, y0).of_size(w as u32, h as u32), mod_rgba);
                } else {
                    // středové pruhy
                    if w - 2 * r > 0 {
                        draw_filled_rect_mut(&mut img, Rect::at(x0 + r, y0).of_size((w - 2 * r) as u32, h as u32), mod_rgba);
                        draw_filled_rect_mut(&mut img, Rect::at(x0, y0 + r).of_size(w as u32, (h - 2 * r) as u32), mod_rgba);
                    }

                    // čtyři kruhy vnitřních rohů
                    let cx1 = x0 + r;
                    let cy1 = y0 + r;
                    let cx2 = x0 + w - r - 1;
                    let cy2 = y0 + h - r - 1;
                    draw_filled_circle_mut(&mut img, (cx1, cy1), r, mod_rgba);
                    draw_filled_circle_mut(&mut img, (cx2, cy1), r, mod_rgba);
                    draw_filled_circle_mut(&mut img, (cx1, cy2), r, mod_rgba);
                    draw_filled_circle_mut(&mut img, (cx2, cy2), r, mod_rgba);
                }
            }
        }
    }

    // downscale na cílovou velikost (vyhlazení hran)
    let final_img = imageops::resize(&img, size_px, size_px, imageops::FilterType::Lanczos3);
    Ok(final_img)
}

/// Slije RGBA na zadané RGB pozadí (pro JPEG).
fn flatten_rgba_to_rgb(src: &RgbaImage, bg: (u8, u8, u8)) -> RgbImage {
    let (w, h) = src.dimensions();
    let mut dst = RgbImage::new(w, h);
    for (x, y, p) in src.enumerate_pixels() {
        let (sr, sg, sb, sa) = (p[0] as u16, p[1] as u16, p[2] as u16, p[3] as u16);
        let a = sa; // 0..255
        let ir = (sr * a + (bg.0 as u16) * (255 - a) + 127) / 255;
        let ig = (sg * a + (bg.1 as u16) * (255 - a) + 127) / 255;
        let ib = (sb * a + (bg.2 as u16) * (255 - a) + 127) / 255;
        dst.put_pixel(x, y, Rgb([ir as u8, ig as u8, ib as u8]));
    }
    dst
}

fn first_nonempty_line(s: &str) -> Option<String> {
    for line in s.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn default_out_path(in_path: Option<&PathBuf>) -> PathBuf {
    match in_path {
        Some(p) => {
            let parent = p.parent().unwrap_or_else(|| Path::new("."));
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("png");
            parent.join(format!("out_{}.{}", stem, ext))
        }
        None => default_qr_out_path(),
    }
}

fn default_qr_out_path() -> PathBuf {
    PathBuf::from("qr.png")
}

fn default_bulk_dir() -> PathBuf {
    PathBuf::from("qr_export")
}

fn make_qr_filename(index1: usize, url: &str, fmt: OutputFormat) -> String {
    let slug = make_slug_from_url(url);
    let hash10 = sha1_hex10(url);
    let base = if slug.is_empty() {
        format!("qr_{:03}_{}", index1, hash10)
    } else {
        format!("qr_{:03}_{}_{}", index1, slug, hash10)
    };
    format!("{base}.{}", fmt.ext())
}

fn sha1_hex10(s: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    let mut out = String::with_capacity(10);
    for b in bytes.iter().take(5) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn make_slug_from_url(url: &str) -> String {
    // jednoduchý slug: host + poslední segment cesty
    let u = url.trim().trim_end_matches('/');
    let host = u.split("://").nth(1).unwrap_or(u);
    let host = host.split('/').next().unwrap_or("");
    let last = u.rsplit('/').next().unwrap_or("");
    let mut s = String::new();
    if !host.is_empty() {
        s.push_str(&sanitize_for_filename(host));
    }
    if !last.is_empty() && last != host {
        if !s.is_empty() {
            s.push('_');
        }
        s.push_str(&sanitize_for_filename(last));
    }
    if s.len() > 40 {
        s.truncate(40);
    }
    s.trim_matches('_').to_string()
}

fn sanitize_for_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_ascii() {
            out.push('-');
        } // ne-ASCII: vynecháme
    }
    // sloučit víc '-' do jednoho
    let mut compact = String::with_capacity(out.len());
    let mut prev_dash = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_dash {
                compact.push(c);
            }
            prev_dash = true;
        } else {
            compact.push(c);
            prev_dash = false;
        }
    }
    compact.trim_matches('-').to_string()
}

fn shorten(p: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        if let Some(rel) = pathdiff::diff_paths(p, cwd) {
            return rel.to_string_lossy().to_string();
        }
    }
    p.to_string_lossy().to_string()
}
