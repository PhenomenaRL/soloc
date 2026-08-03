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
use spacetimestamp::schema::{STS_COLUMN, is_entity_uri};
use spacetimestamp::topology::TransformTree;
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

/// An append-only store of [`RecordBatch`]es forming the soloc Universal Ledger.
///
/// Schema-agnostic: works with any Arrow schema that embeds a spacetimestamp struct column.
/// The schema and `id_column` name are fixed at construction and
/// validated against the provided schema before the ledger is created.
///
/// For the standard entity schema, construct with:
/// ```rust,ignore
/// use soloc::schemas::entity::entity_schema;
/// use soloc::ledger::Ledger;
///
/// let ledger = Ledger::new(&entity_schema(), "entity_id").unwrap();
/// ```
#[derive(Debug)]
pub struct Ledger {
    /// The Arrow schema this ledger was created with. Stored so it is always
    /// available even when the ledger is empty (no batches yet).
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    /// Name of the entity-identity column (e.g. `"entity_id"`). Empty string = no id column.
    id_column: String,
    /// Parent graph derived from appended rows. Topology only — never a pose value.
    transform_tree: TransformTree,
    /// Highest-epoch pose seen for each entity, so the common "where is X now" lookup
    /// does not have to scan every batch. See [`LatestPose`].
    latest_pose: HashMap<String, LatestPose>,
}

/// The most recent pose ingested for one entity — the pose cache backing
/// [`Ledger::resolve_frame_at`]'s fast path.
///
/// Populated in [`Ledger::append`] from the winning row indices `ingest_batch` already
/// computed, so maintaining it costs k targeted reads per append rather than a second
/// full scan. `epoch` is the highest epoch *ever ingested* for the entity, which is what
/// makes the fast path sound: a query at or after `epoch` cannot have a newer row to find.
#[derive(Debug, Clone)]
struct LatestPose {
    /// The `frame_id` the pose is expressed in — another entity URI, or an astronomical frame.
    parent_frame_id: String,
    /// The pose itself, normalised to kilometres.
    isometry_km: Isometry3<f64>,
    /// Offset from the J2000 TAI epoch.
    epoch: Duration,
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
            transform_tree: TransformTree::new(),
            latest_pose: HashMap::new(),
        })
    }

    /// Creates an empty ledger from a [`SolocSchema`] implementor.
    ///
    /// This is the preferred constructor when working with a known schema type:
    ///
    /// ```rust,ignore
    /// use soloc::schemas::entity::EntitySchema;
    /// let ledger = Ledger::for_schema::<EntitySchema>()?;
    /// ```
    pub fn for_schema<S: SolocSchema>() -> Result<Self, String> {
        Self::new(&S::schema(), S::id_column())
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

        // Check that each required STS field is present with the correct Arrow type.
        // Type correctness is critical: transform_batch and filter_batch call
        // downcast_ref().unwrap() on these arrays and panic at runtime on type mismatch.
        // (field name, expected type description, type predicate)
        type FieldCheck = (&'static str, &'static str, fn(&DataType) -> bool);
        let checks: &[FieldCheck] = &[
            (
                "frame_id",
                "Dictionary(UInt32, Utf8)",
                |dt| matches!(dt, DataType::Dictionary(k, v) if **k == DataType::UInt32 && **v == DataType::Utf8),
            ),
            (
                "units_pos",
                "Dictionary(UInt16, Utf8)",
                |dt| matches!(dt, DataType::Dictionary(k, v) if **k == DataType::UInt16 && **v == DataType::Utf8),
            ),
            (
                "timescale_id",
                "Dictionary(UInt32, Utf8)",
                |dt| matches!(dt, DataType::Dictionary(k, v) if **k == DataType::UInt32 && **v == DataType::Utf8),
            ),
            (
                "source_id",
                "Dictionary(UInt32, Utf8)",
                |dt| matches!(dt, DataType::Dictionary(k, v) if **k == DataType::UInt32 && **v == DataType::Utf8),
            ),
            (
                "estimate_type",
                "Dictionary(UInt16, Utf8)",
                |dt| matches!(dt, DataType::Dictionary(k, v) if **k == DataType::UInt16 && **v == DataType::Utf8),
            ),
            (
                "position",
                "FixedSizeList(3, Float64)",
                |dt| matches!(dt, DataType::FixedSizeList(f, 3) if f.data_type() == &DataType::Float64),
            ),
            (
                "quaternion",
                "FixedSizeList(4, Float64)",
                |dt| matches!(dt, DataType::FixedSizeList(f, 4) if f.data_type() == &DataType::Float64),
            ),
            ("duration_centuries", "Int16", |dt| *dt == DataType::Int16),
            ("duration_ns", "UInt64", |dt| *dt == DataType::UInt64),
        ];

        for (name, expected, type_ok) in checks {
            match sts_fields.find(name) {
                None => {
                    return Err(format!(
                        "'{STS_COLUMN}' struct is missing required STS field '{name}'"
                    ));
                }
                Some((_, field)) => {
                    if !type_ok(field.data_type()) {
                        return Err(format!(
                            "'{STS_COLUMN}.{name}' has wrong Arrow type — expected {expected}, got {:?}",
                            field.data_type()
                        ));
                    }
                }
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

        // Topology is derived from the *normalised* rows so every epoch compared is on the
        // TAI scale. A batch that would introduce a cycle, or that names a frame which
        // cannot exist, is rejected as a whole; `ingest_batch` stages its edges internally,
        // so a rejected batch leaves the tree untouched and nothing is pushed below.
        let outcome = self
            .transform_tree
            .ingest_batch(&normalized, &self.id_column)?;
        self.update_pose_cache(&normalized, outcome.latest_rows);

        self.batches.push(normalized);
        self.seal_and_flush_if_needed();
        Ok(())
    }

    /// Rebuilds `transform_tree` and `latest_pose` from the batches already in `self`.
    ///
    /// Both are pure functions of the stored rows, so nothing extra has to be persisted —
    /// a load simply replays the batches through the same path [`Ledger::append`] uses.
    /// The batches were normalised to TAI before being stored, so they are ingested as-is.
    ///
    /// Errors if the stored data contains a cycle, which a ledger built through `append`
    /// cannot produce; a file that trips this was written by something that bypassed it.
    fn rebuild_derived_state(&mut self) -> Result<(), String> {
        if self.id_column.is_empty() || self.batches.is_empty() {
            return Ok(());
        }
        // Moved out so the ingest below can borrow the batches while mutating self.
        let batches = std::mem::take(&mut self.batches);
        let result = (|| {
            for batch in &batches {
                let outcome = self.transform_tree.ingest_batch(batch, &self.id_column)?;
                self.update_pose_cache(batch, outcome.latest_rows);
            }
            Ok(())
        })();
        self.batches = batches;
        result
    }

    /// Drops the pose cache, forcing [`Ledger::resolve_frame_at`] down its scan path.
    /// Test-only: lets a test compare the fast path against the fallback.
    #[cfg(test)]
    fn clear_pose_cache(&mut self) {
        self.latest_pose.clear();
    }

    /// Refreshes the pose cache from the winning row indices `ingest_batch` already found.
    ///
    /// Costs k targeted reads (k = distinct ids in the batch), not a second pass over the
    /// rows. `latest_rows` is taken by value so its keys move into the cache rather than
    /// being reallocated — the per-entity term dominates append cost at high entity counts.
    fn update_pose_cache(&mut self, batch: &RecordBatch, latest_rows: HashMap<String, usize>) {
        if latest_rows.is_empty() {
            return;
        }
        let Some(cols) = PoseColumns::try_new(batch, &self.id_column) else {
            return;
        };

        for (id, row) in latest_rows {
            let epoch = cols.epoch_at(row);
            // Strictly-greater, so an equal epoch keeps the entry already cached. This
            // matches resolve_frame_at's scan, which walks batches in insertion order and
            // only replaces its best on a strictly later row — the two must never disagree.
            // It also means a backfilled older batch cannot clobber a newer cached pose.
            if self
                .latest_pose
                .get(&id)
                .is_some_and(|cached| epoch <= cached.epoch)
            {
                continue;
            }
            let (parent_frame_id, isometry_km) = cols.pose_at(row);
            self.latest_pose.insert(
                id,
                LatestPose {
                    parent_frame_id,
                    isometry_km,
                    epoch,
                },
            );
        }
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
    /// let mut ledger = Ledger::new(&entity_schema(), "entity_id")?;
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
    /// the parent entity's latest pose in the ledger at each row's epoch and composing
    /// the isometry chain, following the topology derived from the appended rows.
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
        // Only pay for a resolver if the batch actually references entity frames.
        if collect_uri_frames(batch).is_empty() {
            return transform_batch(batch, target_frame, almanac, target_units, None);
        }

        // Each row resolves against its own epoch rather than one epoch for the whole
        // batch, so a batch spanning several timesteps is projected correctly.
        let resolver = |frame: &str, epoch: Epoch| self.resolve_to_root(frame, epoch);
        transform_batch(batch, target_frame, almanac, target_units, Some(&resolver))
    }

    /// Resolves one entity frame to `(astronomical_root, isometry_km)` at `epoch`.
    ///
    /// The single-frame form of [`Ledger::build_dynamic_frame_map`], shaped for use as
    /// [`spacetimestamp::transforms::transform_batch`]'s resolver. Returns `None` when the
    /// chain cannot be resolved — `transform_batch` reports that as a frame error.
    pub fn resolve_to_root(&self, frame: &str, epoch: Epoch) -> Option<(String, Isometry3<f64>)> {
        let mut resolved = HashMap::new();
        self.resolve_chain(frame, epoch, &mut resolved).ok()?;
        resolved.remove(frame)
    }

    /// Returns the pose of `entity_id` at the latest timestamp ≤ `epoch` as an
    /// `(parent_frame_id, isometry_km)` pair.
    ///
    /// Returns `None` if this ledger has no `id_column`, or if the entity has no entry
    /// at or before `epoch`.
    ///
    /// O(1) for the common case — a query at or after the entity's most recent row — via
    /// the pose cache. Genuinely historical queries still scan every batch.
    pub fn resolve_frame_at(
        &self,
        entity_id: &str,
        epoch: Epoch,
    ) -> Option<(String, Isometry3<f64>)> {
        if self.id_column.is_empty() {
            return None;
        }

        let target_dur = epoch - j2000_tai();

        // Fast path. The cached epoch is the highest ever ingested for this entity, so if
        // it is already at or before the query there cannot be a later row to find, and
        // the scan below would settle on exactly this pose.
        if let Some(cached) = self.latest_pose.get(entity_id)
            && cached.epoch <= target_dur
        {
            return Some((cached.parent_frame_id.clone(), cached.isometry_km));
        }

        // Slow path: the query predates the entity's latest row, or the cache was never
        // populated (a ledger loaded from IPC).
        let mut best_dur: Option<Duration> = None;
        let mut best: Option<(String, Isometry3<f64>)> = None;

        for batch in &self.batches {
            let Some(cols) = PoseColumns::try_new(batch, &self.id_column) else {
                continue;
            };

            for i in 0..batch.num_rows() {
                if cols.id_at(i) != entity_id {
                    continue;
                }

                let row_dur = cols.epoch_at(i);
                if row_dur > target_dur {
                    continue;
                }
                if best_dur.is_some_and(|b| row_dur <= b) {
                    continue;
                }

                best_dur = Some(row_dur);
                best = Some(cols.pose_at(i));
            }
        }

        best
    }

    /// Builds a dynamic frame map for use with [`spacetimestamp::transforms::transform_batch`].
    ///
    /// Maps each requested entity — and every ancestor resolved along the way — to the
    /// astronomical frame its chain terminates in, plus the isometry taking it there.
    pub fn build_dynamic_frame_map(
        &self,
        entity_ids: &[&str],
        epoch: Epoch,
    ) -> Result<HashMap<String, (String, Isometry3<f64>)>, String> {
        let mut result = HashMap::new();
        for &id in entity_ids {
            // Ancestors get memoised by the walk below, so a later id sharing a chain
            // with an earlier one costs nothing.
            if !result.contains_key(id) {
                self.resolve_chain(id, epoch, &mut result)?;
            }
        }
        Ok(result)
    }

    /// Resolves one entity's chain to its astronomical root, memoising every hop.
    ///
    /// Structure comes from `transform_tree` (which is where cycle detection now lives);
    /// this only performs the per-hop numeric lookups and composes them.
    fn resolve_chain(
        &self,
        entity_id: &str,
        epoch: Epoch,
        result: &mut HashMap<String, (String, Isometry3<f64>)>,
    ) -> Result<(), String> {
        let chain = self
            .transform_tree
            .resolve_chain(entity_id, epoch - j2000_tai())?;

        // `resolve_chain` always returns at least [entity, terminal]: the last element is
        // the astronomical root, everything before it is an entity needing a pose lookup.
        let Some((root, hops)) = chain.split_last() else {
            return Err(format!("Empty frame chain for '{entity_id}'"));
        };

        // Walk inward from the root so each hop composes onto its parent's accumulated pose.
        let mut acc = Isometry3::identity();
        for node in hops.iter().rev() {
            let (_, iso) = self.resolve_frame_at(node, epoch).ok_or_else(|| {
                format!("Entity '{node}' not found in ledger at or before {epoch}")
            })?;
            acc *= iso;
            result.insert(node.clone(), (root.clone(), acc));
        }
        Ok(())
    }

    /// Exports this ledger's full topology history as a [`RecordBatch`] for federation.
    ///
    /// Follows [`spacetimestamp::topology::topology_schema`]. The whole event log is
    /// exported, not just the current parenting, so a recipient can replay history.
    pub fn export_topology(&self) -> Result<RecordBatch, String> {
        self.transform_tree.to_log_batch()
    }

    /// Merges a topology log exported by [`Ledger::export_topology`] (possibly by a
    /// federated peer) into this ledger's tree. Returns the number of events applied.
    ///
    /// Rejected as a whole if the merged result would contain a cycle. This affects
    /// topology only — no poses are added, so an edge merged for an entity this ledger
    /// holds no rows for will resolve structurally but fail the numeric lookup.
    pub fn merge_topology(&mut self, batch: &RecordBatch) -> Result<usize, String> {
        self.transform_tree.merge_log_batch(batch)
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

    /// Writes the ledger's schema to an Arrow IPC file with zero data batches.
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
    /// the schema and its metadata. Frame topology is *not* carried by the schema; it is
    /// rebuilt from the rows themselves as batches are appended or loaded.
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
            transform_tree: TransformTree::new(),
            latest_pose: HashMap::new(),
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
            transform_tree: TransformTree::new(),
            latest_pose: HashMap::new(),
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

                if let Some(ref filter) = id_filter_set
                    && !filter.contains(row_key.as_str())
                {
                    continue;
                }

                let dur = Duration::from_parts(cent_arr.value(row), ns_arr.value(row));

                if let Some(c) = cutoff
                    && dur < c
                {
                    continue;
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

        let mut ledger = Self {
            schema,
            batches,
            id_column: id_column.to_string(),
            transform_tree: TransformTree::new(),
            latest_pose: HashMap::new(),
        };
        ledger.rebuild_derived_state()?;
        Ok(ledger)
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
        let mut ledger = Self {
            schema,
            batches,
            id_column: id_column.to_string(),
            transform_tree: TransformTree::new(),
            latest_pose: HashMap::new(),
        };
        ledger.rebuild_derived_state()?;
        Ok(ledger)
    }
}

/// The identity and pose columns of one batch, located once instead of once per row.
///
/// Shared by [`Ledger::resolve_frame_at`]'s scan and [`Ledger::update_pose_cache`] so the
/// two can never drift in how they read a row. Every accessor is indexed by row number.
struct PoseColumns<'a> {
    id_keys: &'a DictionaryArray<UInt32Type>,
    id_values: &'a StringArray,
    frame_keys: &'a DictionaryArray<UInt32Type>,
    frame_values: &'a StringArray,
    units_keys: &'a DictionaryArray<UInt16Type>,
    units_values: &'a StringArray,
    position: &'a Float64Array,
    position_offset: usize,
    quaternion: &'a Float64Array,
    quaternion_offset: usize,
    centuries: &'a Int16Array,
    nanos: &'a UInt64Array,
}

impl<'a> PoseColumns<'a> {
    /// Locates every column needed to read a pose, or `None` if any is absent or has an
    /// unexpected type — callers skip such a batch rather than failing the whole query.
    fn try_new(batch: &'a RecordBatch, id_column: &str) -> Option<Self> {
        let id_keys = batch
            .column_by_name(id_column)?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()?;
        let id_values = id_keys.values().as_any().downcast_ref::<StringArray>()?;

        let sts = batch
            .column_by_name(STS_COLUMN)?
            .as_any()
            .downcast_ref::<StructArray>()?;

        let frame_keys = sts
            .column_by_name("frame_id")?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()?;
        let frame_values = frame_keys.values().as_any().downcast_ref::<StringArray>()?;

        let units_keys = sts
            .column_by_name("units_pos")?
            .as_any()
            .downcast_ref::<DictionaryArray<UInt16Type>>()?;
        let units_values = units_keys.values().as_any().downcast_ref::<StringArray>()?;

        let pos_list = sts
            .column_by_name("position")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()?;
        let position = pos_list.values().as_any().downcast_ref::<Float64Array>()?;
        let position_offset = pos_list.offset();

        let quat_list = sts
            .column_by_name("quaternion")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()?;
        let quaternion = quat_list.values().as_any().downcast_ref::<Float64Array>()?;
        let quaternion_offset = quat_list.offset();

        let centuries = sts
            .column_by_name("duration_centuries")?
            .as_any()
            .downcast_ref::<Int16Array>()?;
        let nanos = sts
            .column_by_name("duration_ns")?
            .as_any()
            .downcast_ref::<UInt64Array>()?;

        Some(Self {
            id_keys,
            id_values,
            frame_keys,
            frame_values,
            units_keys,
            units_values,
            position,
            position_offset,
            quaternion,
            quaternion_offset,
            centuries,
            nanos,
        })
    }

    /// The entity id at `row`.
    fn id_at(&self, row: usize) -> &str {
        self.id_values
            .value(self.id_keys.keys().value(row) as usize)
    }

    /// The timestamp at `row`, as an offset from the J2000 TAI epoch.
    fn epoch_at(&self, row: usize) -> Duration {
        Duration::from_parts(self.centuries.value(row), self.nanos.value(row))
    }

    /// The `(parent_frame_id, isometry)` at `row`, with the translation converted to km.
    fn pose_at(&self, row: usize) -> (String, Isometry3<f64>) {
        let units = self
            .units_values
            .value(self.units_keys.keys().value(row) as usize);
        let to_km: f64 = match units.to_lowercase().as_str() {
            "m" | "meters" | "meter" => 0.001,
            "au" => 149_597_870.7,
            _ => 1.0,
        };

        let pb = (self.position_offset + row) * 3;
        let translation = Translation3::new(
            self.position.value(pb) * to_km,
            self.position.value(pb + 1) * to_km,
            self.position.value(pb + 2) * to_km,
        );

        let qb = (self.quaternion_offset + row) * 4;
        let rotation = UnitQuaternion::from_quaternion(Quaternion::new(
            self.quaternion.value(qb),
            self.quaternion.value(qb + 1),
            self.quaternion.value(qb + 2),
            self.quaternion.value(qb + 3),
        ));

        let frame_id = self
            .frame_values
            .value(self.frame_keys.keys().value(row) as usize)
            .to_string();

        (frame_id, Isometry3::from_parts(translation, rotation))
    }
}

fn estimate_type_priority(s: &str) -> u8 {
    match s {
        "MEASURED" => 0,
        "PREDICTED" => 1,
        _ => 2,
    }
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
        let sts_ref = sts_schema();
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
        let mut builder = SpaceTimestampBuilder::new(1);
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
        Ledger::new(&entity_schema(), "entity_id").unwrap()
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

    #[test]
    fn test_new_rejects_wrong_sts_field_type() {
        // Build a spacetimestamp struct where `duration_ns` is Int32 instead of UInt64.
        // validate_schema must catch the type mismatch before any runtime downcast panic.
        use spacetimestamp::schema::sts_schema;
        let sts_ref = sts_schema();
        let mut fields: Vec<Field> = sts_ref.fields().iter().map(|f| (**f).clone()).collect();
        let ns_idx = fields
            .iter()
            .position(|f| f.name() == "duration_ns")
            .unwrap();
        fields[ns_idx] = Field::new("duration_ns", DataType::Int32, false);
        let broken_sts = DataType::Struct(Fields::from(fields));
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            broken_sts,
            false,
        )]));
        let err = Ledger::new(&schema, "").unwrap_err();
        assert!(
            err.contains("duration_ns"),
            "error should name the bad field: {err}"
        );
        assert!(
            err.contains("UInt64"),
            "error should name the expected type: {err}"
        );
    }

    #[test]
    fn test_new_rejects_missing_sts_field() {
        // Build a struct that is missing the `position` field entirely.
        use spacetimestamp::schema::sts_schema;
        let sts_ref = sts_schema();
        let fields: Vec<Field> = sts_ref
            .fields()
            .iter()
            .filter(|f| f.name() != "position")
            .map(|f| (**f).clone())
            .collect();
        let broken_sts = DataType::Struct(Fields::from(fields));
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            broken_sts,
            false,
        )]));
        let err = Ledger::new(&schema, "").unwrap_err();
        assert!(
            err.contains("position"),
            "error should name the missing field: {err}"
        );
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
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            entity_id, frame_id, "km", "TAI", "test:src", "MEASURED", pos, quat, 0, ns, None, None,
            None, None, None, None,
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

    /// Cycles are now rejected at append time by the transform tree, rather than being
    /// stored and only discovered later when someone tried to resolve a chain through them.
    #[test]
    fn test_append_rejects_cycle() {
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

        let err = ledger
            .append(make_entity_batch(
                "demo:B",
                "demo:A",
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap_err();
        assert!(
            err.to_lowercase().contains("cycle"),
            "expected cycle error: {err}"
        );

        // The rejected batch must leave no trace: neither the rows nor the pose cache
        // entry it would have created.
        assert_eq!(ledger.len(), 1, "rejected batch must not be stored");
        assert!(
            ledger.resolve_frame_at("demo:B", j2000()).is_none(),
            "rejected batch must not populate the pose cache"
        );
    }

    /// A parent frame that is neither an entity URI nor a name anise knows would leave the
    /// child dangling off nothing. Catching it at append preserves the "typo caught early"
    /// property the old `add_frame_validated` provided at registration time.
    #[test]
    fn test_append_rejects_floating_frame() {
        let mut ledger = make_entity_ledger();
        let err = ledger
            .append(make_entity_batch(
                "demo:rover",
                "IAU_MRAS", // typo for IAU_MARS
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap_err();
        assert!(
            err.contains("IAU_MRAS"),
            "error should name the offending frame: {err}"
        );
        assert_eq!(ledger.len(), 0, "rejected batch must not be stored");
    }

    /// Raw NAIF integer IDs are a legal parent and must survive the floating-frame check.
    #[test]
    fn test_append_accepts_naif_integer_parent() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:lander",
                "499", // Mars
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        assert_eq!(ledger.len(), 1);
    }

    // -----------------------------------------------------------------------
    // topology derivation, persistence, and federation tests
    // -----------------------------------------------------------------------

    /// Builds a ledger holding `demo:robot` → `demo:facility` → `IAU_EARTH`.
    fn make_two_hop_ledger() -> Ledger {
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
        ledger
    }

    /// Topology is derived data, not persisted separately — a reloaded ledger has to
    /// rebuild it from the rows, or chain resolution silently stops working.
    #[test]
    fn test_ipc_round_trip_rebuilds_topology() {
        let ledger = make_two_hop_ledger();
        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let reloaded = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();

        let map = reloaded
            .build_dynamic_frame_map(&["demo:robot"], j2000())
            .unwrap();
        let (root, iso) = map.get("demo:robot").unwrap();
        assert_eq!(root, "IAU_EARTH");
        assert!((iso.translation.vector.x - 55.0).abs() < 1e-9);
    }

    #[test]
    fn test_export_topology_shape() {
        let ledger = make_two_hop_ledger();
        let exported = ledger.export_topology().unwrap();

        assert_eq!(
            exported.schema(),
            spacetimestamp::topology::topology_schema()
        );
        // One edge per entity: facility→IAU_EARTH and robot→facility.
        assert_eq!(exported.num_rows(), 2);
    }

    /// A federated peer receiving an exported log gets the *structure* only — poses still
    /// have to arrive as ordinary rows.
    #[test]
    fn test_topology_export_merges_into_peer() {
        let exported = make_two_hop_ledger().export_topology().unwrap();

        let mut peer = make_entity_ledger();
        assert_eq!(peer.merge_topology(&exported).unwrap(), 2);

        // The chain is now known, so resolution gets far enough to demand a pose the peer
        // does not hold — rather than failing for lack of a parent.
        let err = peer
            .build_dynamic_frame_map(&["demo:robot"], j2000())
            .unwrap_err();
        assert!(
            err.contains("not found in ledger"),
            "expected a missing-pose error, got: {err}"
        );
    }

    /// End-to-end through the public `transform` API: topology derived from appended rows →
    /// pose cache → resolver → `transform_batch`. The individual pieces are covered above;
    /// this pins the composition, which is the seam the transform-tree redesign rewired.
    ///
    /// Chain: `demo:sensor` @ [1,0,0] in `demo:robot` @ [5,0,0] in `demo:facility` @ [50,0,0]
    /// in Earth. Reprojected into Earth the offsets sum to 56 km.
    #[test]
    fn test_transform_resolves_derived_chain_end_to_end() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:facility",
                "Earth",
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

        // A reading expressed in the robot's frame — never appended, just reprojected.
        let observation = make_entity_batch(
            "demo:sensor",
            "demo:robot",
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );

        let result = ledger
            .transform(&observation, "Earth", "km", &Almanac::default())
            .expect("transform should resolve the full derived chain");

        let sts = result
            .column_by_name(STS_COLUMN)
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let pos = sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::FixedSizeListArray>()
            .unwrap();
        let vals = pos
            .values()
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap();

        assert!(
            (vals.value(0) - 56.0).abs() < 1e-9,
            "expected 50 + 5 + 1 = 56 km, got {}",
            vals.value(0)
        );
        assert!(vals.value(1).abs() < 1e-9);
        assert!(vals.value(2).abs() < 1e-9);
    }

    /// A batch with no entity-URI frames must take `transform`'s no-resolver fast path and
    /// still come back correct.
    #[test]
    fn test_transform_passthrough_batch_needs_no_topology() {
        let ledger = make_entity_ledger();
        let observation = make_entity_batch(
            "demo:probe",
            "Earth",
            [7.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );

        let result = ledger
            .transform(&observation, "Earth", "km", &Almanac::default())
            .expect("astronomical-only batch needs no ledger topology");
        assert_eq!(result.num_rows(), 1);
    }

    /// Re-parenting is an ordinary append, and queries at different epochs must see the
    /// parent that was in effect at each one.
    #[test]
    fn test_resolve_to_root_follows_reparenting_over_time() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:hangar",
                "IAU_EARTH",
                [10.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        // Starts parented to the hangar...
        ledger
            .append(make_entity_batch(
                "demo:drone",
                "demo:hangar",
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        // ...then is re-parented straight to IAU_EARTH once airborne.
        ledger
            .append(make_entity_batch(
                "demo:drone",
                "IAU_EARTH",
                [500.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                1_000,
            ))
            .unwrap();

        // Before the re-parenting: through the hangar, so 10 + 1.
        let (root, iso) = ledger.resolve_to_root("demo:drone", j2000()).unwrap();
        assert_eq!(root, "IAU_EARTH");
        assert!((iso.translation.vector.x - 11.0).abs() < 1e-9);

        // After: direct, so just 500.
        let after = j2000() + Duration::from_parts(0, 1_000);
        let (root, iso) = ledger.resolve_to_root("demo:drone", after).unwrap();
        assert_eq!(root, "IAU_EARTH");
        assert!((iso.translation.vector.x - 500.0).abs() < 1e-9);
    }

    #[test]
    fn test_resolve_to_root_unknown_entity_returns_none() {
        let ledger = make_two_hop_ledger();
        assert!(ledger.resolve_to_root("demo:ghost", j2000()).is_none());
    }

    // -----------------------------------------------------------------------
    // pose cache tests
    // -----------------------------------------------------------------------

    /// Checks that `resolve_frame_at`'s cache fast path and its scan fallback give the same
    /// answer — including on the epoch-tie rule, where both must keep the *first* row seen.
    ///
    /// Takes an independent copy through IPC and drops its pose cache, so every lookup on
    /// that copy is forced down the scan path while the original still uses the cache.
    fn assert_cache_agrees_with_scan(ledger: &Ledger, entity_id: &str, queries: &[Epoch]) {
        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let mut scanned = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();
        scanned.clear_pose_cache();

        for &epoch in queries {
            let cached = ledger.resolve_frame_at(entity_id, epoch);
            let scanned = scanned.resolve_frame_at(entity_id, epoch);
            assert_eq!(
                cached.as_ref().map(|(f, i)| (f, i.translation.vector)),
                scanned.as_ref().map(|(f, i)| (f, i.translation.vector)),
                "cache and scan disagree for '{entity_id}' at {epoch}"
            );
        }
    }

    #[test]
    fn test_pose_cache_returns_latest_pose() {
        let mut ledger = make_entity_ledger();
        for (x, ns) in [(1.0, 0), (2.0, 1_000), (3.0, 2_000)] {
            ledger
                .append(make_entity_batch(
                    "demo:A",
                    "ICRF",
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    ns,
                ))
                .unwrap();
        }

        let at_latest = j2000() + Duration::from_parts(0, 2_000);
        let (frame, iso) = ledger.resolve_frame_at("demo:A", at_latest).unwrap();
        assert_eq!(frame, "ICRF");
        assert_eq!(iso.translation.vector.x, 3.0);

        // Well past the last row — still the last row, served from the cache.
        let far_future = j2000() + Duration::from_parts(0, 999_999);
        let (_, iso) = ledger.resolve_frame_at("demo:A", far_future).unwrap();
        assert_eq!(iso.translation.vector.x, 3.0);

        assert_cache_agrees_with_scan(&ledger, "demo:A", &[at_latest, far_future]);
    }

    #[test]
    fn test_resolve_frame_at_historical_query_bypasses_cache() {
        let mut ledger = make_entity_ledger();
        for (x, ns) in [(1.0, 0), (2.0, 1_000), (3.0, 2_000)] {
            ledger
                .append(make_entity_batch(
                    "demo:A",
                    "ICRF",
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    ns,
                ))
                .unwrap();
        }

        // Between rows: the cached epoch (2000) is after the query, so this must fall back
        // to the scan and find the row at 1000 rather than returning the cached pose.
        let midpoint = j2000() + Duration::from_parts(0, 1_500);
        let (_, iso) = ledger.resolve_frame_at("demo:A", midpoint).unwrap();
        assert_eq!(iso.translation.vector.x, 2.0);

        // Before every row: no answer exists.
        assert!(
            ledger
                .resolve_frame_at("demo:A", j2000() - Duration::from_parts(0, 1))
                .is_none()
        );

        assert_cache_agrees_with_scan(
            &ledger,
            "demo:A",
            &[j2000(), midpoint, j2000() + Duration::from_parts(0, 1_000)],
        );
    }

    /// Appending an older batch after a newer one must not move the cache backwards.
    #[test]
    fn test_pose_cache_survives_backfilled_batch() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:A",
                "ICRF",
                [9.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                5_000,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                "demo:A",
                "ICRF",
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let latest = j2000() + Duration::from_parts(0, 5_000);
        let (_, iso) = ledger.resolve_frame_at("demo:A", latest).unwrap();
        assert_eq!(
            iso.translation.vector.x, 9.0,
            "backfilled older row must not overwrite the cached newer pose"
        );

        // The backfilled row is still findable at its own epoch, via the scan path.
        let (_, iso) = ledger.resolve_frame_at("demo:A", j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 1.0);

        assert_cache_agrees_with_scan(&ledger, "demo:A", &[j2000(), latest]);
    }

    /// Two rows at the same epoch: the first one appended wins, in both code paths.
    #[test]
    fn test_pose_cache_epoch_tie_keeps_first_seen() {
        let mut ledger = make_entity_ledger();
        for x in [1.0, 2.0] {
            ledger
                .append(make_entity_batch(
                    "demo:A",
                    "ICRF",
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    0,
                ))
                .unwrap();
        }

        let (_, iso) = ledger.resolve_frame_at("demo:A", j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 1.0);

        assert_cache_agrees_with_scan(&ledger, "demo:A", &[j2000()]);
    }

    /// Poses are cached in km regardless of the units the row was written in, matching
    /// what the scan path returns.
    #[test]
    fn test_pose_cache_normalises_units_to_km() {
        use crate::schemas::entity::EntityBuilder;
        let mut ledger = make_entity_ledger();
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            "demo:A",
            "ICRF",
            "m",
            "TAI",
            "test:src",
            "MEASURED",
            [2_000.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        ledger.append(b.flush()).unwrap();

        let (_, iso) = ledger.resolve_frame_at("demo:A", j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 2.0);

        assert_cache_agrees_with_scan(&ledger, "demo:A", &[j2000()]);
    }

    /// Independent entities each keep their own cache entry.
    #[test]
    fn test_pose_cache_is_per_entity() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                "demo:A",
                "ICRF",
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                "demo:B",
                "ICRF",
                [7.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                1_000,
            ))
            .unwrap();

        let at_b = j2000() + Duration::from_parts(0, 1_000);
        assert_eq!(
            ledger
                .resolve_frame_at("demo:A", at_b)
                .unwrap()
                .1
                .translation
                .vector
                .x,
            1.0
        );
        assert_eq!(
            ledger
                .resolve_frame_at("demo:B", at_b)
                .unwrap()
                .1
                .translation
                .vector
                .x,
            7.0
        );
        assert!(ledger.resolve_frame_at("demo:missing", at_b).is_none());

        assert_cache_agrees_with_scan(&ledger, "demo:A", &[j2000(), at_b]);
        assert_cache_agrees_with_scan(&ledger, "demo:B", &[j2000(), at_b]);
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
        let mut b = EntityBuilder::new(1);
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
            let mut b = EntityBuilder::new(2);
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
