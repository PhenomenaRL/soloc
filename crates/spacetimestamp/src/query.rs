//! Universal spatiotemporal filter API for Arrow batches embedding a `spacetimestamp` struct.
//!
//! Any Arrow [`RecordBatch`] that contains a `spacetimestamp` struct column can be filtered
//! using [`filter_batch`]. This includes entity batches, image batches, sensor-reading batches,
//! or any future schema that embeds [`crate::schema::sts_schema`] as a sub-structure.
//!
//! This is intentionally a pure-Arrow module — it has no dependency on `anise` or any
//! physics engine. Coordinate-frame concerns are left to the caller.
//!
//! # Frame Uniformity for Spatial Queries
//!
//! Spatial filtering computes Euclidean distances in the raw `position` column. This is only
//! meaningful when all rows share the same reference frame. If the batch contains rows in
//! different frames, [`filter_batch`] returns an error suggesting the caller first call
//! [`crate::transforms::transform_batch`] to reproject everything into a common frame.

use arrow::array::{
    Array, BooleanBuilder, DictionaryArray, FixedSizeListArray, Float64Array, Int16Array,
    StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::UInt32Type;
use arrow::record_batch::RecordBatch;
use hifitime::{Epoch, TimeScale};
use std::str::FromStr;
use std::sync::Arc;

use crate::ephemeris::epoch_from_parts;


/// A spatiotemporal filter for use with [`filter_batch`] and ledger query APIs.
///
/// All fields are optional — unset fields are no-ops. Build with the fluent methods.
///
/// # Example
/// ```rust,ignore
/// use spacetimestamp::query::SpatiotemporalFilter;
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

/// Filters a [`RecordBatch`] by a [`SpatiotemporalFilter`], operating on the named
/// `sts_column_name` struct column that holds the embedded `spacetimestamp` fields.
///
/// Returns a new [`RecordBatch`] with only the rows that satisfy all active filter
/// conditions. If no filter fields are set, a cheap clone of the input is returned.
///
/// # Arguments
/// * `batch` — The source batch to filter.
/// * `sts_column_name` — Name of the `StructArray` column containing `spacetimestamp` fields
///   (e.g., `"spacetimestamp"` for entity batches).
/// * `filter` — The spatiotemporal filter to apply.
///
/// # Errors
/// Returns `Err` if:
/// - `sts_column_name` is not found in the batch.
/// - The column is not a `StructArray`.
/// - A spatial filter is set but the batch contains rows in mixed reference frames.
pub fn filter_batch(
    batch: &RecordBatch,
    sts_column_name: &str,
    filter: &SpatiotemporalFilter,
) -> Result<RecordBatch, String> {
    // Nothing to filter — return a cheap Arc-clone of the batch.
    if filter.time_range.is_none() && filter.spatial_origin.is_none() {
        return Ok(batch.clone());
    }

    let schema = batch.schema();
    let col_idx = schema
        .index_of(sts_column_name)
        .map_err(|_| format!("Column '{}' not found in batch schema", sts_column_name))?;

    let struct_array = batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| format!("Column '{}' is not a StructArray", sts_column_name))?;

    // Frame uniformity is required for spatial filtering to be meaningful.
    if filter.spatial_origin.is_some() {
        check_frame_uniformity(struct_array)?;
    }

    let num_rows = batch.num_rows();

    // Extract time arrays — always present in a valid spacetimestamp struct.
    let cent_arr = struct_array
        .column_by_name("duration_centuries")
        .ok_or("'duration_centuries' missing from spacetimestamp struct")?
        .as_any()
        .downcast_ref::<Int16Array>()
        .ok_or("'duration_centuries' is not Int16")?;

    let ns_arr = struct_array
        .column_by_name("duration_ns")
        .ok_or("'duration_ns' missing from spacetimestamp struct")?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or("'duration_ns' is not UInt64")?;

    // Extract timescale_id for the time filter — only looked up when time_range is active.
    let timescale_data: Option<(&DictionaryArray<UInt32Type>, &StringArray)> =
        if filter.time_range.is_some() {
            let tc = struct_array
                .column_by_name("timescale_id")
                .ok_or("'timescale_id' missing from spacetimestamp struct")?
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
                .ok_or("'timescale_id' is not Dictionary<UInt32, Utf8>")?;
            let td = tc
                .values()
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or("'timescale_id' dictionary values are not Utf8")?;
            Some((tc, td))
        } else {
            None
        };

    // Extract position array for spatial filter.
    // We Arc-clone the values array so it outlives the temporary FixedSizeListArray reference.
    let pos_data: Option<(usize, Arc<dyn Array>)> = if filter.spatial_origin.is_some() {
        let pos_list = struct_array
            .column_by_name("position")
            .ok_or("'position' missing from spacetimestamp struct")?
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or("'position' is not FixedSizeList")?;
        Some((pos_list.offset(), Arc::clone(pos_list.values())))
    } else {
        None
    };

    // Downcast once outside the loop so we don't repeat it per row.
    let pos_f64: Option<(usize, &Float64Array)> = pos_data.as_ref().map(|(offset, arr)| {
        (
            *offset,
            arr.as_any()
                .downcast_ref::<Float64Array>()
                .expect("position list values should always be Float64"),
        )
    });

    let mut mask = BooleanBuilder::with_capacity(num_rows);

    for i in 0..num_rows {
        let mut keep = true;

        // Time filter: reconstruct the physical epoch using the row's declared timescale so
        // comparisons against the caller-supplied Epoch bounds are always physically correct.
        if let (Some((t_start, t_end)), Some((tc, td))) = (filter.time_range, timescale_data) {
            let ts_str = td.value(tc.keys().value(i) as usize);
            let ts = TimeScale::from_str(ts_str).unwrap_or(TimeScale::TAI);
            let epoch = epoch_from_parts(cent_arr.value(i), ns_arr.value(i), ts);
            if epoch < t_start || epoch > t_end {
                keep = false;
            }
        }

        // Spatial filter: squared-distance check avoids a sqrt.
        if keep {
            if let (Some(origin), Some(radius), Some((offset, vals))) =
                (filter.spatial_origin, filter.spatial_radius, pos_f64)
            {
                let base = (offset + i) * 3;
                let dx = vals.value(base) - origin[0];
                let dy = vals.value(base + 1) - origin[1];
                let dz = vals.value(base + 2) - origin[2];
                if dx * dx + dy * dy + dz * dz > radius * radius {
                    keep = false;
                }
            }
        }

        mask.append_value(keep);
    }

    apply_boolean_mask(batch, &mask.finish())
}

/// Verifies that every non-null row in the `frame_id` dictionary column refers to the same
/// frame string. Mixed frames make Euclidean distance comparisons meaningless.
fn check_frame_uniformity(struct_array: &StructArray) -> Result<(), String> {
    let frames = struct_array
        .column_by_name("frame_id")
        .ok_or("'frame_id' missing from spacetimestamp struct")?
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .ok_or("'frame_id' is not Dictionary<UInt32, Utf8>")?;

    let frames_dict = frames
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("'frame_id' dictionary values are not Utf8")?;

    let mut seen: Option<String> = None;
    for i in 0..frames.len() {
        if frames.is_null(i) {
            continue;
        }
        let frame = frames_dict.value(frames.keys().value(i) as usize).to_owned();
        match &seen {
            None => {
                seen = Some(frame);
            }
            Some(prev) if *prev != frame => {
                return Err(format!(
                    "Batch contains mixed frames ('{}' and '{}'). \
                     Call spacetimestamp::transforms::transform_batch() to reproject \
                     all rows into a common frame before applying a spatial filter.",
                    prev, frame
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Applies a boolean mask to every column in the batch, returning a new batch with
/// only the rows where the mask is `true`.
fn apply_boolean_mask(
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
    use crate::schema::{FrameRegistry, SpaceTimestampBuilder, sts_schema};
    use hifitime::Duration;
    use arrow::datatypes::{DataType, Field, Schema};

    fn make_sts_batch(
        builder: &mut SpaceTimestampBuilder,
        reg: Option<&FrameRegistry>,
    ) -> RecordBatch {
        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema(reg);
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
    fn test_no_filter_returns_all_rows() {
        let mut builder = SpaceTimestampBuilder::new(2, None);
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s1", "MEASURED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s1", "MEASURED",
            [2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 1000,
            None, None,
        );
        let batch = make_sts_batch(&mut builder, None);
        let result =
            filter_batch(&batch, "spacetimestamp", &SpatiotemporalFilter::new()).unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_time_filter_keeps_in_range_rows() {
        let j2000 = j2000_tai();
        let t1 = j2000 + Duration::from_parts(0, 500);
        let t2 = j2000 + Duration::from_parts(0, 1500);

        let mut builder = SpaceTimestampBuilder::new(3, None);
        // Row 0: ns=0 — before range, excluded
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );
        // Row 1: ns=1000 — inside range, kept
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 1000,
            None, None,
        );
        // Row 2: ns=2000 — after range, excluded
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 2000,
            None, None,
        );

        let batch = make_sts_batch(&mut builder, None);
        let result = filter_batch(
            &batch,
            "spacetimestamp",
            &SpatiotemporalFilter::new().with_time_range(t1, t2),
        )
        .unwrap();
        assert_eq!(result.num_rows(), 1);
    }

    #[test]
    fn test_spatial_filter_keeps_rows_within_radius() {
        let mut builder = SpaceTimestampBuilder::new(3, None);
        // Row 0: origin — inside
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );
        // Row 1: [3, 4, 0] → distance 5 from origin (boundary, ≤ radius, kept)
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [3.0, 4.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );
        // Row 2: [10, 0, 0] — outside
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [10.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );

        let batch = make_sts_batch(&mut builder, None);
        let result = filter_batch(
            &batch,
            "spacetimestamp",
            &SpatiotemporalFilter::new().with_spatial([0.0, 0.0, 0.0], 5.0),
        )
        .unwrap();
        assert_eq!(result.num_rows(), 2);
    }

    #[test]
    fn test_mixed_frame_returns_helpful_error() {
        let mut builder = SpaceTimestampBuilder::new(2, None);
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );
        builder.append_spacetimestamp(
            "IAU_EARTH", "km", "TAI", "s", "MEASURED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            None, None,
        );

        let batch = make_sts_batch(&mut builder, None);
        let err = filter_batch(
            &batch,
            "spacetimestamp",
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

        let mut builder = SpaceTimestampBuilder::new(3, None);
        // Row 0: in time range, inside sphere → kept
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 500,
            None, None,
        );
        // Row 1: in time range, outside sphere → dropped
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [100.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 500,
            None, None,
        );
        // Row 2: outside time range, inside sphere → dropped
        builder.append_spacetimestamp(
            "ICRF", "km", "TAI", "s", "MEASURED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 5000,
            None, None,
        );

        let batch = make_sts_batch(&mut builder, None);
        let result = filter_batch(
            &batch,
            "spacetimestamp",
            &SpatiotemporalFilter::new()
                .with_time_range(t_start, t_end)
                .with_spatial([0.0, 0.0, 0.0], 10.0),
        )
        .unwrap();
        assert_eq!(result.num_rows(), 1);
    }
}
