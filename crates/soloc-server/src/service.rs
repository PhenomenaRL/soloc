use std::pin::Pin;
use std::sync::Arc;

use arrow::ipc::writer::IpcWriteOptions;
use arrow::record_batch::RecordBatch;
use arrow_flight::error::FlightError;
use arrow_flight::{
    decode::FlightRecordBatchStream, encode::FlightDataEncoderBuilder,
    flight_service_server::FlightService, Action, ActionType, Criteria, Empty, FlightData,
    FlightDescriptor, FlightInfo, HandshakeRequest, HandshakeResponse, PollInfo, PutResult,
    SchemaAsIpc, SchemaResult, Ticket,
};
use futures::{Stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use tonic::{Request, Response, Status, Streaming};

use anise::almanac::metaload::MetaFile;
use soloc::ephemeris::naif_snapshot;
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::STS_COLUMN;
use spacetimestamp::transforms::transform_batch;

use arrow::array::{Array, DictionaryArray, StringArray, StructArray};
use arrow::datatypes::UInt32Type;
use hifitime::Epoch;

use crate::state::ServerState;

type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

#[derive(Deserialize)]
struct SaveLedgerBody {
    path: String,
}

#[derive(Deserialize)]
struct LoadKernelBody {
    /// URL (http/https) or local filesystem path to a SPICE kernel (BSP/PCK/BPC).
    source: String,
}

#[derive(Deserialize)]
struct NaifBodyEntry {
    naif_id: i32,
    entity_id: String,
}

#[derive(Deserialize)]
struct AppendSnapshotBody {
    bodies: Vec<NaifBodyEntry>,
    /// Epoch expressed as TAI seconds past J2000.
    epoch_tai_s: f64,
}

#[derive(Deserialize)]
struct ExchangeDescriptor {
    target_frame: String,
    #[serde(default = "default_km")]
    target_units: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum QueryType {
    #[default]
    Filter,
    CurrentState,
}

#[derive(Deserialize)]
struct GetTicket {
    #[serde(default)]
    time_range_tai_s: Option<[f64; 2]>,
    #[serde(default)]
    spatial_origin: Option<[f64; 3]>,
    #[serde(default)]
    spatial_radius: Option<f64>,
    #[serde(default)]
    query_type: QueryType,
    #[serde(default)]
    entity_ids: Option<Vec<String>>,
    #[serde(default)]
    not_before_tai_s: Option<f64>,
}

fn default_km() -> String {
    "km".to_string()
}

pub struct SolocFlightService {
    pub state: Arc<ServerState>,
}

impl SolocFlightService {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }
}

fn unimplemented<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("not implemented"))
}

/// Serialises a single batch as a self-contained Arrow IPC file, for use as a DoAction body.
fn batch_to_ipc_bytes(batch: &RecordBatch) -> Result<Vec<u8>, Status> {
    use arrow::ipc::writer::FileWriter;
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer = FileWriter::try_new(&mut buf, &batch.schema())
            .map_err(|e| Status::internal(format!("IPC writer init failed: {e}")))?;
        writer
            .write(batch)
            .map_err(|e| Status::internal(format!("IPC write failed: {e}")))?;
        writer
            .finish()
            .map_err(|e| Status::internal(format!("IPC finalise failed: {e}")))?;
    }
    Ok(buf)
}

/// Reads back a single batch written by [`batch_to_ipc_bytes`]. Multi-batch payloads are
/// concatenated so a peer that chunked its export still merges correctly.
fn batch_from_ipc_bytes(bytes: &[u8]) -> Result<RecordBatch, Status> {
    use arrow::ipc::reader::FileReader;
    let reader = FileReader::try_new(std::io::Cursor::new(bytes), None)
        .map_err(|e| Status::invalid_argument(format!("invalid Arrow IPC body: {e}")))?;
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<_, _>>()
        .map_err(|e| Status::invalid_argument(format!("failed to read IPC batches: {e}")))?;
    arrow::compute::concat_batches(&schema, &batches)
        .map_err(|e| Status::invalid_argument(format!("failed to concat IPC batches: {e}")))
}

/// Transforms a batch into the descriptor's target frame. Entity-URI frame IDs are resolved
/// through the ledger's transform tree so that child entities (e.g. a robot with
/// frame_id = "urn:soloc:truck_A") are correctly placed.
fn transform_with_ledger_frames(
    state: &ServerState,
    batch: &RecordBatch,
    desc: &ExchangeDescriptor,
) -> Result<RecordBatch, Status> {
    let almanac = state
        .almanac
        .read()
        .map_err(|_| Status::internal("almanac lock poisoned"))?;

    // Fast path: no entity-URI frames means no ledger lookups, so don't take the lock.
    if !batch_has_uri_frames(batch) {
        return transform_batch(
            batch,
            &desc.target_frame,
            &almanac,
            &desc.target_units,
            None,
        )
        .map_err(|e| Status::internal(format!("transform failed: {e}")));
    }

    let ledger = state
        .ledger
        .read()
        .map_err(|_| Status::internal("ledger lock poisoned"))?;

    // Each row resolves at its own epoch. The previous version derived one epoch from the
    // batch's first row and applied it to every row, which was wrong for a batch spanning
    // multiple timesteps.
    let resolver = |frame: &str, epoch: Epoch| ledger.resolve_to_root(frame, epoch);

    transform_batch(
        batch,
        &desc.target_frame,
        &almanac,
        &desc.target_units,
        Some(&resolver),
    )
    .map_err(|e| Status::internal(format!("transform failed: {e}")))
}

/// Whether the batch's `frame_id` dictionary contains any entity URI (e.g. `"demo:truck_A"`),
/// meaning the transform will need ledger lookups to resolve them.
fn batch_has_uri_frames(batch: &RecordBatch) -> bool {
    let Some(sts_col) = batch
        .column_by_name(STS_COLUMN)
        .and_then(|c| c.as_any().downcast_ref::<StructArray>())
    else {
        return false;
    };
    let Some(frames) = sts_col
        .column_by_name("frame_id")
        .and_then(|c| c.as_any().downcast_ref::<DictionaryArray<UInt32Type>>())
    else {
        return false;
    };
    let Some(frames_dict) = frames.values().as_any().downcast_ref::<StringArray>() else {
        return false;
    };

    (0..frames_dict.len())
        .filter(|&i| !frames_dict.is_null(i))
        .any(|i| spacetimestamp::schema::is_entity_uri(frames_dict.value(i)))
}

#[tonic::async_trait]
impl FlightService for SolocFlightService {
    type HandshakeStream = BoxStream<Result<HandshakeResponse, Status>>;
    type ListFlightsStream = BoxStream<Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<Result<FlightData, Status>>;
    type DoPutStream = BoxStream<Result<PutResult, Status>>;
    type DoExchangeStream = BoxStream<Result<FlightData, Status>>;
    type DoActionStream = BoxStream<Result<arrow_flight::Result, Status>>;
    type ListActionsStream = BoxStream<Result<ActionType, Status>>;

    async fn handshake(
        &self,
        _: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        unimplemented()
    }

    async fn list_flights(
        &self,
        _: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        unimplemented()
    }

    async fn get_flight_info(
        &self,
        _: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        unimplemented()
    }

    async fn poll_flight_info(
        &self,
        _: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        unimplemented()
    }

    async fn get_schema(
        &self,
        _: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        let schema = self
            .state
            .ledger
            .read()
            .map_err(|_| Status::internal("ledger lock poisoned"))?
            .schema()
            .clone();
        let ipc_options = IpcWriteOptions::default();
        let schema_as_ipc = SchemaAsIpc::new(&schema, &ipc_options);
        let flight_data: FlightData = schema_as_ipc.into();
        Ok(Response::new(SchemaResult {
            schema: flight_data.data_header,
        }))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let state = self.state.clone();
        let ticket: GetTicket = serde_json::from_slice(&request.into_inner().ticket)
            .map_err(|e| Status::invalid_argument(format!("invalid ticket JSON: {e}")))?;

        let mut filter = SpatiotemporalFilter::new();
        if let Some([start, end]) = ticket.time_range_tai_s {
            filter = filter.with_time_range(
                hifitime::Epoch::from_tai_seconds(start),
                hifitime::Epoch::from_tai_seconds(end),
            );
        }
        if let (Some(origin), Some(radius)) = (ticket.spatial_origin, ticket.spatial_radius) {
            filter = filter.with_spatial(origin, radius);
        }

        let batches: Vec<RecordBatch> = {
            let ledger = state
                .ledger
                .read()
                .map_err(|_| Status::internal("ledger lock poisoned"))?;
            match ticket.query_type {
                QueryType::Filter => ledger
                    .stream_query(&filter)
                    .filter_map(|r| r.ok())
                    .filter(|b| b.num_rows() > 0)
                    .collect::<Vec<_>>(),
                QueryType::CurrentState => {
                    let entity_ids_refs: Option<Vec<&str>> = ticket
                        .entity_ids
                        .as_ref()
                        .map(|v| v.iter().map(|s| s.as_str()).collect());
                    let entity_ids: Option<&[&str]> = entity_ids_refs.as_deref();
                    let not_before = ticket
                        .not_before_tai_s
                        .map(hifitime::Epoch::from_tai_seconds);
                    let result = ledger
                        .current_state(entity_ids, not_before)
                        .map_err(|e| Status::internal(format!("current_state failed: {e}")))?;
                    if result.num_rows() == 0 {
                        vec![]
                    } else {
                        vec![result]
                    }
                }
            }
        };

        let out_stream = FlightDataEncoderBuilder::new()
            .build(futures::stream::iter(
                batches.into_iter().map(Ok::<_, FlightError>),
            ))
            .map_err(|e| Status::internal(e.to_string()));

        Ok(Response::new(Box::pin(out_stream)))
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        let state = self.state.clone();
        let in_stream = request
            .into_inner()
            .map_err(|e| FlightError::Tonic(Box::new(e)));
        let mut batch_stream = FlightRecordBatchStream::new_from_flight_data(in_stream);

        while let Some(batch) = batch_stream.next().await {
            let batch = batch.map_err(|e| Status::internal(e.to_string()))?;

            // Topology is derived from the rows themselves by `Ledger::append`, so the
            // batch is stored as-is. A batch introducing a cycle or an unresolvable parent
            // frame is rejected here.
            state
                .ledger
                .write()
                .map_err(|_| Status::internal("ledger lock poisoned"))?
                .append(batch)
                .map_err(|e| Status::invalid_argument(format!("append failed: {e}")))?;
        }

        Ok(Response::new(Box::pin(futures::stream::empty())))
    }

    async fn do_exchange(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        let state = self.state.clone();
        let mut in_stream = request.into_inner();

        // First message carries the descriptor; it may also contain the IPC schema.
        let first = in_stream
            .message()
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::invalid_argument("exchange stream is empty"))?;

        let descriptor = first.flight_descriptor.clone().ok_or_else(|| {
            Status::invalid_argument("first message must contain a flight_descriptor")
        })?;
        let desc: ExchangeDescriptor = serde_json::from_slice(&descriptor.cmd)
            .map_err(|e| Status::invalid_argument(format!("invalid descriptor JSON: {e}")))?;

        // Prepend first message back so FlightRecordBatchStream sees the schema message.
        let first_stream =
            futures::stream::once(futures::future::ready(Ok::<FlightData, FlightError>(first)));
        let rest = in_stream.map_err(|e| FlightError::Tonic(Box::new(e)));
        let mut batch_stream =
            FlightRecordBatchStream::new_from_flight_data(first_stream.chain(rest));

        let mut transformed: Vec<RecordBatch> = Vec::new();
        while let Some(batch) = batch_stream.next().await {
            let batch = batch.map_err(|e| Status::internal(e.to_string()))?;
            transformed.push(transform_with_ledger_frames(&state, &batch, &desc)?);
        }

        let out_stream = FlightDataEncoderBuilder::new()
            .build(futures::stream::iter(
                transformed.into_iter().map(Ok::<_, FlightError>),
            ))
            .map_err(|e| Status::internal(e.to_string()));

        Ok(Response::new(Box::pin(out_stream)))
    }

    async fn list_actions(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        let actions = vec![
            Ok(ActionType {
                r#type: "export_topology".to_string(),
                description: "Export the full transform-tree event log for federation. \
                              No body. Returns an Arrow IPC file of topology_schema() rows."
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "import_topology".to_string(),
                description: "Merge a topology log exported by a peer's export_topology. \
                              Body: the raw Arrow IPC bytes. Rejected if it would form a cycle."
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "save_ledger".to_string(),
                description: "Persist the ledger to an Arrow IPC file. Body: {path}".to_string(),
            }),
            Ok(ActionType {
                r#type: "load_ledger".to_string(),
                description: "Replace the in-memory ledger from an Arrow IPC file. Body: {path}"
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "load_kernel".to_string(),
                description: "Load a SPICE kernel (BSP/PCK/BPC) into the server almanac. \
                              Body: {source} where source is an http/https URL or local path. \
                              URLs are downloaded and cached in the anise data directory."
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "append_snapshot".to_string(),
                description: "Query the almanac for arbitrary NAIF bodies at a given epoch and \
                              append the result to the ledger. \
                              Body: {bodies: [{naif_id, entity_id}], epoch_tai_s}"
                    .to_string(),
            }),
        ];
        Ok(Response::new(Box::pin(futures::stream::iter(actions))))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        let action = request.into_inner();

        match action.r#type.as_str() {
            "export_topology" => {
                let batch = self
                    .state
                    .ledger
                    .read()
                    .map_err(|_| Status::internal("ledger lock poisoned"))?
                    .export_topology()
                    .map_err(|e| Status::internal(format!("export_topology failed: {e}")))?;
                let bytes = batch_to_ipc_bytes(&batch)?;
                let result = arrow_flight::Result { body: bytes.into() };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            "import_topology" => {
                let batch = batch_from_ipc_bytes(&action.body)?;
                let applied = self
                    .state
                    .ledger
                    .write()
                    .map_err(|_| Status::internal("ledger lock poisoned"))?
                    .merge_topology(&batch)
                    .map_err(|e| {
                        Status::invalid_argument(format!("import_topology failed: {e}"))
                    })?;
                let result = arrow_flight::Result {
                    body: format!("applied {applied} topology events")
                        .into_bytes()
                        .into(),
                };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            "save_ledger" => {
                let body: SaveLedgerBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid save_ledger body: {e}"))
                })?;
                let ledger = self
                    .state
                    .ledger
                    .read()
                    .map_err(|_| Status::internal("ledger lock poisoned"))?;
                let n = ledger.len();
                ledger
                    .save_ipc(std::path::Path::new(&body.path))
                    .map_err(|e| Status::internal(format!("save_ledger failed: {e}")))?;
                let result = arrow_flight::Result {
                    body: format!("saved {n} batches to {}", body.path)
                        .into_bytes()
                        .into(),
                };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            "load_ledger" => {
                let body: SaveLedgerBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid load_ledger body: {e}"))
                })?;
                let new_ledger = soloc::ledger::Ledger::load_ipc(
                    std::path::Path::new(&body.path),
                    &self.state.id_column,
                )
                .map_err(|e| Status::internal(format!("load_ledger failed: {e}")))?;
                let n = new_ledger.len();
                *self
                    .state
                    .ledger
                    .write()
                    .map_err(|_| Status::internal("ledger lock poisoned"))? = new_ledger;
                let result = arrow_flight::Result {
                    body: format!("loaded {n} batches from {}", body.path)
                        .into_bytes()
                        .into(),
                };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            "load_kernel" => {
                let body: LoadKernelBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid load_kernel body: {e}"))
                })?;

                // MetaFile::process() downloads the file if it's a URL and updates the
                // uri field to the local cache path, or leaves it as-is for local paths.
                // It is blocking (network I/O), so we run it on the blocking thread pool.
                let source = body.source.clone();
                let local_path = tokio::task::spawn_blocking(move || {
                    let mut meta = MetaFile {
                        uri: source,
                        crc32: None,
                    };
                    meta.process(false).map(|_| meta.uri)
                })
                .await
                .map_err(|e| Status::internal(format!("load_kernel task panicked: {e}")))?
                .map_err(|e| Status::internal(format!("kernel download/resolve failed: {e}")))?;

                // Clone the current almanac, load the new kernel, then swap if successful.
                let mut almanac_guard = self
                    .state
                    .almanac
                    .write()
                    .map_err(|_| Status::internal("almanac lock poisoned"))?;
                let updated = almanac_guard.clone().load(&local_path).map_err(|e| {
                    Status::internal(format!("failed to load kernel '{local_path}': {e}"))
                })?;
                *almanac_guard = updated;
                drop(almanac_guard);

                let msg = format!("kernel loaded: {}", body.source);
                let result = arrow_flight::Result {
                    body: msg.into_bytes().into(),
                };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            "append_snapshot" => {
                let body: AppendSnapshotBody =
                    serde_json::from_slice(&action.body).map_err(|e| {
                        Status::invalid_argument(format!("invalid append_snapshot body: {e}"))
                    })?;

                if body.bodies.is_empty() {
                    return Err(Status::invalid_argument("bodies list is empty"));
                }

                let epoch = hifitime::Epoch::from_tai_seconds(body.epoch_tai_s);
                let pairs: Vec<(i32, String)> = body
                    .bodies
                    .iter()
                    .map(|b| (b.naif_id, b.entity_id.clone()))
                    .collect();

                let almanac = self
                    .state
                    .almanac
                    .read()
                    .map_err(|_| Status::internal("almanac lock poisoned"))?;

                // Build the &str slice from the owned Strings.
                let ref_pairs: Vec<(i32, &str)> =
                    pairs.iter().map(|(id, eid)| (*id, eid.as_str())).collect();

                let batch = naif_snapshot(&almanac, &ref_pairs, epoch)
                    .map_err(|e| Status::internal(format!("naif_snapshot failed: {e}")))?;
                drop(almanac);

                let n = batch.num_rows();
                self.state
                    .ledger
                    .write()
                    .map_err(|_| Status::internal("ledger lock poisoned"))?
                    .append(batch)
                    .map_err(|e| Status::invalid_argument(format!("append failed: {e}")))?;

                let result = arrow_flight::Result {
                    body: format!("appended {n} rows").into_bytes().into(),
                };
                Ok(Response::new(Box::pin(futures::stream::once(
                    futures::future::ready(Ok(result)),
                ))))
            }

            other => Err(Status::invalid_argument(format!(
                "unknown action type: '{other}'"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anise::almanac::Almanac;
    use soloc::schemas::entity::EntityBuilder;

    /// A one-row entity batch placing `entity_id` relative to `frame_id`.
    fn entity_batch(entity_id: &str, frame_id: &str, x: f64) -> RecordBatch {
        let mut b = EntityBuilder::new(1);
        b.append_entity(
            entity_id,
            frame_id,
            "km",
            "TAI",
            "test:src",
            "MEASURED",
            [x, 0.0, 0.0],
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
        b.flush()
    }

    /// A service backed by an empty in-memory entity ledger — no paths, so no disk I/O.
    async fn make_service() -> SolocFlightService {
        let state = ServerState::new(
            Almanac::default(),
            None,
            None,
            None,
            "entity_id".to_string(),
        )
        .await;
        SolocFlightService::new(Arc::new(state))
    }

    /// Drains a DoAction response stream into the single result body it carries.
    async fn action_body(
        response: Response<<SolocFlightService as FlightService>::DoActionStream>,
    ) -> Vec<u8> {
        let results: Vec<_> = response.into_inner().collect().await;
        assert_eq!(results.len(), 1, "expected exactly one action result");
        results
            .into_iter()
            .next()
            .unwrap()
            .expect("action result should not be an error")
            .body
            .to_vec()
    }

    /// The full federation wire path: one server exports its derived topology, a second
    /// server imports the bytes and ends up holding the same edges. This is the only
    /// coverage of `batch_to_ipc_bytes`/`batch_from_ipc_bytes`, which sit between them.
    #[tokio::test]
    async fn test_export_import_topology_round_trip() {
        let exporter = make_service().await;
        {
            let mut ledger = exporter.state.ledger.write().unwrap();
            ledger
                .append(entity_batch("demo:facility", "IAU_EARTH", 50.0))
                .unwrap();
            ledger
                .append(entity_batch("demo:robot", "demo:facility", 5.0))
                .unwrap();
        }

        let exported = exporter
            .do_action(Request::new(Action {
                r#type: "export_topology".to_string(),
                body: Default::default(),
            }))
            .await
            .expect("export_topology should succeed");
        let bytes = action_body(exported).await;
        assert!(!bytes.is_empty(), "export must produce an IPC payload");

        let importer = make_service().await;
        let imported = importer
            .do_action(Request::new(Action {
                r#type: "import_topology".to_string(),
                body: bytes.into(),
            }))
            .await
            .expect("import_topology should succeed");
        let msg = String::from_utf8(action_body(imported).await).unwrap();
        assert_eq!(msg, "applied 2 topology events");

        // The importer now holds the same two edges, so re-exporting reproduces them.
        let reexported = importer
            .state
            .ledger
            .read()
            .unwrap()
            .export_topology()
            .unwrap();
        assert_eq!(reexported.num_rows(), 2);
    }

    /// A malformed body must be rejected as a client error, not surface as an internal panic.
    #[tokio::test]
    async fn test_import_topology_rejects_garbage_body() {
        let service = make_service().await;
        // The Ok variant is a boxed stream and so is not Debug — match rather than expect_err.
        let result = service
            .do_action(Request::new(Action {
                r#type: "import_topology".to_string(),
                body: vec![0xde, 0xad, 0xbe, 0xef].into(),
            }))
            .await;
        match result {
            Ok(_) => panic!("garbage body must be rejected"),
            Err(status) => assert_eq!(status.code(), tonic::Code::InvalidArgument),
        }
    }

    /// Both federation actions must be discoverable, or a peer cannot find them.
    #[tokio::test]
    async fn test_list_actions_advertises_topology_actions() {
        let service = make_service().await;
        let listed: Vec<String> = service
            .list_actions(Request::new(Empty {}))
            .await
            .unwrap()
            .into_inner()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|a| a.unwrap().r#type)
            .collect();

        assert!(
            listed.contains(&"export_topology".to_string()),
            "{listed:?}"
        );
        assert!(
            listed.contains(&"import_topology".to_string()),
            "{listed:?}"
        );
    }
}
