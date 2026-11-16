//! Complete GPX playback exports with ITRF93 frame poses and row provenance.
//!
//! Stored observations keep their original identity, pose, metres and TAI epoch.
//! The additional ITRF93 timeline rotates them into an Earth-centred GCRF scene;
//! this is not a solar-system ephemeris. A viewer must interpret the versioned
//! playback metadata and the four provenance columns to avoid joining gaps.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use anise::prelude::{Almanac, Epoch};
use arrow::array::{ArrayRef, StringArray, UInt64Array, new_null_array};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, TimeScale};
use spacetimestamp::gps::gpx::{
    GpxContext, GpxImportEvent, GpxImportOptions, GpxImportReport, GpxMapper, GpxObjectId,
    GpxReader, GpxSubmitError, import_gpx,
};
use spacetimestamp::gps::{check_itrf93_interval, itrf93_frame_snapshot};
use spacetimestamp::identity::ASTRO_AUTHORITY;
use spacetimestamp::schemas::entity::entity_schema;
use spacetimestamp::{PrescribedId, epoch_from_parts};

use crate::ledger::Ledger;

/// Import and frame-sampling limits. The display retains the entire accepted
/// journey; bounded import batches do not bound the size of the resulting ledger.
#[derive(Debug, Clone, Copy)]
pub struct GpxPlaybackOptions {
    import: GpxImportOptions,
    frame_step_seconds: f64,
    max_frame_samples: usize,
}

impl GpxPlaybackOptions {
    /// Frame cadence must be positive, at least one nanosecond, and no longer
    /// than 300 seconds. Every observed epoch is additionally sampled exactly.
    /// `max_frame_samples` bounds the union of observation and cadence epochs,
    /// from 1 through 1,000,000. No kernels are downloaded by this API.
    pub fn new(
        import: GpxImportOptions,
        frame_step_seconds: f64,
        max_frame_samples: usize,
    ) -> Result<Self, String> {
        if !frame_step_seconds.is_finite() || !(1e-9..=300.0).contains(&frame_step_seconds) {
            return Err("frame cadence must be finite and between 1 ns and 300 seconds".into());
        }
        if !(1..=1_000_000).contains(&max_frame_samples) {
            return Err("max_frame_samples must be between 1 and 1,000,000".into());
        }
        Ok(Self {
            import,
            frame_step_seconds,
            max_frame_samples,
        })
    }

    pub fn import_options(self) -> GpxImportOptions {
        self.import
    }

    pub fn frame_step_seconds(self) -> f64 {
        self.frame_step_seconds
    }

    pub fn max_frame_samples(self) -> usize {
        self.max_frame_samples
    }
}

impl Default for GpxPlaybackOptions {
    fn default() -> Self {
        Self {
            import: GpxImportOptions::default(),
            frame_step_seconds: 60.0,
            max_frame_samples: 1_000_000,
        }
    }
}

/// Failure to produce a complete, correctly anchored playback artifact.
/// No file is written during import. The report, when present, describes the
/// consumed input and appends to a private ledger that is discarded on failure.
#[derive(Debug)]
pub struct GpxPlaybackError {
    pub message: String,
    pub report: Option<Box<GpxImportReport>>,
}

impl fmt::Display for GpxPlaybackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for GpxPlaybackError {}

/// A complete display ledger with eviction disabled and provenance attached to
/// the exact rows it describes. Only read access is exposed to preserve that
/// relationship. Name registrations change display labels, never row identity.
pub struct GpxPlaybackExport {
    ledger: Ledger,
    report: GpxImportReport,
    frame_samples: usize,
}

impl fmt::Debug for GpxPlaybackExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpxPlaybackExport")
            .field("report", &self.report)
            .field("frame_samples", &self.frame_samples)
            .finish_non_exhaustive()
    }
}

impl GpxPlaybackExport {
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn report(&self) -> &GpxImportReport {
        &self.report
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn register_name(
        &mut self,
        id: PrescribedId,
        authority: &str,
        common_name: &str,
    ) -> Result<(), String> {
        self.ledger.register_name(id, authority, common_name)
    }

    /// Writes the normalized Arrow IPC ledger and its `.names.arrow` sibling
    /// using the existing ledger persistence API. Files are not a transaction:
    /// a filesystem error can leave one file written, as with `Ledger::save_ipc`.
    pub fn save_ipc(&self, path: &Path) -> Result<(), String> {
        self.ledger.save_ipc(path)
    }
}

struct PendingPoint {
    context: GpxContext,
    epoch: (i16, u64),
    frame: PrescribedId,
    entity: PrescribedId,
}

/// Build a complete, Earth-centred playback artifact from the actual GPX importer.
///
/// Only ITRF93 observations are accepted; coordinates are never relabelled. An
/// Earth BPC covering the **entire** first-to-last interval is required, including
/// gaps. The frame timeline samples every observation and a uniform cadence.
/// Interpolation of those frame samples remains a display approximation.
///
/// Source rows stay in file order, including duplicate and reversed timestamps.
/// Nullable source/object/segment/point columns allow the viewer to distinguish
/// anchors, skipped points and segment boundaries after chronological sorting.
/// A route's segment is null. Empty segments remain visible in observer events.
///
/// Input is read in bounded batches; only current-batch provenance is buffered.
/// The output ledger retains the whole journey, and a bounded set of distinct
/// observation epochs supports frame sampling. The callback may retain additional
/// memory at the caller's discretion. No files are written until `save_ipc`.
/// Incomplete, empty, unsupported-frame or uncovered imports return an error and
/// their import report; a successful report may contain explicitly skipped points.
pub fn import_gpx_for_playback<R, O>(
    reader: GpxReader<R>,
    mapper: &GpxMapper<'_>,
    almanac: &Almanac,
    options: GpxPlaybackOptions,
    mut observe: O,
) -> Result<GpxPlaybackExport, GpxPlaybackError>
where
    R: BufRead,
    O: FnMut(GpxImportEvent<'_>),
{
    let before_import = |message| GpxPlaybackError {
        message,
        report: None,
    };
    let itrf93 = PrescribedId::astronomical_from_name("ITRF93").map_err(before_import)?;
    let gcrf = PrescribedId::astronomical_from_name("GCRF").map_err(before_import)?;
    let schema = playback_schema(itrf93, options.frame_step_seconds);
    let mut ledger = Ledger::new(&schema, "entity_id").map_err(before_import)?;
    ledger.set_memory_limit(None).map_err(before_import)?;
    let pending = RefCell::new(Vec::<PendingPoint>::with_capacity(
        options.import.max_rows(),
    ));
    let mut epochs = BTreeSet::new();
    let report = import_gpx(
        reader,
        mapper,
        almanac,
        options.import,
        |batch| {
            let points = std::mem::take(&mut *pending.borrow_mut());
            if points.len() != batch.num_rows() {
                return Err(GpxSubmitError::rejected("playback provenance row mismatch"));
            }
            for point in &points {
                if point.frame != itrf93 {
                    return Err(GpxSubmitError::rejected(format!(
                        "{}: playback export requires ITRF93 observations, got {}",
                        point.context, point.frame
                    )));
                }
                if point.entity == itrf93 || point.entity == gcrf {
                    return Err(GpxSubmitError::rejected(format!(
                        "{}: observation identity collides with a playback frame",
                        point.context
                    )));
                }
                epochs.insert(point.epoch);
                if epochs.len() > options.max_frame_samples {
                    return Err(GpxSubmitError::rejected(
                        "observation epochs exceed max_frame_samples",
                    ));
                }
            }
            let batch =
                with_provenance(batch, &schema, Some(&points)).map_err(GpxSubmitError::rejected)?;
            ledger.append(batch).map_err(GpxSubmitError::rejected)
        },
        |event| {
            if let GpxImportEvent::Mapped { point, .. } = &event {
                pending.borrow_mut().push(PendingPoint {
                    context: point.point.context.clone(),
                    epoch: point.converted.epoch_parts(),
                    frame: point.converted.frame_id(),
                    entity: point.converted.entity_id(),
                });
            }
            observe(event);
        },
    );
    if !report.complete {
        let message = report.failure.as_ref().map_or_else(
            || "GPX import did not complete".into(),
            |failure| format!("GPX playback import failed: {failure}"),
        );
        return Err(GpxPlaybackError {
            message,
            report: Some(Box::new(report)),
        });
    }
    let finish = (|| -> Result<usize, String> {
        if report.submitted == 0 {
            return Err("GPX playback requires at least one submitted observation".into());
        }
        if ledger.row_count() != report.submitted {
            return Err("playback ledger did not retain all submitted observations".into());
        }
        let start = from_parts(*epochs.first().expect("nonempty accepted observations"));
        let end = from_parts(*epochs.last().expect("nonempty accepted observations"));
        check_itrf93_interval(almanac, start, end)?;
        let frame_samples = append_frames(&mut ledger, &schema, &epochs, almanac, options)?;
        if ledger.row_count() != report.submitted + frame_samples {
            return Err("playback ledger did not retain all observations and frame poses".into());
        }
        ledger.register_name(itrf93, ASTRO_AUTHORITY, "ITRF93")?;
        ledger.register_name(gcrf, ASTRO_AUTHORITY, "GCRF")?;
        ledger.register_name(
            PrescribedId::abstract_source("anise", "almanac")?,
            "anise",
            "almanac",
        )?;
        Ok(frame_samples)
    })();
    match finish {
        Ok(frame_samples) => Ok(GpxPlaybackExport {
            ledger,
            report,
            frame_samples,
        }),
        Err(message) => Err(GpxPlaybackError {
            message,
            report: Some(Box::new(report)),
        }),
    }
}

fn playback_schema(itrf93: PrescribedId, cadence: f64) -> SchemaRef {
    let mut fields = entity_schema().fields().to_vec();
    fields.extend([
        Arc::new(Field::new("gpx_source", DataType::Utf8, true)),
        Arc::new(Field::new("gpx_object", DataType::Utf8, true)),
        Arc::new(Field::new("gpx_segment", DataType::UInt64, true)),
        Arc::new(Field::new("gpx_point", DataType::UInt64, true)),
    ]);
    Arc::new(Schema::new_with_metadata(
        fields,
        HashMap::from([
            ("soloc:gpx_playback".into(), "1".into()),
            ("soloc:gpx_earth_frame".into(), itrf93.to_string()),
            ("soloc:gpx_frame_step_seconds".into(), cadence.to_string()),
        ]),
    ))
}

fn with_provenance(
    batch: RecordBatch,
    schema: &SchemaRef,
    points: Option<&[PendingPoint]>,
) -> Result<RecordBatch, String> {
    let mut columns = batch.columns().to_vec();
    if let Some(points) = points {
        let source =
            StringArray::from_iter_values(points.iter().map(|p| p.context.source.as_str()));
        let object = StringArray::from_iter_values(points.iter().map(|p| match p.context.object {
            Some(GpxObjectId::Track(index)) => format!("track:{index}"),
            Some(GpxObjectId::Route(index)) => format!("route:{index}"),
            None => unreachable!("mapped GPX points have an object"),
        }));
        let segment =
            UInt64Array::from_iter(points.iter().map(|p| p.context.segment.map(|n| n as u64)));
        let point =
            UInt64Array::from_iter(points.iter().map(|p| p.context.point.map(|n| n as u64)));
        columns.extend([
            Arc::new(source) as ArrayRef,
            Arc::new(object),
            Arc::new(segment),
            Arc::new(point),
        ]);
    } else {
        columns.extend([
            new_null_array(&DataType::Utf8, batch.num_rows()),
            new_null_array(&DataType::Utf8, batch.num_rows()),
            new_null_array(&DataType::UInt64, batch.num_rows()),
            new_null_array(&DataType::UInt64, batch.num_rows()),
        ]);
    }
    RecordBatch::try_new(schema.clone(), columns).map_err(|error| error.to_string())
}

fn from_parts((centuries, ns): (i16, u64)) -> Epoch {
    epoch_from_parts(centuries, ns, TimeScale::TAI)
}

fn append_frames(
    ledger: &mut Ledger,
    schema: &SchemaRef,
    epochs: &BTreeSet<(i16, u64)>,
    almanac: &Almanac,
    options: GpxPlaybackOptions,
) -> Result<usize, String> {
    let end = from_parts(*epochs.last().expect("validated nonempty"));
    let step = Duration::from_seconds(options.frame_step_seconds);
    let mut grid = Some(from_parts(*epochs.first().expect("validated nonempty")));
    let mut observations = epochs.iter().map(|parts| from_parts(*parts)).peekable();
    let mut frames = Vec::with_capacity(options.import.max_rows());
    let mut count = 0;
    // Merge the regular grid and the observation epochs without constructing a
    // second, potentially huge grid in memory. Equal epochs produce one anchor.
    while grid.is_some() || observations.peek().is_some() {
        let next = match (grid, observations.peek().copied()) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => break,
        };
        if grid == Some(next) {
            grid = if next == end {
                None
            } else {
                Some((next + step).min(end))
            };
        }
        if observations.peek().copied() == Some(next) {
            observations.next();
        }
        count += 1;
        if count > options.max_frame_samples {
            return Err("frame cadence and observation epochs exceed max_frame_samples".into());
        }
        frames.push(itrf93_frame_snapshot(almanac, next)?);
        if frames.len() == options.import.max_rows() {
            append_frame_batch(ledger, schema, &frames)?;
            frames.clear();
        }
    }
    if !frames.is_empty() {
        append_frame_batch(ledger, schema, &frames)?;
    }
    Ok(count)
}

fn append_frame_batch(
    ledger: &mut Ledger,
    schema: &SchemaRef,
    frames: &[RecordBatch],
) -> Result<(), String> {
    let batch = concat_batches(&entity_schema(), frames).map_err(|error| error.to_string())?;
    ledger.append(with_provenance(batch, schema, None)?)
}
