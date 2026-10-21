//! Bounded, transport-independent GPX ingestion with explicit submission outcomes.

use std::fmt;
use std::future::Future;
use std::io::BufRead;

use anise::prelude::Almanac;
use arrow::record_batch::RecordBatch;

use super::{
    GpxContext, GpxError, GpxErrorKind, GpxEvent, GpxMapper, GpxMapping, GpxReader, MappedGpxPoint,
    WaypointPolicy,
};
use crate::schemas::entity::EntityBuilder;

/// Mapping failures may be skipped explicitly. XML/reader failures always stop:
/// the reader cannot safely resume at a later point after a structural error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpxPointErrorPolicy {
    Strict,
    SkipInvalid,
}

/// Limits the number of rows held before each submission, independently of the
/// ledger's history retention policy and the transport's encoded message limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpxImportOptions {
    max_rows: usize,
    point_errors: GpxPointErrorPolicy,
}

impl GpxImportOptions {
    /// Accept 1–65,536 rows per batch. The upper bound guards builder allocation;
    /// it is not a wire-size guarantee. Choose a smaller value for smaller Flight
    /// message limits. No batch is sorted, deduplicated or automatically retried.
    pub fn new(max_rows: usize, point_errors: GpxPointErrorPolicy) -> Result<Self, String> {
        if !(1..=65_536).contains(&max_rows) {
            return Err("GPX batch size must be between 1 and 65,536 rows".into());
        }
        Ok(Self {
            max_rows,
            point_errors,
        })
    }

    pub fn max_rows(self) -> usize {
        self.max_rows
    }

    pub fn point_errors(self) -> GpxPointErrorPolicy {
        self.point_errors
    }
}

impl Default for GpxImportOptions {
    fn default() -> Self {
        Self {
            max_rows: 1024,
            point_errors: GpxPointErrorPolicy::Strict,
        }
    }
}

/// A failed submission can be definitely rejected or have an unknown outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpxSubmitOutcome {
    /// The sink guarantees none of this batch was appended.
    Rejected,
    /// Some or all rows may have been appended. Do not retry automatically.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpxSubmitError {
    pub message: String,
    pub outcome: GpxSubmitOutcome,
}

impl GpxSubmitError {
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome: GpxSubmitOutcome::Rejected,
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome: GpxSubmitOutcome::Unknown,
        }
    }
}

impl fmt::Display for GpxSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.outcome, self.message)
    }
}

impl std::error::Error for GpxSubmitError {}

/// Bounded summary of one batch in original file order. Endpoints identify the
/// accepted prefix, not a continuous trajectory; use observer events for every
/// row's source position, skipped points and segment boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpxBatchInfo {
    /// One-based submission ordinal.
    pub index: usize,
    pub rows: usize,
    pub first: GpxContext,
    pub last: GpxContext,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GpxImportFailure {
    Read(GpxError),
    Mapping(GpxError),
    Submit {
        error: GpxSubmitError,
        batch: GpxBatchInfo,
    },
}

impl fmt::Display for GpxImportFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) | Self::Mapping(error) => error.fmt(f),
            Self::Submit { error, batch } => write!(
                f,
                "batch {} ({} rows, {} through {}): {error}",
                batch.index, batch.rows, batch.first, batch.last
            ),
        }
    }
}

impl std::error::Error for GpxImportFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) | Self::Mapping(error) => Some(error),
            Self::Submit { error, .. } => Some(error),
        }
    }
}

/// Import accounting, retained in constant space regardless of file length.
///
/// `read = converted + skipped + failed` and
/// `converted = submitted + rejected + uncertain + unsubmitted`.
/// A malformed point that never became a reader `Point` event is represented by
/// `failure`, not included in `read` or `failed`. Counts describe only the consumed
/// prefix when the import stops early; they do not estimate the unread remainder.
///
/// Confirmed submission means the sink acknowledged append. It does not promise
/// disk durability, retained history, uniqueness or a whole-file transaction.
/// Assumptions and complete diagnostics are streamed through the observer; this
/// report keeps only endpoints and the last skipped error to avoid unbounded logs.
#[derive(Debug, Clone, PartialEq)]
pub struct GpxImportReport {
    pub source: String,
    pub options: GpxImportOptions,
    pub waypoint_policy: WaypointPolicy,
    /// Complete point events received from the reader.
    pub read: usize,
    pub converted: usize,
    /// Rows whose append the sink confirmed.
    pub submitted: usize,
    /// Points whose mapping failed under `SkipInvalid`.
    pub skipped: usize,
    /// Points whose mapping failed under `Strict` (at most one).
    pub failed: usize,
    /// Converted rows definitely rejected by the sink.
    pub rejected: usize,
    /// Converted rows with an unknown submission outcome.
    pub uncertain: usize,
    /// Converted rows never passed to the sink, e.g. buffered at a fatal error.
    pub unsubmitted: usize,
    pub submitted_batches: usize,
    /// True only after validated EOF and acknowledgement of all output batches.
    /// Explicitly skipped points do not prevent completion under `SkipInvalid`.
    pub complete: bool,
    pub last_submitted: Option<GpxBatchInfo>,
    /// Converted-but-unsubmitted batch when a fatal input/mapping error occurs.
    pub pending: Option<GpxBatchInfo>,
    pub last_skipped: Option<GpxError>,
    pub failure: Option<GpxImportFailure>,
}

/// Streaming provenance, suitable for a caller-owned sidecar or diagnostic log.
/// The importer does not retain these events. `Mapped` precedes submission: use
/// `Submitted`/the final report to distinguish conversion from acceptance.
/// The copied mapping includes identity, frame, estimate, height, time and attitude
/// policies; the mapped point includes actual time origin and converted values.
/// `Input` includes all metadata, ignored elements and even empty segment events.
#[derive(Debug)]
pub enum GpxImportEvent<'a> {
    Input(&'a GpxEvent),
    Mapped {
        point: &'a MappedGpxPoint,
        mapping: GpxMapping<'a>,
    },
    Skipped(&'a GpxError),
    Submitted(&'a GpxBatchInfo),
    Failed(&'a GpxImportFailure),
}

/// Convert and submit a file incrementally using a synchronous append callback.
///
/// Each call to `submit` receives at most `options.max_rows()` standard entity
/// rows. Return `Ok(())` only when the entire batch is acknowledged; classify a
/// failure as rejected only when no rows were accepted, otherwise as unknown.
/// A local ledger callback can map `Ledger::append` errors to
/// [`GpxSubmitError::rejected`](crate::gps::gpx::GpxSubmitError::rejected).
///
/// Pass a fresh reader; a previously consumed reader is rejected. Stops at the
/// first fatal read, strict mapping or submission error. Earlier
/// appends remain; buffered rows are not flushed after an input failure. There
/// are no retries or duplicate checks: running this function again appends again.
/// Batches may cross objects/segments; stream provenance to a sidecar if needed.
/// The importer holds one builder/batch plus current XML/point state, not the
/// entire input. A sink or observer that collects everything can still grow memory.
pub fn import_gpx<R, S, O>(
    reader: GpxReader<R>,
    mapper: &GpxMapper<'_>,
    almanac: &Almanac,
    options: GpxImportOptions,
    mut submit: S,
    observe: O,
) -> GpxImportReport
where
    R: BufRead,
    S: FnMut(RecordBatch) -> Result<(), GpxSubmitError>,
    O: FnMut(GpxImportEvent<'_>),
{
    let mut import = Import::new(reader, mapper, almanac, options, observe);
    while let Some((batch, info)) = import.next_batch() {
        let result = submit(batch);
        import.record_submission(info, result);
    }
    import.report
}

/// Asynchronous version of [`import_gpx`](crate::gps::gpx::import_gpx), with the
/// same accounting and bounded buffering. Submissions are awaited sequentially;
/// the next batch is not read until the previous outcome is known.
///
/// A Flight adapter should send one unsplit batch per `do_put`, preflight actual
/// encoded message sizes, and treat RPC errors as unknown acceptance. Clone the
/// client's channel into each callback future if it cannot borrow a mutable client.
/// Cancellation/panic can lose this in-memory report after an append; retain an
/// external journal if recovery across process interruption is required.
pub async fn import_gpx_async<R, S, F, O>(
    reader: GpxReader<R>,
    mapper: &GpxMapper<'_>,
    almanac: &Almanac,
    options: GpxImportOptions,
    mut submit: S,
    observe: O,
) -> GpxImportReport
where
    R: BufRead,
    S: FnMut(RecordBatch) -> F,
    F: Future<Output = Result<(), GpxSubmitError>>,
    O: FnMut(GpxImportEvent<'_>),
{
    let mut import = Import::new(reader, mapper, almanac, options, observe);
    while let Some((batch, info)) = import.next_batch() {
        let result = submit(batch).await;
        import.record_submission(info, result);
    }
    import.report
}

struct Import<'a, R: BufRead, O> {
    reader: GpxReader<R>,
    mapper: &'a GpxMapper<'a>,
    almanac: &'a Almanac,
    observe: O,
    builder: EntityBuilder,
    first: Option<GpxContext>,
    last: Option<GpxContext>,
    eof: bool,
    report: GpxImportReport,
}

impl<'a, R: BufRead, O: FnMut(GpxImportEvent<'_>)> Import<'a, R, O> {
    fn new(
        reader: GpxReader<R>,
        mapper: &'a GpxMapper<'a>,
        almanac: &'a Almanac,
        options: GpxImportOptions,
        observe: O,
    ) -> Self {
        let report = GpxImportReport {
            source: reader.source().into(),
            options,
            waypoint_policy: reader.waypoint_policy(),
            read: 0,
            converted: 0,
            submitted: 0,
            skipped: 0,
            failed: 0,
            rejected: 0,
            uncertain: 0,
            unsubmitted: 0,
            submitted_batches: 0,
            complete: false,
            last_submitted: None,
            pending: None,
            last_skipped: None,
            failure: None,
        };
        let mut import = Self {
            reader,
            mapper,
            almanac,
            observe,
            builder: EntityBuilder::new(options.max_rows),
            first: None,
            last: None,
            eof: false,
            report,
        };
        if import.reader.is_started() {
            import.fail(GpxImportFailure::Read(GpxError {
                context: GpxContext {
                    source: import.reader.source().into(),
                    object: None,
                    segment: None,
                    point: None,
                },
                kind: GpxErrorKind::InvalidPolicy(
                    "import requires a fresh, unconsumed GPX reader".into(),
                ),
            }));
        } else if import.reader.source() != mapper.source {
            import.fail(GpxImportFailure::Mapping(GpxError {
                context: GpxContext {
                    source: import.reader.source().into(),
                    object: None,
                    segment: None,
                    point: None,
                },
                kind: GpxErrorKind::InvalidPolicy(
                    "reader source does not match this mapper's source".into(),
                ),
            }));
        }
        import
    }

    fn buffered_info(&self) -> Option<GpxBatchInfo> {
        Some(GpxBatchInfo {
            index: self.report.submitted_batches + 1,
            rows: self.builder.len(),
            first: self.first.clone()?,
            last: self.last.clone()?,
        })
    }

    fn fail(&mut self, failure: GpxImportFailure) {
        self.report.pending = self.buffered_info();
        (self.observe)(GpxImportEvent::Failed(&failure));
        self.report.failure = Some(failure);
    }

    fn next_batch(&mut self) -> Option<(RecordBatch, GpxBatchInfo)> {
        if self.report.failure.is_some() {
            return None;
        }
        while !self.eof && self.builder.len() < self.report.options.max_rows {
            let event = match self.reader.next() {
                Some(Ok(event)) => event,
                Some(Err(error)) => {
                    self.fail(GpxImportFailure::Read(error));
                    return None;
                }
                None => {
                    // A successful fresh reader always emits DocumentEnd.
                    self.fail(GpxImportFailure::Read(GpxError {
                        context: GpxContext {
                            source: self.report.source.clone(),
                            object: None,
                            segment: None,
                            point: None,
                        },
                        kind: GpxErrorKind::Structure(
                            "input ended without a validated DocumentEnd event".into(),
                        ),
                    }));
                    return None;
                }
            };
            (self.observe)(GpxImportEvent::Input(&event));
            match event {
                GpxEvent::DocumentEnd => self.eof = true,
                GpxEvent::Point(point) => {
                    self.report.read += 1;
                    match self.mapper.convert(&point, self.almanac) {
                        Ok(mapped) => {
                            let mapping =
                                self.mapper.mappings[&point.context.object.expect("mapped object")];
                            (self.observe)(GpxImportEvent::Mapped {
                                point: &mapped,
                                mapping,
                            });
                            mapped.converted.append_to(&mut self.builder);
                            self.first.get_or_insert_with(|| point.context.clone());
                            self.last = Some(point.context);
                            self.report.converted += 1;
                            self.report.unsubmitted += 1;
                        }
                        Err(error)
                            if self.report.options.point_errors
                                == GpxPointErrorPolicy::SkipInvalid =>
                        {
                            self.report.skipped += 1;
                            (self.observe)(GpxImportEvent::Skipped(&error));
                            self.report.last_skipped = Some(error);
                        }
                        Err(error) => {
                            self.report.failed += 1;
                            self.fail(GpxImportFailure::Mapping(error));
                            return None;
                        }
                    }
                }
                _ => {}
            }
        }
        if self.builder.is_empty() {
            self.report.complete = self.eof;
            return None;
        }
        let info = self
            .buffered_info()
            .expect("nonempty builder has endpoints");
        let batch = self.builder.flush();
        self.first = None;
        self.last = None;
        Some((batch, info))
    }

    fn record_submission(&mut self, info: GpxBatchInfo, result: Result<(), GpxSubmitError>) {
        self.report.unsubmitted -= info.rows;
        match result {
            Ok(()) => {
                self.report.submitted += info.rows;
                self.report.submitted_batches += 1;
                (self.observe)(GpxImportEvent::Submitted(&info));
                self.report.last_submitted = Some(info);
            }
            Err(error) => {
                match error.outcome {
                    GpxSubmitOutcome::Rejected => self.report.rejected += info.rows,
                    GpxSubmitOutcome::Unknown => self.report.uncertain += info.rows,
                }
                self.fail(GpxImportFailure::Submit { error, batch: info });
            }
        }
    }
}
