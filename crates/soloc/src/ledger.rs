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
    Array, BooleanBuilder, DictionaryArray, FixedSizeListArray, Float64Array, Int16Array,
    StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::{DataType, Fields, SchemaRef, UInt16Type, UInt32Type};
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
use spacetimestamp::ephemeris::j2000_tai;
use spacetimestamp::query::{SpatiotemporalFilter, filter_batch};
use spacetimestamp::schema::{FrameRegistry, STS_COLUMN, is_entity_uri};
use spacetimestamp::transforms::{normalize_batch_to_tai, transform_batch};
use spacetimestamp::validation::validate_spacetimestamp_batch;

use crate::schemas::SolocSchema;

/// Merge batches in memory when the count exceeds this to keep query latency bounded.
///
/// Benchmarks show ~11 µs fixed overhead per batch. At 50 batches of ≥1000 rows each,
/// time-filter queries stay under ~1 ms. Beyond this threshold, merging pays off.
const SEGMENT_THRESHOLD: usize = 50;

/// Default staleness window for [`Ledger::current_state`] when `not_before` is not supplied.
/// Rows older than (latest stored timestamp − this window) are excluded.
const CURRENT_STATE_WINDOW_NS: u64 = 3_600 * 1_000_000_000; // 1 hour

/// Required field names inside the spacetimestamp struct column.
const STS_REQUIRED_FIELDS: &[&str] = &[
    "frame_id",
    "units_pos",
    "timescale_id",
    "source_id",
    "estimate_type",
    "position",
    "quaternion",
    "duration_centuries",
    "duration_ns",
];

/// An append-only store of [`RecordBatch`]es forming the soloc Universal Ledger.
///
/// Schema-agnostic: works with any Arrow schema that embeds a spacetimestamp struct column.
/// The schema and `id_column` name are fixed at construction and
/// validated against the provided schema before the ledger is created.
///
/// For the standard entity schema, construct with:
/// ```rust,ignore
/// use soloc::entity::entity_schema;
/// use soloc::ledger::Ledger;
///
/// let ledger = Ledger::new(&entity_schema(None), "spacetimestamp", "entity_id").unwrap();
/// ```
#[derive(Debug)]
pub struct Ledger {
    /// The Arrow schema this ledger was created with. Stored so it is always
    /// available even when the ledger is empty (no batches yet).
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    /// Name of the entity-identity column (e.g. `"entity_id"`). Empty string = no id column.
    id_column: String,
}

impl Ledger {
    /// Creates an empty ledger from `schema`, validating that it contains a `"spacetimestamp"`
    /// struct column with all required STS sub-fields, and (if non-empty) that `id_column`
    /// exists in the schema.
    pub fn new(schema: &SchemaRef, id_column: &str) -> Result<Self, String> {
        Self::validate_schema(schema, id_column)?;
        Ok(Self {
            schema: schema.clone(),
            batches: Vec::new(),
            id_column: id_column.to_string(),
        })
    }

    /// Creates an empty ledger from a [`SolocSchema`] implementor.
    ///
    /// This is the preferred constructor when working with a known schema type:
    ///
    /// ```rust,ignore
    /// use soloc::schemas::entity::EntitySchema;
    /// let ledger = Ledger::for_schema::<EntitySchema>(None)?;
    /// ```
    pub fn for_schema<S: SolocSchema>(registry: Option<&FrameRegistry>) -> Result<Self, String> {
        Self::new(&S::schema(registry), S::id_column())
    }

    /// Returns the Arrow schema this ledger was created with.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// Validates that `schema` contains a `"spacetimestamp"` struct with all required STS fields
    /// and (if non-empty) that `id_column` exists.
    fn validate_schema(schema: &SchemaRef, id_column: &str) -> Result<(), String> {
        let sts_field = schema
            .field_with_name(STS_COLUMN)
            .map_err(|_| format!("'{}' column not found in schema", STS_COLUMN))?;

        let sts_fields: &Fields = match sts_field.data_type() {
            DataType::Struct(f) => f,
            other => {
                return Err(format!(
                    "'{}' must be a Struct column, got {other:?}",
                    STS_COLUMN
                ));
            }
        };

        for &name in STS_REQUIRED_FIELDS {
            if sts_fields.find(name).is_none() {
                return Err(format!(
                    "'{}' struct is missing required STS field '{name}'",
                    STS_COLUMN
                ));
            }
        }

        if !id_column.is_empty() {
            schema
                .field_with_name(id_column)
                .map_err(|_| format!("id_column '{id_column}' not found in schema"))?;
        }

        Ok(())
    }

    /// Validates and appends a batch to the ledger, normalizing all timestamps to TAI.
    ///
    /// Validation checks `timescale_id` and `frame_id` values against hifitime and anise
    /// standards. Returns `Err` if any value is unrecognized — the ledger is unchanged.
    ///
    /// Rows whose `timescale_id` is already `"TAI"` are passed through with no allocation.
    /// Rows in other timescales (UTC, GPS, TDB, …) are converted to TAI-relative
    /// `(duration_centuries, duration_ns)`. After this call all stored data is TAI.
    pub fn append(&mut self, batch: RecordBatch) -> Result<(), String> {
        validate_spacetimestamp_batch(&batch)?;
        let normalized = normalize_batch_to_tai(&batch)?;
        self.batches.push(normalized);
        self.seal_and_flush_if_needed();
        Ok(())
    }

    /// Merges all batches into one when the batch count exceeds [`SEGMENT_THRESHOLD`].
    ///
    /// Per-batch fixed overhead dominates query cost, so keeping the count low is critical.
    /// If concat fails (schema mismatch, OOM), the ledger is left unchanged.
    fn seal_and_flush_if_needed(&mut self) {
        if self.batches.len() <= SEGMENT_THRESHOLD {
            return;
        }
        if let Ok(merged) = arrow::compute::concat_batches(&self.schema, &self.batches) {
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
    pub fn query(&self, filter: &SpatiotemporalFilter) -> Result<RecordBatch, String> {
        if self.batches.is_empty() {
            return Err("Ledger is empty".to_string());
        }

        let mut kept: Vec<RecordBatch> = Vec::new();

        for batch in &self.batches {
            let filtered = filter_batch(batch, filter)?;
            if filtered.num_rows() > 0 {
                kept.push(filtered);
            }
        }

        if kept.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
        }

        arrow::compute::concat_batches(&self.schema, &kept)
            .map_err(|e| format!("Failed to concatenate filtered batches: {e}"))
    }

    /// Lazily yields filtered batches one at a time without concatenation.
    ///
    /// This is the preferred API for streaming data to a UI renderer — the first matching
    /// batch is yielded immediately rather than waiting for a full ledger scan.
    pub fn stream_query<'a>(
        &'a self,
        filter: &'a SpatiotemporalFilter,
    ) -> impl Iterator<Item = Result<RecordBatch, String>> + 'a {
        self.batches
            .iter()
            .map(move |batch| filter_batch(batch, filter))
    }

    /// Returns the most recent batch in the ledger, optionally filtered to specific entity IDs.
    ///
    /// If `entity_ids` is `Some` and this ledger has no `id_column`, returns `None`.
    pub fn latest_snapshot(&self, entity_ids: Option<&[&str]>) -> Option<RecordBatch> {
        let last = self.batches.last()?;

        let ids = match entity_ids {
            None => return Some(last.clone()),
            Some(_) if self.id_column.is_empty() => return None,
            Some(ids) => ids,
        };

        let entity_col = last
            .column_by_name(&self.id_column)?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()?;
        let entity_dict = entity_col.values().as_any().downcast_ref::<StringArray>()?;

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
    /// let mut ledger = Ledger::new(&entity_schema(None), "spacetimestamp", "entity_id")?;
    /// ledger.seed_solar_system(&almanac, CelestialBody::ALL, epoch)?;
    /// ```
    pub fn seed_solar_system(
        &mut self,
        almanac: &Almanac,
        bodies: &[crate::ephemeris::CelestialBody],
        epoch: Epoch,
    ) -> Result<(), String> {
        let batch = crate::ephemeris::celestial_snapshot(almanac, bodies, epoch)?;
        self.append(batch)?;
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
    /// let result = ledger.transform(&my_batch, "ICRF", "km", &almanac)?;
    /// ```
    pub fn transform(
        &self,
        batch: &RecordBatch,
        target_frame: &str,
        target_units: &str,
        almanac: &Almanac,
    ) -> Result<RecordBatch, String> {
        let epoch = epoch_from_batch(batch);
        let uri_frames = collect_uri_frames(batch);

        let dynamic_frames = if uri_frames.is_empty() {
            None
        } else {
            let ids: Vec<&str> = uri_frames.iter().map(|s| s.as_str()).collect();
            Some(self.build_dynamic_frame_map(&ids, epoch)?)
        };

        transform_batch(
            batch,
            target_frame,
            almanac,
            target_units,
            dynamic_frames.as_ref(),
        )
    }

    /// Returns the pose of `entity_id` at the latest timestamp ≤ `epoch` as an
    /// `(parent_frame_id, isometry_km)` pair.
    ///
    /// Returns `None` if this ledger has no `id_column`, or if the entity has no entry
    /// at or before `epoch`.
    pub fn resolve_frame_at(
        &self,
        entity_id: &str,
        epoch: Epoch,
    ) -> Option<(String, Isometry3<f64>)> {
        if self.id_column.is_empty() {
            return None;
        }

        let target_dur = epoch - j2000_tai();

        let mut best_dur: Option<Duration> = None;
        let mut best: Option<(String, Isometry3<f64>)> = None;

        for batch in &self.batches {
            let Some(eid_raw) = batch.column_by_name(&self.id_column) else {
                continue;
            };
            let Some(eid_col) = eid_raw
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
            else {
                continue;
            };
            let Some(eid_dict) = eid_col.values().as_any().downcast_ref::<StringArray>() else {
                continue;
            };

            let Some(sts_raw) = batch.column_by_name(STS_COLUMN) else {
                continue;
            };
            let Some(sts) = sts_raw.as_any().downcast_ref::<StructArray>() else {
                continue;
            };

            let Some(frame_raw) = sts.column_by_name("frame_id") else {
                continue;
            };
            let Some(frame_col) = frame_raw
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
            else {
                continue;
            };
            let Some(frame_dict) = frame_col.values().as_any().downcast_ref::<StringArray>() else {
                continue;
            };

            let Some(units_raw) = sts.column_by_name("units_pos") else {
                continue;
            };
            let Some(units_col) = units_raw
                .as_any()
                .downcast_ref::<DictionaryArray<UInt16Type>>()
            else {
                continue;
            };
            let Some(units_dict) = units_col.values().as_any().downcast_ref::<StringArray>() else {
                continue;
            };

            let Some(pos_raw) = sts.column_by_name("position") else {
                continue;
            };
            let Some(pos_list) = pos_raw.as_any().downcast_ref::<FixedSizeListArray>() else {
                continue;
            };
            let Some(pos_vals) = pos_list.values().as_any().downcast_ref::<Float64Array>() else {
                continue;
            };
            let pos_offset = pos_list.offset();

            let Some(quat_raw) = sts.column_by_name("quaternion") else {
                continue;
            };
            let Some(quat_list) = quat_raw.as_any().downcast_ref::<FixedSizeListArray>() else {
                continue;
            };
            let Some(quat_vals) = quat_list.values().as_any().downcast_ref::<Float64Array>() else {
                continue;
            };
            let quat_offset = quat_list.offset();

            let Some(cent_raw) = sts.column_by_name("duration_centuries") else {
                continue;
            };
            let Some(cent_arr) = cent_raw.as_any().downcast_ref::<Int16Array>() else {
                continue;
            };

            let Some(ns_raw) = sts.column_by_name("duration_ns") else {
                continue;
            };
            let Some(ns_arr) = ns_raw.as_any().downcast_ref::<UInt64Array>() else {
                continue;
            };

            for i in 0..batch.num_rows() {
                let eid = eid_dict.value(eid_col.keys().value(i) as usize);
                if eid != entity_id {
                    continue;
                }

                let centuries = cent_arr.value(i);
                let ns = ns_arr.value(i);
                let row_dur = Duration::from_parts(centuries, ns);

                if row_dur > target_dur {
                    continue;
                }
                if best_dur.map_or(false, |b| row_dur <= b) {
                    continue;
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
                    quat_vals.value(qb),
                    quat_vals.value(qb + 1),
                    quat_vals.value(qb + 2),
                    quat_vals.value(qb + 3),
                ));

                let frame_id = frame_dict
                    .value(frame_col.keys().value(i) as usize)
                    .to_string();
                best_dur = Some(row_dur);
                best = Some((frame_id, Isometry3::from_parts(translation, rotation)));
            }
        }

        best
    }

    /// Builds a dynamic frame map for use with [`spacetimestamp::transforms::transform_batch`].
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

        let (parent_frame, iso) = self.resolve_frame_at(entity_id, epoch).ok_or_else(|| {
            format!("Entity '{entity_id}' not found in ledger at or before {epoch}")
        })?;

        if spacetimestamp::schema::is_entity_uri(&parent_frame) {
            self.resolve_chain(&parent_frame, epoch, result, visiting)?;
            let (root_frame, parent_iso) = result[&parent_frame].clone();
            result.insert(entity_id.to_string(), (root_frame, parent_iso * iso));
        } else {
            result.insert(entity_id.to_string(), (parent_frame, iso));
        }

        visiting.remove(entity_id);
        Ok(())
    }

    /// Merges all batches into a single [`RecordBatch`] for serialisation.
    ///
    /// Arrow IPC's `FileWriter` does not support dictionary replacement — if two batches
    /// carry different dictionary arrays for the same field (even with identical values),
    /// the write fails. `concat_batches` unifies dictionaries, so writing the merged
    /// result as a single batch is always safe.
    fn merge_for_ipc(&self) -> Result<RecordBatch, String> {
        if self.batches.len() == 1 {
            return Ok(self.batches[0].clone());
        }
        arrow::compute::concat_batches(&self.schema, &self.batches)
            .map_err(|e| format!("Failed to merge batches for IPC write: {e}"))
    }

    /// Serializes all batches to an Arrow IPC file at `path`.
    pub fn save_ipc(&self, path: &Path) -> Result<(), String> {
        if self.batches.is_empty() {
            return Err(
                "Cannot save an empty ledger — use save_schema_ipc to persist just the schema"
                    .to_string(),
            );
        }

        let merged = self.merge_for_ipc()?;
        let file = File::create(path)
            .map_err(|e| format!("Failed to create '{}': {e}", path.display()))?;

        let mut writer = FileWriter::try_new(file, &self.schema)
            .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
        writer
            .write(&merged)
            .map_err(|e| format!("Failed to write batch to IPC: {e}"))?;
        writer
            .finish()
            .map_err(|e| format!("Failed to finalise IPC file: {e}"))?;

        Ok(())
    }

    /// Writes the ledger's schema (and embedded [`FrameRegistry`] metadata) to an Arrow IPC
    /// file with zero data batches.
    ///
    /// The file can be read back by [`Ledger::load_schema_ipc`] or by any language that
    /// speaks Arrow IPC (Python `pyarrow`, Java, Go, …) — the schema and all metadata are
    /// preserved in the file header.
    ///
    /// Intended use: bake the output file into a Docker image so a freshly started
    /// `soloc-server` can call [`Ledger::load_schema_ipc`] at startup and be ready to
    /// accept data without any prior knowledge of the schema at the call-site.
    pub fn save_schema_ipc(&self, path: &Path) -> Result<(), String> {
        let file = File::create(path)
            .map_err(|e| format!("Failed to create '{}': {e}", path.display()))?;
        let mut writer = FileWriter::try_new(file, &self.schema)
            .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
        writer
            .finish()
            .map_err(|e| format!("Failed to finalise schema IPC file: {e}"))?;
        Ok(())
    }

    /// Reads the Arrow schema from an IPC file and returns an empty [`Ledger`] configured
    /// with that schema.
    ///
    /// Any data batches present in the file are ignored — this function only cares about
    /// the schema and its metadata (e.g. an embedded [`FrameRegistry`]).
    ///
    /// Pair with [`Ledger::save_schema_ipc`] for schema distribution (e.g. baking a
    /// schema file into a Docker image).
    pub fn load_schema_ipc(path: &Path, id_column: &str) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;
        let reader = FileReader::try_new(file, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;
        let schema = reader.schema();
        Self::validate_schema(&schema, id_column)?;
        Ok(Self {
            schema,
            batches: Vec::new(),
            id_column: id_column.to_string(),
        })
    }

    /// Serializes the ledger's schema to an in-memory Arrow IPC buffer with zero data batches.
    ///
    /// The bytes are in exactly the same format as [`Ledger::save_schema_ipc`] produces.
    /// Useful for transmitting a schema over the network or embedding it in another format
    /// without writing a temporary file.
    pub fn schema_to_ipc_bytes(&self) -> Result<Vec<u8>, String> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = FileWriter::try_new(&mut buf, &self.schema)
                .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
            writer
                .finish()
                .map_err(|e| format!("Failed to finalise schema IPC bytes: {e}"))?;
        }
        Ok(buf)
    }

    /// Deserializes a schema from an in-memory Arrow IPC buffer and returns an empty
    /// [`Ledger`] configured with that schema.
    ///
    /// Any data batches present in the buffer are ignored.
    pub fn from_schema_ipc_bytes(bytes: &[u8], id_column: &str) -> Result<Self, String> {
        let cursor = Cursor::new(bytes);
        let reader = FileReader::try_new(cursor, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;
        let schema = reader.schema();
        Self::validate_schema(&schema, id_column)?;
        Ok(Self {
            schema,
            batches: Vec::new(),
            id_column: id_column.to_string(),
        })
    }

    /// Returns the maximum stored timestamp as a J2000-relative [`Duration`], or `None`
    /// if the ledger is empty or contains no parseable timestamps.
    fn latest_stored_duration(&self) -> Option<Duration> {
        let mut latest: Option<Duration> = None;
        for batch in &self.batches {
            let sts = batch
                .column_by_name(STS_COLUMN)
                .and_then(|c| c.as_any().downcast_ref::<StructArray>())?;
            let cent_arr = sts
                .column_by_name("duration_centuries")
                .and_then(|c| c.as_any().downcast_ref::<Int16Array>())?;
            let ns_arr = sts
                .column_by_name("duration_ns")
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())?;
            for row in 0..batch.num_rows() {
                let dur = Duration::from_parts(cent_arr.value(row), ns_arr.value(row));
                latest = Some(match latest {
                    None => dur,
                    Some(prev) => prev.max(dur),
                });
            }
        }
        latest
    }

    /// Returns the single best pose per row-key across the entire ledger.
    ///
    /// "Best" is determined by:
    /// 1. Most recent timestamp (highest `duration_centuries` / `duration_ns`).
    /// 2. For equal timestamps, source priority: `MEASURED` > `PREDICTED` > `SIMULATED`.
    /// 3. For equal timestamps and equal priority, later insertion order wins.
    ///
    /// `id_filter`: if `Some`, only rows whose id-column value is in the set are included.
    ///   If this ledger has no `id_column`, an `id_filter` of `Some(_)` returns an empty batch.
    ///
    /// `not_before`: rows whose timestamp is strictly before this epoch are excluded.
    ///   When `None`, defaults to (latest stored timestamp − [`CURRENT_STATE_WINDOW_NS`]).
    pub fn current_state(
        &self,
        id_filter: Option<&[&str]>,
        not_before: Option<Epoch>,
    ) -> Result<RecordBatch, String> {
        if self.batches.is_empty() {
            return Err("Ledger is empty".to_string());
        }

        let cutoff: Option<Duration> = match not_before {
            Some(ep) => Some(ep - j2000_tai()),
            None => self
                .latest_stored_duration()
                .map(|latest| latest - Duration::from_parts(0, CURRENT_STATE_WINDOW_NS)),
        };
        let id_filter_set: Option<HashSet<&str>> =
            id_filter.map(|ids| ids.iter().copied().collect());

        // If caller asked for specific ids but we have no id column, return empty.
        if id_filter_set.is_some() && self.id_column.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
        }

        // row_key → (epoch_dur, priority, batch_idx, row_idx)
        let mut best: HashMap<String, (Duration, u8, usize, usize)> = HashMap::new();

        for (batch_idx, batch) in self.batches.iter().enumerate() {
            // id column lookup — skip batch if absent or wrong type
            let eid_col_opt = if self.id_column.is_empty() {
                None
            } else {
                batch
                    .column_by_name(&self.id_column)
                    .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
            };

            let eid_dict_opt = eid_col_opt
                .as_ref()
                .and_then(|col| col.values().as_any().downcast_ref::<StringArray>());

            let Some(sts) = batch
                .column_by_name(STS_COLUMN)
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
                // Derive a row key: use entity id if available, else row index string.
                let row_key = if let (Some(col), Some(dict)) = (eid_col_opt, eid_dict_opt) {
                    dict.value(col.keys().value(row) as usize).to_string()
                } else {
                    row.to_string()
                };

                if let Some(ref filter) = id_filter_set {
                    if !filter.contains(row_key.as_str()) {
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

                let update = match best.get(&row_key) {
                    None => true,
                    Some(&(best_dur, best_pri, _, _)) => {
                        dur > best_dur || (dur == best_dur && priority < best_pri)
                    }
                };

                if update {
                    best.insert(row_key, (dur, priority, batch_idx, row));
                }
            }
        }

        if best.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
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

        arrow::compute::concat_batches(&self.schema, &rows)
            .map_err(|e| format!("failed to concatenate current_state rows: {e}"))
    }

    /// Loads a ledger from an Arrow IPC file previously saved with [`Ledger::save_ipc`].
    ///
    /// Reads the schema from the IPC file and validates it against `sts_column` and `id_column`.
    pub fn load_ipc(path: &Path, id_column: &str) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;

        let reader = FileReader::try_new(file, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;

        let schema = reader.schema();
        Self::validate_schema(&schema, id_column)?;

        let mut batches = Vec::new();
        for result in reader {
            batches.push(result.map_err(|e| format!("Failed to read batch from IPC file: {e}"))?);
        }

        if batches.is_empty() {
            return Err("IPC file contained no record batches".to_string());
        }

        Ok(Self {
            schema,
            batches,
            id_column: id_column.to_string(),
        })
    }

    /// Serializes all batches to an in-memory Arrow IPC buffer.
    pub fn save_ipc_to_bytes(&self) -> Result<Vec<u8>, String> {
        if self.batches.is_empty() {
            return Err(
                "Cannot save an empty ledger — use schema_to_ipc_bytes to persist just the schema"
                    .to_string(),
            );
        }
        let merged = self.merge_for_ipc()?;
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = FileWriter::try_new(&mut buf, &self.schema)
                .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
            writer
                .write(&merged)
                .map_err(|e| format!("Failed to write batch: {e}"))?;
            writer
                .finish()
                .map_err(|e| format!("Failed to finalise IPC: {e}"))?;
        }
        Ok(buf)
    }

    /// Deserializes a ledger from an in-memory Arrow IPC buffer.
    ///
    /// Reads the schema from the IPC bytes and validates it against `sts_column` and `id_column`.
    pub fn load_ipc_from_bytes(bytes: &[u8], id_column: &str) -> Result<Self, String> {
        let cursor = Cursor::new(bytes);
        let reader = FileReader::try_new(cursor, None)
            .map_err(|e| format!("Failed to open Arrow IPC reader: {e}"))?;

        let schema = reader.schema();
        Self::validate_schema(&schema, id_column)?;

        let mut batches = Vec::new();
        for result in reader {
            batches.push(result.map_err(|e| format!("Failed to read batch: {e}"))?);
        }
        if batches.is_empty() {
            return Err("IPC bytes contained no record batches".to_string());
        }
        Ok(Self {
            schema,
            batches,
            id_column: id_column.to_string(),
        })
    }
}

fn estimate_type_priority(s: &str) -> u8 {
    match s {
        "MEASURED" => 0,
        "PREDICTED" => 1,
        _ => 2,
    }
}

/// Extracts the epoch from the first row of the spacetimestamp struct column.
/// Falls back to J2000 TAI if the column or fields are absent.
fn epoch_from_batch(batch: &RecordBatch) -> Epoch {
    let j2000 = j2000_tai();
    if batch.num_rows() == 0 {
        return j2000;
    }
    let Some(sts) = batch
        .column_by_name(STS_COLUMN)
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

/// Returns unique entity-URI values in the frame_id dictionary of the spacetimestamp struct.
fn collect_uri_frames(batch: &RecordBatch) -> Vec<String> {
    let Some(sts) = batch
        .column_by_name(STS_COLUMN)
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
    use spacetimestamp::ephemeris::j2000_tai;
    use spacetimestamp::query::SpatiotemporalFilter;
    use spacetimestamp::schema::{SpaceTimestampBuilder, sts_schema};
    use std::sync::Arc;

    fn j2000() -> Epoch {
        j2000_tai()
    }

    /// Schema matching make_batch() — just a spacetimestamp struct, no entity_id.
    fn sts_only_schema() -> SchemaRef {
        let sts_ref = sts_schema(None);
        Arc::new(
            Schema::new(vec![Field::new(
                "spacetimestamp",
                DataType::Struct(sts_ref.fields().clone()),
                false,
            )])
            .with_metadata(sts_ref.metadata().clone()),
        )
    }

    /// Build a minimal single-row batch that embeds a spacetimestamp struct column.
    fn make_batch(pos: [f64; 3], ns: u64) -> RecordBatch {
        let mut builder = SpaceTimestampBuilder::new(1, None);
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "src",
            "MEASURED",
            pos,
            [1.0, 0.0, 0.0, 0.0],
            0,
            ns,
            None,
            None,
        );
        let struct_array = builder.finish_as_struct();
        let schema = sts_only_schema();
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    fn make_sts_ledger() -> Ledger {
        Ledger::new(&sts_only_schema(), "").unwrap()
    }

    fn make_entity_ledger() -> Ledger {
        use crate::schemas::entity::entity_schema;
        Ledger::new(&entity_schema(None), "entity_id").unwrap()
    }

    #[test]
    fn test_append_and_len() {
        let mut ledger = make_sts_ledger();
        assert!(ledger.is_empty());
        ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([1.0, 0.0, 0.0], 1000)).unwrap();
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn test_query_no_filter_returns_all_rows() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([10.0, 0.0, 0.0], 1000)).unwrap();
        let result = ledger.query(&SpatiotemporalFilter::new()).unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_query_spatial_filter() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([1.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([100.0, 0.0, 0.0], 0)).unwrap();
        let filter = SpatiotemporalFilter::new().with_spatial([0.0, 0.0, 0.0], 5.0);
        let result = ledger.query(&filter).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_query_time_filter() {
        let j2000 = j2000();
        let t1 = j2000 + Duration::from_parts(0, 400);
        let t2 = j2000 + Duration::from_parts(0, 600);

        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([1.0, 0.0, 0.0], 500)).unwrap();
        ledger.append(make_batch([2.0, 0.0, 0.0], 9999)).unwrap();

        let filter = SpatiotemporalFilter::new().with_time_range(t1, t2);
        let result = ledger.query(&filter).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_stream_query_yields_per_batch() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([1.0, 0.0, 0.0], 1000)).unwrap();

        let total_rows: usize = ledger
            .stream_query(&SpatiotemporalFilter::new())
            .filter_map(Result::ok)
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 2);
    }

    #[test]
    fn test_latest_snapshot_returns_last_batch() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap();
        ledger.append(make_batch([99.0, 0.0, 0.0], 9999)).unwrap();
        let snap = ledger.latest_snapshot(None).unwrap();
        assert_eq!(snap.num_rows(), 1);
    }

    #[test]
    fn test_seal_merges_batches_at_threshold() {
        let mut ledger = make_sts_ledger();
        for i in 0..=SEGMENT_THRESHOLD {
            ledger
                .append(make_batch([i as f64, 0.0, 0.0], i as u64))
                .unwrap();
        }
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn test_seal_preserves_row_count() {
        let mut ledger = make_sts_ledger();
        let n = SEGMENT_THRESHOLD + 1;
        for i in 0..n {
            ledger
                .append(make_batch([i as f64, 0.0, 0.0], i as u64))
                .unwrap();
        }
        let total: usize = ledger
            .stream_query(&SpatiotemporalFilter::new())
            .filter_map(Result::ok)
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total, n);
    }

    #[test]
    fn test_no_seal_below_threshold() {
        let mut ledger = make_sts_ledger();
        for i in 0..SEGMENT_THRESHOLD {
            ledger
                .append(make_batch([i as f64, 0.0, 0.0], i as u64))
                .unwrap();
        }
        assert_eq!(ledger.len(), SEGMENT_THRESHOLD);
    }

    #[test]
    fn test_save_and_load_ipc_from_bytes_round_trip() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0)).unwrap();
        ledger.append(make_batch([4.0, 5.0, 6.0], 1000)).unwrap();

        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let loaded = Ledger::load_ipc_from_bytes(&bytes, "").unwrap();
        // Rows are preserved; batches are merged into 1 during save to avoid
        // Arrow IPC "dictionary replacement" errors across separate batches.
        assert_eq!(loaded.len(), 1);
        let total_rows: usize = loaded.batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 2);
    }

    #[test]
    fn test_save_and_load_ipc_preserves_batches() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0)).unwrap();
        ledger.append(make_batch([4.0, 5.0, 6.0], 1000)).unwrap();

        let path = std::env::temp_dir().join("soloc_ledger_test.arrows");
        ledger.save_ipc(&path).unwrap();

        let loaded = Ledger::load_ipc(&path, "").unwrap();
        // Rows are preserved; batches are merged into 1 during save to avoid
        // Arrow IPC "dictionary replacement" errors across separate batches.
        assert_eq!(loaded.len(), 1);
        let total_rows: usize = loaded.batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 2);
        std::fs::remove_file(path).ok();
    }

    // -----------------------------------------------------------------------
    // schema IPC round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_save_and_load_schema_ipc_round_trip() {
        let ledger = make_sts_ledger();
        let path = std::env::temp_dir().join("soloc_schema_test.arrows");
        ledger.save_schema_ipc(&path).unwrap();
        let loaded = Ledger::load_schema_ipc(&path, "").unwrap();
        assert!(loaded.is_empty(), "schema-loaded ledger must be empty");
        assert_eq!(loaded.schema(), ledger.schema());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_schema_to_and_from_ipc_bytes_round_trip() {
        let ledger = make_sts_ledger();
        let bytes = ledger.schema_to_ipc_bytes().unwrap();
        let loaded = Ledger::from_schema_ipc_bytes(&bytes, "").unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.schema(), ledger.schema());
    }

    #[test]
    fn test_load_schema_ipc_ignores_data_batches() {
        // Save a ledger that has data, then reload it as schema-only.
        // The resulting ledger should be empty regardless of what was in the file.
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0)).unwrap();
        let path = std::env::temp_dir().join("soloc_schema_data_test.arrows");
        ledger.save_ipc(&path).unwrap();
        let schema_only = Ledger::load_schema_ipc(&path, "").unwrap();
        assert!(
            schema_only.is_empty(),
            "load_schema_ipc must ignore data batches"
        );
        assert_eq!(schema_only.schema(), ledger.schema());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_schema_ipc_preserves_frame_registry_metadata() {
        use spacetimestamp::schema::{FrameRegistry, sts_schema};
        let mut reg = FrameRegistry::new_with_namespace("test_ns");
        reg.add_frame("cam", "ICRF", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);
        let sts_ref = sts_schema(Some(&reg));
        let schema = Arc::new(
            Schema::new(vec![Field::new(
                "spacetimestamp",
                DataType::Struct(sts_ref.fields().clone()),
                false,
            )])
            .with_metadata(sts_ref.metadata().clone()),
        );
        let ledger = Ledger::new(&schema, "").unwrap();

        let bytes = ledger.schema_to_ipc_bytes().unwrap();
        let loaded = Ledger::from_schema_ipc_bytes(&bytes, "").unwrap();

        let meta = loaded
            .schema()
            .metadata()
            .get("soloc.frame_registry")
            .cloned();
        assert!(
            meta.is_some(),
            "FrameRegistry metadata must survive schema IPC round-trip"
        );
        let recovered = FrameRegistry::from_json(&meta.unwrap()).unwrap();
        assert_eq!(recovered.namespace, "test_ns");
        assert!(recovered.frames.contains_key("test_ns:cam"));
    }

    #[test]
    fn test_save_schema_ipc_accepts_empty_ledger() {
        // Unlike save_ipc, save_schema_ipc must not error on an empty ledger.
        let ledger = make_sts_ledger();
        assert!(ledger.is_empty());
        let path = std::env::temp_dir().join("soloc_schema_empty_test.arrows");
        ledger.save_schema_ipc(&path).unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_save_ipc_still_rejects_empty_ledger() {
        let ledger = make_sts_ledger();
        assert!(
            ledger.save_ipc_to_bytes().is_err(),
            "save_ipc_to_bytes must error on empty ledger"
        );
        let path = std::env::temp_dir().join("soloc_save_empty_reject.arrows");
        assert!(
            ledger.save_ipc(&path).is_err(),
            "save_ipc must error on empty ledger"
        );
    }

    #[test]
    fn test_new_rejects_missing_sts_column() {
        // A schema with no "spacetimestamp" column must be rejected.
        use arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new(
            "other_col",
            DataType::Utf8,
            false,
        )]));
        let err = Ledger::new(&schema, "").unwrap_err();
        assert!(err.contains("spacetimestamp"), "got: {err}");
    }

    #[test]
    fn test_new_rejects_non_struct_sts_column() {
        use arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            DataType::Utf8,
            false,
        )]));
        let err = Ledger::new(&schema, "").unwrap_err();
        assert!(err.contains("Struct"), "got: {err}");
    }

    #[test]
    fn test_new_rejects_missing_id_column() {
        let schema = sts_only_schema();
        let err = Ledger::new(&schema, "entity_id").unwrap_err();
        assert!(err.contains("entity_id"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // entity-URI frame chain resolution
    // -----------------------------------------------------------------------

    fn make_entity_batch(
        entity_id: &str,
        frame_id: &str,
        pos: [f64; 3],
        quat: [f64; 4],
        ns: u64,
    ) -> RecordBatch {
        use crate::schemas::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1, None);
        b.append_entity(
            entity_id, frame_id, "km", "TAI", "test:src", "MEASURED", pos, quat, 0, ns, None, None,
            None, None, None,
        );
        b.flush()
    }

    #[test]
    fn test_build_dynamic_frame_map_single_hop() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:truck_A",
                "IAU_EARTH",
                [100.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                "demo:robot_truck",
                "demo:truck_A",
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let epoch = j2000();
        let map = ledger
            .build_dynamic_frame_map(&["demo:robot_truck"], epoch)
            .unwrap();

        let (root, iso) = map.get("demo:robot_truck").unwrap();
        assert_eq!(root, "IAU_EARTH");

        let origin = nalgebra::Point3::new(0.0, 0.0, 0.0);
        let result = iso.transform_point(&origin);
        assert!(
            (result.x - 101.0).abs() < 1e-9,
            "expected x≈101, got {}",
            result.x
        );
        assert!(result.y.abs() < 1e-9);
        assert!(result.z.abs() < 1e-9);
    }

    #[test]
    fn test_build_dynamic_frame_map_two_hop() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:facility",
                "IAU_EARTH",
                [50.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                "demo:robot",
                "demo:facility",
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let epoch = j2000();
        let map = ledger
            .build_dynamic_frame_map(&["demo:robot"], epoch)
            .unwrap();

        let (root, iso) = map.get("demo:robot").unwrap();
        assert_eq!(root, "IAU_EARTH");

        let origin = nalgebra::Point3::new(0.0, 0.0, 0.0);
        let result = iso.transform_point(&origin);
        assert!(
            (result.x - 55.0).abs() < 1e-9,
            "expected x≈55, got {}",
            result.x
        );
    }

    #[test]
    fn test_build_dynamic_frame_map_entity_not_found() {
        let ledger = make_entity_ledger();
        let epoch = j2000();
        let err = ledger
            .build_dynamic_frame_map(&["demo:ghost"], epoch)
            .unwrap_err();
        assert!(
            err.contains("demo:ghost"),
            "error should name the missing entity: {err}"
        );
    }

    #[test]
    fn test_build_dynamic_frame_map_cycle_detected() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:A",
                "demo:B",
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                "demo:B",
                "demo:A",
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let epoch = j2000();
        let err = ledger
            .build_dynamic_frame_map(&["demo:A"], epoch)
            .unwrap_err();
        assert!(
            err.to_lowercase().contains("cycle"),
            "expected cycle error: {err}"
        );
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
        use crate::schemas::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1, None);
        b.append_entity(
            entity_id,
            "ICRF",
            "km",
            "TAI",
            "test:src",
            estimate_type,
            pos,
            [1.0, 0.0, 0.0, 0.0],
            0,
            ns,
            None,
            None,
            None,
            None,
            None,
        );
        b.flush()
    }

    #[test]
    fn test_current_state_latest_wins() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [1.0, 0.0, 0.0],
                1000,
                "MEASURED",
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [2.0, 0.0, 0.0],
                5000,
                "MEASURED",
            ))
            .unwrap();

        let result = ledger.current_state(None, None).unwrap();
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
        assert_eq!(ns_arr.value(0), 5000);
    }

    #[test]
    fn test_current_state_priority_wins_same_epoch() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [1.0, 0.0, 0.0],
                3000,
                "MEASURED",
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [9.0, 0.0, 0.0],
                3000,
                "SIMULATED",
            ))
            .unwrap();

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 1);

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
        let et_dict = et_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let et = et_dict.value(et_col.keys().value(0) as usize);
        assert_eq!(et, "MEASURED");
    }

    #[test]
    fn test_current_state_staleness_cutoff() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [1.0, 0.0, 0.0],
                100,
                "MEASURED",
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                "demo:sat",
                [2.0, 0.0, 0.0],
                2000,
                "MEASURED",
            ))
            .unwrap();

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
        let mut ledger = make_entity_ledger();
        use crate::schemas::entity::EntityBuilder;
        let batch0 = {
            let mut b = EntityBuilder::new(2, None);
            b.append_entity(
                "demo:A",
                "ICRF",
                "km",
                "TAI",
                "src",
                "MEASURED",
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                100,
                None,
                None,
                None,
                None,
                None,
            );
            b.append_entity(
                "demo:B",
                "ICRF",
                "km",
                "TAI",
                "src",
                "MEASURED",
                [2.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                100,
                None,
                None,
                None,
                None,
                None,
            );
            b.flush()
        };
        ledger.append(batch0).unwrap();
        ledger
            .append(make_entity_batch_et(
                "demo:C",
                [3.0, 0.0, 0.0],
                200,
                "SIMULATED",
            ))
            .unwrap();

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 3, "expected one row per entity");

        let eid_col = result
            .column_by_name("entity_id")
            .unwrap()
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .unwrap();
        let eid_dict = eid_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in 0..result.num_rows() {
            seen.insert(
                eid_dict
                    .value(eid_col.keys().value(row) as usize)
                    .to_string(),
            );
        }
        assert!(seen.contains("demo:A"));
        assert!(seen.contains("demo:B"));
        assert!(seen.contains("demo:C"));
    }
}
