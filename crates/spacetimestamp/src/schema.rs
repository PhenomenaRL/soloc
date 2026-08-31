//! Arrow schema, builder and column accessor for spacetimestamp, the core data required to
//! report a localization in the solar system.
//!
//! [`sts_schema`] is the layout; [`SpaceTimestampBuilder`] writes it row by row and
//! [`StsColumns`] reads it back.

extern crate alloc;

use alloc::sync::Arc;
use arrow::array::{
    Array, ArrayBuilder, ArrayRef, FixedSizeBinaryArray, FixedSizeBinaryBuilder,
    FixedSizeListArray, FixedSizeListBuilder, Float64Array, Float64Builder, Int16Array,
    Int16Builder, StructArray, UInt8Array, UInt8Builder, UInt64Array, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;

use crate::identity::{
    FRAME_ID_COLUMN, PrescribedId, SOURCE_ID_COLUMN, id_at, id_builder, id_field,
};
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode, Vocabulary, vocabulary_field};

/// The fixed name of the spacetimestamp struct column in any soloc schema.
///
/// All Arrow schemas that embed a spacetimestamp must use this exact column name.
/// The name is fixed (not configurable)
pub const STS_COLUMN: &str = "spacetimestamp";

/// Column name for the position units a spacetimestamp row is expressed in.
pub const UNITS_POS_COLUMN: &str = "units_pos";

/// Column name for the timescale a spacetimestamp row's duration is measured on.
pub const TIMESCALE_ID_COLUMN: &str = "timescale_id";

/// Column name for how a spacetimestamp row was arrived at (measured, predicted, …).
pub const ESTIMATE_TYPE_COLUMN: &str = "estimate_type";

/// Column name for the `[x, y, z]` translation on a spacetimestamp row.
pub const POSITION_COLUMN: &str = "position";

/// Column name for the `[w, x, y, z]` orientation on a spacetimestamp row.
pub const QUATERNION_COLUMN: &str = "quaternion";

/// Column name for the whole-centuries half of a spacetimestamp row's duration.
pub const DURATION_CENTURIES_COLUMN: &str = "duration_centuries";

/// Column name for the sub-century nanosecond half of a spacetimestamp row's duration.
pub const DURATION_NS_COLUMN: &str = "duration_ns";

/// Appends one nullable `N`-element entry to a `FixedSizeListBuilder`.
///
/// A null entry still needs `N` null values pushed into the child builder before the list
/// slot is closed, making this worth sharing between the schemas.
pub(crate) fn append_optional_list<const N: usize>(
    builder: &mut FixedSizeListBuilder<Float64Builder>,
    value: Option<[f64; N]>,
) {
    match value {
        Some(v) => {
            for x in v {
                builder.values().append_value(x);
            }
            builder.append(true);
        }
        None => {
            for _ in 0..N {
                builder.values().append_null();
            }
            builder.append(false);
        }
    }
}

/// Returns Arrow schema for SpaceTimestamp.
///
/// The schema consists of:
/// * `frame_id`: [`PrescribedId`] of the reference frame, as plain `FixedSizeBinary(16)`
///   carrying the `arrow.uuid` extension.
/// * `units_pos`: [`LengthUnit`] code for `position`, as `UInt8`.
/// * `timescale_id`: [`TimeScaleCode`] code, as `UInt8`.
/// * `source_id`: [`PrescribedId`] of the observer or process, same encoding as `frame_id`.
/// * `estimate_type`: [`EstimateType`] code, as `UInt8`.
/// * `position`: `FixedSizeList(3, Float64)` containing `[x, y, z]`.
/// * `quaternion`: `FixedSizeList(4, Float64)` containing `[w, x, y, z]`.
/// * `duration_centuries`: Signed 16-bit integer for large time offsets.
/// * `duration_ns`: Unsigned 64-bit integer for nanosecond precision.
/// * `position_covariance`: Nullable `FixedSizeList(6, Float64)` — upper triangle of the
///   3×3 position covariance matrix, row-major: `[σ_xx, σ_xy, σ_xz, σ_yy, σ_yz, σ_zz]`.
///   Expressed in the same frame and units as `position`. Null when unknown.
/// * `orientation_covariance`: Nullable `FixedSizeList(6, Float64)` — upper triangle of the
///   3×3 orientation covariance in the tangent space of SO(3) (axis-angle perturbation),
///   row-major: `[σ_11, σ_12, σ_13, σ_22, σ_23, σ_33]`. Null when unknown.
pub fn sts_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        id_field(FRAME_ID_COLUMN),
        vocabulary_field::<LengthUnit>("units_pos"),
        vocabulary_field::<TimeScaleCode>("timescale_id"),
        id_field(SOURCE_ID_COLUMN),
        vocabulary_field::<EstimateType>("estimate_type"),
        Field::new(
            "position",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            false,
        ),
        Field::new(
            "quaternion",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 4),
            false,
        ),
        Field::new("duration_centuries", DataType::Int16, false),
        Field::new("duration_ns", DataType::UInt64, false),
        Field::new(
            "position_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 6),
            true,
        ),
        Field::new(
            "orientation_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 6),
            true,
        ),
    ]))
}

/// A resolved view of one batch's spacetimestamp columns, borrowed rather than copied.
///
/// Resolving a column name to a typed array costs a field-name scan and two downcasts, so
/// every reader hoists it above its row loop; this is that hoist, written once. Accepts both
/// layouts: fields nested in a [`STS_COLUMN`] struct, or present at the top level.
#[derive(Debug)]
pub struct StsColumns<'a> {
    frames: &'a FixedSizeBinaryArray,
    sources: &'a FixedSizeBinaryArray,
    units: &'a UInt8Array,
    timescales: &'a UInt8Array,
    estimates: &'a UInt8Array,
    position: (&'a Float64Array, usize),
    quaternion: (&'a Float64Array, usize),
    centuries: &'a Int16Array,
    nanos: &'a UInt64Array,
}

fn as_id<'a>(arr: &'a ArrayRef, name: &str) -> Result<&'a FixedSizeBinaryArray, String> {
    crate::identity::as_id_column(arr, name)
}

fn as_u8<'a>(arr: &'a ArrayRef, name: &str) -> Result<&'a UInt8Array, String> {
    arr.as_any()
        .downcast_ref::<UInt8Array>()
        .ok_or_else(|| format!("'{name}' is not UInt8"))
}

/// Decodes a vocabulary code, naming the column and row so the error locates itself.
fn decode_at<V: Vocabulary>(col: &UInt8Array, row: usize, name: &str) -> Result<V, String> {
    V::from_code(col.value(row)).map_err(|e| format!("'{name}' at row {row}: {e}"))
}

/// Returns the flattened values and the list's Arrow offset, which a sliced batch needs.
fn as_f64_list<'a>(arr: &'a ArrayRef, name: &str) -> Result<(&'a Float64Array, usize), String> {
    let list = arr
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .ok_or_else(|| format!("'{name}' is not a FixedSizeList"))?;
    let values = list
        .values()
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| format!("'{name}' list values are not Float64"))?;
    Ok((values, list.offset()))
}

impl<'a> StsColumns<'a> {
    /// Locates every spacetimestamp column of `batch`.
    pub fn try_new(batch: &'a RecordBatch) -> Result<Self, String> {
        let sts = match batch.column_by_name(STS_COLUMN) {
            Some(col) => Some(
                col.as_any()
                    .downcast_ref::<StructArray>()
                    .ok_or_else(|| format!("'{STS_COLUMN}' column is not a StructArray"))?,
            ),
            None => None,
        };

        let field = |name: &str| -> Result<&'a ArrayRef, String> {
            match sts {
                Some(s) => s.column_by_name(name),
                None => batch.column_by_name(name),
            }
            .ok_or_else(|| format!("'{name}' is missing from the spacetimestamp columns"))
        };

        Ok(Self {
            frames: as_id(field(FRAME_ID_COLUMN)?, FRAME_ID_COLUMN)?,
            sources: as_id(field(SOURCE_ID_COLUMN)?, SOURCE_ID_COLUMN)?,
            units: as_u8(field(UNITS_POS_COLUMN)?, UNITS_POS_COLUMN)?,
            timescales: as_u8(field(TIMESCALE_ID_COLUMN)?, TIMESCALE_ID_COLUMN)?,
            estimates: as_u8(field(ESTIMATE_TYPE_COLUMN)?, ESTIMATE_TYPE_COLUMN)?,
            position: as_f64_list(field(POSITION_COLUMN)?, POSITION_COLUMN)?,
            quaternion: as_f64_list(field(QUATERNION_COLUMN)?, QUATERNION_COLUMN)?,
            centuries: field(DURATION_CENTURIES_COLUMN)?
                .as_any()
                .downcast_ref::<Int16Array>()
                .ok_or_else(|| format!("'{DURATION_CENTURIES_COLUMN}' is not Int16"))?,
            nanos: field(DURATION_NS_COLUMN)?
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| format!("'{DURATION_NS_COLUMN}' is not UInt64"))?,
        })
    }

    /// The number of rows these columns cover.
    pub fn len(&self) -> usize {
        self.centuries.len()
    }

    /// Whether these columns cover no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The whole `frame_id` column, for readers that scan it.
    pub fn frames(&self) -> &'a FixedSizeBinaryArray {
        self.frames
    }

    /// The whole `units_pos` code column, for readers that scan it.
    pub fn units(&self) -> &'a UInt8Array {
        self.units
    }

    /// The whole `timescale_id` code column, for readers that scan it.
    pub fn timescales(&self) -> &'a UInt8Array {
        self.timescales
    }

    /// The whole `estimate_type` code column, for readers that scan it.
    pub fn estimates(&self) -> &'a UInt8Array {
        self.estimates
    }

    /// The frame id at `row`.
    pub fn frame_at(&self, row: usize) -> Result<PrescribedId, String> {
        id_at(self.frames, row).map_err(|e| format!("'{FRAME_ID_COLUMN}': {e}"))
    }

    /// The source id at `row`.
    pub fn source_at(&self, row: usize) -> Result<PrescribedId, String> {
        id_at(self.sources, row).map_err(|e| format!("'{SOURCE_ID_COLUMN}': {e}"))
    }

    /// The position units at `row`.
    pub fn units_at(&self, row: usize) -> Result<LengthUnit, String> {
        decode_at(self.units, row, UNITS_POS_COLUMN)
    }

    /// The timescale at `row`.
    pub fn timescale_at(&self, row: usize) -> Result<TimeScaleCode, String> {
        decode_at(self.timescales, row, TIMESCALE_ID_COLUMN)
    }

    /// The estimate type at `row`.
    pub fn estimate_at(&self, row: usize) -> Result<EstimateType, String> {
        decode_at(self.estimates, row, ESTIMATE_TYPE_COLUMN)
    }

    /// The `[x, y, z]` position at `row`, in the batch's own units.
    pub fn position_at(&self, row: usize) -> [f64; 3] {
        let (values, offset) = self.position;
        let base = (offset + row) * 3;
        [
            values.value(base),
            values.value(base + 1),
            values.value(base + 2),
        ]
    }

    /// The `[w, x, y, z]` orientation at `row`.
    pub fn quaternion_at(&self, row: usize) -> [f64; 4] {
        let (values, offset) = self.quaternion;
        let base = (offset + row) * 4;
        [
            values.value(base),
            values.value(base + 1),
            values.value(base + 2),
            values.value(base + 3),
        ]
    }

    /// The stored `(centuries, nanoseconds)` at `row`.
    pub fn epoch_parts_at(&self, row: usize) -> (i16, u64) {
        (self.centuries.value(row), self.nanos.value(row))
    }
}

/// An efficient builder for creating [`RecordBatch`]es following the SpaceTimestamp schema.
///
/// To create [`RecordBatch`]es, the builder pattern allows for row-based entry & gives
/// caller control of when entered data should be flushed into a formal [`RecordBatch`].
pub struct SpaceTimestampBuilder {
    frame_id: FixedSizeBinaryBuilder,
    units_pos: UInt8Builder,
    timescale_id: UInt8Builder,
    source_id: FixedSizeBinaryBuilder,
    estimate_type: UInt8Builder,
    position: FixedSizeListBuilder<Float64Builder>,
    quaternion: FixedSizeListBuilder<Float64Builder>,
    duration_centuries: Int16Builder,
    duration_ns: UInt64Builder,
    position_covariance: FixedSizeListBuilder<Float64Builder>,
    orientation_covariance: FixedSizeListBuilder<Float64Builder>,
}

impl SpaceTimestampBuilder {
    /// Creates a new builder pre-allocated for the given capacity.
    ///
    /// # Arguments
    /// * `capacity` - The expected number of rows to be ingested before a flush.
    pub fn new(capacity: usize) -> Self {
        Self {
            frame_id: id_builder(capacity),
            units_pos: UInt8Builder::with_capacity(capacity),
            timescale_id: UInt8Builder::with_capacity(capacity),
            source_id: id_builder(capacity),
            estimate_type: UInt8Builder::with_capacity(capacity),
            position: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            quaternion: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 4), 4),
            duration_centuries: Int16Builder::with_capacity(capacity),
            duration_ns: UInt64Builder::with_capacity(capacity),
            position_covariance: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 6),
                6,
            ),
            orientation_covariance: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 6),
                6,
            ),
        }
    }

    /// Returns the number of rows currently buffered in the builder.
    pub fn len(&self) -> usize {
        self.duration_ns.len()
    }

    /// Returns true if the builder contains no data.
    pub fn is_empty(&self) -> bool {
        self.duration_ns.is_empty()
    }

    /// Appends a "single spacetimestamp" to the internal builders.
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn append_spacetimestamp(
        &mut self,
        frame_id: PrescribedId,
        units_pos: LengthUnit,
        timescale_id: TimeScaleCode,
        source_id: PrescribedId,
        estimate_type: EstimateType,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
        position_covariance: Option<[f64; 6]>,
        orientation_covariance: Option<[f64; 6]>,
    ) {
        self.frame_id
            .append_value(frame_id.as_bytes())
            .expect("PrescribedId is 16 bytes");
        self.units_pos.append_value(units_pos.code());
        self.timescale_id.append_value(timescale_id.code());
        self.source_id
            .append_value(source_id.as_bytes())
            .expect("PrescribedId is 16 bytes");
        self.estimate_type.append_value(estimate_type.code());

        for p in position {
            self.position.values().append_value(p);
        }
        self.position.append(true);

        for q in quaternion {
            self.quaternion.values().append_value(q);
        }
        self.quaternion.append(true);

        self.duration_centuries.append_value(duration_centuries);
        self.duration_ns.append_value(duration_ns);

        append_optional_list(&mut self.position_covariance, position_covariance);
        append_optional_list(&mut self.orientation_covariance, orientation_covariance);
    }

    /// Consumes the buffered data and returns an Arrow [`RecordBatch`].
    ///
    /// This builds the formal ['RecordBatch'].
    pub fn flush(&mut self) -> RecordBatch {
        let schema = sts_schema();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(self.frame_id.finish()),
                Arc::new(self.units_pos.finish()),
                Arc::new(self.timescale_id.finish()),
                Arc::new(self.source_id.finish()),
                Arc::new(self.estimate_type.finish()),
                Arc::new(self.position.finish()),
                Arc::new(self.quaternion.finish()),
                Arc::new(self.duration_centuries.finish()),
                Arc::new(self.duration_ns.finish()),
                Arc::new(self.position_covariance.finish()),
                Arc::new(self.orientation_covariance.finish()),
            ],
        )
        .expect("should create record batch")
    }

    /// Consumes the buffered data and returns a [`StructArray`].
    ///
    /// This is useful for embedding the SpaceTimestamp as a single nested
    /// column within a larger Arrow schema.
    pub fn finish_as_struct(&mut self) -> StructArray {
        let schema = sts_schema();
        let fields = schema.fields().clone();
        let arrays: Vec<Arc<dyn Array>> = vec![
            Arc::new(self.frame_id.finish()),
            Arc::new(self.units_pos.finish()),
            Arc::new(self.timescale_id.finish()),
            Arc::new(self.source_id.finish()),
            Arc::new(self.estimate_type.finish()),
            Arc::new(self.position.finish()),
            Arc::new(self.quaternion.finish()),
            Arc::new(self.duration_centuries.finish()),
            Arc::new(self.duration_ns.finish()),
            Arc::new(self.position_covariance.finish()),
            Arc::new(self.orientation_covariance.finish()),
        ];
        StructArray::try_new(fields, arrays, None).expect("should create struct array")
    }
}

/// Exports the [`sts_schema`] to an Arrow IPC file with 0 records.
///
/// This may be useful for distributing the schema to other languages (Python, C++, etc.)
/// in a format they can natively understand.
pub fn export_sts_schema_to_file<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::ipc::write_file(path.as_ref(), &[], &sts_schema())?;
    Ok(())
}

#[cfg(test)]
mod sts_columns_tests {
    use super::*;

    fn frame() -> PrescribedId {
        PrescribedId::astronomical_from_name("ICRF").unwrap()
    }

    fn source() -> PrescribedId {
        PrescribedId::abstract_source("test", "src").unwrap()
    }

    /// Two rows whose position x is 10.0 and 20.0, so a slice is detectable.
    fn builder() -> SpaceTimestampBuilder {
        let mut b = SpaceTimestampBuilder::new(2);
        for (i, x) in [10.0_f64, 20.0].into_iter().enumerate() {
            b.append_spacetimestamp(
                frame(),
                LengthUnit::km,
                TimeScaleCode::TAI,
                source(),
                EstimateType::MEASURED,
                [x, x + 1.0, x + 2.0],
                [1.0, 0.0, 0.0, 0.0],
                i as i16,
                i as u64 * 100,
                None,
                None,
            );
        }
        b
    }

    fn flat_batch() -> RecordBatch {
        builder().flush()
    }

    fn nested_batch() -> RecordBatch {
        let sts = builder().finish_as_struct();
        let schema = Arc::new(Schema::new(vec![Field::new(
            STS_COLUMN,
            DataType::Struct(sts_schema().fields().clone()),
            false,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(sts)]).unwrap()
    }

    fn assert_reads_row_zero(cols: &StsColumns<'_>) {
        assert_eq!(cols.position_at(0), [10.0, 11.0, 12.0]);
        assert_eq!(cols.quaternion_at(0), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(cols.epoch_parts_at(0), (0, 0));
        assert_eq!(cols.units_at(0).unwrap(), LengthUnit::km);
        assert_eq!(cols.timescale_at(0).unwrap(), TimeScaleCode::TAI);
        assert_eq!(cols.estimate_at(0).unwrap(), EstimateType::MEASURED);
        assert_eq!(cols.frame_at(0).unwrap(), frame());
        assert_eq!(cols.source_at(0).unwrap(), source());
    }

    #[test]
    fn resolves_both_layouts_identically() {
        let flat = flat_batch();
        let nested = nested_batch();

        let from_flat = StsColumns::try_new(&flat).unwrap();
        let from_nested = StsColumns::try_new(&nested).unwrap();

        assert_eq!(from_flat.len(), 2);
        assert_eq!(from_nested.len(), 2);
        assert_reads_row_zero(&from_flat);
        assert_reads_row_zero(&from_nested);
    }

    #[test]
    fn reads_account_for_a_sliced_batch() {
        // Row 1 of the original becomes row 0 of the slice. Without the list's Arrow
        // offset, position_at(0) would return the unsliced row 0 instead.
        for batch in [flat_batch(), nested_batch()] {
            let sliced = batch.slice(1, 1);
            let cols = StsColumns::try_new(&sliced).unwrap();

            assert_eq!(cols.len(), 1);
            assert_eq!(cols.position_at(0), [20.0, 21.0, 22.0]);
            assert_eq!(cols.epoch_parts_at(0), (1, 100));
        }
    }

    #[test]
    fn a_missing_column_names_itself() {
        let flat = flat_batch();
        let keep: Vec<_> = flat
            .schema()
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name() != POSITION_COLUMN)
            .map(|(i, f)| (i, f.clone()))
            .collect();
        let schema = Arc::new(Schema::new(
            keep.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>(),
        ));
        let columns = keep.iter().map(|(i, _)| flat.column(*i).clone()).collect();
        let without_position = RecordBatch::try_new(schema, columns).unwrap();

        let err = StsColumns::try_new(&without_position).unwrap_err();
        assert!(err.contains(POSITION_COLUMN), "{err}");
    }

    #[test]
    fn a_non_struct_sts_column_is_rejected() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            STS_COLUMN,
            DataType::Utf8,
            false,
        )]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(arrow::array::StringArray::from(vec![
                "not a struct",
            ]))],
        )
        .unwrap();

        let err = StsColumns::try_new(&batch).unwrap_err();
        assert!(err.contains("StructArray"), "{err}");
    }

    #[test]
    fn a_wrongly_typed_column_names_the_expected_type() {
        let flat = flat_batch();
        let idx = flat.schema().index_of(DURATION_NS_COLUMN).unwrap();

        let mut fields: Vec<Field> = flat
            .schema()
            .fields()
            .iter()
            .map(|f| (**f).clone())
            .collect();
        fields[idx] = Field::new(DURATION_NS_COLUMN, DataType::Int32, false);

        let mut columns: Vec<ArrayRef> = flat.columns().to_vec();
        columns[idx] = Arc::new(arrow::array::Int32Array::from(vec![0_i32, 100]));

        let broken = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap();

        let err = StsColumns::try_new(&broken).unwrap_err();
        assert!(err.contains(DURATION_NS_COLUMN), "{err}");
        assert!(err.contains("UInt64"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::DataType;
    use log::info;
    use test_log::test;

    /// Helper function to mint a PrescribedID that is of `KIND_ASTRO`
    fn frame(name: &str) -> PrescribedId {
        PrescribedId::astronomical_from_name(name).expect("test frame name should be valid")
    }

    /// Helper function to mint a PrescribedID that is of `KIND_ABSTRACT`
    fn source(name: &str) -> PrescribedId {
        PrescribedId::abstract_source("test", name).expect("test source name should be valid")
    }

    #[test]
    fn test_schema_type_definition() {
        let s = sts_schema();
        assert_eq!(s.fields().len(), 11);

        for name in [FRAME_ID_COLUMN, SOURCE_ID_COLUMN] {
            let id_field = s.field_with_name(name).unwrap();
            assert_eq!(
                id_field,
                &crate::identity::id_field(name),
                "{name} must be built by id_field"
            );
        }

        // The wire contract for the three vocabularies: a UInt8 code plus the two metadata
        // keys a foreign reader decodes with. Asserted literally, not via the constructor.
        for (name, extension, vocabulary) in [
            (
                "units_pos",
                "soloc.length_unit",
                "-,km,m,cm,mm,au,in,ft,mi,nmi",
            ),
            (
                "timescale_id",
                "soloc.timescale",
                "-,TAI,TT,ET,TDB,UTC,GPST,GST,BDT,QZSST,TCG,TCB,TL,TCL",
            ),
            (
                "estimate_type",
                "soloc.estimate_type",
                "-,MEASURED,ESTIMATED,SIMULATED",
            ),
        ] {
            let f = s.field_with_name(name).unwrap();
            assert_eq!(f.data_type(), &DataType::UInt8, "{name}");
            assert!(!f.is_nullable(), "{name}");
            assert_eq!(
                f.metadata().get("ARROW:extension:name").map(String::as_str),
                Some(extension),
                "{name}"
            );
            assert_eq!(
                f.metadata()
                    .get("ARROW:extension:metadata")
                    .map(String::as_str),
                Some(vocabulary),
                "{name}"
            );
        }

        let pos = s.field_with_name("position").unwrap();
        match pos.data_type() {
            DataType::FixedSizeList(f, size) => {
                assert_eq!(f.data_type(), &DataType::Float64);
                assert_eq!(*size, 3);
            }
            _ => panic!("position should be FixedSizeList(Float64, 3)"),
        }

        let q = s.field_with_name("quaternion").unwrap();
        match q.data_type() {
            DataType::FixedSizeList(f, size) => {
                assert_eq!(f.data_type(), &DataType::Float64);
                assert_eq!(*size, 4);
            }
            _ => panic!("quaternion should be FixedSizeList(Float64, 4)"),
        }

        let dur = s.field_with_name("duration_centuries").unwrap();
        assert_eq!(dur.data_type(), &DataType::Int16);

        let dur_ns = s.field_with_name("duration_ns").unwrap();
        assert_eq!(dur_ns.data_type(), &DataType::UInt64);
    }

    #[test]
    fn test_recordbatch_generation_and_sampling() {
        use arrow::array::{FixedSizeListArray, Float64Array};

        let mut builder = SpaceTimestampBuilder::new(10);

        // Generate 10 rows
        for i in 0..10 {
            builder.append_spacetimestamp(
                frame("EME2000"),
                LengthUnit::km,
                TimeScaleCode::TDB,
                source("sensor_1"),
                EstimateType::MEASURED,
                [i as f64, i as f64 * 10.0, i as f64 * 100.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                i as u64,
                None,
                None,
            );
        }

        assert_eq!(builder.len(), 10);
        let batch = builder.flush();

        info!("Created batch with {} rows", batch.num_rows());
        assert_eq!(batch.num_rows(), 10);

        // Sample a subset
        let subset = batch.slice(2, 4); // Offset 2, length 4
        assert_eq!(subset.num_rows(), 4);

        // Verify data in subset
        let pos_col = subset
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();

        for i in 0..4 {
            // The value at index `i` in the subset corresponds to `i + 2` in the original data
            let list_val = pos_col.value(i);
            let pos_vals = list_val.as_any().downcast_ref::<Float64Array>().unwrap();
            assert_eq!(pos_vals.value(0), (i + 2) as f64);
        }
        info!("Subset verification successful");
    }

    #[test]
    fn test_schema_export_and_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        use arrow::ipc::reader::FileReader;
        use std::fs::File;

        // Create a temporary file path
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("sts_schema_test.arrow");

        // 1. Export the schema
        export_sts_schema_to_file(&file_path)?;

        // 2. Load the schema back from the file
        let file = File::open(&file_path)?;
        let reader = FileReader::try_new(file, None)?;
        let loaded_schema = reader.schema();

        // Verify the loaded schema matches our expectations
        assert_eq!(loaded_schema.fields().len(), 11);
        assert!(loaded_schema.field_with_name("frame_id").is_ok());

        // 3. Generate data using the loaded schema
        let mut builder = SpaceTimestampBuilder::new(5);
        for i in 0..5 {
            builder.append_spacetimestamp(
                frame("ICRF"),
                LengthUnit::m,
                TimeScaleCode::UTC,
                source("sensor_1"),
                EstimateType::ESTIMATED,
                [i as f64; 3],
                [0.0, 0.0, 0.0, 1.0],
                0,
                i as u64,
                None,
                None,
            );
        }

        let batch = builder.flush();

        // 4. Validate the batch
        assert_eq!(batch.num_rows(), 5);
        let timescale_col = batch.column_by_name("timescale_id").unwrap();
        assert_eq!(timescale_col.len(), 5);

        // Clean up
        let _ = std::fs::remove_file(file_path);

        Ok(())
    }
}
