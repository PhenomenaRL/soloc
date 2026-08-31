//! Universal spatiotemporal filter API for Arrow batches embedding a `spacetimestamp` struct.
//!
//! Any Arrow [`RecordBatch`] carrying the spacetimestamp columns can be filtered using
//! [`filter_batch`], whether they are nested in a struct column or at the top level.
//!
//! This is intentionally a pure-Arrow module; it has no dependency on `anise` or any
//! physics engine. Coordinate-frame concerns are left to the caller.
//!
//! # Frame Uniformity for Spatial Queries
//!
//! Spatial filtering computes Euclidean distances in the raw `position` column. This is only
//! meaningful when all rows share the same reference frame. If the batch contains rows in
//! different frames, [`filter_batch`] returns an error suggesting the caller first call
//! [`crate::transforms::transform_batch`] to reproject everything into a common frame.

use arrow::array::{Array, BooleanBuilder};
use arrow::record_batch::RecordBatch;
use hifitime::Epoch;

use crate::ephemeris::epoch_from_parts;
use crate::identity::PrescribedId;
use crate::schema::StsColumns;

/// A spatiotemporal filter for use with [`filter_batch`] and ledger query APIs.
///
/// # Example
/// ```
/// use spacetimestamp::query::SpatiotemporalFilter;
/// use hifitime::Epoch;
///
/// let t_start = Epoch::from_tai_seconds(0.0);
/// let t_end = Epoch::from_tai_seconds(86_400.0);
///
/// let filter = SpatiotemporalFilter::new()
///     .with_time_range(t_start, t_end)
///     .with_spatial([0.0, 0.0, 0.0], 1_000_000.0); // 1M km sphere
/// ```
#[derive(Debug, Clone, Default)]
pub struct SpatiotemporalFilter {
    /// Inclusive epoch range. Rows outside `[start, end]` are excluded.
    pub time_range: Option<(Epoch, Epoch)>,
    /// Center of the spatial sphere filter in the same units as the `position` column.
    pub spatial_origin: Option<[f64; 3]>,
    /// Maximum distance from `spatial_origin`. Rows farther away are excluded.
    pub spatial_radius: Option<f64>,
}

impl SpatiotemporalFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain only rows whose epoch falls within `[start, end]` (inclusive).
    pub fn with_time_range(mut self, start: Epoch, end: Epoch) -> Self {
        self.time_range = Some((start, end));
        self
    }

    /// Retain only rows whose `position` is within `radius` of `origin`.
    ///
    /// All rows in the batch must share the same `frame_id`. Call
    /// `spacetimestamp::transforms::transform_batch()` first if the batch has mixed frames.
    pub fn with_spatial(mut self, origin: [f64; 3], radius: f64) -> Self {
        self.spatial_origin = Some(origin);
        self.spatial_radius = Some(radius);
        self
    }
}

/// Filters a [`RecordBatch`] by a [`SpatiotemporalFilter`].
///
/// Returns a new [`RecordBatch`] holding only the rows that satisfy every active condition.
/// With no filter set, returns a cheap clone.
///
/// # Errors
/// The spacetimestamp columns cannot be located, or a spatial filter is set and the batch
/// contains mixed reference frames.
pub fn filter_batch(
    batch: &RecordBatch,
    filter: &SpatiotemporalFilter,
) -> Result<RecordBatch, String> {
    // Nothing to filter: return a cheap Arc-clone of the batch.
    if filter.time_range.is_none() && filter.spatial_origin.is_none() {
        return Ok(batch.clone());
    }

    let cols = StsColumns::try_new(batch)?;

    // Frame uniformity is required for spatial filtering to be meaningful.
    if filter.spatial_origin.is_some() {
        check_frame_uniformity(&cols)?;
    }

    let num_rows = batch.num_rows();
    let mut mask = BooleanBuilder::with_capacity(num_rows);

    for i in 0..num_rows {
        let mut keep = true;

        // Time filter: reconstruct the physical epoch using the row's declared timescale so
        // comparisons against the caller-supplied Epoch bounds are always physically correct.
        if let Some((t_start, t_end)) = filter.time_range {
            let ts = cols.timescale_at(i)?;
            let (centuries, ns) = cols.epoch_parts_at(i);
            let epoch = epoch_from_parts(centuries, ns, ts.into());
            if epoch < t_start || epoch > t_end {
                keep = false;
            }
        }

        // Spatial filter: squared-distance check avoids a sqrt.
        if keep && let (Some(origin), Some(radius)) = (filter.spatial_origin, filter.spatial_radius)
        {
            let [x, y, z] = cols.position_at(i);
            let (dx, dy, dz) = (x - origin[0], y - origin[1], z - origin[2]);
            if dx * dx + dy * dy + dz * dz > radius * radius {
                keep = false;
            }
        }

        mask.append_value(keep);
    }

    apply_boolean_mask(batch, &mask.finish())
}

/// Verifies that every non-null row in the `frame_id` column refers to the same frame.
/// Mixed frames make Euclidean distance comparisons meaningless.
fn check_frame_uniformity(cols: &StsColumns<'_>) -> Result<(), String> {
    let frames = cols.frames();

    let mut seen: Option<PrescribedId> = None;
    for i in 0..frames.len() {
        if frames.is_null(i) {
            continue;
        }
        let frame = cols.frame_at(i)?;
        match seen {
            None => seen = Some(frame),
            Some(prev) if prev != frame => {
                return Err(format!(
                    "Batch contains mixed frames ({prev} and {frame}). \
                     Call spacetimestamp::transforms::transform_batch() to reproject \
                     all rows into a common frame before applying a spatial filter."
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Applies a boolean mask to every column in the batch, returning a new batch with
/// only the rows where the mask is `true`.
///
/// Public because every row-selecting API needs it: [`filter_batch`] here, and the ledger's
/// snapshot and current-state queries, which build their masks differently but rebuild the
/// batch identically.
pub fn apply_boolean_mask(
    batch: &RecordBatch,
    mask: &arrow::array::BooleanArray,
) -> Result<RecordBatch, String> {
    let filtered_columns: Result<Vec<_>, _> = batch
        .columns()
        .iter()
        .map(|col| arrow::compute::filter(col.as_ref(), mask))
        .collect();

    let filtered_columns =
        filtered_columns.map_err(|e| format!("Arrow filter kernel error: {e}"))?;

    RecordBatch::try_new(batch.schema(), filtered_columns)
        .map_err(|e| format!("Failed to rebuild filtered RecordBatch: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ephemeris::j2000_tai;
    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};
    use arrow::datatypes::{DataType, Field, Schema};
    use hifitime::Duration;
    use std::sync::Arc;

    fn icrf() -> PrescribedId {
        PrescribedId::astronomical_from_name("ICRF").unwrap()
    }

    fn iau_earth() -> PrescribedId {
        PrescribedId::astronomical_from_name("IAU_EARTH").unwrap()
    }

    fn src(name: &str) -> PrescribedId {
        PrescribedId::abstract_source("test", name).unwrap()
    }

    fn make_sts_batch(builder: &mut SpaceTimestampBuilder) -> RecordBatch {
        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            DataType::Struct(sts_ref.fields().clone()),
            false,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    #[test]
    fn test_no_filter_returns_all_rows() {
        let mut builder = SpaceTimestampBuilder::new(2);
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s1"),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s1"),
            EstimateType::MEASURED,
            [2.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder);
        let result = filter_batch(&batch, &SpatiotemporalFilter::new()).unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_time_filter_keeps_in_range_rows() {
        let j2000 = j2000_tai();
        let t1 = j2000 + Duration::from_parts(0, 500);
        let t2 = j2000 + Duration::from_parts(0, 1500);

        let mut builder = SpaceTimestampBuilder::new(3);
        // Row 0: ns=0 — before range, excluded
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        // Row 1: ns=1000 — inside range, kept
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
        );
        // Row 2: ns=2000 — after range, excluded
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [2.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            2000,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let result =
            filter_batch(&batch, &SpatiotemporalFilter::new().with_time_range(t1, t2)).unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_spatial_filter_keeps_rows_within_radius() {
        let mut builder = SpaceTimestampBuilder::new(3);
        // Row 0: origin — inside
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        // Row 1: [3, 4, 0] → distance 5 from origin (boundary, ≤ radius, kept)
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [3.0, 4.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        // Row 2: [10, 0, 0]
        // outside
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [10.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let result = filter_batch(
            &batch,
            &SpatiotemporalFilter::new().with_spatial([0.0, 0.0, 0.0], 5.0),
        )
        .unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_mixed_frame_returns_helpful_error() {
        let mut builder = SpaceTimestampBuilder::new(2);
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        builder.append_spacetimestamp(
            iau_earth(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let err = filter_batch(
            &batch,
            &SpatiotemporalFilter::new().with_spatial([0.0, 0.0, 0.0], 100.0),
        )
        .unwrap_err();

        assert!(err.contains("mixed frames"), "got: {err}");
        assert!(err.contains("transform_batch"), "got: {err}");
    }

    #[test]
    fn test_time_and_spatial_combined() {
        let j2000 = j2000_tai();
        let t_start = j2000 + Duration::from_parts(0, 0);
        let t_end = j2000 + Duration::from_parts(0, 1000);

        let mut builder = SpaceTimestampBuilder::new(3);
        // Row 0: in time range, inside sphere → kept
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            500,
            None,
            None,
        );
        // Row 1: in time range, outside sphere → dropped
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [100.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            500,
            None,
            None,
        );
        // Row 2: outside time range, inside sphere → dropped
        builder.append_spacetimestamp(
            icrf(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            src("s"),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            5000,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let result = filter_batch(
            &batch,
            &SpatiotemporalFilter::new()
                .with_time_range(t_start, t_end)
                .with_spatial([0.0, 0.0, 0.0], 10.0),
        )
        .unwrap();
        assert_eq!(result.num_rows(), 1);
    }
}
