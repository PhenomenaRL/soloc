//! Soloc Ledger: Append-only RecordBatch Store for Soloc Schemas.
//!
//! The [`Ledger`] accumulates Arrow [`RecordBatch`]es or validated Soloc Schemas, and never
//! overwrites existing data. Measured observations (from telescopes, sensors, manual input)
//! and simulation outputs (`estimate_type = SIMULATED`) coexist in the same store
//! and are distinguished by their `estimate_type` field.

use arrow::array::{Array, BooleanBuilder, FixedSizeBinaryArray};
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};
use nalgebra::{Isometry3, Quaternion, Translation3, UnitQuaternion};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anise::prelude::Almanac;
use spacetimestamp::ephemeris::j2000_tai;
use spacetimestamp::identity::{
    IdMap, IdSet, NameRegistry, PrescribedId, as_id_column, id_at, id_type, registry_schema,
};
use spacetimestamp::ipc;
use spacetimestamp::query::{SpatiotemporalFilter, apply_boolean_mask, filter_batch};
use spacetimestamp::schema::StsColumns;
use spacetimestamp::topology::TransformTree;
use spacetimestamp::transforms::{ResolvedFrame, normalize_batch_to_tai, transform_batch};
use spacetimestamp::validation::{validate_spacetimestamp_batch, validate_sts_schema};
use spacetimestamp::vocabulary::LengthUnit;

use spacetimestamp::schemas::SpaceTimestampSchema;

/// Merge batches in memory when the count exceeds this to keep query latency bounded.
///
/// Benchmarks show ~11 µs fixed overhead per batch. At 50 batches of ≥1000 rows each,
/// time-filter queries stay under ~1 ms. Beyond this threshold, merging pays off.
const SEGMENT_THRESHOLD: usize = 50;

/// Default staleness window for [`Ledger::current_state`] when `not_before` is not supplied.
/// Rows older than (latest stored timestamp − this window) are excluded.
const CURRENT_STATE_WINDOW_NS: u64 = 3_600 * 1_000_000_000; // 1 hour

/// How an id column's Arrow type is described in a schema-validation error.
///
/// The type itself always comes from [`id_type`], so this string is documentation
/// for the reader of the error and cannot drift into being the check.
const ID_TYPE_DESC: &str = "FixedSizeBinary(16)";

/// An append-only store of [`RecordBatch`]es forming the soloc Universal Ledger.
///
/// Schema-agnostic: works with any Arrow schema that embeds a spacetimestamp struct column.
/// The schema and `id_column` name are fixed at construction and
/// validated against the provided schema before the ledger is created.
///
/// eg. for the standard entity schema, construct with:
/// ```
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
    /// Parent graph derived from appended rows. Topology only, never a pose value.
    transform_tree: TransformTree,
    /// Highest-epoch pose seen for each entity, so the common "where is X now" lookup
    /// does not have to scan every batch. See [`LatestPose`].
    latest_pose: IdMap<LatestPose>,
    /// Display names for the ids this ledger has been told about.
    names: NameRegistry,
}

/// The most recent pose ingested for one entity. This is the pose cache for
/// [`Ledger::resolve_frame_at`]'s fast path.
///
/// Populated in [`Ledger::append`] from the winning row indices `ingest_batch` already
/// computed, so maintaining it costs k targeted reads per append rather than a second
/// full scan. `epoch` is the highest epoch *ever ingested* for the entity, which is what
/// makes the fast path sound: a query at or after `epoch` cannot have a newer row to find.
#[derive(Debug, Clone)]
struct LatestPose {
    /// The `frame_id` the pose is expressed in: another entity, or an astronomical frame.
    parent_frame_id: PrescribedId,
    /// The pose itself, normalised to kilometres.
    /// This currently makes the system very rigid to having latest pose in km only...
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
            latest_pose: IdMap::default(),
            names: NameRegistry::new(),
        })
    }

    /// Creates an empty ledger from a [`SpaceTimestampSchema`] implementor.
    ///
    /// This is the preferred constructor when working with a known schema type:
    ///
    /// ```
    /// use soloc::schemas::entity::EntitySchema;
    /// use soloc::ledger::Ledger;
    ///
    /// let ledger = Ledger::for_schema::<EntitySchema>()?;
    /// # Ok::<(), String>(())
    /// ```
    pub fn for_schema<S: SpaceTimestampSchema>() -> Result<Self, String> {
        Self::new(&S::schema(), S::id_column())
    }

    /// Returns the Arrow schema this ledger was created with.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// Returns this ledger's display-name registry.
    pub fn names(&self) -> &NameRegistry {
        &self.names
    }

    /// Records the common name `id` was minted from, rejecting a binding that does not hash.
    ///
    /// Nothing about identity, joins, topology, or pose resolution depends on this. An id
    /// with no registered name is a first-class citizen everywhere except rendering. The one
    /// exception is [`Ledger::transform`], which hands astronomical roots to `anise` by name.
    pub fn register_name(
        &mut self,
        id: PrescribedId,
        authority: &str,
        common_name: &str,
    ) -> Result<(), String> {
        self.names.insert(id, authority, common_name)
    }

    /// Renders `id` for a human: its registered common name, or the hyphenated id.
    ///
    /// Error messages go through this so an unregistered id still reads as *something*
    /// rather than making the message conditional on the registry being populated.
    fn display(&self, id: PrescribedId) -> String {
        self.names.display(id)
    }

    /// Validates that `schema` contains a `"spacetimestamp"` struct with all required STS fields
    /// and (if non-empty) that `id_column` exists.
    fn validate_schema(schema: &SchemaRef, id_column: &str) -> Result<(), String> {
        validate_sts_schema(schema)?;

        if !id_column.is_empty() {
            let field = schema
                .field_with_name(id_column)
                .map_err(|_| format!("id_column '{id_column}' not found in schema"))?;
            if *field.data_type() != id_type() {
                return Err(format!(
                    "id_column '{id_column}' has wrong Arrow type — expected \
                     {ID_TYPE_DESC}, got {:?}",
                    field.data_type()
                ));
            }
        }

        Ok(())
    }

    /// Validates and appends a batch to the ledger, normalizing all timestamps to TAI.
    ///
    /// Validation checks `timescale_id` and `frame_id` values against hifitime and anise
    /// standards. Returns `Err` if any value is unrecognized, & the ledger is unchanged.
    ///
    /// Rows whose `timescale_id` is already `TAI` are passed through with no allocation.
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
    fn update_pose_cache(&mut self, batch: &RecordBatch, latest_rows: IdMap<usize>) {
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
            // only replaces its best on a strictly later row. The two must never disagree.
            // It also means a backfilled older batch cannot clobber a newer cached pose.
            if self
                .latest_pose
                .get(&id)
                .is_some_and(|cached| epoch <= cached.epoch)
            {
                continue;
            }
            let Some((parent_frame_id, isometry_km)) = cols.pose_at(row) else {
                continue;
            };
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
    /// This is the preferred API for streaming data to eg. a UI renderer. the first
    /// matching batch is yielded immediately rather than waiting for a full ledger scan.
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
    pub fn latest_snapshot(&self, entity_ids: Option<&[PrescribedId]>) -> Option<RecordBatch> {
        let last = self.batches.last()?;

        let ids = match entity_ids {
            None => return Some(last.clone()),
            Some(_) if self.id_column.is_empty() => return None,
            Some(ids) => ids,
        };

        let entity_col = id_column_of(last.column_by_name(&self.id_column)?, &self.id_column)?;

        let mut mask = BooleanBuilder::with_capacity(last.num_rows());
        for row in 0..last.num_rows() {
            // A row whose id will not decode matches nothing, so it is filtered out.
            let matched = id_at(entity_col, row).is_ok_and(|id| ids.contains(&id));
            mask.append_value(matched);
        }

        apply_boolean_mask(last, &mask.finish()).ok()
    }

    /// Transforms `batch` into `target_frame`, resolving any entity frame chains against
    /// this ledger's current contents.
    ///
    /// [`KIND_SOLOC`](spacetimestamp::identity::KIND_SOLOC) `frame_id` values are resolved by
    /// looking up the parent entity's latest pose in the ledger at each row's epoch and
    /// composing the isometry chain, following the topology derived from the appended rows.
    ///
    /// Every astronomical frame a chain terminates in resolves directly from its embedded
    /// `(ephemeris_id, orientation_id)` pair.
    ///
    /// Not compiled. This needs a populated `Almanac`, which means a kernel download.
    /// ```rust,ignore
    /// let result = ledger.transform(&my_batch, "ICRF", LengthUnit::km, &almanac)?;
    /// ```
    pub fn transform(
        &self,
        batch: &RecordBatch,
        target_frame: &str,
        target_units: LengthUnit,
        almanac: &Almanac,
    ) -> Result<RecordBatch, String> {
        // Only pay for a resolver if the batch actually references entity frames.
        if !batch_references_entity_frames(batch) {
            return transform_batch(batch, target_frame, almanac, target_units, None);
        }

        // Each row resolves against its own epoch rather than one epoch for the whole
        // batch, so a batch spanning several timesteps is projected correctly.
        let resolver = |frame: PrescribedId, epoch: Epoch| self.resolve_to_root(frame, epoch);
        transform_batch(batch, target_frame, almanac, target_units, Some(&resolver))
    }

    /// Resolves one entity frame to `(astronomical_root, isometry_km)` at `epoch`.
    ///
    /// The single-frame form of [`Ledger::build_dynamic_frame_map`], shaped for use as
    /// [`spacetimestamp::transforms::transform_batch`]'s resolver.
    ///
    /// `None` means "not resolved here", which `transform_batch` reads off the id's kind:
    /// an astronomical frame is a root and resolves from the almanac, while an entity
    /// frame with no chain is reported as a frame error.
    pub fn resolve_to_root(&self, frame: PrescribedId, epoch: Epoch) -> Option<ResolvedFrame> {
        let mut resolved = HashMap::new();
        self.accumulate_poses_to_root(frame, epoch, &mut resolved)
            .ok()?;
        resolved.remove(&frame)
    }

    /// Returns the pose of `entity_id` at the latest timestamp ≤ `epoch` as an
    /// `(parent_frame_id, isometry_km)` pair.
    ///
    /// Returns `None` if this ledger has no `id_column`, or if the entity has no entry
    /// at or before `epoch`.
    pub fn resolve_frame_at(&self, entity_id: PrescribedId, epoch: Epoch) -> Option<ResolvedFrame> {
        if self.id_column.is_empty() {
            return None;
        }

        let target_dur = epoch - j2000_tai();

        // Fast path. The cached epoch is the highest ever ingested for this entity, so if
        // it is already at or before the query there cannot be a later row to find, and
        // the scan below would settle on exactly this pose.
        if let Some(cached) = self.latest_pose.get(&entity_id)
            && cached.epoch <= target_dur
        {
            return Some((cached.parent_frame_id, cached.isometry_km));
        }

        // Slow path: the query predates the entity's latest row, or the cache was never
        // populated (a ledger loaded from IPC).
        let mut best_dur: Option<Duration> = None;
        let mut best: Option<ResolvedFrame> = None;

        for batch in &self.batches {
            let Some(cols) = PoseColumns::try_new(batch, &self.id_column) else {
                continue;
            };

            for i in 0..batch.num_rows() {
                if cols.id_at(i) != Some(entity_id) {
                    continue;
                }

                let row_dur = cols.epoch_at(i);
                if row_dur > target_dur {
                    continue;
                }
                if best_dur.is_some_and(|b| row_dur <= b) {
                    continue;
                }

                // Read the pose before advancing `best_dur`: a row this cannot decode must
                // not shadow an earlier row that it beats on epoch alone.
                let Some(pose) = cols.pose_at(i) else {
                    continue;
                };
                best_dur = Some(row_dur);
                best = Some(pose);
            }
        }

        best
    }

    /// Builds a dynamic frame map for use with [`spacetimestamp::transforms::transform_batch`].
    ///
    /// Astronomical ids contribute no entry: they are chain roots, so a lookup miss is the
    /// signal to resolve them from the almanac. An unresolvable entity id is an error.
    pub fn build_dynamic_frame_map(
        &self,
        entity_ids: &[PrescribedId],
        epoch: Epoch,
    ) -> Result<HashMap<PrescribedId, ResolvedFrame>, String> {
        let mut result = HashMap::new();
        for &id in entity_ids {
            // Ancestors get memoised by the walk below, so a later id sharing a chain
            // with an earlier one costs nothing.
            if !result.contains_key(&id) {
                self.accumulate_poses_to_root(id, epoch, &mut result)?;
            }
        }
        Ok(result)
    }

    /// Resolves one entity's chain to its astronomical root, memoising every hop.
    ///
    /// An astronomical `entity_id` has a single-element chain, so it contributes no hops.
    fn accumulate_poses_to_root(
        &self,
        entity_id: PrescribedId,
        epoch: Epoch,
        result: &mut HashMap<PrescribedId, ResolvedFrame>,
    ) -> Result<(), String> {
        let chain = self
            .transform_tree
            .ancestry_at(entity_id, epoch - j2000_tai())?;

        // `ancestry_at` never returns an empty chain: the last element is the astronomical
        // root, everything before it is an entity needing a pose lookup.
        let Some((root, hops)) = chain.split_last() else {
            return Err(format!(
                "Empty frame chain for '{}'",
                self.display(entity_id)
            ));
        };

        // Walk inward from the root so each hop composes onto its parent's accumulated pose.
        let mut acc = Isometry3::identity();
        for &node in hops.iter().rev() {
            let (_, iso) = self.resolve_frame_at(node, epoch).ok_or_else(|| {
                format!(
                    "Entity '{}' not found in ledger at or before {epoch}",
                    self.display(node)
                )
            })?;
            acc *= iso;
            result.insert(node, (*root, acc));
        }
        Ok(())
    }

    /// Exports this ledger's full topology history as a [`RecordBatch`] for federation.
    pub fn export_topology(&self) -> Result<RecordBatch, String> {
        self.transform_tree.to_log_batch()
    }

    /// Merges a topology log exported by [`Ledger::export_topology`] (possibly by a
    /// federated peer) into this ledger's tree. Returns the number of events applied.
    pub fn merge_topology(&mut self, batch: &RecordBatch) -> Result<usize, String> {
        self.transform_tree.merge_log_batch(batch)
    }

    /// Exports this ledger's name registry as a [`spacetimestamp::identity::registry_schema`]
    /// batch, for federation alongside [`Ledger::export_topology`].
    pub fn export_names(&self) -> Result<RecordBatch, String> {
        self.names.to_batch()
    }

    /// Serializes the name registry to an in-memory Arrow IPC buffer.
    pub fn names_to_ipc_bytes(&self) -> Result<Vec<u8>, String> {
        ipc::write_bytes(&[self.export_names()?], &registry_schema())
    }

    /// Merges a registry serialized by [`Ledger::names_to_ipc_bytes`]. Returns the number of
    /// names newly learned.
    pub fn merge_names_from_ipc_bytes(&mut self, bytes: &[u8]) -> Result<usize, String> {
        // Every batch is verified before any is applied, so a forged one rejects the payload
        // whole; that is why the read stops short of concatenating.
        let (_, batches) = ipc::read_bytes(bytes)?;
        self.names.merge_batches(&batches)
    }

    /// Merges all batches into a single [`RecordBatch`] for serialisation.
    fn merge_for_ipc(&self) -> Result<RecordBatch, String> {
        if self.batches.len() == 1 {
            return Ok(self.batches[0].clone());
        }
        arrow::compute::concat_batches(&self.schema, &self.batches)
            .map_err(|e| format!("Failed to merge batches for IPC write: {e}"))
    }

    /// Serializes all batches to an Arrow IPC file at `path`, and the name registry to a
    /// sibling file beside it (see [`names_sibling_path`]).
    pub fn save_ipc(&self, path: &Path) -> Result<(), String> {
        if self.batches.is_empty() {
            return Err(
                "Cannot save an empty ledger — use save_schema_ipc to persist just the schema"
                    .to_string(),
            );
        }

        ipc::write_file(path, &[self.merge_for_ipc()?], &self.schema)?;

        let names_path = names_sibling_path(path);
        std::fs::write(&names_path, self.names_to_ipc_bytes()?).map_err(|e| {
            format!(
                "Failed to write name registry '{}': {e}",
                names_path.display()
            )
        })?;

        Ok(())
    }

    /// Writes the ledger's schema to an Arrow IPC file with zero data batches.
    ///
    /// The file can be read back by [`Ledger::load_schema_ipc`] or by any language that
    /// speaks Arrow IPC (Python `pyarrow`, Java, Go, …).
    ///
    /// Intended use: bake the output file into a Docker image so a freshly started
    /// `soloc-server` can call [`Ledger::load_schema_ipc`] at startup and be ready to
    /// accept data without any prior knowledge of the schema at the call-site.
    pub fn save_schema_ipc(&self, path: &Path) -> Result<(), String> {
        ipc::write_file(path, &[], &self.schema)
    }

    /// Reads the Arrow schema from an IPC file and returns an empty [`Ledger`] configured
    /// with that schema.
    ///
    /// Pair with [`Ledger::save_schema_ipc`] for schema distribution (e.g. baking a
    /// schema file into a Docker image).
    pub fn load_schema_ipc(path: &Path, id_column: &str) -> Result<Self, String> {
        let schema = ipc::read_file_schema(path)?;
        Self::validate_schema(&schema, id_column)?;
        Ok(Self::from_parts(schema, Vec::new(), id_column))
    }

    /// Serializes the ledger's schema to an in-memory Arrow IPC buffer with zero data batches.
    ///
    /// The bytes are in exactly the same format as [`Ledger::save_schema_ipc`] produces.
    /// Useful for transmitting a schema over the network or embedding it in another format
    /// without writing a temporary file.
    pub fn schema_to_ipc_bytes(&self) -> Result<Vec<u8>, String> {
        ipc::write_bytes(&[], &self.schema)
    }

    /// Deserializes a schema from an in-memory Arrow IPC buffer and returns an empty
    /// [`Ledger`] configured with that schema.
    pub fn from_schema_ipc_bytes(bytes: &[u8], id_column: &str) -> Result<Self, String> {
        let schema = ipc::read_bytes_schema(bytes)?;
        Self::validate_schema(&schema, id_column)?;
        Ok(Self::from_parts(schema, Vec::new(), id_column))
    }

    /// Returns the maximum stored timestamp as a J2000-relative [`Duration`], or `None`
    /// if the ledger is empty or contains no parseable timestamps.
    fn latest_stored_duration(&self) -> Option<Duration> {
        let mut latest: Option<Duration> = None;
        for batch in &self.batches {
            let sts = StsColumns::try_new(batch).ok()?;
            for row in 0..batch.num_rows() {
                let (centuries, nanos) = sts.epoch_parts_at(row);
                let dur = Duration::from_parts(centuries, nanos);
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
    /// 2. For equal timestamps, source priority: `MEASURED` > `ESTIMATED` > `SIMULATED`.
    /// 3. For equal timestamps and equal priority, later insertion order wins.
    ///
    /// `id_filter`: if `Some`, only rows whose id-column value is in the set are included.
    ///   If this ledger has no `id_column`, an `id_filter` of `Some(_)` returns an empty batch.
    ///
    /// `not_before`: rows whose timestamp is strictly before this epoch are excluded.
    ///   When `None`, defaults to (latest stored timestamp. [`CURRENT_STATE_WINDOW_NS`]).
    pub fn current_state(
        &self,
        id_filter: Option<&[PrescribedId]>,
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
        let id_filter_set: Option<IdSet> = id_filter.map(|ids| ids.iter().copied().collect());

        // If caller asked for specific ids but we have no id column, return empty.
        if id_filter_set.is_some() && self.id_column.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
        }

        // row_key → (epoch_dur, priority, batch_idx, row_idx)
        let mut best: HashMap<RowKey, (Duration, u8, usize, usize)> = HashMap::new();

        for (batch_idx, batch) in self.batches.iter().enumerate() {
            // id column lookup — `None` if absent, wrong type, or null-bearing, in which case
            // the rows below fall back to being keyed by index.
            let eid_col_opt = if self.id_column.is_empty() {
                None
            } else {
                batch
                    .column_by_name(&self.id_column)
                    .and_then(|c| id_column_of(c, &self.id_column))
            };

            let Ok(sts) = StsColumns::try_new(batch) else {
                continue;
            };

            for row in 0..batch.num_rows() {
                // Derive a row key: the entity id if there is one, else the row index.
                let row_key = match eid_col_opt {
                    Some(col) => match id_at(col, row) {
                        Ok(id) => RowKey::Id(id),
                        // A row whose id will not decode cannot be grouped with anything.
                        // Falling back to `RowKey::Row` here would mix index keys into an
                        // id-keyed batch and let this row collide with an unrelated one.
                        Err(_) => continue,
                    },
                    None => RowKey::Row(row),
                };

                if let Some(ref filter) = id_filter_set
                    && !matches!(row_key, RowKey::Id(id) if filter.contains(&id))
                {
                    continue;
                }

                let (centuries, nanos) = sts.epoch_parts_at(row);
                let dur = Duration::from_parts(centuries, nanos);

                if let Some(c) = cutoff
                    && dur < c
                {
                    continue;
                }

                let priority = sts.estimate_at(row)?.priority();

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
            rows.push(apply_boolean_mask(batch, &mask.finish())?);
        }

        arrow::compute::concat_batches(&self.schema, &rows)
            .map_err(|e| format!("failed to concatenate current_state rows: {e}"))
    }

    /// Loads a ledger from an Arrow IPC file previously saved with [`Ledger::save_ipc`].
    ///
    /// Reads the schema from the IPC file and validates it against `sts_column` and `id_column`.
    pub fn load_ipc(path: &Path, id_column: &str) -> Result<Self, String> {
        let (schema, batches) = ipc::read_file(path)?;
        let mut ledger = Self::from_loaded(schema, batches, id_column, "IPC file")?;

        let names_path = names_sibling_path(path);
        if names_path.exists() {
            let bytes = std::fs::read(&names_path).map_err(|e| {
                format!(
                    "Failed to read name registry '{}': {e}",
                    names_path.display()
                )
            })?;
            ledger.merge_names_from_ipc_bytes(&bytes)?;
        }

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
        ipc::write_bytes(&[self.merge_for_ipc()?], &self.schema)
    }

    /// Deserializes a ledger from an in-memory Arrow IPC buffer.
    ///
    /// Reads the schema from the IPC bytes and validates it against `sts_column` and `id_column`.
    pub fn load_ipc_from_bytes(bytes: &[u8], id_column: &str) -> Result<Self, String> {
        let (schema, batches) = ipc::read_bytes(bytes)?;
        Self::from_loaded(schema, batches, id_column, "IPC bytes")
    }

    /// Assembles a ledger with no derived state built yet.
    fn from_parts(schema: SchemaRef, batches: Vec<RecordBatch>, id_column: &str) -> Self {
        Self {
            schema,
            batches,
            id_column: id_column.to_string(),
            transform_tree: TransformTree::new(),
            latest_pose: IdMap::default(),
            names: NameRegistry::new(),
        }
    }

    /// Validates a loaded schema, rejects an empty payload, and rebuilds the derived state.
    fn from_loaded(
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
        id_column: &str,
        source: &str,
    ) -> Result<Self, String> {
        Self::validate_schema(&schema, id_column)?;
        if batches.is_empty() {
            return Err(format!("{source} contained no record batches"));
        }
        let mut ledger = Self::from_parts(schema, batches, id_column);
        ledger.rebuild_derived_state()?;
        Ok(ledger)
    }
}

/// Where [`Ledger::save_ipc`] writes the name registry for a ledger saved at `path`.
pub fn names_sibling_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".names.arrow");
    PathBuf::from(name)
}

/// Type-checks an id column and rejects one carrying nulls, or `None` to skip the batch.
fn id_column_of<'a>(arr: &'a dyn Array, name: &str) -> Option<&'a FixedSizeBinaryArray> {
    let col = as_id_column(arr, name).ok()?;
    (col.null_count() == 0).then_some(col)
}

/// What [`Ledger::current_state`] groups rows by: the entity id when the ledger has an id
/// column, and the row index otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RowKey {
    Id(PrescribedId),
    Row(usize),
}

/// The identity and pose columns of one batch.
///
/// Shared by [`Ledger::resolve_frame_at`]'s scan and [`Ledger::update_pose_cache`] so the
/// two can never drift in how they read a row. Every accessor is indexed by row number.
struct PoseColumns<'a> {
    ids: &'a FixedSizeBinaryArray,
    sts: StsColumns<'a>,
}

impl<'a> PoseColumns<'a> {
    /// Locates every column needed to read a pose, or `None` if any is absent or has an
    /// unexpected type
    fn try_new(batch: &'a RecordBatch, id_column: &str) -> Option<Self> {
        Some(Self {
            ids: id_column_of(batch.column_by_name(id_column)?, id_column)?,
            sts: StsColumns::try_new(batch).ok()?,
        })
    }

    /// The entity id at `row`, or `None` if those 16 bytes are not a valid id.
    fn id_at(&self, row: usize) -> Option<PrescribedId> {
        id_at(self.ids, row).ok()
    }

    /// The timestamp at `row`, as an offset from the J2000 TAI epoch.
    fn epoch_at(&self, row: usize) -> Duration {
        let (centuries, nanos) = self.sts.epoch_parts_at(row);
        Duration::from_parts(centuries, nanos)
    }

    /// The `(parent_frame_id, isometry)` at `row`, with the translation converted to km.
    fn pose_at(&self, row: usize) -> Option<ResolvedFrame> {
        let unit = self.sts.units_at(row).ok()?;

        let [px, py, pz] = self.sts.position_at(row);
        let translation = Translation3::new(unit.to_km(px), unit.to_km(py), unit.to_km(pz));

        let [qw, qx, qy, qz] = self.sts.quaternion_at(row);
        let rotation = UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        let frame_id = self.sts.frame_at(row).ok()?;

        Some((frame_id, Isometry3::from_parts(translation, rotation)))
    }
}

/// Whether any `frame_id` in `batch` names another entity rather than an astronomical frame.
pub fn batch_references_entity_frames(batch: &RecordBatch) -> bool {
    let Ok(sts) = StsColumns::try_new(batch) else {
        return false;
    };
    (0..sts.frames().len()).any(|row| sts.frame_at(row).is_ok_and(|id| id.is_soloc()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{StringArray, StructArray, UInt8Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Fields, Schema};
    use hifitime::Duration;
    use spacetimestamp::ephemeris::j2000_tai;
    use spacetimestamp::query::SpatiotemporalFilter;
    use spacetimestamp::schema::STS_COLUMN;
    use spacetimestamp::schema::{SpaceTimestampBuilder, sts_schema};
    use spacetimestamp::vocabulary::{EstimateType, TimeScaleCode, Vocabulary};
    use std::sync::Arc;

    fn j2000() -> Epoch {
        j2000_tai()
    }

    /// An entity this module owns. `"demo"` is the authority.
    fn demo(name: &str) -> PrescribedId {
        PrescribedId::new("demo", name).expect("demo entity names are valid")
    }

    /// The test source id
    fn test_source() -> PrescribedId {
        PrescribedId::abstract_source("test", "src").expect("test source id is valid")
    }

    /// Removes both files a `save_ipc` produces including the rows and the name registry beside them.
    fn remove_saved_ledger(path: &Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(names_sibling_path(path)).ok();
    }

    /// Schema matching make_batch(), just a spacetimestamp struct
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
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            test_source(),
            EstimateType::MEASURED,
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

    /// Both transports, because they are separate entry points: the byte form is what the
    /// server hands over Flight, and the path form additionally writes the sibling names file.
    #[test]
    fn test_save_and_load_ipc_round_trips_through_both_transports() {
        let mut ledger = make_sts_ledger();
        ledger.append(make_batch([1.0, 2.0, 3.0], 0)).unwrap();
        ledger.append(make_batch([4.0, 5.0, 6.0], 1000)).unwrap();

        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let path = std::env::temp_dir().join("soloc_ledger_test.arrows");
        ledger.save_ipc(&path).unwrap();

        for loaded in [
            Ledger::load_ipc_from_bytes(&bytes, "").unwrap(),
            Ledger::load_ipc(&path, "").unwrap(),
        ] {
            // Rows are preserved; `merge_for_ipc` concatenates every batch into 1 on save.
            assert_eq!(loaded.len(), 1);
            let total_rows: usize = loaded.batches.iter().map(|b| b.num_rows()).sum();
            assert_eq!(total_rows, 2);
        }

        remove_saved_ledger(&path);
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
        remove_saved_ledger(&path);
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
    // entity frame chain resolution
    // -----------------------------------------------------------------------

    fn make_entity_batch(
        entity_id: PrescribedId,
        frame_id: PrescribedId,
        pos: [f64; 3],
        quat: [f64; 4],
        ns: u64,
    ) -> RecordBatch {
        use crate::schemas::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            entity_id,
            frame_id,
            LengthUnit::km,
            TimeScaleCode::TAI,
            test_source(),
            EstimateType::MEASURED,
            pos,
            quat,
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
    fn test_build_dynamic_frame_map_single_hop() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                demo("truck_A"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [100.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot_truck"),
                demo("truck_A"),
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let epoch = j2000();
        let map = ledger
            .build_dynamic_frame_map(&[demo("robot_truck")], epoch)
            .unwrap();

        let (root, iso) = map.get(&demo("robot_truck")).unwrap();
        assert_eq!(
            *root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );

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
                demo("facility"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [50.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot"),
                demo("facility"),
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let epoch = j2000();
        let map = ledger
            .build_dynamic_frame_map(&[demo("robot")], epoch)
            .unwrap();

        let (root, iso) = map.get(&demo("robot")).unwrap();
        assert_eq!(
            *root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );

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
            .build_dynamic_frame_map(&[demo("ghost")], epoch)
            .unwrap_err();
        assert!(
            err.contains(&demo("ghost").to_hyphenated()),
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
                demo("A"),
                demo("B"),
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let err = ledger
            .append(make_entity_batch(
                demo("B"),
                demo("A"),
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
            ledger.resolve_frame_at(demo("B"), j2000()).is_none(),
            "rejected batch must not populate the pose cache"
        );
    }

    /// A parent frame that names nothing real would leave the child dangling off nothing.
    #[test]
    fn test_append_rejects_typo_and_abstract_frames() {
        let err = PrescribedId::astronomical_from_name("IAU_MRAS").unwrap_err();
        assert!(
            err.contains("IAU_MRAS"),
            "mint should name the offending frame: {err}"
        );

        let mut ledger = make_entity_ledger();
        let provenance = PrescribedId::abstract_source("demo", "rover-log").unwrap();
        let err = ledger
            .append(make_entity_batch(
                demo("rover"),
                provenance,
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap_err();
        assert!(
            err.contains(&provenance.to_hyphenated()),
            "error should name the offending frame: {err}"
        );
        assert_eq!(ledger.len(), 0, "rejected batch must not be stored");
    }

    /// The predicate that decides whether `transform` needs a resolver at all. Getting it
    /// backwards is silent in both directions
    #[test]
    fn test_batch_references_entity_frames_reads_the_kind_nibble() {
        let astro_rooted = make_entity_batch(
            demo("rover"),
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );
        assert!(
            !batch_references_entity_frames(&astro_rooted),
            "an astronomical root needs no ledger resolution"
        );

        let entity_rooted = make_entity_batch(
            demo("sensor"),
            demo("rover"),
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );
        assert!(
            batch_references_entity_frames(&entity_rooted),
            "a frame naming another entity must be resolved through the ledger"
        );
    }

    /// Raw NAIF integer IDs are a legal parent and must survive the floating-frame check.
    #[test]
    fn test_append_accepts_naif_integer_parent() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                demo("lander"),
                PrescribedId::astronomical_from_name("Mars").unwrap(),
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

    /// Builds a ledger holding demo's `robot` → demo's `facility` → `IAU_EARTH`.
    fn make_two_hop_ledger() -> Ledger {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                demo("facility"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [50.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot"),
                demo("facility"),
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
    }

    /// Topology is derived data. A reloaded ledger has to
    /// rebuild it from the rows, or chain resolution silently stops working.
    #[test]
    fn test_ipc_round_trip_rebuilds_topology() {
        let ledger = make_two_hop_ledger();
        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let reloaded = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();

        let map = reloaded
            .build_dynamic_frame_map(&[demo("robot")], j2000())
            .unwrap();
        let (root, iso) = map.get(&demo("robot")).unwrap();
        assert_eq!(
            *root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
        assert!((iso.translation.vector.x - 55.0).abs() < 1e-9);
    }

    /// A subset ships only the subset's ids
    #[test]
    fn test_snapshot_subset_carries_only_the_requested_ids() {
        let mut ledger = make_entity_ledger();
        let mut b = crate::schemas::entity::EntityBuilder::new(3);
        for name in ["alpha", "bravo", "charlie"] {
            b.append_entity(
                demo(name),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                LengthUnit::km,
                TimeScaleCode::TAI,
                test_source(),
                EstimateType::MEASURED,
                [1.0, 0.0, 0.0],
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
        }
        ledger.append(b.flush()).unwrap();

        let subset = ledger.latest_snapshot(Some(&[demo("alpha")])).unwrap();
        assert_eq!(subset.num_rows(), 1);

        let ids = id_column_of(subset.column_by_name("entity_id").unwrap(), "entity_id").unwrap();
        assert_eq!(
            ids.value_data().len(),
            16,
            "one row must ship 16 bytes of identity, not the source batch's whole vocabulary"
        );
        for withheld in ["bravo", "charlie"] {
            assert!(
                !ids.value_data()
                    .windows(16)
                    .any(|w| w == demo(withheld).as_bytes()),
                "{withheld}'s id must not appear in a subset that did not ask for it"
            );
        }
    }

    /// The load path's regression test for the key-collision bug in topology Pass A/C.
    #[test]
    fn test_merged_reload_does_not_re_emit_topology_events() {
        let mut ledger = make_entity_ledger();
        // robot starts under facility, then re-parents to IAU_EARTH: two events, in two
        // separate batches so the merge has something to concatenate.
        ledger
            .append(make_entity_batch(
                demo("robot"),
                demo("facility"),
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [6.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                1_000,
            ))
            .unwrap();
        assert_eq!(ledger.export_topology().unwrap().num_rows(), 2);

        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let reloaded = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();

        assert_eq!(
            reloaded.export_topology().unwrap().num_rows(),
            2,
            "reloading a merged ledger must replay the same events, not invent a third"
        );
        assert_eq!(
            reloaded.transform_tree.current_parent(demo("robot")),
            Some(PrescribedId::astronomical_from_name("IAU_EARTH").unwrap())
        );
    }

    #[test]
    fn test_ipc_round_trip_preserves_pose_cache() {
        let ledger = make_two_hop_ledger();
        let before = ledger.resolve_frame_at(demo("robot"), j2000()).unwrap();

        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let mut reloaded = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();

        let cached = reloaded
            .resolve_frame_at(demo("robot"), j2000())
            .expect("the reloaded pose cache should answer this");
        assert_eq!(cached.0, before.0);
        assert!((cached.1.translation.vector.x - before.1.translation.vector.x).abs() < 1e-9);

        // Drop the cache and make the scan answer instead: the fast path and the fallback
        // read the same columns, so they must not be able to disagree.
        reloaded.clear_pose_cache();
        let scanned = reloaded.resolve_frame_at(demo("robot"), j2000()).unwrap();
        assert_eq!(scanned.0, cached.0);
        assert!((scanned.1.translation.vector.x - cached.1.translation.vector.x).abs() < 1e-9);
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

        let err = peer
            .build_dynamic_frame_map(&[demo("robot")], j2000())
            .unwrap_err();
        assert!(
            err.contains("not found in ledger"),
            "expected a missing-pose error, got: {err}"
        );
    }

    /// The registry is a separate artifact from the rows, so a saved ledger has to carry it
    /// beside them or every reloaded id renders as hex.
    #[test]
    fn test_ipc_round_trip_preserves_names() {
        let mut ledger = make_two_hop_ledger();
        ledger
            .register_name(demo("robot"), "demo", "robot")
            .unwrap();

        let path = std::env::temp_dir().join("soloc_names_round_trip.arrows");
        ledger.save_ipc(&path).unwrap();
        assert!(
            names_sibling_path(&path).exists(),
            "save_ipc must write the registry beside the rows"
        );

        let loaded = Ledger::load_ipc(&path, "entity_id").unwrap();
        assert_eq!(
            loaded.names().name_of(&demo("robot")),
            Some(("demo", "robot"))
        );

        remove_saved_ledger(&path);
    }

    /// Losing the registry degrades display and nothing else.
    #[test]
    fn test_load_ipc_without_registry_degrades_display_only() {
        let mut ledger = make_two_hop_ledger();
        ledger
            .register_name(demo("robot"), "demo", "robot")
            .unwrap();

        let path = std::env::temp_dir().join("soloc_names_absent.arrows");
        ledger.save_ipc(&path).unwrap();
        std::fs::remove_file(names_sibling_path(&path)).unwrap();

        let loaded =
            Ledger::load_ipc(&path, "entity_id").expect("an absent registry is not an error");
        assert!(loaded.names().is_empty());

        // The chain still resolves to the same root and the same offset.
        let (root, iso) = loaded.resolve_to_root(demo("robot"), j2000()).unwrap();
        assert_eq!(
            root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
        assert!((iso.translation.vector.x - 55.0).abs() < 1e-9);

        remove_saved_ledger(&path);
    }

    /// A peer's claimed binding is re-derived from the id, never trusted.
    #[test]
    fn test_merge_names_rejects_a_forged_binding() {
        let mut source = make_entity_ledger();
        source
            .register_name(demo("robot"), "demo", "robot")
            .unwrap();
        let exported = source.export_names().unwrap();

        // The same id, relabelled — exactly what a hostile or simply buggy peer would send.
        let forged = RecordBatch::try_new(
            exported.schema(),
            vec![
                exported.column(0).clone(),
                exported.column(1).clone(),
                Arc::new(StringArray::from(vec!["impostor"])),
                exported.column(3).clone(),
            ],
        )
        .unwrap();

        let buf = ipc::write_bytes(&[forged], &registry_schema()).unwrap();

        let mut peer = make_entity_ledger();
        let err = peer.merge_names_from_ipc_bytes(&buf).unwrap_err();
        assert!(
            err.contains("mints"),
            "error should say the claimed name hashes to something else: {err}"
        );
        assert!(
            peer.names().is_empty(),
            "a rejected registry must leave the peer's names untouched"
        );
    }

    #[test]
    fn test_merge_names_from_ipc_bytes_rejects_a_multi_batch_payload_atomically() {
        let mut source = make_entity_ledger();
        source
            .register_name(demo("robot"), "demo", "robot")
            .unwrap();
        let good = source.export_names().unwrap();

        let forged = RecordBatch::try_new(
            good.schema(),
            vec![
                good.column(0).clone(),
                good.column(1).clone(),
                Arc::new(StringArray::from(vec!["impostor"])),
                good.column(3).clone(),
            ],
        )
        .unwrap();

        // Two batches in one IPC file, written directly rather than through `merge_for_ipc`,
        // so the forged batch stays a separate batch the reader must reject on its own.
        let buf = ipc::write_bytes(&[good, forged], &registry_schema()).unwrap();

        let mut peer = make_entity_ledger();
        let err = peer.merge_names_from_ipc_bytes(&buf).unwrap_err();
        assert!(
            peer.names().is_empty(),
            "the honest leading batch must not survive the payload's rejection"
        );
        assert!(
            err.contains("registry batch 1"),
            "error should locate the bad batch: {err}"
        );
    }

    /// Two operators observing what they each call a `truck` and a `robot` produce two
    /// distinct sets of ids with nothing agreed in advance, and exchanging topology plus
    /// names leaves each side's own identities untouched.
    #[test]
    fn test_federation_keeps_distinct_ids_for_the_same_common_names() {
        fn under(authority: &str, name: &str) -> PrescribedId {
            PrescribedId::new(authority, name).expect("test authorities and names are valid")
        }
        fn rig(authority: &str, truck_x: f64, robot_x: f64) -> Ledger {
            let truck = under(authority, "truck");
            let robot = under(authority, "robot");
            let mut ledger = make_entity_ledger();
            let earth = PrescribedId::astronomical_from_name("IAU_EARTH").unwrap();
            ledger.register_name(truck, authority, "truck").unwrap();
            ledger.register_name(robot, authority, "robot").unwrap();
            ledger
                .append(make_entity_batch(
                    truck,
                    earth,
                    [truck_x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    0,
                ))
                .unwrap();
            ledger
                .append(make_entity_batch(
                    robot,
                    truck,
                    [robot_x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    0,
                ))
                .unwrap();
            ledger
        }

        let alpha = rig("alpha", 50.0, 5.0);
        let mut beta = rig("beta", 7.0, 2.0);

        // The names collide; the identities do not.
        assert_ne!(under("alpha", "truck"), under("beta", "truck"));
        assert_ne!(under("alpha", "robot"), under("beta", "robot"));

        let topology = alpha.export_topology().unwrap();
        let names = alpha.names_to_ipc_bytes().unwrap();
        assert_eq!(beta.merge_topology(&topology).unwrap(), 2);
        // `IAU_EARTH` is already known to beta: both peers minted the same id from the same
        // name without coordinating, which is the whole point. Only alpha's two are new.
        assert_eq!(beta.merge_names_from_ipc_bytes(&names).unwrap(), 2);

        // Beta's own robot still resolves to beta's own pose, nothing was remapped.
        let (root, iso) = beta
            .resolve_to_root(under("beta", "robot"), j2000())
            .unwrap();
        assert_eq!(
            root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
        assert!(
            (iso.translation.vector.x - 9.0).abs() < 1e-9,
            "expected beta's own 7 + 2, got {}",
            iso.translation.vector.x
        );

        // Alpha's robot is structurally known to beta now, but beta holds no pose for it.
        let err = beta
            .build_dynamic_frame_map(&[under("alpha", "robot")], j2000())
            .unwrap_err();
        assert!(
            err.contains("not found in ledger"),
            "expected a missing-pose error, got: {err}"
        );

        // Both trucks are readable side by side, each under its own authority.
        assert_eq!(
            beta.names().name_of(&under("alpha", "truck")),
            Some(("alpha", "truck"))
        );
        assert_eq!(
            beta.names().name_of(&under("beta", "truck")),
            Some(("beta", "truck"))
        );

        // Re-importing the same export changes nothing.
        let log_len_before = beta.export_topology().unwrap().num_rows();
        let names_before = beta.names().len();
        assert_eq!(beta.merge_topology(&topology).unwrap(), 0);
        assert_eq!(beta.merge_names_from_ipc_bytes(&names).unwrap(), 0);
        assert_eq!(beta.export_topology().unwrap().num_rows(), log_len_before);
        assert_eq!(beta.names().len(), names_before);
    }

    /// End-to-end through the public `transform` API: topology derived from appended rows →
    /// pose cache → resolver → `transform_batch`.
    ///
    /// Chain: `demo:sensor` @ [1,0,0] in `demo:robot` @ [5,0,0] in `demo:facility` @ [50,0,0]
    /// in Earth. Reprojected into Earth the offsets sum to 56 km.
    #[test]
    fn test_transform_resolves_derived_chain_end_to_end() {
        let mut ledger = make_entity_ledger();
        // Rows carry ids; an astronomical root resolves straight from the id's embedded frame
        // pair, so no registration is needed.
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        ledger
            .append(make_entity_batch(
                demo("facility"),
                earth,
                [50.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot"),
                demo("facility"),
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        // A reading expressed in the robot's frame
        let observation = make_entity_batch(
            demo("sensor"),
            demo("robot"),
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );

        let result = ledger
            .transform(&observation, "Earth", LengthUnit::km, &Almanac::default())
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

    #[test]
    fn test_astronomical_frame_resolves_identically_regardless_of_batch_composition() {
        let mut ledger = make_entity_ledger();
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let icrf = PrescribedId::astronomical_from_name("ICRF").unwrap();

        // Earth has rows of its own, displaced far from where the almanac puts it, and an
        // entity is parented to it so a mixed batch can reference an entity frame.
        ledger
            .append(make_entity_batch(
                earth,
                icrf,
                [1_000.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("robot"),
                earth,
                [5.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let sat = make_entity_batch(demo("sat"), earth, [7.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0);
        let sensor = make_entity_batch(
            demo("sensor"),
            demo("robot"),
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );
        let mixed = arrow::compute::concat_batches(&sat.schema(), [&sat, &sensor]).unwrap();

        let x_of = |batch: &RecordBatch, row: usize| -> f64 {
            let result = ledger
                .transform(batch, "Earth", LengthUnit::km, &Almanac::default())
                .expect("an astronomical frame needs no kernels to reach itself");
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
            pos.values()
                .as_any()
                .downcast_ref::<arrow::array::Float64Array>()
                .unwrap()
                .value(row * 3)
        };

        assert!((x_of(&sat, 0) - 7.0).abs() < 1e-9, "fast path moved sat");
        assert!(
            (x_of(&mixed, 0) - 7.0).abs() < 1e-9,
            "resolver path moved sat"
        );
        // The entity frame still resolves through the ledger: 5 + 1 = 6 km.
        assert!((x_of(&mixed, 1) - 6.0).abs() < 1e-9, "sensor chain broke");
    }

    /// A batch with no entity frames must take `transform`'s no-resolver fast path and
    /// still come back correct.
    #[test]
    fn test_transform_passthrough_batch_needs_no_topology() {
        let ledger = make_entity_ledger();
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let observation = make_entity_batch(
            demo("probe"),
            earth,
            [7.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
        );

        let result = ledger
            .transform(&observation, "Earth", LengthUnit::km, &Almanac::default())
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
                demo("hangar"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [10.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        // Starts parented to the hangar...
        ledger
            .append(make_entity_batch(
                demo("drone"),
                demo("hangar"),
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        // ...then is re-parented straight to IAU_EARTH once airborne.
        ledger
            .append(make_entity_batch(
                demo("drone"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                [500.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                1_000,
            ))
            .unwrap();

        // Before the re-parenting: through the hangar, so 10 + 1.
        let (root, iso) = ledger.resolve_to_root(demo("drone"), j2000()).unwrap();
        assert_eq!(
            root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
        assert!((iso.translation.vector.x - 11.0).abs() < 1e-9);

        // After: direct, so just 500.
        let after = j2000() + Duration::from_parts(0, 1_000);
        let (root, iso) = ledger.resolve_to_root(demo("drone"), after).unwrap();
        assert_eq!(
            root,
            PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
        );
        assert!((iso.translation.vector.x - 500.0).abs() < 1e-9);
    }

    #[test]
    fn test_resolve_to_root_unknown_entity_returns_none() {
        let ledger = make_two_hop_ledger();
        assert!(ledger.resolve_to_root(demo("ghost"), j2000()).is_none());
    }

    // -----------------------------------------------------------------------
    // pose cache tests
    // -----------------------------------------------------------------------

    /// Checks that `resolve_frame_at`'s cache fast path and its scan fallback give the same
    /// answer, including on the epoch-tie rule, where both must keep the *first* row seen.
    fn assert_cache_agrees_with_scan(ledger: &Ledger, entity_id: PrescribedId, queries: &[Epoch]) {
        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let mut scanned = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();
        scanned.clear_pose_cache();

        for &epoch in queries {
            let cached = ledger.resolve_frame_at(entity_id, epoch);
            let scanned = scanned.resolve_frame_at(entity_id, epoch);
            assert_eq!(
                cached.as_ref().map(|(f, i)| (f, i.translation.vector)),
                scanned.as_ref().map(|(f, i)| (f, i.translation.vector)),
                "cache and scan disagree for {entity_id} at {epoch}"
            );
        }
    }

    #[test]
    fn test_pose_cache_returns_latest_pose() {
        let mut ledger = make_entity_ledger();
        for (x, ns) in [(1.0, 0), (2.0, 1_000), (3.0, 2_000)] {
            ledger
                .append(make_entity_batch(
                    demo("A"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    ns,
                ))
                .unwrap();
        }

        let at_latest = j2000() + Duration::from_parts(0, 2_000);
        let (frame, iso) = ledger.resolve_frame_at(demo("A"), at_latest).unwrap();
        assert_eq!(frame, PrescribedId::astronomical_from_name("ICRF").unwrap());
        assert_eq!(iso.translation.vector.x, 3.0);

        // Well past the last row — still the last row, served from the cache.
        let far_future = j2000() + Duration::from_parts(0, 999_999);
        let (_, iso) = ledger.resolve_frame_at(demo("A"), far_future).unwrap();
        assert_eq!(iso.translation.vector.x, 3.0);

        assert_cache_agrees_with_scan(&ledger, demo("A"), &[at_latest, far_future]);
    }

    #[test]
    fn test_resolve_frame_at_historical_query_bypasses_cache() {
        let mut ledger = make_entity_ledger();
        for (x, ns) in [(1.0, 0), (2.0, 1_000), (3.0, 2_000)] {
            ledger
                .append(make_entity_batch(
                    demo("A"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    ns,
                ))
                .unwrap();
        }

        // Between rows: the cached epoch (2000) is after the query, so this must fall back
        // to the scan and find the row at 1000 rather than returning the cached pose.
        let midpoint = j2000() + Duration::from_parts(0, 1_500);
        let (_, iso) = ledger.resolve_frame_at(demo("A"), midpoint).unwrap();
        assert_eq!(iso.translation.vector.x, 2.0);

        // Before every row: no answer exists.
        assert!(
            ledger
                .resolve_frame_at(demo("A"), j2000() - Duration::from_parts(0, 1))
                .is_none()
        );

        assert_cache_agrees_with_scan(
            &ledger,
            demo("A"),
            &[j2000(), midpoint, j2000() + Duration::from_parts(0, 1_000)],
        );
    }

    /// Appending an older batch after a newer one must not move the cache backwards.
    #[test]
    fn test_pose_cache_survives_backfilled_batch() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                demo("A"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                [9.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                5_000,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("A"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();

        let latest = j2000() + Duration::from_parts(0, 5_000);
        let (_, iso) = ledger.resolve_frame_at(demo("A"), latest).unwrap();
        assert_eq!(
            iso.translation.vector.x, 9.0,
            "backfilled older row must not overwrite the cached newer pose"
        );

        // The backfilled row is still findable at its own epoch, via the scan path.
        let (_, iso) = ledger.resolve_frame_at(demo("A"), j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 1.0);

        assert_cache_agrees_with_scan(&ledger, demo("A"), &[j2000(), latest]);
    }

    /// Two rows at the same epoch: the first one appended wins, in both code paths.
    #[test]
    fn test_pose_cache_epoch_tie_keeps_first_seen() {
        let mut ledger = make_entity_ledger();
        for x in [1.0, 2.0] {
            ledger
                .append(make_entity_batch(
                    demo("A"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    [x, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    0,
                ))
                .unwrap();
        }

        let (_, iso) = ledger.resolve_frame_at(demo("A"), j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 1.0);

        assert_cache_agrees_with_scan(&ledger, demo("A"), &[j2000()]);
    }

    /// Poses are cached in km regardless of the units the row was written in, matching
    /// what the scan path returns.
    #[test]
    fn test_pose_cache_normalises_units_to_km() {
        use crate::schemas::entity::EntityBuilder;
        let mut ledger = make_entity_ledger();
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            demo("A"),
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::m,
            TimeScaleCode::TAI,
            test_source(),
            EstimateType::MEASURED,
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

        let (_, iso) = ledger.resolve_frame_at(demo("A"), j2000()).unwrap();
        assert_eq!(iso.translation.vector.x, 2.0);

        assert_cache_agrees_with_scan(&ledger, demo("A"), &[j2000()]);
    }

    /// Independent entities each keep their own cache entry.
    #[test]
    fn test_pose_cache_is_per_entity() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(
                demo("A"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch(
                demo("B"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                [7.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                1_000,
            ))
            .unwrap();

        let at_b = j2000() + Duration::from_parts(0, 1_000);
        assert_eq!(
            ledger
                .resolve_frame_at(demo("A"), at_b)
                .unwrap()
                .1
                .translation
                .vector
                .x,
            1.0
        );
        assert_eq!(
            ledger
                .resolve_frame_at(demo("B"), at_b)
                .unwrap()
                .1
                .translation
                .vector
                .x,
            7.0
        );
        assert!(ledger.resolve_frame_at(demo("missing"), at_b).is_none());

        assert_cache_agrees_with_scan(&ledger, demo("A"), &[j2000(), at_b]);
        assert_cache_agrees_with_scan(&ledger, demo("B"), &[j2000(), at_b]);
    }

    // -----------------------------------------------------------------------
    // current_state tests
    // -----------------------------------------------------------------------

    fn make_entity_batch_et(
        entity_id: PrescribedId,
        pos: [f64; 3],
        ns: u64,
        estimate_type: EstimateType,
    ) -> RecordBatch {
        use crate::schemas::entity::EntityBuilder;
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            entity_id,
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            test_source(),
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
                demo("sat"),
                [1.0, 0.0, 0.0],
                1000,
                EstimateType::MEASURED,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                demo("sat"),
                [2.0, 0.0, 0.0],
                5000,
                EstimateType::MEASURED,
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
                demo("sat"),
                [1.0, 0.0, 0.0],
                3000,
                EstimateType::MEASURED,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                demo("sat"),
                [9.0, 0.0, 0.0],
                3000,
                EstimateType::SIMULATED,
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
            .downcast_ref::<UInt8Array>()
            .unwrap();
        let et = EstimateType::from_code(et_col.value(0)).unwrap();
        assert_eq!(et, EstimateType::MEASURED);
    }

    #[test]
    fn test_current_state_staleness_cutoff() {
        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch_et(
                demo("sat"),
                [1.0, 0.0, 0.0],
                100,
                EstimateType::MEASURED,
            ))
            .unwrap();
        ledger
            .append(make_entity_batch_et(
                demo("sat"),
                [2.0, 0.0, 0.0],
                2000,
                EstimateType::MEASURED,
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
                demo("A"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                LengthUnit::km,
                TimeScaleCode::TAI,
                test_source(),
                EstimateType::MEASURED,
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
                demo("B"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                LengthUnit::km,
                TimeScaleCode::TAI,
                test_source(),
                EstimateType::MEASURED,
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
                demo("C"),
                [3.0, 0.0, 0.0],
                200,
                EstimateType::SIMULATED,
            ))
            .unwrap();

        let result = ledger.current_state(None, None).unwrap();
        assert_eq!(result.num_rows(), 3, "expected one row per entity");

        let eid_col = id_column_of(result.column_by_name("entity_id").unwrap(), "entity_id")
            .expect("entity_id is a non-null FixedSizeBinary(16) column");
        let seen: IdSet = (0..result.num_rows())
            .map(|row| id_at(eid_col, row).unwrap())
            .collect();
        assert!(seen.contains(&demo("A")));
        assert!(seen.contains(&demo("B")));
        assert!(seen.contains(&demo("C")));
    }
}
