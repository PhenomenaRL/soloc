//! Row-derived transform topology: who is parented to whom, and since when.
//!
//! Topology is not declared through a side channel; it is *derived* from the data.
//! Every spacetimestamp row already says "entity X is at this pose, expressed relative
//! to `frame_id`, as of this epoch".
//!
//! # Node identity
//!
//! Every node is a [`PrescribedId`]: a 16-byte identity. What a node *is*
//! comes from its kind nibble, read as one masked byte load:
//!
//! - [`KIND_SOLOC`](crate::identity::KIND_SOLOC): its pose lives in ledger rows, so
//!   resolution recurses into it;
//! - [`KIND_ASTRO`](crate::identity::KIND_ASTRO): a terminal astronomical frame or body,
//!   handed to `anise`;
//! - [`KIND_ABSTRACT`](crate::identity::KIND_ABSTRACT): only applicable for source_id,
//!   rejected outright as a frame.
//!
//! A **child** is always the value of the ledger's `id_column`. There is no stored edge
//! *kind*: a camera rigidly bolted to a chassis and a spacecraft orbiting a planet are
//! expressed identically in the data, and so are treated identically here. This module
//! never looks at a name. Display is the [`NameRegistry`](crate::identity::NameRegistry)'s
//! job, and topology is unaffected by its absence.
//!
//! # Ingest algorithm
//!
//! [`TransformTree::ingest_batch`] runs three passes over narrow columns only. It never
//! reads `position`, `quaternion`, or covariance:
//!
//! - **Pass A**: one O(N) sweep over `(entity_id, frame_id, duration_centuries,
//!   duration_ns)`. Per distinct id it records the argmax-epoch row (ties: first row
//!   wins, matching `Ledger::resolve_frame_at`) and whether more than one distinct
//!   frame was seen. It also carries the **abstract-id check**: an id that is
//!   [`KIND_ABSTRACT`](crate::identity::KIND_ABSTRACT) rejects the whole batch, in either
//!   the id column or `frame_id`. Provenance labels where a row came from; it is neither a
//!   place nor a thing that occupies one, so it belongs only in `source_id`. Both ids are
//!   already in hand here and the test is a single nibble comparison, so this costs nothing
//!   over a separate sweep. Typos never reach this point: an unresolvable name cannot be
//!   minted into an astronomical id in the first place, so the check that used to call
//!   `anise` here is gone along with the memo it needed.
//! - **Pass B**: bounded by k = distinct ids. Flags "dirty" ids: first sighting,
//!   differing parent, or `multi_frame` from Pass A.
//!   The common case (steady-state pose updates, no re-parenting) stops here.
//! - **Pass C**: dirty ids only. Sorts those rows by `(id, epoch)` and walks them in
//!   order, emitting an event at every parent change. so a
//!   multi-hop re-parent inside one batch is captured faithfully.
//!
//! Cycle detection then runs over the staged result. Nothing is committed until every
//! check passes, so a rejected batch leaves the tree exactly as it was.

use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryArray, FixedSizeBinaryBuilder, Int16Array, Int16Builder,
    RecordBatch, UInt64Array, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use hifitime::Duration;
use std::collections::HashSet;
use std::sync::Arc;

use crate::identity::{IdMap, IdSet, PrescribedId, as_id_column, id_at, id_field};
use crate::schema::{DURATION_CENTURIES_COLUMN, DURATION_NS_COLUMN, StsColumns};

/// A single parent change: `child_id` became parented to `parent_id` at `epoch`.
///
/// `epoch` is a duration offset from the J2000 TAI epoch, matching the storage
/// convention of the `duration_centuries` / `duration_ns` columns. Callers are
/// responsible for having normalised timestamps to TAI first.
/// `Copy` and allocation-free: 40 bytes of plain data, down from two heap allocations
/// plus their pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TopologyEvent {
    /// The entity whose parent changed.
    pub child_id: PrescribedId,
    /// The frame or entity it is now expressed relative to.
    pub parent_id: PrescribedId,
    /// When the change took effect, as an offset from J2000 TAI.
    pub epoch: Duration,
}

/// What [`TransformTree::ingest_batch`] learned from a batch.
#[derive(Debug, Clone, Default)]
pub struct IngestOutcome {
    /// Parent changes detected, in `(id, epoch)` order. Empty in the common case.
    pub events: Vec<TopologyEvent>,
    /// For every id seen in the batch, the row index holding its highest epoch.
    ///
    /// Pass A computes this as a byproduct, so a caller can populate a pose cache
    /// with k targeted wide-column reads instead of a second full scan.
    pub latest_rows: IdMap<usize>,
}

/// The parent graph derived from ingested rows.
///
/// Stores topology only, no pose data. `latest` is the O(1) fast path for
/// "who is X's parent right now"; `log` is the append-only history that makes
/// "who was X's parent at epoch E" answerable. There is no deletion. A topology
/// correction is just another event that supersedes the old edge, mirroring how
/// the ledger already corrects bad poses.
#[derive(Debug, Clone, Default)]
pub struct TransformTree {
    /// child_id → (parent_id, epoch the edge took effect).
    latest: IdMap<(PrescribedId, Duration)>,
    /// Every parent change ever seen, in application order.
    log: Vec<TopologyEvent>,
}

/// Per-id state accumulated during Pass A.
struct IdScan {
    /// Row index of the highest epoch seen so far (first row wins on ties).
    best_row: usize,
    best_dur: Duration,
    /// Frame of the first row seen for this id.
    first_frame: PrescribedId,
    /// Set when a second, different frame shows up for this id.
    multi_frame: bool,
}

/// The spacetimestamp columns topology derivation needs, plus the caller-named id column.
struct NarrowColumns<'a> {
    ids: &'a FixedSizeBinaryArray,
    sts: StsColumns<'a>,
}

impl TransformTree {
    /// Creates an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of children with a known parent.
    pub fn len(&self) -> usize {
        self.latest.len()
    }

    /// Returns `true` if no topology has been derived yet.
    pub fn is_empty(&self) -> bool {
        self.latest.is_empty()
    }

    /// Returns the number of parent-change events recorded.
    ///
    /// Test-only: the event log's length is an assertion target, not a published property.
    /// `to_log_batch()` is the supported way to observe the log.
    #[cfg(test)]
    pub(crate) fn log_len(&self) -> usize {
        self.log.len()
    }

    /// Returns `child_id`'s current parent, ignoring history.
    pub fn current_parent(&self, child_id: PrescribedId) -> Option<PrescribedId> {
        self.latest.get(&child_id).map(|&(p, _)| p)
    }

    /// Returns `child_id`'s parent as of `epoch`, or `None` if it had none yet.
    ///
    /// Uses the O(1) `latest` entry whenever the current edge was already in effect
    /// at `epoch`; otherwise fall back `log` for historical queries,
    pub fn parent_at(&self, child_id: PrescribedId, epoch: Duration) -> Option<PrescribedId> {
        let &(parent, since) = self.latest.get(&child_id)?;
        if since <= epoch {
            return Some(parent);
        }
        self.log
            .iter()
            .rev()
            .find(|e| e.child_id == child_id && e.epoch <= epoch)
            .map(|e| e.parent_id)
    }

    /// Returns the chain of node ids from `entity_id` up to its astronomical root,
    /// as it stood at `epoch`.
    ///
    /// The first element is always `entity_id` and the last is a terminal id. An
    /// astronomical frame or body that `anise` is expected to resolve. Every intermediate
    /// element is another entity whose pose must be looked up.
    ///
    /// An astronomical `entity_id` is already a root: its chain is `[entity_id]`. The
    /// almanac defines its frame; rows stored under it are data, not a frame definition.
    ///
    /// The caller performs the per-hop numeric lookups and composes the isometries.
    /// Ids in error messages render as hyphenated UUIDs; a caller holding a
    /// [`NameRegistry`](crate::identity::NameRegistry) can substitute common names.
    pub fn ancestry_at(
        &self,
        entity_id: PrescribedId,
        epoch: Duration,
    ) -> Result<Vec<PrescribedId>, String> {
        // Astronomical ids are always roots; the almanac owns their frames.
        if entity_id.is_astro() {
            return Ok(vec![entity_id]);
        }

        let mut chain = vec![entity_id];
        let mut visiting: IdSet = IdSet::default();
        visiting.insert(entity_id);

        let mut current = entity_id;
        loop {
            let parent = self.parent_at(current, epoch).ok_or_else(|| {
                if current == entity_id {
                    format!("Entity '{entity_id}' has no known parent at or before {epoch}")
                } else {
                    format!(
                        "Frame chain for '{entity_id}' breaks at '{current}': \
                         no known parent at or before {epoch}"
                    )
                }
            })?;

            chain.push(parent);

            if !parent.is_soloc() {
                // Terminal: an astronomical frame or body, resolved by the almanac.
                return Ok(chain);
            }
            if !self.latest.contains_key(&parent) {
                // A ledger-resolved id we have no rows for — treat as terminal and let
                // the caller decide whether it means anything.
                return Ok(chain);
            }
            if !visiting.insert(parent) {
                return Err(format!(
                    "Cycle detected in frame chain for '{entity_id}' at '{parent}'"
                ));
            }
            current = parent;
        }
    }

    /// Derives topology from `batch` and folds it into this tree.
    ///
    /// `id_column` names the entity-identity column (e.g. `"entity_id"`); the
    /// spacetimestamp fields are read from the nested `"spacetimestamp"` struct when
    /// present, or from top-level columns otherwise.
    ///
    /// The batch is rejected as a whole if it uses a KIND_ABSTRACT id as a frame,
    /// or if applying it would create a cycle.
    ///
    /// Timestamps are assumed already normalised to TAI; ingesting a batch with mixed
    /// timescales would compare epochs that are not on a common scale.
    pub fn ingest_batch(
        &mut self,
        batch: &RecordBatch,
        id_column: &str,
    ) -> Result<IngestOutcome, String> {
        if batch.num_rows() == 0 || id_column.is_empty() {
            return Ok(IngestOutcome::default());
        }
        let cols = locate_columns(batch, id_column)?;

        // --- Pass A: one O(N) sweep over four narrow columns ---------------------
        let mut scans: IdMap<IdScan> = IdMap::default();

        for row in 0..batch.num_rows() {
            let id = id_at(cols.ids, row).map_err(|e| format!("'{id_column}': {e}"))?;
            if id.is_abstract() {
                return Err(format!(
                    "'{id_column}' row {row}: {id} is a provenance-only id. Abstract ids \
                     label where a row came from and never occupy space, so one can be a \
                     source_id but never an entity."
                ));
            }
            let frame = cols.sts.frame_at(row)?;
            if frame.is_abstract() {
                return Err(format!(
                    "Frame '{frame}' is a provenance-only id and can never be a frame"
                ));
            }
            let (centuries, nanos) = cols.sts.epoch_parts_at(row);
            let dur = Duration::from_parts(centuries, nanos);

            match scans.get_mut(&id) {
                None => {
                    scans.insert(
                        id,
                        IdScan {
                            best_row: row,
                            best_dur: dur,
                            first_frame: frame,
                            multi_frame: false,
                        },
                    );
                }
                Some(scan) => {
                    // Strictly-greater keeps the first row on ties, matching
                    // Ledger::resolve_frame_at so the pose cache agrees with the scan.
                    if dur > scan.best_dur {
                        scan.best_dur = dur;
                        scan.best_row = row;
                    }
                    // A global id compared against a global id: mismatch means different
                    // ids
                    if frame != scan.first_frame {
                        scan.multi_frame = true;
                    }
                }
            }
        }

        // --- Pass B: bounded by k = distinct ids ---------------------------------
        let mut latest_rows: IdMap<usize> =
            IdMap::with_capacity_and_hasher(scans.len(), Default::default());
        let mut dirty_ids: IdSet = IdSet::default();

        for (&id, scan) in &scans {
            latest_rows.insert(id, scan.best_row);

            let winning_frame = cols.sts.frame_at(scan.best_row)?;

            let dirty = scan.multi_frame
                || match self.latest.get(&id) {
                    None => true,
                    Some(&(parent, _)) => parent != winning_frame,
                };
            if dirty {
                dirty_ids.insert(id);
            }
        }

        if dirty_ids.is_empty() {
            // Steady state: poses moved, topology did not. Nothing to commit.
            return Ok(IngestOutcome {
                events: Vec::new(),
                latest_rows,
            });
        }

        // --- Pass C: dirty ids only, in (id, epoch) order ------------------------
        let mut dirty_rows: Vec<(PrescribedId, Duration, usize)> = Vec::new();
        for row in 0..batch.num_rows() {
            let id = id_at(cols.ids, row).map_err(|e| format!("'{id_column}': {e}"))?;
            if dirty_ids.contains(&id) {
                let (centuries, nanos) = cols.sts.epoch_parts_at(row);
                dirty_rows.push((id, Duration::from_parts(centuries, nanos), row));
            }
        }
        // Stable sort so equal-epoch rows stay in batch order. Same tie-break as Pass A.
        // Grouping is by id bytes. Only the order of events *between* ids changes;
        // within an id it is still epoch order, and byte order has the property of
        // not depending on how the batch happened to be laid out.
        dirty_rows.sort_by_key(|r| (r.0, r.1));

        let mut events: Vec<TopologyEvent> = Vec::new();
        // Staged edges: applied only once every check below passes.
        let mut staged: IdMap<(PrescribedId, Duration)> = IdMap::default();

        let mut group_id: Option<PrescribedId> = None;
        let mut running_parent: Option<PrescribedId> = None;

        for (id, dur, row) in dirty_rows {
            if group_id != Some(id) {
                group_id = Some(id);
                // Start each id from the parent it had *before* this batch.
                running_parent = self.latest.get(&id).map(|&(p, _)| p);
            }

            let frame = cols.sts.frame_at(row)?;
            if running_parent == Some(frame) {
                continue;
            }
            running_parent = Some(frame);
            staged.insert(id, (frame, dur));
            events.push(TopologyEvent {
                child_id: id,
                parent_id: frame,
                epoch: dur,
            });
        }

        // --- Cycle check over base tree + staged edges ---------------------------
        // One reusable visited set: allocating per child dominates when a batch
        // introduces many entities at once.
        let mut seen: IdSet = IdSet::default();
        for (&child, &(parent, _)) in &staged {
            if !parent.is_soloc() {
                continue; // terminal anchor: this edge cannot be part of a cycle
            }
            seen.clear();
            seen.insert(child);
            let mut node = parent;
            loop {
                if !seen.insert(node) {
                    return Err(format!(
                        "Cycle detected in frame topology involving '{node}'"
                    ));
                }
                let Some(next) = staged
                    .get(&node)
                    .or_else(|| self.latest.get(&node))
                    .map(|&(p, _)| p)
                else {
                    break; // reached a root
                };
                if !next.is_soloc() {
                    break; // terminal astronomical anchor
                }
                node = next;
            }
        }

        // --- Commit --------------------------------------------------------------
        for &event in &events {
            self.apply_event(event);
        }

        Ok(IngestOutcome {
            events,
            latest_rows,
        })
    }

    /// Records `event` in the log and advances `latest` if it is not superseded.
    ///
    /// An event older than the child's current edge is still logged but does
    /// not roll `latest` backwards. This is what makes out-of-order
    /// federation merges safe.
    fn apply_event(&mut self, event: TopologyEvent) {
        let supersedes = match self.latest.get(&event.child_id) {
            None => true,
            Some(&(_, since)) => event.epoch >= since,
        };
        if supersedes {
            self.latest
                .insert(event.child_id, (event.parent_id, event.epoch));
        }
        self.log.push(event);
    }

    // -----------------------------------------------------------------------
    // Federation
    // -----------------------------------------------------------------------

    /// Exports the full event log as a [`RecordBatch`] following [`topology_schema`].
    ///
    /// The whole log is exported, so a recipient can
    /// replay history rather than only seeing where everything ended up.
    pub fn to_log_batch(&self) -> Result<RecordBatch, String> {
        let mut child = FixedSizeBinaryBuilder::with_capacity(self.log.len(), 16);
        let mut parent = FixedSizeBinaryBuilder::with_capacity(self.log.len(), 16);
        let mut centuries = Int16Builder::with_capacity(self.log.len());
        let mut nanos = UInt64Builder::with_capacity(self.log.len());

        for event in &self.log {
            // Infallible in practice: the builder's width matches the id's, and the only
            // documented error is a length mismatch.
            child
                .append_value(event.child_id.as_bytes())
                .map_err(|e| format!("failed to append child id: {e}"))?;
            parent
                .append_value(event.parent_id.as_bytes())
                .map_err(|e| format!("failed to append parent id: {e}"))?;
            let (c, ns) = event.epoch.to_parts();
            centuries.append_value(c);
            nanos.append_value(ns);
        }

        let columns: Vec<ArrayRef> = vec![
            Arc::new(child.finish()),
            Arc::new(parent.finish()),
            Arc::new(centuries.finish()),
            Arc::new(nanos.finish()),
        ];
        RecordBatch::try_new(topology_schema(), columns)
            .map_err(|e| format!("Failed to build topology log batch: {e}"))
    }

    /// Merges an exported log (see [`to_log_batch`](Self::to_log_batch)) into this tree.
    ///
    /// Events are applied in the order they appear, through the same path
    /// [`ingest_batch`](Self::ingest_batch) uses. Returns the number of events applied.
    /// The merge is rejected as a whole if it would create a cycle.
    ///
    /// # Idempotency
    ///
    /// An event this tree has already logged is skipped, so re-merging the same peer log
    /// is a no-op that returns 0 rather than doubling the log. An event is identified by
    /// `(child, parent, epoch)`, so two events that compare equal are
    /// the same fact rather than merely similar ones.
    ///
    /// This matters beyond tidiness: `log` is replayed by
    /// [`parent_at`](Self::parent_at) for historical queries, so duplicates are a
    /// permanent tax on every such query afterwards, and periodic re-sync between two
    /// peers is the expected federation pattern rather than an edge case.
    pub fn merge_log_batch(&mut self, batch: &RecordBatch) -> Result<usize, String> {
        let child = as_id_column(
            batch
                .column_by_name("child_id")
                .ok_or_else(|| "topology batch is missing a 'child_id' column".to_string())?,
            "child_id",
        )?;
        let parent = as_id_column(
            batch
                .column_by_name("parent_id")
                .ok_or_else(|| "topology batch is missing a 'parent_id' column".to_string())?,
            "parent_id",
        )?;
        let centuries = batch
            .column_by_name(DURATION_CENTURIES_COLUMN)
            .and_then(|c| c.as_any().downcast_ref::<Int16Array>())
            .ok_or_else(|| {
                "topology batch is missing an Int16 'duration_centuries' column".to_string()
            })?;
        let nanos = batch
            .column_by_name(DURATION_NS_COLUMN)
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| "topology batch is missing a UInt64 'duration_ns' column".to_string())?;

        // Decode every row before applying any, so a malformed id rejects the batch as a
        // whole rather than half-merging it; The same contract as the cycle check below.
        let mut seen: HashSet<TopologyEvent> = self.log.iter().copied().collect();
        let mut events: Vec<TopologyEvent> = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            let event = TopologyEvent {
                child_id: id_at(child, row)
                    .map_err(|e| format!("topology row {row}, child_id: {e}"))?,
                parent_id: id_at(parent, row)
                    .map_err(|e| format!("topology row {row}, parent_id: {e}"))?,
                epoch: Duration::from_parts(centuries.value(row), nanos.value(row)),
            };
            // `seen` grows as we go, so a batch that repeats an event internally is
            // deduped too, not just one that repeats what we already had.
            if seen.insert(event) {
                events.push(event);
            }
        }

        // Validate against a trial copy so a cyclic log leaves this tree untouched.
        let mut trial = self.clone();
        for &event in &events {
            trial.apply_event(event);
        }
        trial.check_acyclic()?;

        let applied = events.len();
        *self = trial;
        Ok(applied)
    }

    /// Returns `Err` if any child's parent chain loops back on itself.
    fn check_acyclic(&self) -> Result<(), String> {
        // One reusable visited set, and no walk at all for the common case of an entity
        // parented straight to an astronomical anchor. This is the same shaping as the
        // ingest-time check, which matters here because this runs over every known child.
        let mut seen: IdSet = IdSet::default();
        for (&start, &(parent, _)) in &self.latest {
            if !parent.is_soloc() {
                continue;
            }
            seen.clear();
            seen.insert(start);
            let mut node = parent;
            loop {
                if !seen.insert(node) {
                    return Err(format!(
                        "Cycle detected in frame topology involving '{node}'"
                    ));
                }
                match self.latest.get(&node) {
                    Some(&(next, _)) if next.is_soloc() => node = next,
                    _ => break, // reached a root or a terminal anchor
                }
            }
        }
        Ok(())
    }
}

/// The Arrow schema for an exported topology log.
pub fn topology_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        id_field("child_id"),
        id_field("parent_id"),
        Field::new("duration_centuries", DataType::Int16, false),
        Field::new("duration_ns", DataType::UInt64, false),
    ]))
}

/// Locates the caller-named id column and the spacetimestamp columns.
fn locate_columns<'a>(
    batch: &'a RecordBatch,
    id_column: &str,
) -> Result<NarrowColumns<'a>, String> {
    let ids = as_id_column(
        batch
            .column_by_name(id_column)
            .ok_or_else(|| format!("id column '{id_column}' is missing"))?,
        id_column,
    )?;

    Ok(NarrowColumns {
        ids,
        sts: StsColumns::try_new(batch)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::id_builder;
    use crate::schema::{STS_COLUMN, SpaceTimestampBuilder, sts_schema};
    use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};
    use arrow::array::StructArray;
    use arrow::datatypes::{Field, Schema};

    /// The authority every test entity mints under.
    const AUTH: &str = "demo";

    /// A ledger-resolved entity id: chain resolution recurses into it.
    fn ent(name: &str) -> PrescribedId {
        PrescribedId::new(AUTH, name).unwrap()
    }

    /// A source_id
    fn abs(name: &str) -> PrescribedId {
        PrescribedId::abstract_source(AUTH, name).unwrap()
    }

    /// Minimal entity-shaped schema: an id column plus a nested spacetimestamp struct.
    fn test_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            id_field("entity_id"),
            Field::new(
                STS_COLUMN,
                DataType::Struct(sts_schema().fields().clone()),
                false,
            ),
        ]))
    }

    /// Builds a batch from `(entity_id, frame_id, duration_ns)` triples.
    fn make_batch(rows: &[(PrescribedId, PrescribedId, u64)]) -> RecordBatch {
        let mut ids = id_builder(rows.len());
        let mut sts = SpaceTimestampBuilder::new(rows.len());
        for (id, frame, ns) in rows {
            ids.append_value(id.as_bytes())
                .expect("PrescribedId is 16 bytes");
            sts.append_spacetimestamp(
                *frame,
                LengthUnit::km,
                TimeScaleCode::TAI,
                abs("src"),
                EstimateType::MEASURED,
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                *ns,
                None,
                None,
            );
        }
        let sts_array: StructArray = sts.finish_as_struct();
        RecordBatch::try_new(
            test_schema(),
            vec![Arc::new(ids.finish()), Arc::new(sts_array)],
        )
        .unwrap()
    }

    fn ns(n: u64) -> Duration {
        Duration::from_parts(0, n)
    }

    #[test]
    fn test_first_sighting_creates_edges() {
        let mut tree = TransformTree::new();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    (
                        ent("truck"),
                        PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                        100,
                    ),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        100,
                    ),
                ]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 2);
        assert_eq!(
            tree.current_parent(ent("truck")),
            Some(PrescribedId::astronomical_from_name("IAU_EARTH").unwrap())
        );
        assert_eq!(
            tree.current_parent(ent("sat")),
            Some(PrescribedId::astronomical_from_name("ICRF").unwrap())
        );
        assert_eq!(outcome.latest_rows.len(), 2);
    }

    #[test]
    fn test_steady_state_emits_no_events() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(
                ent("sat"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                100,
            )]),
            "entity_id",
        )
        .unwrap();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        200,
                    ),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        300,
                    ),
                ]),
                "entity_id",
            )
            .unwrap();

        assert!(
            outcome.events.is_empty(),
            "unchanged topology must be quiet"
        );
        assert_eq!(tree.log_len(), 1, "only the first sighting is logged");
        // The edge keeps the epoch it took effect, not the newest row's epoch.
        assert_eq!(tree.latest[&ent("sat")].1, ns(100));
    }

    #[test]
    fn test_latest_rows_tracks_argmax_epoch_first_wins_on_tie() {
        let mut tree = TransformTree::new();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        100,
                    ),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        900,
                    ),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        900,
                    ),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        400,
                    ),
                ]),
                "entity_id",
            )
            .unwrap();
        // Row 1 and row 2 tie at the max epoch; the earlier row must win.
        assert_eq!(outcome.latest_rows[&ent("sat")], 1);
    }

    #[test]
    fn test_reparent_across_batches() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(ent("robot"), ent("facility"), 100)]),
            "entity_id",
        )
        .unwrap();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[(ent("robot"), ent("truck"), 500)]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].parent_id, ent("truck"));
        assert_eq!(outcome.events[0].epoch, ns(500));
        assert_eq!(tree.current_parent(ent("robot")), Some(ent("truck")));
    }

    #[test]
    fn test_multi_hop_reparent_within_one_batch() {
        // facility → truck → spaceship, all inside a single unsorted batch.
        let mut tree = TransformTree::new();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    (ent("robot"), ent("truck"), 200),
                    (ent("robot"), ent("spaceship"), 300),
                    (ent("robot"), ent("facility"), 100),
                ]),
                "entity_id",
            )
            .unwrap();

        let parents: Vec<PrescribedId> = outcome.events.iter().map(|e| e.parent_id).collect();
        assert_eq!(
            parents,
            vec![ent("facility"), ent("truck"), ent("spaceship")],
            "every intermediate hop must be logged, in epoch order"
        );
        assert_eq!(tree.current_parent(ent("robot")), Some(ent("spaceship")));
    }

    #[test]
    fn test_reparent_that_reverts_within_one_batch_is_still_logged() {
        // The case epoch-only change detection would miss: the batch's newest row
        // names the same parent the tree already had, but a detour happened between.
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(
                ent("robot"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                50,
            )]),
            "entity_id",
        )
        .unwrap();

        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    (ent("robot"), ent("truck"), 100),
                    (
                        ent("robot"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        200,
                    ),
                ]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 2, "the detour must not be swallowed");
        assert_eq!(tree.parent_at(ent("robot"), ns(150)), Some(ent("truck")));
        assert_eq!(
            tree.parent_at(ent("robot"), ns(250)),
            Some(PrescribedId::astronomical_from_name("ICRF").unwrap())
        );
    }

    #[test]
    fn test_parent_at_historical_replay() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(ent("robot"), ent("facility"), 100)]),
            "entity_id",
        )
        .unwrap();
        tree.ingest_batch(
            &make_batch(&[(ent("robot"), ent("truck"), 500)]),
            "entity_id",
        )
        .unwrap();

        assert_eq!(tree.parent_at(ent("robot"), ns(50)), None);
        assert_eq!(tree.parent_at(ent("robot"), ns(100)), Some(ent("facility")));
        assert_eq!(tree.parent_at(ent("robot"), ns(499)), Some(ent("facility")));
        assert_eq!(tree.parent_at(ent("robot"), ns(500)), Some(ent("truck")));
        assert_eq!(tree.parent_at(ent("robot"), ns(9999)), Some(ent("truck")));
    }

    #[test]
    fn test_ancestry_at_multi_hop() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[
                (
                    ent("facility"),
                    PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                    100,
                ),
                (ent("truck"), ent("facility"), 100),
                (ent("robot"), ent("truck"), 100),
            ]),
            "entity_id",
        )
        .unwrap();

        let chain = tree.ancestry_at(ent("robot"), ns(100)).unwrap();
        assert_eq!(
            chain,
            vec![
                ent("robot"),
                ent("truck"),
                ent("facility"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
            ]
        );
    }

    #[test]
    fn test_ancestry_at_unknown_entity() {
        let tree = TransformTree::new();
        let ghost = ent("ghost");
        let err = tree.ancestry_at(ghost, ns(0)).unwrap_err();
        assert!(
            err.contains(&ghost.to_hyphenated()),
            "error should name it: {err}"
        );
    }

    /// An astronomical id is a chain root even when its own rows gave it a parent edge.
    /// The edge is still recorded; only the walk refuses to pass through it, so children
    /// resolve against the almanac rather than the body's stored rows.
    #[test]
    fn test_ancestry_at_treats_an_astronomical_id_as_a_root() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let icrf = PrescribedId::astronomical_from_name("ICRF").unwrap();
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(earth, icrf, 100), (ent("sat"), earth, 100)]),
            "entity_id",
        )
        .unwrap();

        assert_eq!(tree.current_parent(earth), Some(icrf));
        assert_eq!(tree.ancestry_at(earth, ns(100)).unwrap(), vec![earth]);
        assert_eq!(
            tree.ancestry_at(ent("sat"), ns(100)).unwrap(),
            vec![ent("sat"), earth]
        );
    }

    #[test]
    fn test_ancestry_at_stops_at_entity_with_no_rows() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(ent("robot"), ent("never_reported"), 100)]),
            "entity_id",
        )
        .unwrap();
        let chain = tree.ancestry_at(ent("robot"), ns(100)).unwrap();
        assert_eq!(chain, vec![ent("robot"), ent("never_reported")]);
    }

    #[test]
    fn test_cycle_rejected_at_ingest() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[(ent("A"), ent("B"), 100)]), "entity_id")
            .unwrap();
        let err = tree
            .ingest_batch(&make_batch(&[(ent("B"), ent("A"), 100)]), "entity_id")
            .unwrap_err();

        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert!(
            tree.current_parent(ent("B")).is_none(),
            "a rejected batch must leave the tree untouched"
        );
        assert_eq!(tree.log_len(), 1);
    }

    #[test]
    fn test_self_cycle_rejected() {
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(&make_batch(&[(ent("A"), ent("A"), 100)]), "entity_id")
            .unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert!(tree.is_empty());
    }

    #[test]
    fn test_abstract_frame_rejected() {
        let mut tree = TransformTree::new();
        let pipeline = abs("pipeline_v3");
        let err = tree
            .ingest_batch(&make_batch(&[(ent("sat"), pipeline, 100)]), "entity_id")
            .unwrap_err();

        assert!(
            err.contains(&pipeline.to_hyphenated()),
            "error should name it: {err}"
        );
        assert!(
            tree.is_empty(),
            "a rejected batch must leave the tree untouched"
        );
    }

    #[test]
    fn test_abstract_entity_id_rejected() {
        let mut tree = TransformTree::new();
        let pipeline = abs("pipeline_v3");
        let err = tree
            .ingest_batch(
                &make_batch(&[(
                    pipeline,
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    100,
                )]),
                "entity_id",
            )
            .unwrap_err();

        assert!(
            err.contains(&pipeline.to_hyphenated()),
            "error should name it: {err}"
        );
        assert!(err.contains("entity_id"), "error should name it: {err}");
        assert!(
            tree.is_empty(),
            "a rejected batch must leave the tree untouched"
        );
    }

    #[test]
    fn test_concatenated_batch_groups_an_entity_once() {
        let mut tree = TransformTree::new();
        let first = make_batch(&[
            (
                ent("sat"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                100,
            ),
            (
                ent("sat"),
                PrescribedId::astronomical_from_name("Earth").unwrap(),
                200,
            ),
        ]);
        let second = make_batch(&[(
            ent("sat"),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
            300,
        )]);
        let joined = arrow::compute::concat_batches(&first.schema(), [&first, &second]).unwrap();

        let outcome = tree.ingest_batch(&joined, "entity_id").unwrap();

        assert_eq!(
            outcome.events.len(),
            2,
            "one re-parent is two edges, not three: {:?}",
            outcome.events
        );
        assert_eq!(
            outcome.events[0].parent_id,
            PrescribedId::astronomical_from_name("ICRF").unwrap()
        );
        assert_eq!(
            outcome.events[1].parent_id,
            PrescribedId::astronomical_from_name("Earth").unwrap()
        );
        assert_eq!(
            tree.current_parent(ent("sat")),
            Some(PrescribedId::astronomical_from_name("Earth").unwrap())
        );
    }

    #[test]
    fn test_abstract_frame_check_covers_non_winning_rows() {
        // The bad frame is on the *older* row, which the argmax-epoch scan discards; so
        // this pins that the check sweeps every row rather than only the winners.
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(
                &make_batch(&[
                    (ent("sat"), abs("pipeline_v3"), 100),
                    (
                        ent("sat"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        200,
                    ),
                ]),
                "entity_id",
            )
            .unwrap_err();
        assert!(err.to_lowercase().contains("provenance"), "got: {err}");
    }

    #[test]
    fn test_astronomical_frame_accepted_as_parent() {
        // A terminal astronomical id (here Mars, a body-fixed frame) is a legal parent.
        let mars = PrescribedId::astronomical_from_name("Mars").unwrap();
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[(ent("probe"), mars, 100)]), "entity_id")
            .unwrap();
        assert_eq!(tree.current_parent(ent("probe")), Some(mars));
    }

    #[test]
    fn test_corrupt_id_column_is_rejected() {
        // Right Arrow type, wrong contents. Ids are decoded through
        // `PrescribedId::from_bytes`, so the RFC 9562 version and variant checks catch a
        // mistyped column at the boundary instead of letting it become plausible garbage.
        let mut ids = id_builder(1);
        // 16 bytes wide, but no version or variant bits
        ids.append_value([0u8; 16]).unwrap();
        let mut sts = SpaceTimestampBuilder::new(1);
        sts.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            abs("src"),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            100,
            None,
            None,
        );
        let batch = RecordBatch::try_new(
            test_schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(sts.finish_as_struct()) as ArrayRef,
            ],
        )
        .unwrap();

        let err = TransformTree::new()
            .ingest_batch(&batch, "entity_id")
            .unwrap_err();
        assert!(err.contains("version"), "got: {err}");
    }

    #[test]
    fn test_empty_batch_and_missing_id_column_are_noops() {
        let mut tree = TransformTree::new();
        let empty = RecordBatch::new_empty(test_schema());
        assert!(
            tree.ingest_batch(&empty, "entity_id")
                .unwrap()
                .events
                .is_empty()
        );
        // An empty id_column means the caller's schema has no identity column.
        assert!(
            tree.ingest_batch(
                &make_batch(&[(
                    ent("a"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    1
                )]),
                ""
            )
            .unwrap()
            .events
            .is_empty()
        );
        assert!(tree.is_empty());
    }

    #[test]
    fn test_missing_id_column_errors() {
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(
                &make_batch(&[(
                    ent("a"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    1,
                )]),
                "nope",
            )
            .unwrap_err();
        assert!(err.contains("nope"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // Federation round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_log_batch_round_trip_preserves_history() {
        let mut source = TransformTree::new();
        source
            .ingest_batch(
                &make_batch(&[
                    (
                        ent("facility"),
                        PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                        100,
                    ),
                    (ent("robot"), ent("facility"), 100),
                ]),
                "entity_id",
            )
            .unwrap();
        source
            .ingest_batch(
                &make_batch(&[(
                    ent("robot"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    500,
                )]),
                "entity_id",
            )
            .unwrap();

        let batch = source.to_log_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema(), topology_schema());

        let mut dest = TransformTree::new();
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 3);

        // Not just the final state — the history replays too.
        assert_eq!(
            dest.current_parent(ent("robot")),
            Some(PrescribedId::astronomical_from_name("ICRF").unwrap())
        );
        assert_eq!(
            dest.parent_at(ent("robot"), ns(200)),
            Some(ent("facility")),
            "recipient must be able to replay history, not just the snapshot"
        );
        assert_eq!(
            dest.ancestry_at(ent("robot"), ns(200)).unwrap(),
            vec![
                ent("robot"),
                ent("facility"),
                PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
            ]
        );
    }

    #[test]
    fn test_merge_log_batch_is_idempotent() {
        // Re-syncing with a peer is the expected federation pattern.
        let mut source = TransformTree::new();
        source
            .ingest_batch(
                &make_batch(&[(ent("robot"), ent("facility"), 100)]),
                "entity_id",
            )
            .unwrap();
        source
            .ingest_batch(
                &make_batch(&[(
                    ent("robot"),
                    PrescribedId::astronomical_from_name("ICRF").unwrap(),
                    500,
                )]),
                "entity_id",
            )
            .unwrap();
        let batch = source.to_log_batch().unwrap();

        let mut dest = TransformTree::new();
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 2);
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 0, "nothing new");
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 0);
        assert_eq!(dest.log_len(), 2, "re-merging must not lengthen the log");

        // And history still replays correctly afterwards.
        assert_eq!(dest.parent_at(ent("robot"), ns(200)), Some(ent("facility")));
        assert_eq!(
            dest.current_parent(ent("robot")),
            Some(PrescribedId::astronomical_from_name("ICRF").unwrap())
        );
    }

    #[test]
    fn test_merge_log_batch_keeps_distinct_events_that_look_similar() {
        let mut source = TransformTree::new();
        source
            .ingest_batch(
                &make_batch(&[
                    (
                        ent("robot"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        100,
                    ),
                    (ent("robot"), ent("truck"), 200),
                    (
                        ent("robot"),
                        PrescribedId::astronomical_from_name("ICRF").unwrap(),
                        300,
                    ),
                ]),
                "entity_id",
            )
            .unwrap();

        let mut dest = TransformTree::new();
        assert_eq!(
            dest.merge_log_batch(&source.to_log_batch().unwrap())
                .unwrap(),
            3,
            "the two ICRF edges are distinct facts, one per epoch"
        );
        assert_eq!(dest.parent_at(ent("robot"), ns(250)), Some(ent("truck")));
    }

    #[test]
    fn test_merge_log_batch_rejects_cycle() {
        let mut a = TransformTree::new();
        a.ingest_batch(&make_batch(&[(ent("A"), ent("B"), 100)]), "entity_id")
            .unwrap();
        let mut b = TransformTree::new();
        b.ingest_batch(&make_batch(&[(ent("B"), ent("A"), 100)]), "entity_id")
            .unwrap();

        let err = a.merge_log_batch(&b.to_log_batch().unwrap()).unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert_eq!(
            a.current_parent(ent("A")),
            Some(ent("B")),
            "a rejected merge must leave the tree untouched"
        );
        assert!(a.current_parent(ent("B")).is_none());
    }

    #[test]
    fn test_merge_log_batch_out_of_order_does_not_roll_back() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[(
                ent("robot"),
                PrescribedId::astronomical_from_name("ICRF").unwrap(),
                500,
            )]),
            "entity_id",
        )
        .unwrap();

        // A backfilled event older than what we already know.
        let mut other = TransformTree::new();
        other
            .ingest_batch(
                &make_batch(&[(
                    ent("robot"),
                    PrescribedId::astronomical_from_name("IAU_EARTH").unwrap(),
                    100,
                )]),
                "entity_id",
            )
            .unwrap();
        tree.merge_log_batch(&other.to_log_batch().unwrap())
            .unwrap();

        assert_eq!(
            tree.current_parent(ent("robot")),
            Some(PrescribedId::astronomical_from_name("ICRF").unwrap()),
            "an older event must not roll `latest` backwards"
        );
        assert_eq!(
            tree.parent_at(ent("robot"), ns(200)),
            Some(PrescribedId::astronomical_from_name("IAU_EARTH").unwrap())
        );
    }

    #[test]
    fn test_empty_log_round_trip() {
        let tree = TransformTree::new();
        let batch = tree.to_log_batch().unwrap();
        assert_eq!(batch.num_rows(), 0);
        let mut dest = TransformTree::new();
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 0);
        assert!(dest.is_empty());
    }
}
