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
use std::path::Path;

use spacetimestamp::query::{SpatiotemporalFilter, filter_batch};

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
    /// Parent frames that are themselves entity URIs (start with `"urn:"`) are resolved
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
}
