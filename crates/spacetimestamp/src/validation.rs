use crate::identity::FRAME_ID_COLUMN;
use crate::schema::{
    ESTIMATE_TYPE_COLUMN, STS_COLUMN, StsColumns, TIMESCALE_ID_COLUMN, UNITS_POS_COLUMN, sts_schema,
};
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode, Vocabulary};
use arrow::array::{Array, UInt8Array};
use arrow::compute::kernels::aggregate::{max, min};
use arrow::datatypes::{DataType, SchemaRef};
use arrow::record_batch::RecordBatch;

/// Validates that `schema` embeds a [`STS_COLUMN`] struct carrying every [`sts_schema`] field
/// with the right Arrow type.
///
/// Types are compared exactly because the readers downcast on them; nullability and field
/// metadata are left free.
pub fn validate_sts_schema(schema: &SchemaRef) -> Result<(), String> {
    let sts_field = schema
        .field_with_name(STS_COLUMN)
        .map_err(|_| format!("'{STS_COLUMN}' column not found in schema"))?;

    let DataType::Struct(fields) = sts_field.data_type() else {
        return Err(format!(
            "'{STS_COLUMN}' must be a Struct column, got {:?}",
            sts_field.data_type()
        ));
    };

    for expected in sts_schema().fields() {
        let Some((_, actual)) = fields.find(expected.name()) else {
            return Err(format!(
                "'{STS_COLUMN}' struct is missing required STS field '{}'",
                expected.name()
            ));
        };
        if actual.data_type() != expected.data_type() {
            return Err(format!(
                "'{STS_COLUMN}.{}' has wrong Arrow type — expected {:?}, got {:?}",
                expected.name(),
                expected.data_type(),
                actual.data_type()
            ));
        }
    }
    Ok(())
}

/// Rejects a vocabulary column holding a null or a code outside `V`'s range.
fn validate_vocabulary<V: Vocabulary>(col: &UInt8Array, name: &str) -> Result<(), String> {
    if col.null_count() != 0 {
        return Err(format!(
            "'{name}' has {} null rows; a null vocabulary code has no valid interpretation",
            col.null_count()
        ));
    }

    for bound in [min(col), max(col)].into_iter().flatten() {
        V::from_code(bound).map_err(|e| format!("'{name}': {e}"))?;
    }
    Ok(())
}

/// Validates `units_pos`, `timescale_id`, `estimate_type` and `frame_id` *values*, in either
/// layout: nested in a [`STS_COLUMN`] struct, or flat as
/// [`crate::schema::SpaceTimestampBuilder::flush`] emits.
///
/// # What the frame check does here
///
/// Two things only the column can tell us: that the bytes decode to a well-formed id at all
/// (corruption is caught by [`PrescribedId::from_bytes`](crate::identity::PrescribedId::from_bytes)'s
/// RFC 9562 checks), and that the id is not
/// [`KIND_ABSTRACT`](crate::identity::KIND_ABSTRACT).
///
/// [`KIND_SOLOC`](crate::identity::KIND_SOLOC) frames pass: they resolve against the ledger's
/// derived topology, not here. Reachability is checked at append time by
/// [`crate::topology::TransformTree`].
pub fn validate_spacetimestamp_batch(batch: &RecordBatch) -> Result<(), String> {
    // Resolves both layouts and checks every column's Arrow type, so the checks below only
    // have to answer for values.
    let cols = StsColumns::try_new(batch)?;

    validate_vocabulary::<LengthUnit>(cols.units(), UNITS_POS_COLUMN)?;
    validate_vocabulary::<TimeScaleCode>(cols.timescales(), TIMESCALE_ID_COLUMN)?;
    validate_vocabulary::<EstimateType>(cols.estimates(), ESTIMATE_TYPE_COLUMN)?;

    let frames = cols.frames();
    for i in 0..frames.len() {
        // The column is declared non-nullable, but a caller-assembled batch need not
        // honour that; a null row carries no frame to object to.
        if frames.is_null(i) {
            continue;
        }
        let id = cols
            .frame_at(i)
            .map_err(|e| format!("Invalid {FRAME_ID_COLUMN}: {e}"))?;
        if id.is_abstract() {
            return Err(format!(
                "Invalid frame_id: {id} is an abstract id. Abstract ids label where a \
                 row came from and never occupy space, so one cannot be a frame."
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PrescribedId;
    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    /// The frame every test that is not about frames uses.
    fn icrf() -> PrescribedId {
        PrescribedId::astronomical_from_name("ICRF").unwrap()
    }

    fn sensor() -> PrescribedId {
        PrescribedId::abstract_source("test", "sensor_1").unwrap()
    }

    fn one_row(frame: PrescribedId, timescale: TimeScaleCode) -> SpaceTimestampBuilder {
        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            frame,
            LengthUnit::km,
            timescale,
            sensor(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        builder
    }

    fn make_flat_batch(frame: PrescribedId, timescale: TimeScaleCode) -> RecordBatch {
        one_row(frame, timescale).flush()
    }

    fn make_nested_batch(frame: PrescribedId, timescale: TimeScaleCode) -> RecordBatch {
        let struct_array = one_row(frame, timescale).finish_as_struct();
        let schema = Arc::new(Schema::new(vec![Field::new(
            STS_COLUMN,
            DataType::Struct(sts_schema().fields().clone()),
            false,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    #[test]
    fn test_valid_flat_batch() {
        assert!(
            validate_spacetimestamp_batch(&make_flat_batch(icrf(), TimeScaleCode::TAI)).is_ok()
        );
    }

    #[test]
    fn test_valid_nested_batch() {
        assert!(
            validate_spacetimestamp_batch(&make_nested_batch(icrf(), TimeScaleCode::TAI)).is_ok()
        );
    }

    #[test]
    fn test_abstract_frame_is_rejected_flat() {
        let source = PrescribedId::abstract_source("acme.com", "pipeline").unwrap();
        let result = validate_spacetimestamp_batch(&make_flat_batch(source, TimeScaleCode::TAI));
        assert!(result.unwrap_err().contains("Invalid frame_id"));
    }

    #[test]
    fn test_abstract_frame_is_rejected_nested() {
        let source = PrescribedId::abstract_source("acme.com", "pipeline").unwrap();
        let result = validate_spacetimestamp_batch(&make_nested_batch(source, TimeScaleCode::TAI));
        assert!(result.unwrap_err().contains("Invalid frame_id"));
    }

    /// Entity frames pass validation
    #[test]
    fn test_soloc_frames_are_valid_including_bare_names() {
        let truck = PrescribedId::new("demo", "truck_A").unwrap();
        assert!(validate_spacetimestamp_batch(&make_flat_batch(truck, TimeScaleCode::TAI)).is_ok());
        assert!(
            validate_spacetimestamp_batch(&make_nested_batch(truck, TimeScaleCode::TAI)).is_ok()
        );

        let cam = PrescribedId::new("acme.com", "cam").unwrap();
        assert!(validate_spacetimestamp_batch(&make_flat_batch(cam, TimeScaleCode::TAI)).is_ok());
        assert!(validate_spacetimestamp_batch(&make_nested_batch(cam, TimeScaleCode::TAI)).is_ok());
    }

    /// An astronomical frame id is a valid frame identifier and must not be rejected.
    #[test]
    fn test_astronomical_frame_is_valid() {
        let mars = PrescribedId::astronomical_from_name("Mars").unwrap();
        assert!(validate_spacetimestamp_batch(&make_flat_batch(mars, TimeScaleCode::TAI)).is_ok());
        assert!(
            validate_spacetimestamp_batch(&make_nested_batch(mars, TimeScaleCode::TAI)).is_ok()
        );
    }

    #[test]
    fn test_corrupt_frame_column_is_rejected() {
        use arrow::array::FixedSizeBinaryBuilder;

        let valid = make_flat_batch(icrf(), TimeScaleCode::TAI);

        let mut corrupt = FixedSizeBinaryBuilder::new(16);
        corrupt.append_value([0u8; 16]).unwrap();

        let mut columns = valid.columns().to_vec();
        let frame_idx = valid.schema().index_of(FRAME_ID_COLUMN).unwrap();
        columns[frame_idx] = Arc::new(corrupt.finish());

        let batch = RecordBatch::try_new(valid.schema(), columns).unwrap();

        let err = validate_spacetimestamp_batch(&batch).unwrap_err();
        assert!(err.contains(FRAME_ID_COLUMN), "unexpected error: {err}");
    }
}
