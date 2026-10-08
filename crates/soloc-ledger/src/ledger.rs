//! Soloc Ledger: Append-only RecordBatch Store for Soloc Schemas.
//!
//! The [`Ledger`] accumulates Arrow [`RecordBatch`]es or validated Soloc Schemas, and never
//! overwrites existing data. Measured observations (from telescopes, sensors, manual input)
//! and simulation outputs (`estimate_type = SIMULATED`) coexist in the same store
//! and are distinguished by their `estimate_type` field.
//!
//! Batches are held as bounded segments. Under a memory limit
//! ([`Ledger::set_memory_limit`]) the oldest segments are evicted, keeping each entity's
//! latest row, so the ledger holds a rolling window of recent history.

use arrow::array::{Array, ArrayData, BooleanBuilder, FixedSizeBinaryArray, UInt64Array};
use arrow::datatypes::{Field, SchemaRef};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};
use nalgebra::{Isometry3, Quaternion, Translation3, UnitQuaternion};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anise::prelude::Almanac;
use spacetimestamp::ephemeris::{epoch_from_parts, j2000_tai};
use spacetimestamp::identity::{
    IdMap, IdSet, NameRegistry, PrescribedId, as_id_column, id_at, id_type, registry_schema,
};
use spacetimestamp::ipc;
use spacetimestamp::query::{SpatiotemporalFilter, apply_boolean_mask, filter_batch};
use spacetimestamp::schema::StsColumns;
use spacetimestamp::topology::TransformTree;
use spacetimestamp::transforms::{ResolvedFrame, normalize_batch_to_tai, transform_batch};
use spacetimestamp::validation::{validate_spacetimestamp_batch, validate_sts_schema};
use spacetimestamp::vocabulary::{LengthUnit, TimeScaleCode};

use spacetimestamp::schemas::SpaceTimestampSchema;

/// Seal the unsealed tail into one segment when it holds more batches than this, to keep
/// query latency bounded.
///
/// Benchmarks show ~11 µs fixed overhead per batch. At 50 batches of ≥1000 rows each,
/// time-filter queries stay under ~1 ms. Beyond this threshold, merging pays off.
const SEGMENT_THRESHOLD: usize = 50;

/// The largest segment the ledger holds with no memory limit: the tail seals when it holds
/// more bytes than this, so a few large appends still produce bounded segments. Under a limit
/// the target can be smaller (see [`Ledger::segment_target`]).
const SEGMENT_TARGET_BYTES: usize = 256 * 1024 * 1024;

/// Under a memory limit, segments are kept to at most this fraction of it (see
/// [`Ledger::segment_target`]), so eviction never has to drop more than an eighth of the
/// budget at once.
const SEGMENTS_PER_LIMIT: usize = 8;

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
/// use soloc_ledger::schemas::entity::entity_schema;
/// use soloc_ledger::ledger::Ledger;
///
/// let ledger = Ledger::new(&entity_schema(), "entity_id").unwrap();
/// ```
#[derive(Debug)]
pub struct Ledger {
    /// The Arrow schema this ledger was created with. Stored so it is always
    /// available even when the ledger is empty (no batches yet).
    schema: SchemaRef,
    /// Rows kept past eviction because each is still its entity's latest. First in
    /// iteration order (see [`Ledger::all_segments`]).
    pinned: Option<Segment>,
    /// Sealed segments, oldest first. A sealed segment is never copied again.
    segments: Vec<Segment>,
    /// One segment per append since the last seal, oldest first.
    tail: Vec<Segment>,
    /// Sum of [`Segment::bytes`] over `pinned`, `segments` and `tail`.
    resident_bytes: usize,
    /// Byte budget for `resident_bytes`, enforced by eviction. `None` keeps every row.
    memory_limit: Option<usize>,
    /// Set once the over-budget warning has been logged, so it is not repeated on every
    /// append. Cleared when eviction brings the ledger back within budget.
    over_budget_warned: bool,
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

/// One immutable stored batch, with its epoch range and size measured once.
#[derive(Debug)]
struct Segment {
    batch: RecordBatch,
    /// Earliest row epoch, as an offset from J2000 TAI. For an empty batch it is greater than
    /// `max_epoch`, so every range check skips it.
    min_epoch: Duration,
    /// Latest row epoch, as an offset from J2000 TAI.
    max_epoch: Duration,
    /// [`allocated_bytes`] of `batch`.
    bytes: usize,
}

impl Segment {
    /// Measures `batch`. Epochs honour each row's `timescale_id`, so the range is right even
    /// for a loaded batch that was never normalised to TAI.
    fn new(batch: RecordBatch) -> Result<Self, String> {
        let (mut min_epoch, mut max_epoch) = (Duration::MAX, Duration::MIN);
        let sts = StsColumns::try_new(&batch)?;
        for row in 0..batch.num_rows() {
            let (centuries, nanos) = sts.epoch_parts_at(row);
            let epoch = match sts.timescale_at(row)? {
                TimeScaleCode::TAI => Duration::from_parts(centuries, nanos),
                ts => epoch_from_parts(centuries, nanos, ts.into()) - j2000_tai(),
            };
            min_epoch = min_epoch.min(epoch);
            max_epoch = max_epoch.max(epoch);
        }
        let bytes = allocated_bytes(&batch);
        Ok(Self {
            batch,
            min_epoch,
            max_epoch,
            bytes,
        })
    }

    /// Whether any row may fall in the inclusive range `[start, end]`.
    fn overlaps(&self, start: Duration, end: Duration) -> bool {
        self.min_epoch <= end && self.max_epoch >= start
    }
}

/// Every allocation `batch`'s buffers live in: its start address → its capacity in bytes.
fn allocations(batch: &RecordBatch) -> HashMap<*const u8, usize> {
    let mut found = HashMap::new();
    let mut pending: Vec<ArrayData> = batch.columns().iter().map(|c| c.to_data()).collect();
    while let Some(data) = pending.pop() {
        let nulls = data.nulls().map(|n| n.buffer());
        for buffer in data.buffers().iter().chain(nulls) {
            found.insert(buffer.data_ptr().as_ptr().cast_const(), buffer.capacity());
        }
        pending.extend(data.child_data().iter().cloned());
    }
    found
}

/// Bytes held by `batch`'s buffers, counting each underlying allocation once.
///
/// Arrow's `get_array_memory_size` charges a shared allocation to every array that slices
/// it. A batch decoded from IPC or Flight keeps all its arrays in one message body, so that
/// count comes out many times what the batch actually holds.
fn allocated_bytes(batch: &RecordBatch) -> usize {
    allocations(batch).values().sum()
}

/// Whether `bytes` is close enough to `target` to leave unsplit: at most an eighth over.
///
/// The slack stops a chunk that was cut to size from being split again when it is
/// re-measured a few bytes larger (buffer padding, or the IPC body it is reloaded into).
fn within_target(bytes: usize, target: usize) -> bool {
    bytes <= target + target / 8
}

/// `batch` as segments of about `max_bytes` each: one uncopied segment when it is
/// [`within_target`], else row-range chunks in order, cut to `max_bytes`. A chunk can exceed
/// its share by its buffers' 64-byte alignment padding.
///
/// Each chunk is copied into its own buffers, so evicting one frees its memory. A zero-copy
/// slice would keep the whole original allocation alive, and so would a single-input
/// `concat`, which arrow returns as a slice.
fn split_oversized(batch: &RecordBatch, max_bytes: usize) -> Result<Vec<Segment>, String> {
    let (bytes, rows) = (allocated_bytes(batch), batch.num_rows());
    if within_target(bytes, max_bytes) || rows < 2 {
        return Ok(vec![Segment::new(batch.clone())?]);
    }
    // Rounded down, so a chunk's share of the bytes stays within `max_bytes`.
    let chunk_rows = (rows * max_bytes / bytes).max(1);
    (0..rows)
        .step_by(chunk_rows)
        .map(|start| {
            let end = (start + chunk_rows).min(rows) as u64;
            let indices = UInt64Array::from_iter_values(start as u64..end);
            let chunk = arrow::compute::take_record_batch(batch, &indices)
                .map_err(|e| format!("failed to split a batch: {e}"))?;
            Segment::new(chunk)
        })
        .collect()
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
        Ok(Self::from_parts(schema.clone(), id_column))
    }

    /// Creates an empty ledger from a [`SpaceTimestampSchema`] implementor.
    ///
    /// This is the preferred constructor when working with a known schema type:
    ///
    /// ```
    /// use soloc_ledger::schemas::entity::EntitySchema;
    /// use soloc_ledger::ledger::Ledger;
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
        let batch = conform_to_schema(batch, &self.schema)?;
        validate_spacetimestamp_batch(&batch)?;
        // Measured before anything is mutated, so a failure here leaves the ledger unchanged.
        let segment = Segment::new(normalize_batch_to_tai(&batch)?)?;

        // Topology is derived from the *normalised* rows so every epoch compared is on the
        // TAI scale. A batch that would introduce a cycle, or that names a frame which
        // cannot exist, is rejected as a whole; `ingest_batch` stages its edges internally,
        // so a rejected batch leaves the tree untouched and nothing is pushed below.
        let outcome = self
            .transform_tree
            .ingest_batch(&segment.batch, &self.id_column)?;
        self.update_pose_cache(&segment.batch, outcome.latest_rows);

        self.push_segment(segment);
        Ok(())
    }

    /// Rebuilds `transform_tree` and `latest_pose` from the batches already in `self`.
    ///
    /// Errors if the stored data contains a cycle, which a ledger built through `append`
    /// cannot produce; a file that trips this was written by something that bypassed it.
    fn rebuild_derived_state(&mut self) -> Result<(), String> {
        if self.id_column.is_empty() {
            return Ok(());
        }
        // Cloned (cheap, `Arc`s) so the ingest below can mutate self.
        let batches: Vec<RecordBatch> = self.all_batches().cloned().collect();
        for batch in &batches {
            let outcome = self.transform_tree.ingest_batch(batch, &self.id_column)?;
            self.update_pose_cache(batch, outcome.latest_rows);
        }
        Ok(())
    }

    /// Every stored segment in insertion order: pinned, then sealed, then tail.
    fn all_segments(&self) -> impl Iterator<Item = &Segment> {
        self.pinned.iter().chain(&self.segments).chain(&self.tail)
    }

    /// Every stored batch, in the order of [`Ledger::all_segments`].
    fn all_batches(&self) -> impl Iterator<Item = &RecordBatch> {
        self.all_segments().map(|s| &s.batch)
    }

    /// The stored batches that may hold rows passing `filter`'s time range.
    fn batches_for<'a>(
        &'a self,
        filter: &SpatiotemporalFilter,
    ) -> impl Iterator<Item = &'a RecordBatch> + 'a {
        let range = filter
            .time_range
            .map(|(start, end)| (start - j2000_tai(), end - j2000_tai()));
        self.all_segments()
            .filter(move |s| range.is_none_or(|(start, end)| s.overlaps(start, end)))
            .map(|s| &s.batch)
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

    /// Adds `segment` to the tail, seals the tail if it has grown past a threshold, then
    /// evicts down to the memory limit.
    fn push_segment(&mut self, segment: Segment) {
        self.resident_bytes += segment.bytes;
        self.tail.push(segment);
        self.seal_tail_if_needed();
        self.enforce_memory_limit();
    }

    /// Caps [`Ledger::resident_bytes`] at `limit` by evicting the oldest data, or lifts the
    /// cap with `None`. Applies immediately.
    ///
    /// Eviction drops whole sealed segments, oldest `max_epoch` first, and evicted rows are
    /// gone. Each entity's latest row is kept ("pinned"), so `current_state` and
    /// present-epoch transforms still see every entity, and the topology log is trimmed to
    /// the window that remains. The unsealed tail is never evicted, so the ledger can stay
    /// over budget by up to the tail plus the pinned rows.
    ///
    /// The limit bounds [`Ledger::resident_bytes`], not the process: decode buffers and
    /// memory the allocator keeps after frees come on top of it.
    ///
    /// Segments are kept to about an eighth of the limit (at most 256 MiB). Sealed segments
    /// well over that (e.g. a file saved before segments existed) are first split into copied
    /// chunks, one segment at a time, so eviction can drop them a piece at a time.
    ///
    /// Errors for a ledger with no id column, which has no entities whose rows to pin.
    pub fn set_memory_limit(&mut self, limit: Option<usize>) -> Result<(), String> {
        if limit.is_some() && self.id_column.is_empty() {
            return Err(
                "a memory limit needs an id column: without one, no row is any entity's latest \
                 and eviction could drop everything"
                    .to_string(),
            );
        }
        self.memory_limit = limit;
        self.over_budget_warned = false;
        self.split_sealed_segments();
        self.enforce_memory_limit();
        Ok(())
    }

    /// The largest segment the ledger should hold: [`SEGMENT_TARGET_BYTES`], or
    /// 1/[`SEGMENTS_PER_LIMIT`] of the memory limit when that is smaller.
    fn segment_target(&self) -> usize {
        self.memory_limit.map_or(SEGMENT_TARGET_BYTES, |limit| {
            (limit / SEGMENTS_PER_LIMIT).clamp(1, SEGMENT_TARGET_BYTES)
        })
    }

    /// Splits every sealed segment not [`within_target`] of [`Ledger::segment_target`], in
    /// place and in order. A segment that fails to split is kept whole (with one warning), so the ledger
    /// is never left part-way.
    fn split_sealed_segments(&mut self) {
        let target = self.segment_target();
        for segment in std::mem::take(&mut self.segments) {
            if within_target(segment.bytes, target) {
                self.segments.push(segment);
                continue;
            }
            match split_oversized(&segment.batch, target) {
                Ok(chunks) => {
                    let chunk_bytes: usize = chunks.iter().map(|c| c.bytes).sum();
                    self.resident_bytes = self.resident_bytes - segment.bytes + chunk_bytes;
                    self.segments.extend(chunks);
                }
                Err(e) => {
                    log::warn!("ledger kept a {}-byte segment whole: {e}", segment.bytes);
                    self.segments.push(segment);
                }
            }
        }
    }

    /// Evicts sealed segments until within the memory limit, then trims the topology log.
    fn enforce_memory_limit(&mut self) {
        let Some(limit) = self.memory_limit else {
            return;
        };
        let mut evicted = false;
        while self.resident_bytes > limit {
            let stopped = match self.evict_oldest_segment() {
                Ok(true) => {
                    evicted = true;
                    continue;
                }
                Ok(false) => format!(
                    "only pinned rows and the unsealed tail remain ({} bytes)",
                    self.resident_bytes
                ),
                Err(e) => format!("eviction failed: {e}"),
            };
            if !self.over_budget_warned {
                log::warn!("ledger is over its {limit}-byte memory limit: {stopped}");
                self.over_budget_warned = true;
            }
            break;
        }
        if self.resident_bytes <= limit {
            self.over_budget_warned = false;
        }
        if evicted && let Some(start) = self.window_start() {
            self.transform_tree.trim_before(start);
        }
    }

    /// Evicts the sealed segment with the oldest `max_epoch`, first moving its rows that are
    /// still their entity's latest into `pinned`. Returns `false` if there is no sealed
    /// segment. On error nothing has changed.
    fn evict_oldest_segment(&mut self) -> Result<bool, String> {
        let Some(idx) = (0..self.segments.len()).min_by_key(|&i| self.segments[i].max_epoch) else {
            return Ok(false);
        };

        // Build the new pinned set before touching anything. Re-filtering the old pinned
        // rows drops those a later row has since superseded.
        let survivors = self.still_latest(&self.segments[idx].batch)?;
        let pinned = match &self.pinned {
            Some(old) => {
                let merged = arrow::compute::concat_batches(&self.schema, [&old.batch, &survivors])
                    .map_err(|e| format!("failed to merge pinned rows: {e}"))?;
                self.still_latest(&merged)?
            }
            None => survivors,
        };
        let pinned = match pinned.num_rows() {
            0 => None,
            _ => Some(Segment::new(pinned)?),
        };

        let victim = self.segments.remove(idx);
        self.resident_bytes -= victim.bytes;
        if let Some(old) = self.pinned.take() {
            self.resident_bytes -= old.bytes;
        }
        if let Some(new) = &pinned {
            self.resident_bytes += new.bytes;
        }
        self.pinned = pinned;
        Ok(true)
    }

    /// The rows of `batch` still at their entity's latest epoch, which eviction must keep.
    ///
    /// Every row at that epoch is kept, not just one, so `current_state`'s priority rule
    /// still sees every candidate.
    fn still_latest(&self, batch: &RecordBatch) -> Result<RecordBatch, String> {
        let Some(cols) = PoseColumns::try_new(batch, &self.id_column) else {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
        };
        let mut mask = BooleanBuilder::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            let latest = cols.id_at(row).and_then(|id| self.latest_pose.get(&id));
            mask.append_value(latest.is_some_and(|p| cols.epoch_at(row) == p.epoch));
        }
        apply_boolean_mask(batch, &mask.finish())
    }

    /// The epoch the ledger holds in full from: the oldest row outside `pinned`.
    fn window_start(&self) -> Option<Duration> {
        self.segments
            .iter()
            .chain(&self.tail)
            .filter(|s| s.batch.num_rows() > 0)
            .map(|s| s.min_epoch)
            .min()
    }

    /// Seals the tail once it holds more than [`SEGMENT_THRESHOLD`] batches or
    /// [`Ledger::segment_target`] bytes: concatenated into one batch (a lone append is used
    /// as it is), then split down to the segment target.
    ///
    /// Only the tail is copied, never an older segment, so total copy work is linear in the
    /// rows ingested and the extra memory a seal needs is one segment. If the copy fails
    /// (OOM), the tail stays unsealed and the ledger is otherwise unchanged.
    fn seal_tail_if_needed(&mut self) {
        let target = self.segment_target();
        let tail_bytes: usize = self.tail.iter().map(|s| s.bytes).sum();
        if self.tail.len() <= SEGMENT_THRESHOLD && tail_bytes <= target {
            return;
        }
        let merged = match self.tail.as_slice() {
            [only] => Ok(only.batch.clone()),
            tail => arrow::compute::concat_batches(&self.schema, tail.iter().map(|s| &s.batch))
                .map_err(|e| e.to_string()),
        };
        let Ok(sealed) = merged.and_then(|batch| split_oversized(&batch, target)) else {
            return;
        };
        let sealed_bytes: usize = sealed.iter().map(|s| s.bytes).sum();
        self.resident_bytes = self.resident_bytes - tail_bytes + sealed_bytes;
        self.tail.clear();
        self.segments.extend(sealed);
    }

    /// Returns the number of batches currently stored.
    pub fn len(&self) -> usize {
        self.all_segments().count()
    }

    /// Returns `true` if the ledger holds no batches.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes held by the stored batches, each underlying allocation counted once. This is
    /// what the memory limit bounds; the process uses more (see [`Ledger::set_memory_limit`]).
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Filters all batches and returns the matching rows as a single concatenated
    /// [`RecordBatch`].
    ///
    /// Uses [`spacetimestamp::query::filter_batch`] internally, so the same frame-uniformity
    /// rules apply: spatial filters require all rows to be in the same frame.
    ///
    /// Returns an empty batch (correct schema, 0 rows) when there are no matches. A segment
    /// whose epoch range misses the filter's time range is skipped unread.
    pub fn query(&self, filter: &SpatiotemporalFilter) -> Result<RecordBatch, String> {
        if self.is_empty() {
            return Err("Ledger is empty".to_string());
        }

        let mut kept: Vec<RecordBatch> = Vec::new();

        for batch in self.batches_for(filter) {
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
    /// A segment whose epoch range misses the filter's time range yields nothing.
    pub fn stream_query<'a>(
        &'a self,
        filter: &'a SpatiotemporalFilter,
    ) -> impl Iterator<Item = Result<RecordBatch, String>> + 'a {
        self.batches_for(filter)
            .map(move |batch| filter_batch(batch, filter))
    }

    /// Returns the most recent batch in the ledger, optionally filtered to specific entity IDs.
    ///
    /// If `entity_ids` is `Some` and this ledger has no `id_column`, returns `None`.
    pub fn latest_snapshot(&self, entity_ids: Option<&[PrescribedId]>) -> Option<RecordBatch> {
        let last = self.all_batches().last()?;

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

        for batch in self.all_batches() {
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

    /// Every stored batch as its own IPC batch, in insertion order. Nothing is concatenated,
    /// so saving costs no second copy of the ledger.
    fn batches_for_ipc(&self) -> Vec<RecordBatch> {
        self.all_batches().cloned().collect()
    }

    /// Serializes all batches to an Arrow IPC file at `path`, and the name registry to a
    /// sibling file beside it (see [`names_sibling_path`]).
    pub fn save_ipc(&self, path: &Path) -> Result<(), String> {
        if self.is_empty() {
            return Err(
                "Cannot save an empty ledger — use save_schema_ipc to persist just the schema"
                    .to_string(),
            );
        }

        ipc::write_file(path, &self.batches_for_ipc(), &self.schema)?;

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
        Ok(Self::from_parts(schema, id_column))
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
        Ok(Self::from_parts(schema, id_column))
    }

    /// Returns the maximum stored timestamp as a J2000-relative [`Duration`], or `None`
    /// if the ledger holds no rows.
    fn latest_stored_duration(&self) -> Option<Duration> {
        self.all_segments()
            .filter(|s| s.batch.num_rows() > 0)
            .map(|s| s.max_epoch)
            .max()
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
    ///   When `None`, defaults to one hour before the latest stored timestamp.
    pub fn current_state(
        &self,
        id_filter: Option<&[PrescribedId]>,
        not_before: Option<Epoch>,
    ) -> Result<RecordBatch, String> {
        if self.is_empty() {
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

        let batches: Vec<&RecordBatch> = self.all_batches().collect();
        for (batch_idx, batch) in batches.iter().enumerate() {
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
            let batch = batches[*batch_idx];
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
        if self.is_empty() {
            return Err(
                "Cannot save an empty ledger — use schema_to_ipc_bytes to persist just the schema"
                    .to_string(),
            );
        }
        ipc::write_bytes(&self.batches_for_ipc(), &self.schema)
    }

    /// Deserializes a ledger from an in-memory Arrow IPC buffer.
    ///
    /// Reads the schema from the IPC bytes and validates it against `sts_column` and `id_column`.
    pub fn load_ipc_from_bytes(bytes: &[u8], id_column: &str) -> Result<Self, String> {
        let (schema, batches) = ipc::read_bytes(bytes)?;
        Self::from_loaded(schema, batches, id_column, "IPC bytes")
    }

    /// Assembles an empty ledger without validating `schema`.
    fn from_parts(schema: SchemaRef, id_column: &str) -> Self {
        Self {
            schema,
            pinned: None,
            segments: Vec::new(),
            tail: Vec::new(),
            resident_bytes: 0,
            memory_limit: None,
            over_budget_warned: false,
            id_column: id_column.to_string(),
            transform_tree: TransformTree::new(),
            latest_pose: IdMap::default(),
            names: NameRegistry::new(),
        }
    }

    /// Validates a loaded schema, rejects an empty payload, and rebuilds the derived state.
    ///
    /// Each batch becomes a sealed segment as it stands, in saved order: nothing is copied,
    /// and all of it can be evicted. A loaded ledger has no memory limit yet; setting one
    /// splits any oversized segment (see [`Ledger::set_memory_limit`]). A file this ledger
    /// wrote holds at most [`SEGMENT_THRESHOLD`] small batches (its old tail), so the segment
    /// count stays bounded.
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
        let mut ledger = Self::from_parts(schema, id_column);
        for batch in batches {
            let segment = Segment::new(batch)?;
            ledger.resident_bytes += segment.bytes;
            ledger.segments.push(segment);
        }
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

/// Relabels `batch` with the ledger's `schema` when the two differ only in field metadata,
/// and rejects any other mismatch.
///
/// Every stored batch must carry the ledger schema exactly, or a later concat (`current_state`,
/// `save_ipc`) fails for the whole ledger. pyarrow, for one, writes `arrow.uuid` fields with an
/// empty `ARROW:extension:metadata`. The relabel swaps only the type, never the buffers.
fn conform_to_schema(batch: RecordBatch, schema: &SchemaRef) -> Result<RecordBatch, String> {
    if batch.schema() == *schema {
        return Ok(batch);
    }
    let found = batch.schema();
    let fields = schema.fields();
    if found.fields().len() != fields.len()
        || !found
            .fields()
            .iter()
            .zip(fields)
            .all(|(f, g)| same_but_metadata(f, g))
    {
        return Err(format!(
            "batch schema does not match the ledger schema: expected {schema:?}, found {found:?}"
        ));
    }
    let columns = batch
        .columns()
        .iter()
        .zip(fields)
        .map(|(col, field)| {
            col.to_data()
                .into_builder()
                .data_type(field.data_type().clone())
                .build()
                .map(arrow::array::make_array)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to relabel batch with the ledger schema: {e}"))?;
    RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| format!("failed to relabel batch with the ledger schema: {e}"))
}

/// `a` and `b` agree in name, nullability and type, recursively, ignoring field metadata.
///
/// Unlike `DataType::equals_datatype`, nested names count: two id fields swapped inside a
/// struct must not pass.
fn same_but_metadata(a: &Field, b: &Field) -> bool {
    use arrow::datatypes::DataType::{FixedSizeList, LargeList, List, Struct};
    a.name() == b.name()
        && a.is_nullable() == b.is_nullable()
        && match (a.data_type(), b.data_type()) {
            (Struct(x), Struct(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(f, g)| same_but_metadata(f, g))
            }
            (FixedSizeList(f, n), FixedSizeList(g, m)) => n == m && same_but_metadata(f, g),
            (List(f), List(g)) | (LargeList(f), LargeList(g)) => same_but_metadata(f, g),
            (x, y) => x == y,
        }
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
        assert_eq!((ledger.segments.len(), ledger.tail.len()), (1, 0));
    }

    /// The column of `segment` as a bare address, to tell whether it was copied.
    fn column_addr(segment: &Segment) -> *const () {
        Arc::as_ptr(segment.batch.column(0)) as *const ()
    }

    /// A second seal concatenates only the new tail; the first sealed segment keeps its
    /// buffers, so no row is ever copied twice.
    #[test]
    fn test_seal_never_recopies_a_sealed_segment() {
        let mut ledger = make_sts_ledger();
        let n = SEGMENT_THRESHOLD + 1;
        for i in 0..n {
            ledger.append(make_batch([0.0; 3], i as u64)).unwrap();
        }
        let first = column_addr(&ledger.segments[0]);

        for i in n..2 * n {
            ledger.append(make_batch([0.0; 3], i as u64)).unwrap();
        }
        assert_eq!(ledger.segments.len(), 2);
        assert_eq!(column_addr(&ledger.segments[0]), first);
        assert_eq!(ledger.segments[0].batch.num_rows(), n);
        assert_eq!(ledger.segments[1].batch.num_rows(), n);
    }

    /// `resident_bytes` tracks the batches actually held, through appends and seals.
    #[test]
    fn test_resident_bytes_is_the_sum_of_stored_batches() {
        let mut ledger = make_sts_ledger();
        assert_eq!(ledger.resident_bytes(), 0);
        for i in 0..SEGMENT_THRESHOLD + 5 {
            ledger.append(make_batch([0.0; 3], i as u64)).unwrap();
            let held: usize = ledger.all_batches().map(allocated_bytes).sum();
            assert_eq!(ledger.resident_bytes(), held, "after append {i}");
        }
    }

    /// Segment epoch ranges come from the rows, including a sealed segment's.
    #[test]
    fn test_segment_epoch_range() {
        let mut ledger = make_sts_ledger();
        for i in 0..=SEGMENT_THRESHOLD {
            ledger
                .append(make_batch([0.0; 3], 1000 + i as u64))
                .unwrap();
        }
        let sealed = &ledger.segments[0];
        assert_eq!(sealed.min_epoch, Duration::from_parts(0, 1000));
        assert_eq!(
            sealed.max_epoch,
            Duration::from_parts(0, 1000 + SEGMENT_THRESHOLD as u64)
        );
        assert_eq!(
            ledger.latest_stored_duration(),
            Some(Duration::from_parts(0, 1000 + SEGMENT_THRESHOLD as u64))
        );
    }

    /// A time range that misses whole segments skips them, and returns exactly the rows a
    /// full scan would.
    #[test]
    fn test_time_range_query_prunes_segments_but_returns_the_same_rows() {
        let mut ledger = make_sts_ledger();
        let n = SEGMENT_THRESHOLD + 1;
        // Two sealed segments, ns [0, n) and [n, 2n), and a tail at ns [2n, 2n + 3).
        for i in 0..2 * n + 3 {
            ledger
                .append(make_batch([i as f64, 0.0, 0.0], i as u64))
                .unwrap();
        }
        assert_eq!((ledger.segments.len(), ledger.tail.len()), (2, 3));

        let at = |ns: usize| j2000() + Duration::from_parts(0, ns as u64);
        let filter = SpatiotemporalFilter::new().with_time_range(at(n + 5), at(n + 9));
        assert_eq!(
            ledger.batches_for(&filter).count(),
            1,
            "only the second segment overlaps"
        );

        let pruned = ledger.query(&filter).unwrap();
        let mut scanned = 0;
        for batch in ledger.all_batches() {
            scanned += filter_batch(batch, &filter).unwrap().num_rows();
        }
        assert_eq!(pruned.num_rows(), 5);
        assert_eq!(pruned.num_rows(), scanned);
        let streamed: usize = ledger
            .stream_query(&filter)
            .map(|b| b.unwrap().num_rows())
            .sum();
        assert_eq!(streamed, 5);
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
            // Each stored batch is saved as its own IPC batch, so both come back.
            assert_eq!(loaded.len(), 2);
            let total_rows: usize = loaded.all_batches().map(|b| b.num_rows()).sum();
            assert_eq!(total_rows, 2);
        }

        remove_saved_ledger(&path);
    }

    /// A ledger of sealed segments plus a tail saves as many IPC batches and reloads to the
    /// same batches, in the same order.
    #[test]
    fn test_multi_batch_ipc_round_trip_keeps_batches_and_order() {
        let mut ledger = make_sts_ledger();
        for i in 0..2 * (SEGMENT_THRESHOLD + 1) + 3 {
            ledger
                .append(make_batch([i as f64, 0.0, 0.0], i as u64))
                .unwrap();
        }
        let loaded = Ledger::load_ipc_from_bytes(&ledger.save_ipc_to_bytes().unwrap(), "").unwrap();

        assert_eq!(loaded.len(), ledger.len());
        let before: Vec<&RecordBatch> = ledger.all_batches().collect();
        let after: Vec<&RecordBatch> = loaded.all_batches().collect();
        assert_eq!(before, after);
        let held: usize = after.iter().copied().map(allocated_bytes).sum();
        assert_eq!(loaded.resident_bytes(), held);
    }

    /// Saving after a seal keeps an entity ledger whole: the reloaded ledger has the same
    /// current state, and resolves the same entity chain at the same epochs.
    #[test]
    fn test_save_after_a_seal_round_trips_entity_data() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let (base, rover) = (demo("base"), demo("rover"));
        let ident = [1.0, 0.0, 0.0, 0.0];

        let mut ledger = make_entity_ledger();
        ledger
            .append(make_entity_batch(base, earth, [10.0, 0.0, 0.0], ident, 0))
            .unwrap();
        for i in 1..=SEGMENT_THRESHOLD + 5 {
            ledger
                .append(make_entity_batch(
                    rover,
                    base,
                    [i as f64, 0.0, 0.0],
                    ident,
                    i as u64,
                ))
                .unwrap();
        }
        assert_eq!(ledger.segments.len(), 1, "a seal has happened");

        let loaded =
            Ledger::load_ipc_from_bytes(&ledger.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();

        let sorted = |l: &Ledger| {
            let state = l.current_state(None, Some(j2000())).unwrap();
            let ids = state.column_by_name("entity_id").unwrap();
            let order = arrow::compute::sort_to_indices(ids, None, None).unwrap();
            arrow::compute::take_record_batch(&state, &order).unwrap()
        };
        assert_eq!(sorted(&loaded), sorted(&ledger));

        // ns 0 predates the rover's first row, so both must agree it does not resolve.
        assert!(ledger.resolve_to_root(rover, j2000()).is_none());
        for ns in [0, 1, 30, SEGMENT_THRESHOLD as u64 + 5] {
            let at = j2000() + Duration::from_parts(0, ns);
            let before = ledger.resolve_to_root(rover, at);
            let after = loaded.resolve_to_root(rover, at);
            assert_eq!(before, after, "at ns {ns}");
        }
        let latest = j2000() + Duration::from_parts(0, SEGMENT_THRESHOLD as u64 + 5);
        let (_, iso) = loaded.resolve_to_root(rover, latest).unwrap();
        assert_eq!(iso.translation.x, 10.0 + (SEGMENT_THRESHOLD + 5) as f64);
    }

    // -----------------------------------------------------------------------
    // memory limit and eviction
    // -----------------------------------------------------------------------

    /// One entity batch holding a row per `(entity, frame, x_km)`, all at epoch `ns`.
    fn entity_rows(rows: &[(PrescribedId, PrescribedId, f64)], ns: u64) -> RecordBatch {
        use crate::schemas::entity::EntityBuilder;
        let mut b = EntityBuilder::new(rows.len());
        for &(id, frame, x) in rows {
            b.append_entity(
                id,
                frame,
                LengthUnit::km,
                TimeScaleCode::TAI,
                test_source(),
                EstimateType::MEASURED,
                [x, 0.0, 0.0],
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
        }
        b.flush()
    }

    fn at_ns(ns: u64) -> Epoch {
        j2000() + Duration::from_parts(0, ns)
    }

    /// The bytes a full unsealed tail of `batch(t)` appends occupies, so a test can set a
    /// limit that the tail alone never exceeds.
    fn full_tail_bytes(batch: impl Fn(u64) -> RecordBatch) -> usize {
        let mut probe = make_entity_ledger();
        for t in 0..SEGMENT_THRESHOLD as u64 {
            probe.append(batch(t)).unwrap();
        }
        probe.resident_bytes()
    }

    fn row_count(ledger: &Ledger) -> usize {
        ledger.all_batches().map(|b| b.num_rows()).sum()
    }

    /// A batch decoded from IPC (or Flight) keeps every array in one message body. Each
    /// allocation must count once, or a reloaded ledger reports many times its real size
    /// and the memory limit evicts far too much.
    #[test]
    fn test_resident_bytes_count_a_shared_ipc_allocation_once() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let mut ledger = make_entity_ledger();
        for t in 0..200 {
            ledger
                .append(entity_rows(&[(demo("bot"), earth, 1.0)], t))
                .unwrap();
        }
        let bytes = ledger.save_ipc_to_bytes().unwrap();
        let loaded = Ledger::load_ipc_from_bytes(&bytes, "entity_id").unwrap();
        assert!(
            loaded.resident_bytes() <= 2 * bytes.len(),
            "{} resident for a {}-byte file",
            loaded.resident_bytes(),
            bytes.len()
        );
    }

    /// One batch holding `bot`'s track for epochs `0..n` ns, one row each, x = epoch: the
    /// shape of a file saved before segments existed.
    fn timeline(n: u64) -> RecordBatch {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let steps: Vec<RecordBatch> = (0..n)
            .map(|t| entity_rows(&[(demo("bot"), earth, t as f64)], t))
            .collect();
        arrow::compute::concat_batches(&steps[0].schema(), &steps).unwrap()
    }

    /// An oversized batch splits into copied chunks: they reassemble the original rows in
    /// order, each is within the cap, and none shares an allocation with the original, so
    /// evicting one frees its memory.
    #[test]
    fn test_split_oversized_copies_chunks_within_the_cap() {
        let batch = timeline(1000);
        let cap = allocated_bytes(&batch) / 4;
        let chunks = split_oversized(&batch, cap).unwrap();

        assert!(chunks.len() >= 4, "{} chunks", chunks.len());
        let rejoined =
            arrow::compute::concat_batches(&batch.schema(), chunks.iter().map(|c| &c.batch))
                .unwrap();
        assert_eq!(rejoined, batch);
        let original = allocations(&batch);
        for chunk in &chunks {
            // Each buffer is padded to 64 bytes, so a chunk may exceed its share by that much.
            let padding = 64 * allocations(&chunk.batch).len();
            assert!(chunk.bytes <= cap + padding, "{} > {cap}", chunk.bytes);
            assert!(
                allocations(&chunk.batch)
                    .keys()
                    .all(|p| !original.contains_key(p))
            );
        }
    }

    /// A batch within the cap comes back as it is, with no copy.
    #[test]
    fn test_split_oversized_keeps_a_batch_within_the_cap() {
        let batch = timeline(10);
        let chunks = split_oversized(&batch, allocated_bytes(&batch)).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(allocations(&chunks[0].batch), allocations(&batch));
    }

    /// A ledger loaded from a single-batch file holds one big segment. Setting a limit splits
    /// it to the segment target first, so eviction keeps a recent window rather than dropping
    /// the whole segment and leaving only pinned rows.
    #[test]
    fn test_memory_limit_splits_a_loaded_single_batch_segment() {
        let mut source = make_entity_ledger();
        source.append(timeline(1000)).unwrap();
        let mut ledger =
            Ledger::load_ipc_from_bytes(&source.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();
        assert_eq!(ledger.segments.len(), 1);

        let limit = ledger.resident_bytes() / 2;
        ledger.set_memory_limit(Some(limit)).unwrap();

        assert!(ledger.resident_bytes() <= limit);
        let target = limit / SEGMENTS_PER_LIMIT;
        assert!(
            ledger
                .segments
                .iter()
                .all(|s| within_target(s.bytes, target))
        );
        let kept = row_count(&ledger);
        assert!(kept > 250, "only {kept} of 1000 rows kept");
        assert!(ledger.window_start().unwrap() > Duration::from_parts(0, 0));
    }

    /// Segments cut to size under a limit are not split again when the ledger is saved and
    /// reloaded under the same limit, though the reload measures them a few bytes larger.
    #[test]
    fn test_reload_under_the_same_limit_does_not_resplit() {
        let batch = timeline(1000);
        let limit = allocated_bytes(&batch) / 2;
        let mut ledger = make_entity_ledger();
        ledger.set_memory_limit(Some(limit)).unwrap();
        ledger.append(batch).unwrap();

        let mut loaded =
            Ledger::load_ipc_from_bytes(&ledger.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();
        loaded.set_memory_limit(Some(limit)).unwrap();
        assert_eq!(loaded.len(), ledger.len());
        assert_eq!(row_count(&loaded), row_count(&ledger));
    }

    /// One append larger than the segment target is split when it seals, so eviction can
    /// still keep part of it.
    #[test]
    fn test_memory_limit_splits_an_oversized_append() {
        let batch = timeline(1000);
        let limit = allocated_bytes(&batch) / 2;
        let mut ledger = make_entity_ledger();
        ledger.set_memory_limit(Some(limit)).unwrap();
        ledger.append(batch).unwrap();

        assert!(ledger.resident_bytes() <= limit);
        let kept = row_count(&ledger);
        assert!(kept > 250, "only {kept} of 1000 rows kept");
        assert!(kept < 1000, "nothing was evicted");
    }

    #[test]
    fn test_set_memory_limit_requires_an_id_column() {
        let mut ledger = make_sts_ledger();
        assert!(ledger.set_memory_limit(Some(1 << 20)).is_err());
        assert!(ledger.set_memory_limit(None).is_ok());
    }

    /// Over a long run, resident bytes never exceed the limit (which is above what the tail
    /// alone can hold), old rows are evicted, and every entity's latest row survives.
    #[test]
    fn test_memory_limit_bounds_resident_bytes() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let (a, b) = (demo("a"), demo("b"));
        let batch = |t: u64| entity_rows(&[(a, earth, t as f64), (b, earth, 0.0)], t);
        let limit = 4 * full_tail_bytes(batch);

        let mut ledger = make_entity_ledger();
        ledger.set_memory_limit(Some(limit)).unwrap();
        let steps = 20 * (SEGMENT_THRESHOLD as u64 + 1);
        for t in 0..steps {
            ledger.append(batch(t)).unwrap();
            assert!(
                ledger.resident_bytes() <= limit,
                "step {t}: {} > {limit}",
                ledger.resident_bytes()
            );
        }

        assert!(
            row_count(&ledger) < 2 * steps as usize,
            "nothing was evicted"
        );
        assert!(ledger.window_start().unwrap() > Duration::from_parts(0, 0));
        let state = ledger.current_state(None, Some(j2000())).unwrap();
        assert_eq!(state.num_rows(), 2);
        let (_, iso) = ledger.resolve_frame_at(a, at_ns(steps)).unwrap();
        assert_eq!(iso.translation.x, (steps - 1) as f64);
    }

    /// An entity appended once, at t0, has its only row pinned when that row's segment is
    /// evicted. `current_state` still returns it, and a chain through it still resolves.
    #[test]
    fn test_static_entity_survives_eviction() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let (beacon, rover) = (demo("beacon"), demo("rover"));
        let batch = |t: u64| entity_rows(&[(rover, beacon, t as f64)], t);

        let mut ledger = make_entity_ledger();
        ledger
            .set_memory_limit(Some(2 * full_tail_bytes(batch)))
            .unwrap();
        ledger
            .append(entity_rows(&[(beacon, earth, 100.0)], 0))
            .unwrap();
        let steps = 10 * (SEGMENT_THRESHOLD as u64 + 1);
        for t in 1..=steps {
            ledger.append(batch(t)).unwrap();
        }

        assert!(
            ledger.window_start().unwrap() > Duration::from_parts(0, 0),
            "the t0 segment must have been evicted"
        );
        let pinned = ledger.pinned.as_ref().expect("the beacon row is pinned");
        assert_eq!(pinned.batch.num_rows(), 1);

        let state = ledger.current_state(None, Some(j2000())).unwrap();
        assert_eq!(state.num_rows(), 2, "beacon and rover");
        let (root, iso) = ledger.resolve_to_root(rover, at_ns(steps)).unwrap();
        assert_eq!(root, earth);
        assert_eq!(iso.translation.x, 100.0 + steps as f64);
    }

    /// After eviction, a save and reload keeps every entity's current parent and pose, and
    /// agrees on ancestry at every in-window epoch. The pre-window re-parent is trimmed from
    /// the topology log; the in-window one is kept.
    #[test]
    fn test_evict_save_reload_keeps_current_topology_and_poses() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let (beacon, rover, truck) = (demo("beacon"), demo("rover"), demo("truck"));
        let steps = 10 * (SEGMENT_THRESHOLD as u64 + 1);
        let (early_hop, late_hop) = (10, steps - 20);
        // rover: under Earth, then the beacon (pre-window), then the truck (in-window).
        let rover_parent = |t: u64| match t {
            t if t < early_hop => earth,
            t if t < late_hop => beacon,
            _ => truck,
        };
        let batch = |t: u64| {
            entity_rows(
                &[(rover, rover_parent(t), 1.0), (truck, earth, t as f64)],
                t,
            )
        };

        let mut ledger = make_entity_ledger();
        ledger
            .set_memory_limit(Some(2 * full_tail_bytes(batch)))
            .unwrap();
        ledger
            .append(entity_rows(&[(beacon, earth, 100.0)], 0))
            .unwrap();
        for t in 1..=steps {
            ledger.append(batch(t)).unwrap();
        }

        let window = ledger.window_start().unwrap();
        assert!(
            window > Duration::from_parts(0, early_hop)
                && window <= Duration::from_parts(0, late_hop)
        );
        // beacon@0, truck@1, rover@1 (Earth) trimmed, rover@early (beacon) in force, rover@late.
        assert_eq!(ledger.export_topology().unwrap().num_rows(), 4);

        let loaded =
            Ledger::load_ipc_from_bytes(&ledger.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();
        let future = at_ns(steps + 1000);
        for id in [beacon, rover, truck] {
            assert_eq!(
                loaded.transform_tree.current_parent(id),
                ledger.transform_tree.current_parent(id)
            );
            assert_eq!(
                loaded.resolve_frame_at(id, future),
                ledger.resolve_frame_at(id, future)
            );
        }

        let first = window.to_parts().1;
        for ns in [first, first + 7, late_hop - 1, late_hop, steps] {
            let at = Duration::from_parts(0, ns);
            for id in [beacon, rover, truck] {
                assert_eq!(
                    loaded.transform_tree.ancestry_at(id, at),
                    ledger.transform_tree.ancestry_at(id, at),
                    "ancestry of {id} at ns {ns}"
                );
            }
        }
    }

    /// A limit below what the tail alone holds evicts every sealed segment and then stops,
    /// keeping the tail and pinned rows rather than failing or looping.
    #[test]
    fn test_limit_below_the_tail_stops_without_losing_latest_rows() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let a = demo("a");
        let mut ledger = make_entity_ledger();
        ledger.set_memory_limit(Some(1)).unwrap();
        for t in 0..3 * (SEGMENT_THRESHOLD as u64 + 1) {
            ledger
                .append(entity_rows(&[(a, earth, t as f64)], t))
                .unwrap();
        }
        assert!(ledger.segments.is_empty());
        assert!(ledger.resident_bytes() > 1);
        // Each of the three seals pinned `a`'s latest row; the two superseded ones are gone.
        let pinned = ledger.pinned.as_ref().unwrap();
        assert_eq!(pinned.batch.num_rows(), 1);
        assert_eq!(
            pinned.max_epoch,
            Duration::from_parts(0, 3 * (SEGMENT_THRESHOLD as u64 + 1) - 1)
        );
        assert_eq!(
            ledger
                .current_state(None, Some(j2000()))
                .unwrap()
                .num_rows(),
            1
        );
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

    /// `batch` with an empty `ARROW:extension:metadata` added to each spacetimestamp field
    /// that lacks one, the way pyarrow writes `arrow.uuid` fields.
    fn with_pyarrow_metadata(batch: RecordBatch) -> RecordBatch {
        let idx = batch.schema().index_of(STS_COLUMN).unwrap();
        let sts = batch
            .column(idx)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let fields: Fields = sts
            .fields()
            .iter()
            .map(|f| {
                let mut metadata = f.metadata().clone();
                if !metadata.contains_key("ARROW:extension:metadata") {
                    metadata.insert("ARROW:extension:metadata", "");
                }
                f.as_ref().clone().with_metadata(metadata)
            })
            .collect();
        let sts =
            StructArray::try_new(fields, sts.columns().to_vec(), sts.nulls().cloned()).unwrap();
        let mut schema_fields = batch.schema().fields().to_vec();
        schema_fields[idx] = Arc::new(
            schema_fields[idx]
                .as_ref()
                .clone()
                .with_data_type(sts.data_type().clone()),
        );
        let mut columns = batch.columns().to_vec();
        columns[idx] = Arc::new(sts);
        RecordBatch::try_new(Arc::new(Schema::new(schema_fields)), columns).unwrap()
    }

    /// A batch differing from the ledger schema only in field metadata is stored under the
    /// ledger schema, so the concats in `current_state` and `save_ipc` still succeed.
    #[test]
    fn test_append_relabels_a_metadata_only_schema_difference() {
        let mut ledger = make_entity_ledger();
        let earth = PrescribedId::astronomical_from_name("IAU_EARTH").unwrap();
        let identity = [1.0, 0.0, 0.0, 0.0];
        ledger
            .append(make_entity_batch(
                demo("a"),
                earth,
                [1.0, 0.0, 0.0],
                identity,
                0,
            ))
            .unwrap();
        ledger
            .append(with_pyarrow_metadata(make_entity_batch(
                demo("b"),
                earth,
                [2.0, 0.0, 0.0],
                identity,
                0,
            )))
            .unwrap();

        assert_eq!(ledger.current_state(None, None).unwrap().num_rows(), 2);
        ledger.save_ipc_to_bytes().unwrap();
    }

    /// Any difference beyond metadata is rejected at append, not left to fail a later concat.
    #[test]
    fn test_append_rejects_a_batch_of_another_schema() {
        let mut ledger = make_entity_ledger();
        let err = ledger.append(make_batch([0.0, 0.0, 0.0], 0)).unwrap_err();
        assert!(err.contains("does not match the ledger schema"), "{err}");
        assert!(ledger.is_empty());
    }

    /// Nested field names count, which `DataType::equals_datatype` alone would ignore.
    #[test]
    fn test_same_but_metadata_distinguishes_swapped_nested_names() {
        let strukt = |a: &str, b: &str| {
            Field::new(
                "s",
                DataType::Struct(
                    vec![
                        Field::new(a, DataType::Int8, false),
                        Field::new(b, DataType::Int8, false),
                    ]
                    .into(),
                ),
                false,
            )
        };
        let (xy, yx) = (strukt("x", "y"), strukt("y", "x"));
        assert!(xy.data_type().equals_datatype(yx.data_type()));
        assert!(!same_but_metadata(&xy, &yx));
        assert!(same_but_metadata(&xy, &xy));
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

        // Two batches in one IPC file, so the forged batch stays a separate batch the reader
        // must reject on its own.
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
