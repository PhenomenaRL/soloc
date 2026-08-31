//! Arrow IPC file-format read and write, shared by every persistence and federation path.
//!
//! Callers own what happens to the batches that come back: concatenate them, merge them
//! atomically, or validate a schema before use. This module only moves bytes.

use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;

use arrow::datatypes::SchemaRef;

/// Writes `batches` under `schema` as a self-contained Arrow IPC file.
fn write_to<W: Write>(sink: W, batches: &[RecordBatch], schema: &SchemaRef) -> Result<(), String> {
    let mut writer = FileWriter::try_new(sink, schema)
        .map_err(|e| format!("Failed to create Arrow IPC writer: {e}"))?;
    for batch in batches {
        writer
            .write(batch)
            .map_err(|e| format!("Failed to write Arrow IPC batch: {e}"))?;
    }
    writer
        .finish()
        .map_err(|e| format!("Failed to finalise Arrow IPC payload: {e}"))
}

/// Reads an Arrow IPC payload into its schema and every batch it carries.
fn read_from<R: Read + Seek>(source: R) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
    let reader =
        FileReader::try_new(source, None).map_err(|e| format!("Failed to open Arrow IPC: {e}"))?;
    let schema = reader.schema();
    let batches = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to read Arrow IPC batch: {e}"))?;
    Ok((schema, batches))
}

/// Serialises `batches` to an in-memory Arrow IPC buffer.
pub fn write_bytes(batches: &[RecordBatch], schema: &SchemaRef) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    write_to(&mut buf, batches, schema)?;
    Ok(buf)
}

/// Reads an in-memory Arrow IPC buffer.
pub fn read_bytes(bytes: &[u8]) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
    read_from(Cursor::new(bytes))
}

/// Reads only the schema of an in-memory Arrow IPC buffer, which works on a zero-batch payload.
pub fn read_bytes_schema(bytes: &[u8]) -> Result<SchemaRef, String> {
    FileReader::try_new(Cursor::new(bytes), None)
        .map(|r| r.schema())
        .map_err(|e| format!("Failed to open Arrow IPC: {e}"))
}

/// Writes `batches` straight to `path`, without buffering the payload in memory.
pub fn write_file(path: &Path, batches: &[RecordBatch], schema: &SchemaRef) -> Result<(), String> {
    let file =
        File::create(path).map_err(|e| format!("Failed to create '{}': {e}", path.display()))?;
    write_to(file, batches, schema)
}

/// Reads an Arrow IPC file.
pub fn read_file(path: &Path) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
    let file = File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;
    read_from(file)
}

/// Reads only the schema of an Arrow IPC file, which works on a zero-batch file.
pub fn read_file_schema(path: &Path) -> Result<SchemaRef, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;
    FileReader::try_new(file, None)
        .map(|r| r.schema())
        .map_err(|e| format!("Failed to open Arrow IPC: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PrescribedId;
    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};

    fn frame() -> PrescribedId {
        PrescribedId::astronomical_from_name("ICRF").unwrap()
    }

    fn source() -> PrescribedId {
        PrescribedId::abstract_source("test", "src").unwrap()
    }

    fn batch(x: f64) -> RecordBatch {
        let mut b = SpaceTimestampBuilder::new(1);
        b.append_spacetimestamp(
            frame(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [x, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        b.flush()
    }

    #[test]
    fn bytes_round_trip_preserves_every_batch_separately() {
        let schema = sts_schema();
        let bytes = write_bytes(&[batch(1.0), batch(2.0)], &schema).unwrap();

        let (read_schema, batches) = read_bytes(&bytes).unwrap();
        assert_eq!(read_schema, schema);
        // Two batches in, two batches out: concatenation is the caller's decision, not ours.
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), 1);
        assert_eq!(batches[1].num_rows(), 1);
    }

    #[test]
    fn file_round_trip_matches_the_bytes_round_trip() {
        let schema = sts_schema();
        let dir = std::env::temp_dir().join("soloc_ipc_file_round_trip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("batches.arrow");

        write_file(&path, &[batch(3.0)], &schema).unwrap();
        let (read_schema, batches) = read_file(&path).unwrap();

        assert_eq!(read_schema, schema);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_zero_batch_payload_still_carries_its_schema() {
        let schema = sts_schema();
        let bytes = write_bytes(&[], &schema).unwrap();

        assert_eq!(read_bytes_schema(&bytes).unwrap(), schema);
        let (read_schema, batches) = read_bytes(&bytes).unwrap();
        assert_eq!(read_schema, schema);
        assert!(batches.is_empty());
    }

    #[test]
    fn schema_only_reads_do_not_need_the_batches() {
        let schema = sts_schema();
        let dir = std::env::temp_dir().join("soloc_ipc_schema_only");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("schema.arrow");

        write_file(&path, &[batch(4.0)], &schema).unwrap();
        assert_eq!(read_file_schema(&path).unwrap(), schema);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(read_bytes(b"not arrow ipc").is_err());
        assert!(read_bytes_schema(b"not arrow ipc").is_err());
        assert!(read_file(Path::new("/nonexistent/soloc/ledger.arrow")).is_err());
    }
}
