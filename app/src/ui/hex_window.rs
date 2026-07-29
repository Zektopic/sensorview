//! Hex Viewer — the raw-byte inspector.
//!
//! Classic three-column dump (offset · hex · ASCII) over whatever blobs the
//! slow lane collected: ACPI tables, the SMBIOS blob, and later SPD/PCI config
//! once a kernel driver is available. Layout follows the dense terminal
//! reference: 16 bytes per row split into 4-byte groups, a mid-row gutter, and
//! an ASCII column that tracks the selection.
//!
//! Three things make it an inspector rather than a wall of hex:
//!
//! - **Field annotations.** Blobs carry [`HexRegion`]s (the ACPI header's
//!   signature/length/checksum, an SMBIOS version stamp); those bytes are
//!   tinted and named under the cursor.
//! - **Byte decoding.** The selected offset is decoded as every common scalar
//!   type in either byte order — how you read a length field or a version.
//! - **Search and go-to.** Hex or ASCII needles, wrapping.
//!
//! Rows are drawn through `egui_extras::TableBuilder`, which only builds the
//! visible ones — a DSDT is tens of thousands of rows.
//!
//! **Read-only.** Nothing here writes to hardware: committing bytes back to
//! SPD EEPROM or PCI configuration space can brick a board and needs the same
//! kernel driver we don't have. Export writes a copy to a file instead.

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use super::{Palette, Shared, WindowFlags};
use crate::model::hexblob::{inspect, Endian, HexBlob, RegionKind, ROW_WIDTH};

/// Per-window state. Lives in egui's temp store, keyed by the viewport.
#[derive(Clone)]
struct HexState {
    /// Index into `inventory.hex`.
    blob: usize,
    /// Selected byte offset within the blob.
    cursor: usize,
    endian: Endian,
    /// Text in the go-to box. Must live in state: a local would be discarded
    /// every frame, so nothing could ever be typed into it.
    goto: String,
    search: String,
    /// Interpret the search box as hex byte pairs rather than ASCII.
    search_hex: bool,
    /// Set when a jump is requested, consumed by the table to scroll.
    scroll_to: Option<usize>,
    status: String,
}

impl Default for HexState {
    fn default() -> Self {
        Self {
            blob: 0,
            cursor: 0,
            endian: Endian::Little,
            goto: String::new(),
            search: String::new(),
            search_hex: true,
            scroll_to: None,
            status: String::new(),
        }
    }
}

pub fn show(ui: &mut egui::Ui, s: &Shared) {
    super::handle_close(ui, &s.windows.hex);
    let pal = s.palette();

    let frame = s.frame();
    let blobs = &frame.inventory.hex;

    let id = egui::Id::new("hex_state");
    let mut state: HexState = ui.ctx().data_mut(|d| d.get_temp_mut_or_default::<HexState>(id).clone());
    state.blob = state.blob.min(blobs.len().saturating_sub(1));

    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(match blobs.get(state.blob) {
        Some(b) => format!("Hex Viewer — {}", b.source.label()),
        None => "Hex Viewer".to_string(),
    }));

    if blobs.is_empty() {
        empty_state(ui, s, &pal);
        return;
    }

    toolbar(ui, s, blobs, &mut state, &pal);
    inspector_panel(ui, &blobs[state.blob], &mut state, &pal);
    dump(ui, &blobs[state.blob], &mut state, &pal);

    ui.ctx().data_mut(|d| d.insert_temp(id, state));
}

/// Shown while the slow lane hasn't produced anything — which on Linux usually
/// means the ACPI tables need root, so say that rather than showing a blank.
fn empty_state(ui: &mut egui::Ui, s: &Shared, pal: &Palette) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::same(16)))
        .show(ui, |ui| {
            ui.label(RichText::new("No byte dumps available").color(pal.text).size(13.0).strong());
            ui.add_space(6.0);
            let waiting = s.frame().seq < 2;
            ui.label(
                RichText::new(if waiting {
                    "Enumerating firmware tables…"
                } else if cfg!(target_os = "linux") {
                    "ACPI tables live in /sys/firmware/acpi/tables, which is normally \
                     readable only by root. Run elevated to inspect them."
                } else if cfg!(windows) {
                    "No ACPI or SMBIOS tables were returned by the firmware. SPD and PCI \
                     configuration dumps additionally need a kernel driver — see \
                     Settings → Driver Management."
                } else if cfg!(target_os = "macos") {
                    "Apple Silicon has no ACPI or SMBIOS tables — the firmware describes \
                     hardware with an ARM device tree instead. Storage identity and health \
                     are collected from IOKit and appear here once the slow lane has run."
                } else {
                    "Firmware table enumeration is not implemented on this platform yet."
                })
                .color(pal.text_dim)
                .size(11.5),
            );
        });
}

fn toolbar(
    ui: &mut egui::Ui,
    s: &Shared,
    blobs: &[HexBlob],
    state: &mut HexState,
    pal: &Palette,
) {
    egui::Panel::top("hex_toolbar")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_header)
                .inner_margin(egui::Margin::symmetric(6, 4)),
        )
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                // ---- Blob picker -------------------------------------------
                let current = blobs[state.blob].source.label();
                egui::ComboBox::from_id_salt("hex_blob")
                    .selected_text(RichText::new(current).size(11.0))
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for (i, b) in blobs.iter().enumerate() {
                            let label = format!("{}  ({})", b.source.label(), size_text(b.bytes.len()));
                            if ui
                                .selectable_label(state.blob == i, RichText::new(label).size(11.0))
                                .clicked()
                                && state.blob != i
                            {
                                state.blob = i;
                                state.cursor = 0;
                                state.scroll_to = Some(0);
                                state.status.clear();
                            }
                        }
                    });

                ui.separator();

                // ---- Go to offset ------------------------------------------
                ui.label(RichText::new("Go to").color(pal.text_dim).size(11.0));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.goto)
                        .hint_text("0x0")
                        .desired_width(64.0)
                        .font(egui::TextStyle::Monospace),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let len = blobs[state.blob].bytes.len();
                    match parse_offset(&state.goto) {
                        Some(off) => jump_to(state, off, len, pal),
                        None if !state.goto.trim().is_empty() => {
                            state.status = format!("{:?} is not an offset", state.goto);
                        }
                        None => {}
                    }
                }

                ui.separator();

                // ---- Search -------------------------------------------------
                ui.label(RichText::new("Find").color(pal.text_dim).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut state.search)
                        .hint_text(if state.search_hex { "DE AD BE EF" } else { "text" })
                        .desired_width(120.0)
                        .font(egui::TextStyle::Monospace),
                );
                if ui
                    .selectable_label(state.search_hex, RichText::new("hex").size(10.5))
                    .on_hover_text("Interpret the needle as hex byte pairs")
                    .clicked()
                {
                    state.search_hex = !state.search_hex;
                }
                if ui.button(RichText::new("Next").size(11.0)).on_hover_text("Find next").clicked() {
                    find_next(state, &blobs[state.blob]);
                }

                ui.separator();

                // ---- Endianness ---------------------------------------------
                if ui
                    .button(RichText::new(state.endian.label()).size(11.0).monospace())
                    .on_hover_text("Byte order used by the inspector — click to switch")
                    .clicked()
                {
                    state.endian = match state.endian {
                        Endian::Little => Endian::Big,
                        Endian::Big => Endian::Little,
                    };
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("Export").size(11.0))
                        .on_hover_text("Write this dump to a .bin and .txt beside the reports")
                        .clicked()
                    {
                        state.status = match export(&blobs[state.blob]) {
                            Ok(p) => format!("Exported: {}", p.display()),
                            Err(e) => format!("Export failed: {e}"),
                        };
                    }
                    if ui
                        .button(RichText::new("Close").color(pal.accent).size(11.0))
                        .on_hover_text("Close")
                        .clicked()
                    {
                        WindowFlags::close(&s.windows.hex);
                    }
                });
            });
        });

    if !state.status.is_empty() {
        egui::Panel::top("hex_status")
            .frame(
                egui::Frame::new()
                    .fill(pal.bg_panel)
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ui, |ui| {
                ui.label(RichText::new(&state.status).color(pal.text_dim).size(10.5));
            });
    }
}

/// Bottom strip: what the cursor is sitting on, and what those bytes decode to.
fn inspector_panel(ui: &mut egui::Ui, blob: &HexBlob, state: &mut HexState, pal: &Palette) {
    egui::Panel::bottom("hex_inspector")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .inner_margin(egui::Margin::symmetric(8, 5)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("offset {:08X}", blob.base_addr as usize + state.cursor))
                        .color(pal.accent)
                        .monospace()
                        .size(11.0),
                );
                if let Some(region) = blob.region_at(state.cursor) {
                    ui.separator();
                    ui.label(
                        RichText::new(&region.label)
                            .color(region_color(region.kind, pal))
                            .size(11.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "[{:X}..{:X}]",
                            region.start,
                            region.start + region.len
                        ))
                        .color(pal.text_dim)
                        .monospace()
                        .size(10.5),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} of {} bytes",
                            state.cursor + 1,
                            blob.bytes.len()
                        ))
                        .color(pal.text_dim)
                        .size(10.5),
                    );
                });
            });

            ui.add_space(3.0);
            // Decoded values, wrapped so the strip stays usable when narrow.
            ui.horizontal_wrapped(|ui| {
                for (name, value) in inspect(&blob.bytes, state.cursor, state.endian) {
                    ui.label(RichText::new(name).color(pal.text_dim).size(10.0));
                    ui.label(
                        RichText::new(value).color(pal.value).monospace().size(10.5),
                    );
                    ui.add_space(6.0);
                }
            });
        });
}

/// The dump itself: offset · 16 hex cells · ASCII gutter.
fn dump(ui: &mut egui::Ui, blob: &HexBlob, state: &mut HexState, pal: &Palette) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::same(4)))
        .show(ui, |ui| {
            let row_h = 16.0;
            let rows = blob.rows();
            let mono = egui::FontId::monospace(11.5);
            // Width of one "FF " cell plus the group gutters.
            let cell_w = 21.0;

            let mut table = TableBuilder::new(ui)
                .striped(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(76.0)) // offset
                .column(Column::exact(cell_w * ROW_WIDTH as f32 + 20.0)) // hex
                .column(Column::remainder()) // ascii
                .min_scrolled_height(0.0);

            if let Some(target) = state.scroll_to.take() {
                table = table.scroll_to_row(target / ROW_WIDTH, Some(egui::Align::Center));
            }

            table.body(|body| {
                body.rows(row_h, rows, |mut row| {
                    let r = row.index();
                    let start = r * ROW_WIDTH;
                    let end = (start + ROW_WIDTH).min(blob.bytes.len());

                    // ---- Offset ------------------------------------------
                    row.col(|ui| {
                        ui.label(
                            RichText::new(format!("{:08X}", blob.base_addr as usize + start))
                                .color(pal.text_dim)
                                .font(mono.clone()),
                        );
                    });

                    // ---- Hex cells ---------------------------------------
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for i in start..end {
                            // 4-byte grouping, with a wider gutter mid-row.
                            if i > start && (i - start).is_multiple_of(4) {
                                ui.add_space(if (i - start) == ROW_WIDTH / 2 { 8.0 } else { 4.0 });
                            }
                            byte_cell(ui, blob, i, state, pal, &mono, cell_w);
                        }
                    });

                    // ---- ASCII gutter ------------------------------------
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for i in start..end {
                            ascii_cell(ui, blob, i, state, pal, &mono);
                        }
                    });
                });
            });
        });
}

/// One clickable hex byte, tinted by the region it belongs to.
fn byte_cell(
    ui: &mut egui::Ui,
    blob: &HexBlob,
    offset: usize,
    state: &mut HexState,
    pal: &Palette,
    mono: &egui::FontId,
    width: f32,
) {
    let b = blob.bytes[offset];
    let selected = offset == state.cursor;
    let color = if selected {
        pal.bg
    } else if b == 0 {
        // Zero-fill is the bulk of most dumps; dim it so structure stands out.
        pal.text_dim.gamma_multiply(0.55)
    } else {
        blob.region_at(offset)
            .map(|r| region_color(r.kind, pal))
            .unwrap_or(pal.value)
    };

    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(width, ui.available_height().min(16.0)),
        egui::Sense::click(),
    );
    if selected {
        ui.painter().rect_filled(rect, 0.0, pal.accent);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, pal.bg_header);
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{b:02X}"),
        mono.clone(),
        color,
    );
    if resp.clicked() {
        state.cursor = offset;
    }
    if resp.hovered() {
        if let Some(region) = blob.region_at(offset) {
            resp.on_hover_text(format!("{} @ {:08X}", region.label, blob.base_addr as usize + offset));
        }
    }
}

/// The ASCII counterpart of a byte, selectable in step with the hex column.
fn ascii_cell(
    ui: &mut egui::Ui,
    blob: &HexBlob,
    offset: usize,
    state: &mut HexState,
    pal: &Palette,
    mono: &egui::FontId,
) {
    let b = blob.bytes[offset];
    let printable = (0x20..0x7f).contains(&b);
    let selected = offset == state.cursor;

    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(8.0, ui.available_height().min(16.0)),
        egui::Sense::click(),
    );
    if selected {
        ui.painter().rect_filled(rect, 0.0, pal.accent);
    }
    let color = if selected {
        pal.bg
    } else if printable {
        blob.region_at(offset)
            .map(|r| region_color(r.kind, pal))
            .unwrap_or(pal.text)
    } else {
        pal.text_dim.gamma_multiply(0.55)
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        if printable { b as char } else { '.' },
        mono.clone(),
        color,
    );
    if resp.clicked() {
        state.cursor = offset;
    }
}

/// One colour per region kind, so the same hue always means the same thing.
fn region_color(kind: RegionKind, pal: &Palette) -> Color32 {
    match kind {
        RegionKind::Identity => pal.accent,
        RegionKind::Length => pal.clockc,
        RegionKind::Checksum => pal.warn,
        RegionKind::Payload => pal.value,
    }
}

fn size_text(len: usize) -> String {
    if len >= 1024 {
        format!("{:.1} KiB", len as f32 / 1024.0)
    } else {
        format!("{len} B")
    }
}

/// Accepts `0x2C`, `2C`, or decimal `44`.
fn parse_offset(text: &str) -> Option<usize> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return usize::from_str_radix(hex, 16).ok();
    }
    // Bare input is hex — this is a hex viewer — but a plain decimal that isn't
    // valid hex still resolves rather than being rejected.
    usize::from_str_radix(t, 16).ok().or_else(|| t.parse().ok())
}

fn jump_to(state: &mut HexState, offset: usize, len: usize, _pal: &Palette) {
    if len == 0 {
        return;
    }
    if offset >= len {
        state.status = format!("Offset {offset:X} is past the end of this dump ({len:X})");
        return;
    }
    state.cursor = offset;
    state.scroll_to = Some(offset);
    state.status.clear();
}

/// Parse the search box into a byte needle, per the hex/text toggle.
fn parse_needle(text: &str, as_hex: bool) -> Option<Vec<u8>> {
    if as_hex {
        // Tolerate "DEADBEEF", "DE AD BE EF" and "de:ad:be:ef".
        let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
            return None;
        }
        cleaned
            .as_bytes()
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
            .collect()
    } else {
        (!text.is_empty()).then(|| text.as_bytes().to_vec())
    }
}

fn find_next(state: &mut HexState, blob: &HexBlob) {
    let Some(needle) = parse_needle(&state.search, state.search_hex) else {
        state.status = if state.search_hex {
            "Search needs an even number of hex digits, e.g. DE AD BE EF".into()
        } else {
            "Enter text to search for".into()
        };
        return;
    };
    match blob.find_wrapping(&needle, state.cursor + 1) {
        Some(found) => {
            state.cursor = found;
            state.scroll_to = Some(found);
            state.status = format!("Found at {:08X}", blob.base_addr as usize + found);
        }
        None => state.status = "Not found".into(),
    }
}

/// Write the dump beside the text reports: `.bin` for the raw bytes, `.txt`
/// for the formatted dump. Never touches hardware.
fn export(blob: &HexBlob) -> Result<std::path::PathBuf, String> {
    let dir = dirs::document_dir()
        .or_else(dirs::desktop_dir)
        .filter(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        .unwrap_or_else(std::env::temp_dir);

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe: String = blob
        .source
        .label()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let stem = dir.join(format!("SensorView_{safe}_{stamp}"));

    let bin = stem.with_extension("bin");
    std::fs::write(&bin, &blob.bytes).map_err(|e| e.to_string())?;
    let txt = stem.with_extension("txt");
    std::fs::write(&txt, blob.to_text()).map_err(|e| e.to_string())?;
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::hexblob::HexSource;

    #[test]
    fn offsets_parse_as_hex_with_or_without_prefix() {
        assert_eq!(parse_offset("0x2C"), Some(0x2C));
        assert_eq!(parse_offset("0X2c"), Some(0x2C));
        assert_eq!(parse_offset("2C"), Some(0x2C));
        assert_eq!(parse_offset("  10 "), Some(0x10), "bare input is hex");
        // Decimal-only digits that aren't valid hex still resolve.
        assert_eq!(parse_offset("99"), Some(0x99));
        assert_eq!(parse_offset(""), None);
        assert_eq!(parse_offset("zz"), None);
    }

    #[test]
    fn hex_needles_tolerate_separators() {
        assert_eq!(parse_needle("DEADBEEF", true), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(parse_needle("de ad be ef", true), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(parse_needle("DE:AD", true), Some(vec![0xDE, 0xAD]));
        // An odd digit count is ambiguous — rejected rather than guessed at.
        assert_eq!(parse_needle("DEA", true), None);
        assert_eq!(parse_needle("", true), None);
        // Text mode is literal bytes.
        assert_eq!(parse_needle("AB", false), Some(b"AB".to_vec()));
        assert_eq!(parse_needle("", false), None);
    }

    #[test]
    fn find_next_advances_past_the_current_hit_and_wraps() {
        let blob = HexBlob::new(
            HexSource::EcRam { offset: 0 },
            0,
            vec![0xDE, 0xAD, 0x00, 0x00, 0xDE, 0xAD],
        );
        let mut state = HexState { search: "DEAD".into(), search_hex: true, ..Default::default() };

        find_next(&mut state, &blob);
        assert_eq!(state.cursor, 4, "advances rather than re-finding the hit at 0");
        find_next(&mut state, &blob);
        assert_eq!(state.cursor, 0, "wraps back to the first match");

        state.search = "ZZ".into();
        find_next(&mut state, &blob);
        assert!(state.status.contains("hex digits"), "{}", state.status);
    }

    #[test]
    fn jump_rejects_offsets_past_the_end() {
        let pal = Palette::of(crate::settings::ColorMode::Black);
        let mut state = HexState::default();
        jump_to(&mut state, 5, 16, &pal);
        assert_eq!(state.cursor, 5);
        assert_eq!(state.scroll_to, Some(5));

        jump_to(&mut state, 99, 16, &pal);
        assert_eq!(state.cursor, 5, "cursor is left where it was");
        assert!(state.status.contains("past the end"), "{}", state.status);
    }

    #[test]
    fn sizes_read_in_the_right_unit() {
        assert_eq!(size_text(128), "128 B");
        assert_eq!(size_text(1024), "1.0 KiB");
        assert_eq!(size_text(35_000), "34.2 KiB");
    }
}
