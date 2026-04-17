use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow::array::{Array, DictionaryArray, FixedSizeListArray, Float64Array, StringArray, StructArray};
use arrow::datatypes::UInt32Type;
use arrow::record_batch::RecordBatch;
use eframe::egui;
use egui_plot::{Plot, Points};

use crate::sim_thread::SimState;

const AU_KM: f64 = 149_597_870.7;

pub struct SolVizApp {
    pub state: Arc<Mutex<SimState>>,
    pub playing: Arc<AtomicBool>,
    pub steps_per_tick: Arc<AtomicUsize>,
    pub use_au: bool,
    /// Label of the entity whose position is subtracted from all others before plotting.
    /// `None` = ICRF/SSB origin (no offset).
    pub camera_entity: Option<String>,
}

impl eframe::App for SolVizApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.playing.load(Ordering::Relaxed) {
            ctx.request_repaint_after(std::time::Duration::from_millis(32));
        }

        let scale = if self.use_au { 1.0 / AU_KM } else { 1.0 };

        // Extract entity data once and drop the lock before rendering panels.
        let (entities, current_epoch, step_count) = {
            let guard = self.state.lock().unwrap();
            let ents = guard
                .snapshot
                .as_ref()
                .map(|s| extract_entities(s, scale))
                .unwrap_or_default();
            (ents, guard.current_epoch, guard.step_count)
        };

        // Camera offset: position of the selected entity (already scaled).
        // All other entity positions are expressed relative to this point.
        let camera_offset: Option<(f64, f64)> = self.camera_entity.as_ref().and_then(|cam| {
            entities
                .iter()
                .find(|(label, _, _)| label == cam)
                .map(|(_, x, y)| (*x, *y))
        });

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Play / pause
                let playing = self.playing.load(Ordering::Relaxed);
                if ui.button(if playing { "⏸ Pause" } else { "▶ Play" }).clicked() {
                    self.playing.store(!playing, Ordering::Relaxed);
                }

                ui.separator();

                // Speed selector
                let current_speed = self.steps_per_tick.load(Ordering::Relaxed);
                ui.label("Speed:");
                for (label, val) in [("1×", 1usize), ("10×", 10), ("100×", 100), ("1000×", 1000)] {
                    if ui.selectable_label(current_speed == val, label).clicked() {
                        self.steps_per_tick.store(val, Ordering::Relaxed);
                    }
                }

                ui.separator();
                ui.checkbox(&mut self.use_au, "AU scale");

                ui.separator();

                // Camera reference frame selector
                egui::ComboBox::from_label("Camera")
                    .selected_text(
                        self.camera_entity.as_deref().unwrap_or("SSB (origin)"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.camera_entity,
                            None,
                            "SSB (origin)",
                        );
                        for (label, _, _) in &entities {
                            ui.selectable_value(
                                &mut self.camera_entity,
                                Some(label.clone()),
                                label,
                            );
                        }
                    });

                ui.separator();
                ui.label(format!("Epoch: {current_epoch}"));
                ui.label(format!("Steps: {step_count}"));
            });
        });

        let axis_label = if self.use_au { "AU" } else { "km" };

        egui::CentralPanel::default().show(ctx, |ui| {
            Plot::new("orbital_view")
                .data_aspect(1.0)
                .x_axis_label(format!("X [{axis_label}]"))
                .y_axis_label(format!("Y [{axis_label}]"))
                .show(ui, |plot_ui| {
                    let (ox, oy) = camera_offset.unwrap_or((0.0, 0.0));
                    for (label, x, y) in &entities {
                        plot_ui.points(
                            Points::new(vec![[x - ox, y - oy]])
                                .name(label)
                                .radius(5.0),
                        );
                    }
                });
        });
    }
}

/// Extracts `(display_label, x, y)` for each entity in the snapshot.
/// Positions are scaled by `scale` (pass `1.0` for km, `1/AU_KM` for AU).
/// Returns an empty vec if required columns are missing.
fn extract_entities(snap: &RecordBatch, scale: f64) -> Vec<(String, f64, f64)> {
    extract_entities_inner(snap, scale).unwrap_or_default()
}

fn extract_entities_inner(snap: &RecordBatch, scale: f64) -> Option<Vec<(String, f64, f64)>> {
    let entity_col = snap
        .column_by_name("entity_id")?
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()?;
    let entity_dict = entity_col.values().as_any().downcast_ref::<StringArray>()?;

    let sts = snap
        .column_by_name("spacetimestamp")?
        .as_any()
        .downcast_ref::<StructArray>()?;

    let pos_list = sts
        .column_by_name("position")?
        .as_any()
        .downcast_ref::<FixedSizeListArray>()?;
    let pos_vals = pos_list.values().as_any().downcast_ref::<Float64Array>()?;
    let offset = pos_list.offset();

    let result = (0..snap.num_rows())
        .map(|i| {
            let full_id = entity_dict.value(entity_col.keys().value(i) as usize);
            // Trim URI prefix for display: "urn:soloc:viz:spacecraft" → "spacecraft"
            let label = full_id.split(':').last().unwrap_or(full_id).to_string();
            let base = (offset + i) * 3;
            let x = pos_vals.value(base) * scale;
            let y = pos_vals.value(base + 1) * scale;
            (label, x, y)
        })
        .collect();

    Some(result)
}
