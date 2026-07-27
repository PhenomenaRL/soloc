//! Row-derived transform topology — who is parented to whom, and since when.
//!
//! Topology is not declared through a side channel; it is *derived* from the data.
//! Every spacetimestamp row already says "entity X is at this pose, expressed relative
//! to `frame_id`, as of this epoch" — so scanning `(id, frame_id, epoch)` triples as
//! batches are appended reconstructs the full parent graph, including its history.
//!
//! # Node identity
//!
//! A **child** is always an entity id (the value of the ledger's `id_column`).
//! A **parent** is one of:
//! - another entity id — detected by [`is_entity_uri`], resolution recurses;
//! - an astronomical frame name (`ICRF`, `IAU_EARTH`, …) — terminal, handed to `anise`;
//! - a raw NAIF integer id (`"499"`) — terminal.
//!
//! There is no stored edge *kind*: a camera rigidly bolted to a chassis and a spacecraft
//! orbiting a planet are expressed identically in the data, and so are treated identically
//! here. Names are used verbatim — this module never namespaces or qualifies anything.
//!
//! # Ingest algorithm
//!
//! [`TransformTree::ingest_batch`] runs three passes, all over narrow columns only —
//! it never touches `position`, `quaternion`, or covariance:
//!
//! - **Floating-frame check** — over the `frame_id` dictionary *values*, so its cost is
//!   O(distinct frame names in the batch), independent of row count. A name that is
//!   neither entity-URI-shaped, NAIF-numeric, nor resolvable by `anise` rejects the
//!   whole batch. This is what catches typos at append time.
//! - **Pass A** — one O(N) sweep over `(id_key, frame_key, duration_centuries,
//!   duration_ns)`. Per distinct id it records the argmax-epoch row (ties: first row
//!   wins, matching `Ledger::resolve_frame_at`) and whether more than one distinct
//!   frame key was seen.
//! - **Pass B** — bounded by k = distinct ids. Resolves dictionary keys to strings and
//!   flags "dirty" ids: first sighting, differing parent, or `multi_frame` from Pass A.
//!   The common case (steady-state pose updates, no re-parenting) stops here.
//! - **Pass C** — dirty ids only. Sorts those rows by `(id, epoch)` and walks them in
//!   order, emitting an event at every parent change — not just the final state — so a
//!   multi-hop re-parent inside one batch is captured faithfully.
//!
//! Cycle detection then runs over the staged result. Nothing is committed until every
//! check passes, so a rejected batch leaves the tree exactly as it was.

use arrow::array::{
    Array, ArrayRef, AsArray, DictionaryArray, Int16Array, Int16Builder, RecordBatch, StringArray,
    StringBuilder, UInt64Array, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt32Type};
use hifitime::Duration;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ephemeris::is_valid_astronomical_frame;
use crate::schema::{STS_COLUMN, is_entity_uri};

/// A single parent change: `child_id` became parented to `parent_id` at `epoch`.
///
/// `epoch` is a duration offset from the J2000 TAI epoch, matching the storage
/// convention of the `duration_centuries` / `duration_ns` columns. Callers are
/// responsible for having normalised timestamps to TAI first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyEvent {
    /// The entity whose parent changed.
    pub child_id: String,
    /// The frame or entity it is now expressed relative to.
    pub parent_id: String,
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
    pub latest_rows: HashMap<String, usize>,
}

/// The parent graph derived from ingested rows.
///
/// Stores topology only — never a pose value. `latest` is the O(1) fast path for
/// "who is X's parent right now"; `log` is the append-only history that makes
/// "who was X's parent at epoch E" answerable. There is no deletion: a topology
/// correction is just another event that supersedes the old edge, mirroring how
/// the ledger already corrects bad poses.
#[derive(Debug, Clone, Default)]
pub struct TransformTree {
    /// child_id → (parent_id, epoch the edge took effect).
    latest: HashMap<String, (String, Duration)>,
    /// Every parent change ever seen, in application order.
    log: Vec<TopologyEvent>,
    /// Astronomical names already validated as reachable — memoises `anise` lookups.
    known_external: HashSet<String>,
}

/// Per-id state accumulated during Pass A. Keyed by dictionary key, not string,
/// so the hot loop never touches the dictionary values buffer.
struct IdScan {
    /// Row index of the highest epoch seen so far (first row wins on ties).
    best_row: usize,
    best_dur: Duration,
    /// Frame key of the first row seen for this id.
    first_frame_key: u32,
    /// Set when a second, different frame key shows up for this id.
    multi_frame: bool,
}

/// The four narrow columns topology derivation needs, located once up front.
struct NarrowColumns<'a> {
    id_keys: &'a DictionaryArray<UInt32Type>,
    id_values: &'a StringArray,
    frame_keys: &'a DictionaryArray<UInt32Type>,
    frame_values: &'a StringArray,
    centuries: &'a Int16Array,
    nanos: &'a UInt64Array,
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
    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    /// Returns `child_id`'s current parent, ignoring history.
    pub fn current_parent(&self, child_id: &str) -> Option<&str> {
        self.latest.get(child_id).map(|(p, _)| p.as_str())
    }

    /// Returns `child_id`'s parent as of `epoch`, or `None` if it had none yet.
    ///
    /// Uses the O(1) `latest` entry whenever the current edge was already in effect
    /// at `epoch`; only genuinely historical queries fall back to replaying `log`,
    /// whose length is bounded by the number of re-parentings ever seen — not by
    /// the number of rows in the ledger.
    pub fn parent_at(&self, child_id: &str, epoch: Duration) -> Option<&str> {
        let (parent, since) = self.latest.get(child_id)?;
        if *since <= epoch {
            return Some(parent.as_str());
        }
        self.log
            .iter()
            .rev()
            .find(|e| e.child_id == child_id && e.epoch <= epoch)
            .map(|e| e.parent_id.as_str())
    }

    /// Returns the chain of node ids from `entity_id` up to its astronomical root,
    /// as it stood at `epoch`.
    ///
    /// The first element is always `entity_id` and the last is a terminal frame name
    /// (an astronomical frame or a raw NAIF id) that `anise` is expected to resolve.
    /// Every intermediate element is another entity whose pose must be looked up.
    ///
    /// This is a purely structural walk — no pose is read. The caller performs the
    /// per-hop numeric lookups and composes the isometries.
    pub fn resolve_chain(&self, entity_id: &str, epoch: Duration) -> Result<Vec<String>, String> {
        let mut chain = vec![entity_id.to_string()];
        let mut visiting: HashSet<&str> = HashSet::new();
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

            chain.push(parent.to_string());

            if !is_entity_uri(parent) {
                // Terminal: an astronomical frame name or raw NAIF id.
                return Ok(chain);
            }
            if !self.latest.contains_key(parent) {
                // An entity-shaped name we have no rows for — treat as terminal and
                // let the caller's almanac decide whether it means anything.
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
    /// The batch is rejected as a whole — leaving this tree untouched — if it names a
    /// frame that cannot exist, or if applying it would create a cycle.
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

        // --- Floating-frame check ------------------------------------------------
        // Runs over dictionary values, so its cost is O(distinct frame names), not O(N).
        // Collected first because a bad name invalidates the whole batch.
        let mut newly_external: Vec<String> = Vec::new();
        for i in 0..cols.frame_values.len() {
            if cols.frame_values.is_null(i) {
                continue;
            }
            let name = cols.frame_values.value(i);
            if self.known_external.contains(name) || is_entity_uri(name) {
                continue;
            }
            if name.parse::<i32>().is_ok() || is_valid_astronomical_frame(name) {
                newly_external.push(name.to_string());
                continue;
            }
            return Err(format!(
                "Unreachable frame '{name}': not an entity id, a NAIF id, or a frame anise recognises"
            ));
        }

        // --- Pass A: one O(N) sweep over four narrow columns ---------------------
        let mut scans: HashMap<u32, IdScan> = HashMap::new();
        let id_key_buf = cols.id_keys.keys();
        let frame_key_buf = cols.frame_keys.keys();

        for row in 0..batch.num_rows() {
            let id_key = id_key_buf.value(row);
            let frame_key = frame_key_buf.value(row);
            let dur = Duration::from_parts(cols.centuries.value(row), cols.nanos.value(row));

            match scans.get_mut(&id_key) {
                None => {
                    scans.insert(
                        id_key,
                        IdScan {
                            best_row: row,
                            best_dur: dur,
                            first_frame_key: frame_key,
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
                    // Dictionary keys are per-batch and canonical, so a key mismatch
                    // means a genuinely different name. A non-canonical dictionary could
                    // only cause a spurious Pass C, which then emits nothing.
                    if frame_key != scan.first_frame_key {
                        scan.multi_frame = true;
                    }
                }
            }
        }

        // --- Pass B: bounded by k = distinct ids ---------------------------------
        let mut latest_rows: HashMap<String, usize> = HashMap::with_capacity(scans.len());
        let mut dirty_keys: HashSet<u32> = HashSet::new();

        for (&id_key, scan) in &scans {
            let id = cols.id_values.value(id_key as usize);
            latest_rows.insert(id.to_string(), scan.best_row);

            let winning_frame = cols
                .frame_values
                .value(frame_key_buf.value(scan.best_row) as usize);

            let dirty = scan.multi_frame
                || match self.latest.get(id) {
                    None => true,
                    Some((parent, _)) => parent != winning_frame,
                };
            if dirty {
                dirty_keys.insert(id_key);
            }
        }

        if dirty_keys.is_empty() {
            // Steady state: poses moved, topology did not. Commit the memoised frame
            // names so later batches skip the anise lookups, and stop.
            self.known_external.extend(newly_external);
            return Ok(IngestOutcome {
                events: Vec::new(),
                latest_rows,
            });
        }

        // --- Pass C: dirty ids only, in (id, epoch) order ------------------------
        let mut dirty_rows: Vec<(u32, Duration, usize)> = (0..batch.num_rows())
            .filter(|&row| dirty_keys.contains(&id_key_buf.value(row)))
            .map(|row| {
                (
                    id_key_buf.value(row),
                    Duration::from_parts(cols.centuries.value(row), cols.nanos.value(row)),
                    row,
                )
            })
            .collect();
        // Stable sort so equal-epoch rows stay in batch order — same tie-break as Pass A.
        dirty_rows.sort_by_key(|r| (r.0, r.1));

        let mut events: Vec<TopologyEvent> = Vec::new();
        // Staged edges: applied only once every check below passes.
        let mut staged: HashMap<&str, (&str, Duration)> = HashMap::new();

        let mut group_id_key: Option<u32> = None;
        let mut running_parent: Option<&str> = None;

        for (id_key, dur, row) in dirty_rows {
            let id = cols.id_values.value(id_key as usize);
            if group_id_key != Some(id_key) {
                group_id_key = Some(id_key);
                // Start each id from the parent it had *before* this batch.
                running_parent = self.latest.get(id).map(|(p, _)| p.as_str());
            }

            let frame = cols.frame_values.value(frame_key_buf.value(row) as usize);
            if running_parent == Some(frame) {
                continue;
            }
            running_parent = Some(frame);
            staged.insert(id, (frame, dur));
            events.push(TopologyEvent {
                child_id: id.to_string(),
                parent_id: frame.to_string(),
                epoch: dur,
            });
        }

        // --- Cycle check over base tree + staged edges ---------------------------
        // One reusable visited set: allocating per child dominates when a batch
        // introduces many entities at once. The overwhelmingly common edge — an entity
        // parented straight to an astronomical anchor — costs a single `is_entity_uri`
        // and never walks or allocates at all.
        let mut seen: HashSet<&str> = HashSet::new();
        for (&child, &(parent, _)) in &staged {
            if !is_entity_uri(parent) {
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
                    .get(node)
                    .map(|(p, _)| *p)
                    .or_else(|| self.latest.get(node).map(|(p, _)| p.as_str()))
                else {
                    break; // reached a root
                };
                if !is_entity_uri(next) {
                    break; // terminal astronomical anchor
                }
                node = next;
            }
        }

        // --- Commit --------------------------------------------------------------
        self.known_external.extend(newly_external);
        for event in &events {
            self.apply_event(event.clone());
        }

        Ok(IngestOutcome {
            events,
            latest_rows,
        })
    }

    /// Records `event` in the log and advances `latest` if it is not superseded.
    ///
    /// An event older than the child's current edge is still logged — it belongs to the
    /// history — but does not roll `latest` backwards. This is what makes out-of-order
    /// federation merges safe.
    fn apply_event(&mut self, event: TopologyEvent) {
        let supersedes = match self.latest.get(&event.child_id) {
            None => true,
            Some((_, since)) => event.epoch >= *since,
        };
        if supersedes {
            self.latest.insert(
                event.child_id.clone(),
                (event.parent_id.clone(), event.epoch),
            );
        }
        self.log.push(event);
    }

    // -----------------------------------------------------------------------
    // Federation
    // -----------------------------------------------------------------------

    /// Exports the full event log as a [`RecordBatch`] following [`topology_schema`].
    ///
    /// The whole log is exported, not just the current snapshot, so a recipient can
    /// replay history rather than only seeing where everything ended up.
    pub fn to_log_batch(&self) -> Result<RecordBatch, String> {
        let mut child = StringBuilder::with_capacity(self.log.len(), self.log.len() * 24);
        let mut parent = StringBuilder::with_capacity(self.log.len(), self.log.len() * 24);
        let mut centuries = Int16Builder::with_capacity(self.log.len());
        let mut nanos = UInt64Builder::with_capacity(self.log.len());

        for event in &self.log {
            child.append_value(&event.child_id);
            parent.append_value(&event.parent_id);
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
    pub fn merge_log_batch(&mut self, batch: &RecordBatch) -> Result<usize, String> {
        let child = batch
            .column_by_name("child_id")
            .and_then(|c| c.as_string_opt::<i32>())
            .ok_or_else(|| "topology batch is missing a Utf8 'child_id' column".to_string())?;
        let parent = batch
            .column_by_name("parent_id")
            .and_then(|c| c.as_string_opt::<i32>())
            .ok_or_else(|| "topology batch is missing a Utf8 'parent_id' column".to_string())?;
        let centuries = batch
            .column_by_name("duration_centuries")
            .and_then(|c| c.as_any().downcast_ref::<Int16Array>())
            .ok_or_else(|| {
                "topology batch is missing an Int16 'duration_centuries' column".to_string()
            })?;
        let nanos = batch
            .column_by_name("duration_ns")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| "topology batch is missing a UInt64 'duration_ns' column".to_string())?;

        let events: Vec<TopologyEvent> = (0..batch.num_rows())
            .map(|row| TopologyEvent {
                child_id: child.value(row).to_string(),
                parent_id: parent.value(row).to_string(),
                epoch: Duration::from_parts(centuries.value(row), nanos.value(row)),
            })
            .collect();

        // Validate against a trial copy so a cyclic log leaves this tree untouched.
        let mut trial = self.clone();
        for event in &events {
            trial.apply_event(event.clone());
        }
        trial.check_acyclic()?;

        let applied = events.len();
        *self = trial;
        Ok(applied)
    }

    /// Returns `Err` if any child's parent chain loops back on itself.
    fn check_acyclic(&self) -> Result<(), String> {
        // One reusable visited set, and no walk at all for the common case of an entity
        // parented straight to an astronomical anchor — same shaping as the ingest-time
        // check, which matters here because this runs over every known child.
        let mut seen: HashSet<&str> = HashSet::new();
        for (start, (parent, _)) in &self.latest {
            if !is_entity_uri(parent) {
                continue;
            }
            seen.clear();
            seen.insert(start.as_str());
            let mut node = parent.as_str();
            loop {
                if !seen.insert(node) {
                    return Err(format!(
                        "Cycle detected in frame topology involving '{node}'"
                    ));
                }
                match self.latest.get(node) {
                    Some((next, _)) if is_entity_uri(next) => node = next.as_str(),
                    _ => break, // reached a root or a terminal anchor
                }
            }
        }
        Ok(())
    }
}

/// The Arrow schema for an exported topology log.
///
/// Four plain columns, deliberately *not* dictionary-encoded: a topology log is short
/// (one row per re-parenting ever) and is meant to be trivially readable by any Arrow
/// implementation without dictionary handling.
pub fn topology_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("child_id", DataType::Utf8, false),
        Field::new("parent_id", DataType::Utf8, false),
        Field::new("duration_centuries", DataType::Int16, false),
        Field::new("duration_ns", DataType::UInt64, false),
    ]))
}

/// Locates the id column and the four spacetimestamp fields topology derivation reads.
///
/// Handles both layouts: STS fields nested inside a `"spacetimestamp"` struct (the ledger
/// case) or present as top-level columns.
fn locate_columns<'a>(
    batch: &'a RecordBatch,
    id_column: &str,
) -> Result<NarrowColumns<'a>, String> {
    let id_keys = batch
        .column_by_name(id_column)
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
        .ok_or_else(|| {
            format!("id column '{id_column}' is missing or not Dictionary(UInt32, Utf8)")
        })?;
    let id_values = id_keys
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| format!("id column '{id_column}' values are not Utf8"))?;

    let sts = batch
        .column_by_name(STS_COLUMN)
        .map(|c| {
            c.as_struct_opt()
                .ok_or_else(|| format!("'{STS_COLUMN}' column is not a StructArray"))
        })
        .transpose()?;

    let field = |name: &str| -> Option<&'a ArrayRef> {
        match sts {
            Some(s) => s.column_by_name(name),
            None => batch.column_by_name(name),
        }
    };

    let frame_keys = field("frame_id")
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
        .ok_or_else(|| "'frame_id' is missing or not Dictionary(UInt32, Utf8)".to_string())?;
    let frame_values = frame_keys
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| "'frame_id' dictionary values are not Utf8".to_string())?;

    let centuries = field("duration_centuries")
        .and_then(|c| c.as_any().downcast_ref::<Int16Array>())
        .ok_or_else(|| "'duration_centuries' is missing or not Int16".to_string())?;
    let nanos = field("duration_ns")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| "'duration_ns' is missing or not UInt64".to_string())?;

    Ok(NarrowColumns {
        id_keys,
        id_values,
        frame_keys,
        frame_values,
        centuries,
        nanos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use arrow::array::StructArray;
    use arrow::datatypes::{Field, Schema};

    /// Minimal entity-shaped schema: an id column plus a nested spacetimestamp struct.
    fn test_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new(
                "entity_id",
                DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
                false,
            ),
            Field::new(
                STS_COLUMN,
                DataType::Struct(sts_schema(None).fields().clone()),
                false,
            ),
        ]))
    }

    /// Builds a batch from `(entity_id, frame_id, duration_ns)` triples.
    fn make_batch(rows: &[(&str, &str, u64)]) -> RecordBatch {
        use arrow::array::StringDictionaryBuilder;

        let mut ids = StringDictionaryBuilder::<UInt32Type>::new();
        let mut sts = SpaceTimestampBuilder::new(rows.len(), None);
        for (id, frame, ns) in rows {
            ids.append_value(id);
            sts.append_spacetimestamp(
                frame,
                "km",
                "TAI",
                "test:src",
                "MEASURED",
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
                &make_batch(&[("demo:truck", "IAU_EARTH", 100), ("demo:sat", "ICRF", 100)]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 2);
        assert_eq!(tree.current_parent("demo:truck"), Some("IAU_EARTH"));
        assert_eq!(tree.current_parent("demo:sat"), Some("ICRF"));
        assert_eq!(outcome.latest_rows.len(), 2);
    }

    #[test]
    fn test_steady_state_emits_no_events() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[("demo:sat", "ICRF", 100)]), "entity_id")
            .unwrap();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[("demo:sat", "ICRF", 200), ("demo:sat", "ICRF", 300)]),
                "entity_id",
            )
            .unwrap();

        assert!(
            outcome.events.is_empty(),
            "unchanged topology must be quiet"
        );
        assert_eq!(tree.log_len(), 1, "only the first sighting is logged");
        // The edge keeps the epoch it took effect, not the newest row's epoch.
        assert_eq!(tree.latest["demo:sat"].1, ns(100));
    }

    #[test]
    fn test_latest_rows_tracks_argmax_epoch_first_wins_on_tie() {
        let mut tree = TransformTree::new();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    ("demo:sat", "ICRF", 100),
                    ("demo:sat", "ICRF", 900),
                    ("demo:sat", "ICRF", 900),
                    ("demo:sat", "ICRF", 400),
                ]),
                "entity_id",
            )
            .unwrap();
        // Row 1 and row 2 tie at the max epoch; the earlier row must win.
        assert_eq!(outcome.latest_rows["demo:sat"], 1);
    }

    #[test]
    fn test_reparent_across_batches() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[("demo:robot", "demo:facility", 100)]),
            "entity_id",
        )
        .unwrap();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[("demo:robot", "demo:truck", 500)]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].parent_id, "demo:truck");
        assert_eq!(outcome.events[0].epoch, ns(500));
        assert_eq!(tree.current_parent("demo:robot"), Some("demo:truck"));
    }

    #[test]
    fn test_multi_hop_reparent_within_one_batch() {
        // facility → truck → spaceship, all inside a single unsorted batch.
        let mut tree = TransformTree::new();
        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    ("demo:robot", "demo:truck", 200),
                    ("demo:robot", "demo:spaceship", 300),
                    ("demo:robot", "demo:facility", 100),
                ]),
                "entity_id",
            )
            .unwrap();

        let parents: Vec<&str> = outcome
            .events
            .iter()
            .map(|e| e.parent_id.as_str())
            .collect();
        assert_eq!(
            parents,
            vec!["demo:facility", "demo:truck", "demo:spaceship"],
            "every intermediate hop must be logged, in epoch order"
        );
        assert_eq!(tree.current_parent("demo:robot"), Some("demo:spaceship"));
    }

    #[test]
    fn test_reparent_that_reverts_within_one_batch_is_still_logged() {
        // The case epoch-only change detection would miss: the batch's newest row
        // names the same parent the tree already had, but a detour happened between.
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[("demo:robot", "ICRF", 50)]), "entity_id")
            .unwrap();

        let outcome = tree
            .ingest_batch(
                &make_batch(&[
                    ("demo:robot", "demo:truck", 100),
                    ("demo:robot", "ICRF", 200),
                ]),
                "entity_id",
            )
            .unwrap();

        assert_eq!(outcome.events.len(), 2, "the detour must not be swallowed");
        assert_eq!(tree.parent_at("demo:robot", ns(150)), Some("demo:truck"));
        assert_eq!(tree.parent_at("demo:robot", ns(250)), Some("ICRF"));
    }

    #[test]
    fn test_parent_at_historical_replay() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[("demo:robot", "demo:facility", 100)]),
            "entity_id",
        )
        .unwrap();
        tree.ingest_batch(
            &make_batch(&[("demo:robot", "demo:truck", 500)]),
            "entity_id",
        )
        .unwrap();

        assert_eq!(tree.parent_at("demo:robot", ns(50)), None);
        assert_eq!(tree.parent_at("demo:robot", ns(100)), Some("demo:facility"));
        assert_eq!(tree.parent_at("demo:robot", ns(499)), Some("demo:facility"));
        assert_eq!(tree.parent_at("demo:robot", ns(500)), Some("demo:truck"));
        assert_eq!(tree.parent_at("demo:robot", ns(9999)), Some("demo:truck"));
    }

    #[test]
    fn test_resolve_chain_multi_hop() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[
                ("demo:facility", "IAU_EARTH", 100),
                ("demo:truck", "demo:facility", 100),
                ("demo:robot", "demo:truck", 100),
            ]),
            "entity_id",
        )
        .unwrap();

        let chain = tree.resolve_chain("demo:robot", ns(100)).unwrap();
        assert_eq!(
            chain,
            vec!["demo:robot", "demo:truck", "demo:facility", "IAU_EARTH"]
        );
    }

    #[test]
    fn test_resolve_chain_unknown_entity() {
        let tree = TransformTree::new();
        let err = tree.resolve_chain("demo:ghost", ns(0)).unwrap_err();
        assert!(err.contains("demo:ghost"), "error should name it: {err}");
    }

    #[test]
    fn test_resolve_chain_stops_at_entity_with_no_rows() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(
            &make_batch(&[("demo:robot", "demo:never_reported", 100)]),
            "entity_id",
        )
        .unwrap();
        let chain = tree.resolve_chain("demo:robot", ns(100)).unwrap();
        assert_eq!(chain, vec!["demo:robot", "demo:never_reported"]);
    }

    #[test]
    fn test_cycle_rejected_at_ingest() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[("demo:A", "demo:B", 100)]), "entity_id")
            .unwrap();
        let err = tree
            .ingest_batch(&make_batch(&[("demo:B", "demo:A", 100)]), "entity_id")
            .unwrap_err();

        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert!(
            tree.current_parent("demo:B").is_none(),
            "a rejected batch must leave the tree untouched"
        );
        assert_eq!(tree.log_len(), 1);
    }

    #[test]
    fn test_self_cycle_rejected() {
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(&make_batch(&[("demo:A", "demo:A", 100)]), "entity_id")
            .unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert!(tree.is_empty());
    }

    #[test]
    fn test_floating_frame_rejected() {
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(
                &make_batch(&[("demo:sat", "NOT_A_FRAME", 100)]),
                "entity_id",
            )
            .unwrap_err();
        assert!(err.contains("NOT_A_FRAME"), "error should name it: {err}");
        assert!(
            tree.is_empty(),
            "a rejected batch must leave the tree untouched"
        );
    }

    #[test]
    fn test_floating_frame_check_covers_non_winning_rows() {
        // The typo is on the *older* row; only a dictionary-values scan catches it.
        let mut tree = TransformTree::new();
        let err = tree
            .ingest_batch(
                &make_batch(&[("demo:sat", "TYPOFRAME", 100), ("demo:sat", "ICRF", 200)]),
                "entity_id",
            )
            .unwrap_err();
        assert!(err.contains("TYPOFRAME"), "got: {err}");
    }

    #[test]
    fn test_naif_integer_frame_accepted() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[("demo:probe", "499", 100)]), "entity_id")
            .unwrap();
        assert_eq!(tree.current_parent("demo:probe"), Some("499"));
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
            tree.ingest_batch(&make_batch(&[("demo:a", "ICRF", 1)]), "")
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
            .ingest_batch(&make_batch(&[("demo:a", "ICRF", 1)]), "nope")
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
                    ("demo:facility", "IAU_EARTH", 100),
                    ("demo:robot", "demo:facility", 100),
                ]),
                "entity_id",
            )
            .unwrap();
        source
            .ingest_batch(&make_batch(&[("demo:robot", "ICRF", 500)]), "entity_id")
            .unwrap();

        let batch = source.to_log_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema(), topology_schema());

        let mut dest = TransformTree::new();
        assert_eq!(dest.merge_log_batch(&batch).unwrap(), 3);

        // Not just the final state — the history replays too.
        assert_eq!(dest.current_parent("demo:robot"), Some("ICRF"));
        assert_eq!(
            dest.parent_at("demo:robot", ns(200)),
            Some("demo:facility"),
            "recipient must be able to replay history, not just the snapshot"
        );
        assert_eq!(
            dest.resolve_chain("demo:robot", ns(200)).unwrap(),
            vec!["demo:robot", "demo:facility", "IAU_EARTH"]
        );
    }

    #[test]
    fn test_merge_log_batch_rejects_cycle() {
        let mut a = TransformTree::new();
        a.ingest_batch(&make_batch(&[("demo:A", "demo:B", 100)]), "entity_id")
            .unwrap();
        let mut b = TransformTree::new();
        b.ingest_batch(&make_batch(&[("demo:B", "demo:A", 100)]), "entity_id")
            .unwrap();

        let err = a.merge_log_batch(&b.to_log_batch().unwrap()).unwrap_err();
        assert!(err.to_lowercase().contains("cycle"), "got: {err}");
        assert_eq!(
            a.current_parent("demo:A"),
            Some("demo:B"),
            "a rejected merge must leave the tree untouched"
        );
        assert!(a.current_parent("demo:B").is_none());
    }

    #[test]
    fn test_merge_log_batch_out_of_order_does_not_roll_back() {
        let mut tree = TransformTree::new();
        tree.ingest_batch(&make_batch(&[("demo:robot", "ICRF", 500)]), "entity_id")
            .unwrap();

        // A backfilled event older than what we already know.
        let mut other = TransformTree::new();
        other
            .ingest_batch(
                &make_batch(&[("demo:robot", "IAU_EARTH", 100)]),
                "entity_id",
            )
            .unwrap();
        tree.merge_log_batch(&other.to_log_batch().unwrap())
            .unwrap();

        assert_eq!(
            tree.current_parent("demo:robot"),
            Some("ICRF"),
            "an older event must not roll `latest` backwards"
        );
        assert_eq!(tree.parent_at("demo:robot", ns(200)), Some("IAU_EARTH"));
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
