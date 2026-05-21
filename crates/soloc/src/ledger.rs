//! Append-only Arrow RecordBatch store — the soloc Universal Ledger.
//!
//! The [`Ledger`] accumulates entity snapshots as Arrow [`RecordBatch`]es and never
//! overwrites existing data. Measured observations (from telescopes, sensors, manual input)
//! and simulation outputs (`estimate_type = "SIMULATED"`) coexist in the same store
//! and are distinguished by their `estimate_type` field.
//!
//! # Layout
//!
//! Each appended batch is a "snapshot" — typically all tracked entities at one timestep.
//! Batches are stored in insertion order. After the first simulation step the last batch
//! is always the most recent complete state of every tracked entity.
//!
//! # Streaming
//!
//! [`Ledger::stream_query`] yields matching batches one at a time without concatenating.
//! This is the primary path for a UI renderer: the first visible entities arrive before
//! the full ledger has been scanned.
//!
//! # Persistence
//!
//! [`Ledger::save_ipc`] / [`Ledger::load_ipc`] use the Arrow IPC file format. All batches
//! are serialized to a single file in insertion order and reconstructed on load.

use arrow::array::{
    Array, BooleanBuilder, DictionaryArray, FixedSizeListArray, Float64Array,
    Int16Array, StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::{UInt16Type, UInt32Type};
use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};
use nalgebra::{Isometry3, Quaternion, Translation3, UnitQuaternion};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use anise::prelude::Almanac;
use spacetimestamp::query::{SpatiotemporalFilter, filter_batch};
use spacetimestamp::schema::is_entity_uri;
use spacetimestamp::transforms::transform_batch;

/// Merge batches in memory when the count exceeds this to keep query latency bounded.
///
/// Benchmarks show ~11 µs fixed overhead per batch. At 50 batches of ≥1000 rows each,
/// time-filter queries stay under ~1 ms. Beyond this threshold, merging pays off.
const SEGMENT_THRESHOLD: usize = 50;

/// An append-only store of [`RecordBatch`]es forming the soloc Universal Ledger.
///
/// Designed for use with entity batches following [`crate::entity::entity_schema`], but
/// accepts any batch — schema validation is deferred to higher-level ingestion logic.
#[derive(Default)]
pub struct Ledger {
    batches: Vec<RecordBatch>,
}

impl Ledger {
    /// Creates an empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a batch to the ledger. Existing data is never modified.
    pub fn append(&mut self, batch: RecordBatch) {
        self.batches.push(batch);
        self.seal_and_flush_if_needed();
    }

    /// Merges all batches into one when the batch count exceeds [`SEGMENT_THRESHOLD`].
    ///
    /// Per-batch fixed overhead dominates query cost, so keeping the count low is critical.
    /// If concat fails (schema mismatch, OOM), the ledger is left unchanged.
    fn seal_and_flush_if_needed(&mut self) {
        if self.batches.len() <= SEGMENT_THRESHOLD {
            return;
        }
        if let Ok(merged) = arrow::compute::concat_batches(&self.batches[0].schema(), &self.batches) {
            self.batches = vec![merged];
        }
    }

    /// Returns the number of batches currently stored.
    pub fn len(&self) -> usize {
        self.batches.len()
    }

    /// Returns `true` if the ledger holds no batches.
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Filters all batches and returns the matching rows as a single concatenated
    /// [`RecordBatch`].
    ///
    /// Uses [`spacetimestamp::query::filter_batch`] internally, so the same frame-uniformity
    /// rules apply: spatial filters require all rows to be in the same frame.
    ///
    /// Returns an empty batch (correct schema, 0 rows) when there are no matches.
    pub fn query(
        &self,
        filter: &SpatiotemporalFilter,
        sts_column: &str,
    ) -> Result<RecordBatch, String> {
        if self.batches.is_empty() {
            return Err("Ledger is empty".to_string());
        }

        let schema = self.batches[0].schema();
        let mut kept: Vec<RecordBatch> = Vec::new();

        for batch in &self.batches {
            let filtered = filter_batch(batch, sts_column, filter)?;
            if filtered.num_rows() > 0 {
                kept.push(filtered);
            }
        }

        if kept.is_empty() {
            return Ok(RecordBatch::new_empty(schema));
        }

        arrow::compute::concat_batches(&schema, &kept)
            .map_err(|e| format!("Failed to concatenate filtered batches: {e}"))
    }

    /// Lazily yields filtered batches one at a time without concatenation.
    ///
    /// This is the preferred API for streaming data to a UI renderer — the first matching
    /// batch is yielded immediately rather than waiting for a full ledger scan.
    pub fn stream_query<'a>(
        &'a self,
        filter: &'a SpatiotemporalFilter,
        sts_column: &'a str,
    ) -> impl Iterator<Item = Result<RecordBatch, String>> + 'a {
        self.batches
            .iter()
            .map(move |batch| filter_batch(batch, sts_column, filter))
    }

    /// Returns the most recent batch in the ledger, optionally filtered to specific entity IDs.
    ///
    /// After the first simulation step every subsequent batch is a full-entity snapshot, so
    /// the last batch always represents the latest known state of all tracked entities.
    ///
    /// If `entity_ids` is provided, only rows whose `entity_id` matches one of the given
    /// strings are returned.
    ///
    /// # Note
    ///
    /// This is a fast-path implementation that returns the last batch only. It is correct
    /// for the common simulation workflow where all entities are seeded in a single initial
    /// batch. For heterogeneous ingestion where different entities may have their most recent
    /// state in different batches, a full ledger scan is needed (planned for a future version).
    pub fn latest_snapshot(&self, entity_ids: Option<&[&str]>) -> Option<RecordBatch> {
        let last = self.batches.last()?;

        let ids = match entity_ids {
            None => return Some(last.clone()),
            Some(ids) => ids,
        };

        let entity_col = last
            .column_by_name("entity_id")?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()?;
        let entity_dict = entity_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()?;

        let mut mask = BooleanBuilder::with_capacity(last.num_rows());
        for row in 0..last.num_rows() {
            let key = entity_col.keys().value(row) as usize;
            mask.append_value(ids.contains(&entity_dict.value(key)));
        }

        let mask_arr = mask.finish();
        let filtered: Result<Vec<_>, _> = last
            .columns()
            .iter()
            .map(|col| arrow::compute::filter(col.as_ref(), &mask_arr))
            .collect();

        RecordBatch::try_new(last.schema(), filtered.ok()?).ok()
    }

    /// Seeds the ledger with a celestial body snapshot at the given epoch.
    ///
    /// Equivalent to calling [`crate::ephemeris::celestial_snapshot`] and appending the result.
    /// This is the recommended way to initialise a ledger for simulation or analysis without
    /// a running server.
    ///
    /// ```rust,ignore
    /// let almanac = MetaAlmanac::latest()?;
    /// let mut ledger = Ledger::new();
    /// ledger.seed_solar_system(&almanac, CelestialBody::ALL, epoch)?;
    /// ```
    pub fn seed_solar_system(
        &mut self,
        almanac: &Almanac,
        bodies: &[crate::ephemeris::CelestialBody],
        epoch: Epoch,
    ) -> Result<(), String> {
        let batch = crate::ephemeris::celestial_snapshot(almanac, bodies, epoch)?;
        self.append(batch);
        Ok(())
    }

    /// Transforms `batch` into `target_frame`, resolving any entity-URI frame chains
    /// against this ledger's current contents.
    ///
    /// This is the in-process equivalent of the server's `DoExchange` endpoint. The caller
    /// supplies the almanac so the ledger itself remains a pure data store.
    ///
    /// Entity-URI `frame_id` values (e.g. `"demo:truck_A"`) are resolved by looking up
    /// the parent entity's latest pose in the ledger at the batch's epoch and composing
    /// the isometry chain. Static [`spacetimestamp::schema::FrameRegistry`] entries embedded
    /// in `batch`'s schema metadata are honoured automatically.
    ///
    /// ```rust,ignore
    /// let result = ledger.transform(&my_batch, "spacetimestamp", "ICRF", "km", &almanac)?;
    /// ```
    pub fn transform(
        &self,
        batch: &RecordBatch,
        sts_column: &str,
        target_frame: &str,
        target_units: &str,
        almanac: &Almanac,
    ) -> Result<RecordBatch, String> {
        // Extract the epoch from the first row so dynamic frame lookup is time-consistent.
        let epoch = epoch_from_batch(batch, sts_column);

        // Collect unique entity-URI frame_ids from the batch's frame_id dictionary.
        let uri_frames = collect_uri_frames(batch, sts_column);

        let dynamic_frames = if uri_frames.is_empty() {
            None
        } else {
            let ids: Vec<&str> = uri_frames.iter().map(|s| s.as_str()).collect();
            Some(self.build_dynamic_frame_map(&ids, epoch)?)
        };

        transform_batch(batch, sts_column, target_frame, almanac, target_units, dynamic_frames.as_ref())
    }

    /// Returns the pose of `entity_id` at the latest timestamp ≤ `epoch` as an
    /// `(parent_frame_id, isometry_km)` pair, where the isometry translates child-frame
    /// coordinates into `parent_frame_id` coordinates, with the translation in km.
    ///
    /// Returns `None` if the entity has no entry at or before `epoch` in the ledger.
    pub fn resolve_frame_at(
        &self,
        entity_id: &str,
        epoch: Epoch,
    ) -> Option<(String, Isometry3<f64>)> {
        let j2000 = Epoch::from_gregorian_tai(2000, 1, 1, 12, 0, 0, 0);
        let target_dur = epoch - j2000;

        let mut best_dur: Option<Duration> = None;
        let mut best: Option<(String, Isometry3<f64>)> = None;

        for batch in &self.batches {
            let Some(eid_raw) = batch.column_by_name("entity_id") else { continue };
            let Some(eid_col) = eid_raw.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() else { continue };
            let Some(eid_dict) = eid_col.values().as_any().downcast_ref::<StringArray>() else { continue };

            let Some(sts_raw) = batch.column_by_name("spacetimestamp") else { continue };
            let Some(sts) = sts_raw.as_any().downcast_ref::<StructArray>() else { continue };

            let Some(frame_raw) = sts.column_by_name("frame_id") else { continue };
            let Some(frame_col) = frame_raw.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() else { continue };
            let Some(frame_dict) = frame_col.values().as_any().downcast_ref::<StringArray>() else { continue };

            let Some(units_raw) = sts.column_by_name("units_pos") else { continue };
            let Some(units_col) = units_raw.as_any().downcast_ref::<DictionaryArray<UInt16Type>>() else { continue };
            let Some(units_dict) = units_col.values().as_any().downcast_ref::<StringArray>() else { continue };

            let Some(pos_raw) = sts.column_by_name("position") else { continue };
            let Some(pos_list) = pos_raw.as_any().downcast_ref::<FixedSizeListArray>() else { continue };
            let Some(pos_vals) = pos_list.values().as_any().downcast_ref::<Float64Array>() else { continue };
            let pos_offset = pos_list.offset();

            let Some(quat_raw) = sts.column_by_name("quaternion") else { continue };
            let Some(quat_list) = quat_raw.as_any().downcast_ref::<FixedSizeListArray>() else { continue };
            let Some(quat_vals) = quat_list.values().as_any().downcast_ref::<Float64Array>() else { continue };
            let quat_offset = quat_list.offset();

            let Some(cent_raw) = sts.column_by_name("duration_centuries") else { continue };
            let Some(cent_arr) = cent_raw.as_any().downcast_ref::<Int16Array>() else { continue };

            let Some(ns_raw) = sts.column_by_name("duration_ns") else { continue };
            let Some(ns_arr) = ns_raw.as_any().downcast_ref::<UInt64Array>() else { continue };

            for i in 0..batch.num_rows() {
                let eid = eid_dict.value(eid_col.keys().value(i) as usize);
                if eid != entity_id {
                    continue;
                }

                let centuries = cent_arr.value(i);
                let ns = ns_arr.value(i);
                let row_dur = Duration::from_parts(centuries, ns);

                if row_dur > target_dur {
                    continue; // future entry — skip
                }
                if best_dur.map_or(false, |b| row_dur <= b) {
                    continue; // not a better match
                }

                let units = units_dict.value(units_col.keys().value(i) as usize);
                let to_km: f64 = match units.to_lowercase().as_str() {
                    "m" | "meters" | "meter" => 0.001,
                    "au" => 149_597_870.7,
                    _ => 1.0,
                };

                let pb = (pos_offset + i) * 3;
                let translation = Translation3::new(
                    pos_vals.value(pb) * to_km,
                    pos_vals.value(pb + 1) * to_km,
                    pos_vals.value(pb + 2) * to_km,
                );

                let qb = (quat_offset + i) * 4;
                let rotation = UnitQuaternion::from_quaternion(Quaternion::new(
                    quat_vals.value(qb),     // w
                    quat_vals.value(qb + 1), // x
                    quat_vals.value(qb + 2), // y
                    quat_vals.value(qb + 3), // z
                ));

                let frame_id = frame_dict.value(frame_col.keys().value(i) as usize).to_string();
                best_dur = Some(row_dur);
                best = Some((frame_id, Isometry3::from_parts(translation, rotation)));
            }
        }

        best
    }

    /// Builds a dynamic frame map for use with [`spacetimestamp::transforms::transform_batch`].
    ///
    /// For each entity URI in `entity_ids`, looks up the entity's latest pose at or before
    /// `epoch` and returns it as `(astronomical_root_frame, composed_isometry_km)`. The
    /// isometry transforms coordinates expressed in that entity's body frame into the
    /// astronomical root frame, with translation in km.
    ///
    /// Parent frames that are themselves entity URIs (contain `":"`) are resolved
    /// recursively and the isometries composed. Returns `Err` if any entity is not found
    /// in the ledger or if a cycle is detected in the parent chain.
    pub fn build_dynamic_frame_map(
        &self,
        entity_ids: &[&str],
        epoch: Epoch,
    ) -> Result<HashMap<String, (String, Isometry3<f64>)>, String> {
        let mut result = HashMap::new();
        for &id in entity_ids {
            if !result.contains_key(id) {
                self.resolve_chain(id, epoch, &mut result, &mut HashSet::new())?;
            }
        }
        Ok(result)
    }

    fn resolve_chain(
        &self,
        entity_id: &str,
        epoch: Epoch,
        result: &mut HashMap<String, (String, Isometry3<f64>)>,
        visiting: &mut HashSet<String>,
    ) -> Result<(), String> {
        if result.contains_key(entity_id) {
            return Ok(());
        }
        if !visiting.insert(entity_id.to_string()) {
            return Err(format!(
                "Cycle detected in entity frame chain involving '{entity_id}'"
            ));
        }

        let (parent_frame, iso) = self
            .resolve_frame_at(entity_id, epoch)
            .ok_or_else(|| format!(
                "Entity '{entity_id}' not found in ledger at or before {epoch}"
            ))?;

        if spacetimestamp::schema::is_entity_uri(&parent_frame) {
            // Parent is another entity — recurse to get its composed isometry.
            self.resolve_chain(&parent_frame, epoch, result, visiting)?;
            let (root_frame, parent_iso) = result[&parent_frame].clone();
            result.insert(entity_id.to_string(), (root_frame, parent_iso * iso));
        } else {
            result.insert(entity_id.to_string(), (parent_frame, iso));
        }

        visiting.remove(entity_id);
        Ok(())
    }

    /// Serializes all batches to an Arrow IPC file at `path`.
    ///
    /// All batches are written in insertion order. The file can be reloaded with
    /// [`Ledger::load_ipc`].
    pub fn save_ipc(&self, path: &Path) -> Result<(), String> {
        if self.batches.is_empty() {
            return Err("Cannot save an empty ledger".to_string());
        }

        let schema = self.batches[0].schema();
        let file = File::create(path)
            .map_err(|e| format!("Failed to create '{}': {e}", path.display()))?;

        let mut writer = FileWriter::try_new(file, &schema)
            .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;

        for batch in &self.batches {
            writer
                .write(batch)
                .map_err(|e| format!("Failed to write batch to IPC: {e}"))?;
        }

        writer
            .finish()
            .map_err(|e| format!("Failed to finalise IPC file: {e}"))?;

        Ok(())
    }

    /// Returns the single best pose per entity across the entire ledger.
    ///
    /// "Best" is determined by:
    /// 1. Most recent timestamp (highest `duration_centuries` / `duration_ns`).
    /// 2. For equal timestamps, source priority: `MEASURED` > `PREDICTED` > `SIMULATED`.
    /// 3. For equal timestamps and equal priority, later insertion order wins.
    ///
    /// `entity_ids`: if `Some`, only the listed entity IDs are included; `None` = all.
    /// `not_before`: rows whose timestamp is strictly before this epoch are excluded.
    ///
    /// Returns an empty batch (correct schema, 0 rows) when the ledger has data but no rows
    /// match the filters.
    pub fn current_state(
        &self,
        entity_ids: Option<&[&str]>,
        not_before: Option<Epoch>,
    ) -> Result<RecordBatch, String> {
        if self.batches.is_empty() {
            return Err("Ledger is empty".to_string());
        }

        let j2000 = Epoch::from_gregorian_tai(2000, 1, 1, 12, 0, 0, 0);
        let cutoff: Option<Duration> = not_before.map(|ep| ep - j2000);
        let entity_filter: Option<HashSet<&str>> =
            entity_ids.map(|ids| ids.iter().copied().collect());

        // entity_id → (epoch_dur, priority, batch_idx, row_idx)
        let mut best: HashMap<String, (Duration, u8, usize, usize)> = HashMap::new();

        for (batch_idx, batch) in self.batches.iter().enumerate() {
            let Some(eid_col) = batch
                .column_by_name("entity_id")
                .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
            else {
                continue;
            };
            let Some(eid_dict) = eid_col.values().as_any().downcast_ref::<StringArray>() else {
                continue;
            };

            let Some(sts) = batch
                .column_by_name("spacetimestamp")
                .and_then(|c| c.as_any().downcast_ref::<StructArray>())
            else {
                continue;
            };
            let Some(cent_arr) = sts
                .column_by_name("duration_centuries")
                .and_then(|c| c.as_any().downcast_ref::<Int16Array>())
            else {
                continue;
            };
            let Some(ns_arr) = sts
                .column_by_name("duration_ns")
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            else {
                continue;
            };
            let Some(et_col) = sts
                .column_by_name("estimate_type")
                .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt16Type>>())
            else {
                continue;
            };
            let Some(et_dict) = et_col.values().as_any().downcast_ref::<StringArray>() else {
                continue;
            };

            for row in 0..batch.num_rows() {
                let eid = eid_dict.value(eid_col.keys().value(row) as usize);

                if let Some(ref filter) = entity_filter {
                    if !filter.contains(eid) {
                        continue;
                    }
                }

                let dur = Duration::from_parts(cent_arr.value(row), ns_arr.value(row));

                if let Some(c) = cutoff {
                    if dur < c {
                        continue;
                    }
                }

                let et = et_dict.value(et_col.keys().value(row) as usize);
                let priority = estimate_type_priority(et);

                let update = match best.get(eid) {
                    None => true,
                    Some(&(best_dur, best_pri, _, _)) => {
                        dur > best_dur || (dur == best_dur && priority < best_pri)
                    }
                };

                if update {
                    best.insert(eid.to_string(), (dur, priority, batch_idx, row));
                }
            }
        }

        let schema = self.batches[0].schema();

        if best.is_empty() {
            return Ok(RecordBatch::new_empty(schema));
        }

        let mut rows: Vec<RecordBatch> = Vec::with_capacity(best.len());
        for (_, _, batch_idx, row_idx) in best.values() {
            let batch = &self.batches[*batch_idx];
            let mut mask = BooleanBuilder::with_capacity(batch.num_rows());
            for i in 0..batch.num_rows() {
                mask.append_value(i == *row_idx);
            }
            let mask = mask.finish();
            let cols: Result<Vec<_>, _> = batch
                .columns()
                .iter()
                .map(|col| arrow::compute::filter(col.as_ref(), &mask))
                .collect();
            let cols = cols.map_err(|e| format!("row extraction failed: {e}"))?;
            rows.push(
                RecordBatch::try_new(batch.schema(), cols)
                    .map_err(|e| format!("failed to build result row: {e}"))?,
            );
        }

        arrow::compute::concat_batches(&schema, &rows)
            .map_err(|e| format!("failed to concatenate current_state rows: {e}"))
    }

    /// Loads a ledger from an Arrow IPC file previously saved with [`Ledger::save_ipc`].
    pub fn load_ipc(path: &Path) -> Result<Self, String> {
        let file = File::open(path)
            .map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;

        let reader = FileReader::try_new(file, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;

        let mut batches = Vec::new();
        for result in reader {
            batches.push(
                result.map_err(|e| format!("Failed to read batch from IPC file: {e}"))?,
            );
        }

        if batches.is_empty() {
            return Err("IPC file contained no record batches".to_string());
        }

        Ok(Self { batches })
    }

    /// Serializes all batches to an in-memory Arrow IPC buffer.
    ///
    /// Equivalent to [`Ledger::save_ipc`] but writes to a `Vec<u8>` instead of a file.
    /// Used by the S3 storage backend.
    pub fn save_ipc_to_bytes(&self) -> Result<Vec<u8>, String> {
        if self.batches.is_empty() {
            return Err("Cannot save an empty ledger".to_string());
        }
        let schema = self.batches[0].schema();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = FileWriter::try_new(&mut buf, &schema)
                .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
            for batch in &self.batches {
                writer.write(batch).map_err(|e| format!("Failed to write batch: {e}"))?;
            }
            writer.finish().map_err(|e| format!("Failed to finalise IPC: {e}"))?;
        }
        Ok(buf)
    }

    /// Deserializes a ledger from an in-memory Arrow IPC buffer.
    ///
    /// Equivalent to [`Ledger::load_ipc`] but reads from `&[u8]` instead of a file.
    /// Used by the S3 storage backend.
    pub fn load_ipc_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let cursor = Cursor::new(bytes);
        let reader = FileReader::try_new(cursor, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;
        let mut batches = Vec::new();
        for result in reader {
            batches.push(result.map_err(|e| format!("Failed to read batch: {e}"))?);
        }
        if batches.is_empty() {
            return Err("IPC bytes contained no record batches".to_string());
        }
        Ok(Self { batches })
    }
}

fn estimate_type_priority(s: &str) -> u8 {
    match s {
        "MEASURED" => 0,
        "PREDICTED" => 1,
        _ => 2, // SIMULATED or unknown
    }
}

/// Extracts the epoch from the first row of the spacetimestamp struct column.
/// Falls back to J2000 TAI if the column or fields are absent.
fn epoch_from_batch(batch: &RecordBatch, sts_column: &str) -> Epoch {
    let j2000 = Epoch::from_gregorian_tai(2000, 1, 1, 12, 0, 0, 0);
    if batch.num_rows() == 0 {
        return j2000;
    }
    let Some(sts) = batch
        .column_by_name(sts_column)
        .and_then(|c| c.as_any().downcast_ref::<StructArray>())
    else {
        return j2000;
    };
    let cent = sts
        .column_by_name("duration_centuries")
        .and_then(|c| c.as_any().downcast_ref::<Int16Array>())
        .map(|a| a.value(0))
        .unwrap_or(0);
    let ns = sts
        .column_by_name("duration_ns")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .map(|a| a.value(0))
        .unwrap_or(0);
    j2000 + Duration::from_parts(cent, ns)
}

/// Returns the set of unique entity-URI values present in the frame_id dictionary
/// of the spacetimestamp struct column. These are the frames that need ledger resolution.
fn collect_uri_frames(batch: &RecordBatch, sts_column: &str) -> Vec<String> {
    let Some(sts) = batch
        .column_by_name(sts_column)
        .and_then(|c| c.as_any().downcast_ref::<StructArray>())
    else {
        return vec![];
    };
    let Some(frames) = sts
        .column_by_name("frame_id")
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
    else {
        return vec![];
    };
    let Some(dict) = frames.values().as_any().downcast_ref::<StringArray>() else {
        return vec![];
    };
    (0..dict.len())
        .filter(|&i| !dict.is_null(i))
        .map(|i| dict.value(i))
        .filter(|s| is_entity_uri(s))
        .map(|s| s.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{DataType, Field, Schema};
    use hifitime::Duration;
    use spacetimestamp::query::SpatiotemporalFilter;
    use spacetimestamp::schema::{SpaceTimestampBuilder, sts_schema};
    use std::str::FromStr;
    use std::sync::Arc;

    fn j2000() -> hifitime::Epoch {
        hifitime::Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap()
    }

    /// Build a minimal single-row batch that embeds a spacetimestamp struct column.
    fn make_batch(pos: [f64; 3], ns: u64) -> RecordBatch {
        let mut builder = SpaceTimestampBuilder::new(1, None);
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "src", "MEASURED",
            pos, [1.0, 0.0, 0.0, 0.0], 0, ns,
            None, None,
        );
        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema(None);
        let schema = Arc::new(
            Schema::new(vec![Field::new(
                "spacetimestamp",
                DataType::Struct(sts_ref.fields().clone()),
                false,
            )])
            .with_metadata(sts_ref.metadata().clone()),
        );
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    #[test]
    fn test_append_and_len() {
        let mut ledger = Ledger::new();
        assert!(ledger.is_empty());
        ledger.append(make_batch([0.0, 0.0, 0.0], 0));
        ledger.append(make_batch([1.0, 0.0, 0.0], 1000));
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn test_query_no_filter_returns_all_rows() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0));
        ledger.append(make_batch([10.0, 0.0, 0.0], 1000));
        let result = ledger.query(&SpatiotemporalFilter::new(), "spacetimestamp").unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_query_spatial_filter() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([1.0, 0.0, 0.0], 0));   // inside 5 km sphere
        ledger.append(make_batch([100.0, 0.0, 0.0], 0)); // outside
        let filter = SpatiotemporalFilter::new().with_spatial([0.0, 0.0, 0.0], 5.0);
        let result = ledger.query(&filter, "spacetimestamp").unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_query_time_filter() {
        let j2000 = j2000();
        let t1 = j2000 + Duration::from_parts(0, 400);
        let t2 = j2000 + Duration::from_parts(0, 600);

        let mut ledger = Ledger::new();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0));    // before range
        ledger.append(make_batch([1.0, 0.0, 0.0], 500));  // inside
        ledger.append(make_batch([2.0, 0.0, 0.0], 9999)); // after range

        let filter = SpatiotemporalFilter::new().with_time_range(t1, t2);
        let result = ledger.query(&filter, "spacetimestamp").unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_stream_query_yields_per_batch() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0));
        ledger.append(make_batch([1.0, 0.0, 0.0], 1000));

        let total_rows: usize = ledger
            .stream_query(&SpatiotemporalFilter::new(), "spacetimestamp")
            .filter_map(Result::ok)
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 2);
    }

    #[test]
    fn test_latest_snapshot_returns_last_batch() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0));
        ledger.append(make_batch([99.0, 0.0, 0.0], 9999));
        let snap = ledger.latest_snapshot(None).unwrap();
        assert_eq!(snap.num_rows(), 1);
        // The last batch has position [99, 0, 0].
        // Just verify it round-trips without error.
    }

    #[test]
    fn test_seal_merges_batches_at_threshold() {
        let mut ledger = Ledger::new();
        for i in 0..=SEGMENT_THRESHOLD {
            ledger.append(make_batch([i as f64, 0.0, 0.0], i as u64));
        }
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn test_seal_preserves_row_count() {
        let mut ledger = Ledger::new();
        let n = SEGMENT_THRESHOLD + 1;
        for i in 0..n {
            ledger.append(make_batch([i as f64, 0.0, 0.0], i as u64));
        }
        let total: usize = ledger
            .stream_query(&SpatiotemporalFilter::new(), "spacetimestamp")
            .filter_map(Result::ok)
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total, n);
    }

    #[test]
    fn test_no_seal_below_threshold() {
        let mut ledger = Ledger::new();
        for i in 0..SEGMENT_THRESHOLD {
            ledger.append(make_batch([i as f64, 0.0, 0.0], i as u64));
        }
        assert_eq!(ledger.len(), SEGMENT_THRESHOLD);
    }

    #[test]
    fn test_save_and_load_ipc_from_bytes_round_trip() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0));
        ledger.append(make_batch([4.0, 5.0, 6.0], 1000));

        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let loaded = Ledger::load_ipc_from_bytes(&bytes).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn test_save_and_load_ipc_preserves_batches() {
        let mut ledger = Ledger::new();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0));
        ledger.append(make_batch([4.0, 5.0, 6.0], 1000));

        let path = std::env::temp_dir().join("soloc_ledger_test.arrows");
        ledger.save_ipc(&path).unwrap();

        let loaded = Ledger::load_ipc(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        std::fs::remove_file(path).ok();
    }

    // -----------------------------------------------------------------------
    // FRICTION 7: entity-URI frame chain resolution
    // -----------------------------------------------------------------------

    /// Builds an entity batch using EntityBuilder (includes entity_id column).
    fn make_entity_batch(
        entity_id: &str,
        frame_id: &str,
        pos: [f64; 3],
        quat: [f64; 4],
        ns: u64,
    ) -> RecordBatch {
        use crate::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1, None);
        b.append_entity(
            entity_id, frame_id, "km", "TAI", "test:src", "MEASURED",
            pos, quat, 0, ns,
            None, None, None, None, None,
        );
        b.flush()
    }

    #[test]
    fn test_build_dynamic_frame_map_single_hop() {
        // truck_A at [100, 0, 0] km in IAU_EARTH, identity orientation.
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch(
            "demo:truck_A", "IAU_EARTH",
            [100.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));
        // robot_truck at [1, 0, 0] km in truck_A body frame.
        ledger.append(make_entity_batch(
            "demo:robot_truck", "demo:truck_A",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));

        let epoch = j2000();
        let map = ledger.build_dynamic_frame_map(&["demo:robot_truck"], epoch).unwrap();

        let (root, iso) = map.get("demo:robot_truck").unwrap();
        assert_eq!(root, "IAU_EARTH");

        // Composed isometry: robot at [1,0,0] in truck frame, truck at [100,0,0] in ECEF.
        // Applying iso to the robot's local origin [0,0,0] should give [101,0,0] in ECEF.
        let origin = nalgebra::Point3::new(0.0, 0.0, 0.0);
        let result = iso.transform_point(&origin);
        assert!((result.x - 101.0).abs() < 1e-9, "expected x≈101, got {}", result.x);
        assert!(result.y.abs() < 1e-9);
        assert!(result.z.abs() < 1e-9);
    }

    #[test]
    fn test_build_dynamic_frame_map_two_hop() {
        // facility at [50, 0, 0] km in IAU_EARTH.
        // robot at [5, 0, 0] km in facility frame.
        // Expected: robot origin in ECEF = [55, 0, 0] km.
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch(
            "demo:facility", "IAU_EARTH",
            [50.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));
        ledger.append(make_entity_batch(
            "demo:robot", "demo:facility",
            [5.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));

        let epoch = j2000();
        let map = ledger.build_dynamic_frame_map(&["demo:robot"], epoch).unwrap();

        let (root, iso) = map.get("demo:robot").unwrap();
        assert_eq!(root, "IAU_EARTH");

        let origin = nalgebra::Point3::new(0.0, 0.0, 0.0);
        let result = iso.transform_point(&origin);
        assert!((result.x - 55.0).abs() < 1e-9, "expected x≈55, got {}", result.x);
    }

    #[test]
    fn test_build_dynamic_frame_map_entity_not_found() {
        let ledger = Ledger::new();
        let epoch = j2000();
        let err = ledger.build_dynamic_frame_map(&["demo:ghost"], epoch).unwrap_err();
        assert!(err.contains("demo:ghost"), "error should name the missing entity: {err}");
    }

    #[test]
    fn test_build_dynamic_frame_map_cycle_detected() {
        // A → B → A forms a cycle.
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch(
            "demo:A", "demo:B", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));
        ledger.append(make_entity_batch(
            "demo:B", "demo:A", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0,
        ));

        let epoch = j2000();
        let err = ledger.build_dynamic_frame_map(&["demo:A"], epoch).unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "expected cycle error: {err}");
    }

    // -----------------------------------------------------------------------
    // current_state tests
    // -----------------------------------------------------------------------

    fn make_entity_batch_et(
        entity_id: &str,
        pos: [f64; 3],
        ns: u64,
        estimate_type: &str,
    ) -> RecordBatch {
        use crate::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1, None);
        b.append_entity(
            entity_id, "ICRF", "km", "TAI", "test:src", estimate_type,
            pos, [1.0, 0.0, 0.0, 0.0], 0, ns,
            None, None, None, None, None,
        );
        b.flush()
    }

    #[test]
    fn test_current_state_latest_wins() {
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch_et("demo:sat", [1.0, 0.0, 0.0], 1000, "MEASURED"));
        ledger.append(make_entity_batch_et("demo:sat", [2.0, 0.0, 0.0], 5000, "MEASURED"));

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 1);

        // Verify the later timestamp's position was selected.
        let sts = result
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let ns_arr = sts
            .column_by_name("duration_ns")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(ns_arr.value(0), 5000);
    }

    #[test]
    fn test_current_state_priority_wins_same_epoch() {
        // MEASURED arrives first (batch 0); SIMULATED arrives second (batch 1) — same timestamp.
        // MEASURED must win because its priority (0) < SIMULATED priority (2).
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch_et("demo:sat", [1.0, 0.0, 0.0], 3000, "MEASURED"));
        ledger.append(make_entity_batch_et("demo:sat", [9.0, 0.0, 0.0], 3000, "SIMULATED"));

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 1);

        // MEASURED row has position [1,0,0]; SIMULATED has [9,0,0].
        let sts = result
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let et_col = sts
            .column_by_name("estimate_type")
            .unwrap()
            .as_any()
            .downcast_ref::<DictionaryArray<UInt16Type>>()
            .unwrap();
        let et_dict = et_col.values().as_any().downcast_ref::<StringArray>().unwrap();
        let et = et_dict.value(et_col.keys().value(0) as usize);
        assert_eq!(et, "MEASURED");
    }

    #[test]
    fn test_current_state_staleness_cutoff() {
        let mut ledger = Ledger::new();
        ledger.append(make_entity_batch_et("demo:sat", [1.0, 0.0, 0.0], 100, "MEASURED"));
        ledger.append(make_entity_batch_et("demo:sat", [2.0, 0.0, 0.0], 2000, "MEASURED"));

        // Cutoff: only rows at or after ns=500 are accepted.
        let cutoff = j2000() + Duration::from_parts(0, 500);
        let result = ledger.current_state(None, Some(cutoff)).unwrap();
        assert_eq!(result.num_rows(), 1);

        let sts = result
            .column_by_name("spacetimestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let ns_arr = sts
            .column_by_name("duration_ns")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(ns_arr.value(0), 2000);
    }

    #[test]
    fn test_current_state_all_entities() {
        let mut ledger = Ledger::new();
        // Batch 0: entity A and B.
        use crate::entity::EntityBuilder;
        let batch0 = {
            let mut b = EntityBuilder::new(2, None);
            b.append_entity("demo:A", "ICRF", "km", "TAI", "src", "MEASURED",
                [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 100,
                None, None, None, None, None);
            b.append_entity("demo:B", "ICRF", "km", "TAI", "src", "MEASURED",
                [2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 100,
                None, None, None, None, None);
            b.flush()
        };
        ledger.append(batch0);
        // Batch 1: entity C only.
        ledger.append(make_entity_batch_et("demo:C", [3.0, 0.0, 0.0], 200, "SIMULATED"));

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 3, "expected one row per entity");

        // Verify entity IDs are all present.
        let eid_col = result
            .column_by_name("entity_id")
            .unwrap()
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .unwrap();
        let eid_dict = eid_col.values().as_any().downcast_ref::<StringArray>().unwrap();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in 0..result.num_rows() {
            seen.insert(eid_dict.value(eid_col.keys().value(row) as usize).to_string());
        }
        assert!(seen.contains("demo:A"));
        assert!(seen.contains("demo:B"));
        assert!(seen.contains("demo:C"));
    }
}
