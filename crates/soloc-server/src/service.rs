use std::pin::Pin;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use arrow::ipc::writer::IpcWriteOptions;
use arrow::record_batch::RecordBatch;
use arrow_flight::error::FlightError;
use arrow_flight::{
    decode::FlightRecordBatchStream,
    encode::FlightDataEncoderBuilder,
    flight_service_server::{FlightService, FlightServiceServer},
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaAsIpc, SchemaResult, Ticket,
};
use futures::{Stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use tonic::{Request, Response, Status, Streaming};

use anise::almanac::metaload::MetaFile;
use soloc_ledger::ephemeris::{celestial_orbits, celestial_snapshot};
use spacetimestamp::identity::PrescribedId;
use spacetimestamp::ipc;
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::vocabulary::{LengthUnit, Vocabulary};

use crate::state::{describe, ServerState};

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

/// An identity as it arrives over the wire, in any of the three forms a client can hold.
///
/// A client that minted an id from a common name still has that name; a client that read one
/// out of a query result holds 16 opaque bytes and nothing else. Both have to be able to name
/// the same entity, so both forms are accepted:
///
/// ```jsonc
/// {"authority": "acme.com", "name": "truck_A"}  // KIND_SOLOC, minted here
/// {"ephemeris_id": 399, "orientation_id": 399}  // KIND_ASTRO (Earth), minted here
/// {"id": "018f2a...c41d"}                       // 32 hex digits, hyphens optional
/// ```
#[derive(Deserialize)]
#[serde(untagged)]
enum WireId {
    Named {
        authority: String,
        name: String,
    },
    Astronomical {
        ephemeris_id: i32,
        orientation_id: i32,
    },
    Hex {
        id: String,
    },
}

impl WireId {
    /// Resolves to the 16-byte id, validating as it goes
    fn mint(&self) -> Result<PrescribedId, String> {
        match self {
            WireId::Named { authority, name } => PrescribedId::new(authority, name),
            WireId::Astronomical {
                ephemeris_id,
                orientation_id,
            } => PrescribedId::astronomical(*ephemeris_id, *orientation_id),
            WireId::Hex { id } => parse_hex_id(id),
        }
    }
}

/// Parses the 32 hex digits of [`PrescribedId::to_hyphenated`], with or without hyphens.
///
/// Both spellings are accepted because clients produce both: `bytes.hex()` in Python gives
/// the bare form, `uuid.UUID(bytes=...)` gives the hyphenated one, and neither is more
/// correct than the other for bytes read straight out of an Arrow column.
fn parse_hex_id(s: &str) -> Result<PrescribedId, String> {
    let digits: String = s.chars().filter(|c| *c != '-').collect();
    if digits.len() != 32 {
        return Err(format!(
            "id must be 32 hex digits (hyphens optional), got {} in '{s}'",
            digits.len()
        ));
    }
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("id is not valid hex: '{s}'"))?;
    }
    PrescribedId::from_bytes(&bytes)
}

/// Mints one id, prefixing any error with `label` (e.g. `orbits[3].centre`).
fn mint_one(w: &WireId, label: &str) -> Result<PrescribedId, Status> {
    w.mint()
        .map_err(|e| Status::invalid_argument(format!("{label}: {e}")))
}

/// Mints a whole list, reporting which entry failed — `field` names the list and an index
/// locates the bad one when a client sends fifty.
fn mint_all(ids: &[WireId], field: &str) -> Result<Vec<PrescribedId>, Status> {
    ids.iter()
        .enumerate()
        .map(|(i, w)| mint_one(w, &format!("{field}[{i}]")))
        .collect()
}

#[derive(Deserialize)]
struct AppendSnapshotBody {
    /// The bodies to snapshot, each a [`WireId`] for its `(naif, naif)` astronomical id —
    /// e.g. `{"ephemeris_id": 399, "orientation_id": 399}` for Earth. The id carries the body,
    /// so there is no separate `naif_id`.
    bodies: Vec<WireId>,
    /// Epoch expressed as TAI seconds past J2000.
    epoch_tai_s: f64,
}

#[derive(Deserialize)]
struct OrbitRequest {
    body: WireId,
    /// Defaults to the body's NAIF parent (planets → Sun, moons → planet).
    #[serde(default)]
    centre: Option<WireId>,
}

#[derive(Deserialize)]
struct QueryOrbitsBody {
    orbits: Vec<OrbitRequest>,
    /// Epoch expressed as TAI seconds past J2000.
    epoch_tai_s: f64,
    /// Points per path, at most [`MAX_ORBIT_SAMPLES`].
    #[serde(default = "default_orbit_samples")]
    samples: usize,
}

/// Bounds one path at 2.4 MB, so a request cannot ask for an unbounded reply.
const MAX_ORBIT_SAMPLES: usize = 100_000;

fn default_orbit_samples() -> usize {
    361
}

#[derive(Deserialize)]
struct ExchangeDescriptor {
    target_frame: String,
    #[serde(default = "default_km")]
    target_units: u8,
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
    entity_ids: Option<Vec<WireId>>,
    #[serde(default)]
    not_before_tai_s: Option<f64>,
}

fn default_km() -> u8 {
    LengthUnit::km.code()
}

pub struct SolocFlightService {
    pub state: Arc<ServerState>,
}

/// Default cap on one gRPC message, in bytes, applied to decoding and encoding alike.
/// tonic's own default is 4 MiB.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

impl SolocFlightService {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// Wraps the service for tonic, capping each message at `max_message_size` bytes in both
    /// directions.
    pub fn into_server(self, max_message_size: usize) -> FlightServiceServer<Self> {
        FlightServiceServer::new(self)
            .max_decoding_message_size(max_message_size)
            .max_encoding_message_size(max_message_size)
    }
}

fn unimplemented<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("not implemented"))
}

/// Serialises a single batch as a self-contained Arrow IPC file, for use as a DoAction body.
fn batch_to_ipc_bytes(batch: &RecordBatch) -> Result<Vec<u8>, Status> {
    ipc::write_bytes(std::slice::from_ref(batch), &batch.schema())
        .map_err(|e| Status::internal(format!("IPC write failed: {e}")))
}

/// Reads back a single batch written by [`batch_to_ipc_bytes`]. Multi-batch payloads are
/// concatenated so a peer that chunked its export still merges correctly.
fn batch_from_ipc_bytes(bytes: &[u8]) -> Result<RecordBatch, Status> {
    let (schema, batches) = ipc::read_bytes(bytes)
        .map_err(|e| Status::invalid_argument(format!("invalid Arrow IPC body: {e}")))?;
    arrow::compute::concat_batches(&schema, &batches)
        .map_err(|e| Status::invalid_argument(format!("failed to concat IPC batches: {e}")))
}

/// A DoAction reply carrying one result body.
fn single_result(
    body: Vec<u8>,
) -> Result<Response<BoxStream<Result<arrow_flight::Result, Status>>>, Status> {
    let result = arrow_flight::Result { body: body.into() };
    Ok(Response::new(Box::pin(futures::stream::once(
        futures::future::ready(Ok(result)),
    ))))
}

/// Read-locks `lock`; a poisoned lock is `internal`, naming `what`.
fn read<'a, T>(lock: &'a RwLock<T>, what: &str) -> Result<RwLockReadGuard<'a, T>, Status> {
    lock.read()
        .map_err(|_| Status::internal(format!("{what} lock poisoned")))
}

/// Write-locks `lock`; a poisoned lock is `internal`, naming `what`.
fn write<'a, T>(lock: &'a RwLock<T>, what: &str) -> Result<RwLockWriteGuard<'a, T>, Status> {
    lock.write()
        .map_err(|_| Status::internal(format!("{what} lock poisoned")))
}

/// Transforms a batch into the descriptor's target frame. Frame IDs that name another entity
/// are resolved through the ledger's transform tree
fn transform_with_ledger_frames(
    state: &ServerState,
    batch: &RecordBatch,
    desc: &ExchangeDescriptor,
) -> Result<RecordBatch, Status> {
    let almanac = read(&state.almanac, "almanac")?;
    let ledger = read(&state.ledger, "ledger")?;

    // An unknown unit is a client error, not a silent km.
    let target_units = LengthUnit::from_code(desc.target_units)
        .map_err(|e| Status::invalid_argument(format!("target_units: {e}")))?;

    ledger
        .transform(batch, &desc.target_frame, target_units, &almanac)
        .map_err(|e| Status::internal(format!("transform failed: {e}")))
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
        let schema = read(&self.state.ledger, "ledger")?.schema().clone();
        let ipc_options = IpcWriteOptions::default();
        let result = SchemaResult::try_from(SchemaAsIpc::new(&schema, &ipc_options))
            .map_err(|e| Status::internal(format!("schema encoding failed: {e}")))?;
        Ok(Response::new(result))
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

        let (schema, batches) = {
            let ledger = read(&state.ledger, "ledger")?;
            let batches: Vec<RecordBatch> = match ticket.query_type {
                QueryType::Filter => ledger
                    .stream_query(&filter)
                    .filter_map(|r| r.ok())
                    .filter(|b| b.num_rows() > 0)
                    .collect::<Vec<_>>(),
                QueryType::CurrentState => {
                    let minted: Option<Vec<PrescribedId>> = ticket
                        .entity_ids
                        .as_deref()
                        .map(|ids| mint_all(ids, "entity_ids"))
                        .transpose()?;
                    let entity_ids: Option<&[PrescribedId]> = minted.as_deref();
                    let not_before = ticket
                        .not_before_tai_s
                        .map(hifitime::Epoch::from_tai_seconds);
                    // An empty ledger has no current state, which is an empty answer, not a
                    // server error.
                    if ledger.is_empty() {
                        vec![]
                    } else {
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
            (ledger.schema().clone(), batches)
        };

        // The schema is sent up front so an empty result is still a readable, empty stream.
        let out_stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
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
            write(&state.ledger, "ledger")?
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

        // Prepend the first message back when it also carries the schema; a client may send
        // the descriptor alone (pyarrow does), which the decoder would reject.
        let first_stream =
            futures::stream::iter(
                (!first.data_header.is_empty()).then_some(Ok::<FlightData, FlightError>(first)),
            );
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
                description: "Query the almanac for astronomical bodies at a given epoch and \
                              append the result to the ledger. \
                              Body: {bodies: [<astronomical id>], epoch_tai_s}"
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "query_orbits".to_string(),
                description: "Query the almanac for astronomical bodies' osculating orbits at a \
                              given epoch. Read-only. \
                              Body: {orbits: [{body, centre?}], epoch_tai_s, samples?}. \
                              Returns an Arrow IPC file of orbit_schema() rows: ICRF-axis \
                              elements and a kernel-sampled path over one period."
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "export_names".to_string(),
                description: "Export the display-name registry for federation. No body. \
                              Returns an Arrow IPC file binding each 16-byte id to the \
                              (authority, common_name) it was minted from."
                    .to_string(),
            }),
            Ok(ActionType {
                r#type: "import_names".to_string(),
                description: "Merge a name registry exported by a peer's export_names. \
                              Body: the raw Arrow IPC bytes. Every binding is re-minted and \
                              rejected if it does not hash to the id it claims. Names are \
                              display-only: importing none costs legibility, never correctness."
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
                let batch = read(&self.state.ledger, "ledger")?
                    .export_topology()
                    .map_err(|e| Status::internal(format!("export_topology failed: {e}")))?;
                single_result(batch_to_ipc_bytes(&batch)?)
            }

            "import_topology" => {
                let batch = batch_from_ipc_bytes(&action.body)?;
                let applied = write(&self.state.ledger, "ledger")?
                    .merge_topology(&batch)
                    .map_err(|e| {
                        Status::invalid_argument(format!("import_topology failed: {e}"))
                    })?;
                // `merge_topology` counts events it had not already seen, so re-importing the
                // same peer log correctly reports 0 rather than the row count.
                single_result(format!("applied {applied} topology events").into_bytes())
            }

            "export_names" => {
                let bytes = read(&self.state.ledger, "ledger")?
                    .names_to_ipc_bytes()
                    .map_err(|e| Status::internal(format!("export_names failed: {e}")))?;
                single_result(bytes)
            }

            "import_names" => {
                // Verification is all-or-nothing across the whole payload
                let merged = write(&self.state.ledger, "ledger")?
                    .merge_names_from_ipc_bytes(&action.body)
                    .map_err(|e| Status::invalid_argument(format!("import_names failed: {e}")))?;
                single_result(format!("merged {merged} names").into_bytes())
            }

            "save_ledger" => {
                let body: SaveLedgerBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid save_ledger body: {e}"))
                })?;
                let ledger = read(&self.state.ledger, "ledger")?;
                ledger
                    .save_ipc(std::path::Path::new(&body.path))
                    .map_err(|e| Status::internal(format!("save_ledger failed: {e}")))?;
                single_result(format!("saved {} to {}", describe(&ledger), body.path).into_bytes())
            }

            "load_ledger" => {
                let body: SaveLedgerBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid load_ledger body: {e}"))
                })?;
                let mut new_ledger = soloc_ledger::ledger::Ledger::load_ipc(
                    std::path::Path::new(&body.path),
                    &self.state.id_column,
                )
                .map_err(|e| Status::internal(format!("load_ledger failed: {e}")))?;
                // A replaced ledger keeps the configured limit, or a reload would lift it.
                new_ledger
                    .set_memory_limit(self.state.memory_limit)
                    .map_err(|e| Status::internal(format!("load_ledger failed: {e}")))?;
                let size = describe(&new_ledger);
                *write(&self.state.ledger, "ledger")? = new_ledger;
                single_result(format!("loaded {size} from {}", body.path).into_bytes())
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
                let mut almanac_guard = write(&self.state.almanac, "almanac")?;
                let updated = almanac_guard.clone().load(&local_path).map_err(|e| {
                    Status::internal(format!("failed to load kernel '{local_path}': {e}"))
                })?;
                *almanac_guard = updated;
                drop(almanac_guard);

                single_result(format!("kernel loaded: {}", body.source).into_bytes())
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
                let ids = mint_all(&body.bodies, "bodies")?;

                let almanac = read(&self.state.almanac, "almanac")?;

                let batch = celestial_snapshot(&almanac, &ids, epoch)
                    .map_err(|e| Status::internal(format!("celestial_snapshot failed: {e}")))?;
                drop(almanac);

                let n = batch.num_rows();
                write(&self.state.ledger, "ledger")?
                    .append(batch)
                    .map_err(|e| Status::invalid_argument(format!("append failed: {e}")))?;

                single_result(format!("appended {n} rows").into_bytes())
            }

            "query_orbits" => {
                let body: QueryOrbitsBody = serde_json::from_slice(&action.body).map_err(|e| {
                    Status::invalid_argument(format!("invalid query_orbits body: {e}"))
                })?;

                if body.orbits.is_empty() {
                    return Err(Status::invalid_argument("orbits list is empty"));
                }
                if body.samples > MAX_ORBIT_SAMPLES {
                    return Err(Status::invalid_argument(format!(
                        "samples {} exceeds the cap of {MAX_ORBIT_SAMPLES}",
                        body.samples
                    )));
                }

                let epoch = hifitime::Epoch::from_tai_seconds(body.epoch_tai_s);
                let requests = body
                    .orbits
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        let centre = o
                            .centre
                            .as_ref()
                            .map(|c| mint_one(c, &format!("orbits[{i}].centre")))
                            .transpose()?;
                        Ok((mint_one(&o.body, &format!("orbits[{i}].body"))?, centre))
                    })
                    .collect::<Result<Vec<_>, Status>>()?;

                let almanac = read(&self.state.almanac, "almanac")?;
                let batch = celestial_orbits(&almanac, &requests, epoch, body.samples)
                    .map_err(|e| Status::internal(format!("celestial_orbits failed: {e}")))?;
                drop(almanac);

                single_result(batch_to_ipc_bytes(&batch)?)
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
    use soloc_ledger::schemas::entity::EntityBuilder;
    use spacetimestamp::vocabulary::{EstimateType, TimeScaleCode};

    /// Mints a test entity id under the `demo` authority.
    fn demo(name: &str) -> PrescribedId {
        PrescribedId::new("demo", name).unwrap()
    }

    /// An entity batch placing `entity_id` at `x` km from `frame_id`, one row per TAI
    /// nanosecond offset in `epochs_ns`.
    fn entity_rows(
        entity_id: PrescribedId,
        frame_id: PrescribedId,
        x: f64,
        epochs_ns: std::ops::Range<u64>,
    ) -> RecordBatch {
        let mut b = EntityBuilder::new(epochs_ns.clone().count());
        for ns in epochs_ns {
            b.append_entity(
                entity_id,
                frame_id,
                LengthUnit::km,
                TimeScaleCode::TAI,
                PrescribedId::abstract_source("test", "src").unwrap(),
                EstimateType::MEASURED,
                [x, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                ns,
                None,
                None,
                None,
                None,
                None,
                None,
            );
        }
        b.flush()
    }

    /// A service backed by an empty in-memory entity ledger — no paths, so no disk I/O.
    async fn make_service() -> SolocFlightService {
        make_limited_service(None).await
    }

    /// [`make_service`] with `storage.memory_limit` set to `memory_limit`.
    async fn make_limited_service(memory_limit: Option<usize>) -> SolocFlightService {
        let state = ServerState::new(
            Almanac::default(),
            None,
            None,
            None,
            "entity_id".to_string(),
            memory_limit,
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
            let earth = PrescribedId::astronomical_from_name("IAU_EARTH").unwrap();
            ledger
                .append(entity_rows(demo("facility"), earth, 50.0, 0..1))
                .unwrap();
            ledger
                .append(entity_rows(demo("robot"), demo("facility"), 5.0, 0..1))
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

    /// The schema must arrive as the framed IPC message the Flight spec requires, or pyarrow
    /// and arrow-flight's own decoder reject it.
    #[tokio::test]
    async fn test_get_schema_decodes_as_a_flight_schema() {
        let service = make_service().await;
        let result = service
            .get_schema(Request::new(FlightDescriptor::new_path(vec!["x".into()])))
            .await
            .unwrap()
            .into_inner();
        let schema = arrow::datatypes::Schema::try_from(&result).expect("schema should decode");
        assert_eq!(
            &schema,
            service.state.ledger.read().unwrap().schema().as_ref()
        );
    }

    /// An empty result must still carry the schema, and an empty ledger has an empty current
    /// state rather than an error, or a client cannot tell "nothing yet" from a failure.
    #[tokio::test]
    async fn test_do_get_on_an_empty_ledger_returns_an_empty_table() {
        let service = make_service().await;
        for ticket in [
            r#"{"query_type": "filter"}"#,
            r#"{"query_type": "current_state"}"#,
        ] {
            let response = service
                .do_get(Request::new(Ticket {
                    ticket: ticket.into(),
                }))
                .await
                .unwrap_or_else(|s| panic!("{ticket}: {s}"));
            let mut decoded = FlightRecordBatchStream::new_from_flight_data(
                response
                    .into_inner()
                    .map_err(|e| FlightError::Tonic(Box::new(e))),
            );
            while let Some(batch) = decoded.next().await {
                assert_eq!(batch.unwrap().num_rows(), 0, "{ticket}");
            }
            assert_eq!(
                decoded.schema(),
                Some(service.state.ledger.read().unwrap().schema()),
                "{ticket}"
            );
        }
    }

    /// Serves `service` on a free local port, built as `main` builds it, and connects a client.
    async fn serve(
        service: SolocFlightService,
    ) -> arrow_flight::flight_service_client::FlightServiceClient<tonic::transport::Channel> {
        let incoming =
            tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = incoming.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(service.into_server(DEFAULT_MAX_MESSAGE_SIZE))
                .serve_with_incoming(incoming),
        );

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        arrow_flight::flight_service_client::FlightServiceClient::new(channel)
    }

    /// A single exchange message over tonic's 4 MiB default must be accepted, so a client
    /// need not chunk its rows to stay under it.
    #[tokio::test]
    async fn test_do_exchange_accepts_a_message_over_4_mib() {
        const ROWS: usize = 30_000;
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let batch = entity_rows(demo("bot"), earth, 1.0, 0..ROWS as u64);

        let mut client = serve(make_service().await).await;
        let descriptor = FlightData::new().with_descriptor(FlightDescriptor::new_cmd(
            br#"{"target_frame": "Earth"}"#.to_vec(),
        ));
        // One unsplit message for the whole batch; the encoder's default would split it.
        let mut messages: Vec<FlightData> = FlightDataEncoderBuilder::new()
            .with_max_flight_data_size(usize::MAX)
            .build(futures::stream::iter([Ok(batch)]))
            .try_collect()
            .await
            .unwrap();
        let largest = messages.iter().map(|m| m.data_body.len()).max().unwrap();
        assert!(largest > 4 * 1024 * 1024, "message is only {largest} bytes");
        messages.insert(0, descriptor);

        let replies: Vec<FlightData> = client
            .do_exchange(futures::stream::iter(messages))
            .await
            .expect("exchange should be accepted")
            .into_inner()
            .try_collect()
            .await
            .expect("every reply should decode");
        let mut decoded = FlightRecordBatchStream::new_from_flight_data(futures::stream::iter(
            replies.into_iter().map(Ok),
        ));
        let mut rows = 0;
        while let Some(batch) = decoded.next().await {
            rows += batch.unwrap().num_rows();
        }
        assert_eq!(rows, ROWS);
    }

    /// A client may send the exchange descriptor in a message of its own, ahead of the schema.
    #[tokio::test]
    async fn test_do_exchange_accepts_a_descriptor_only_first_message() {
        let mut client = serve(make_service().await).await;
        let descriptor = FlightData::new().with_descriptor(FlightDescriptor::new_cmd(
            br#"{"target_frame": "IAU_EARTH"}"#.to_vec(),
        ));
        let schema = soloc_ledger::schemas::entity::entity_schema();
        let schema_msg: FlightData = SchemaAsIpc::new(&schema, &IpcWriteOptions::default()).into();

        let replies: Vec<_> = client
            .do_exchange(futures::stream::iter(vec![descriptor, schema_msg]))
            .await
            .expect("exchange should be accepted")
            .into_inner()
            .collect()
            .await;
        assert!(replies.iter().all(Result::is_ok), "{replies:?}");
    }

    /// `load_ledger` builds a fresh ledger from the file; it must carry the configured
    /// memory limit, or a reload would silently lift it.
    #[tokio::test]
    async fn test_load_ledger_keeps_the_memory_limit() {
        let earth = PrescribedId::astronomical_from_name("Earth").unwrap();
        let mut full = soloc_ledger::ledger::Ledger::new(
            &soloc_ledger::schemas::entity::entity_schema(),
            "entity_id",
        )
        .unwrap();
        for t in 0..200 {
            full.append(entity_rows(demo("bot"), earth, 1.0, t..t + 1))
                .unwrap();
        }
        let path = std::env::temp_dir().join("soloc_server_load_limit_test.arrows");
        full.save_ipc(&path).unwrap();
        let limit = full.resident_bytes() / 2;

        let service = make_limited_service(Some(limit)).await;
        let reply = service
            .do_action(Request::new(Action {
                r#type: "load_ledger".to_string(),
                body: format!(r#"{{"path": "{}"}}"#, path.display()).into(),
            }))
            .await
            .expect("load_ledger should succeed");
        action_body(reply).await;

        let ledger = service.state.ledger.read().unwrap();
        assert!(
            ledger.resident_bytes() <= limit,
            "{} > {limit}",
            ledger.resident_bytes()
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(soloc_ledger::ledger::names_sibling_path(&path)).ok();
    }

    /// A malformed body must be rejected as a client error, not surface as an internal panic.
    #[tokio::test]
    async fn test_import_topology_rejects_garbage_body() {
        let service = make_service().await;
        // The Ok variant is a boxed stream and so is not Debug
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
        assert!(listed.contains(&"export_names".to_string()), "{listed:?}");
        assert!(listed.contains(&"import_names".to_string()), "{listed:?}");
        assert!(listed.contains(&"query_orbits".to_string()), "{listed:?}");
    }

    /// Runs `query_orbits` with a JSON body, returning only the error status.
    async fn query_orbits_error(body: &str) -> Status {
        let result = make_service()
            .await
            .do_action(Request::new(Action {
                r#type: "query_orbits".to_string(),
                body: body.as_bytes().to_vec().into(),
            }))
            .await;
        match result {
            Ok(_) => panic!("query_orbits should fail for {body}"),
            Err(status) => status,
        }
    }

    #[tokio::test]
    async fn test_query_orbits_rejects_bad_requests_as_client_errors() {
        let earth = r#"{"ephemeris_id": 399, "orientation_id": 399}"#;
        for body in [
            r#"{"orbits": [], "epoch_tai_s": 0}"#.to_string(),
            r#"{"orbits": [{"body": {"ephemeris_id": 399, "orientation_id": 499}}], "epoch_tai_s": 0}"#
                .to_string(),
            format!(r#"{{"orbits": [{{"body": {earth}}}], "epoch_tai_s": 0, "samples": 100001}}"#),
        ] {
            let status = query_orbits_error(&body).await;
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{body}: {status}");
        }

        let bad_centre = format!(
            r#"{{"orbits": [{{"body": {earth}, "centre": {{"id": "zz"}}}}], "epoch_tai_s": 0}}"#
        );
        let status = query_orbits_error(&bad_centre).await;
        assert!(status.message().contains("orbits[0].centre"), "{status}");
    }

    #[tokio::test]
    async fn test_query_orbits_without_kernels_is_descriptive() {
        let status = query_orbits_error(
            r#"{"orbits": [{"body": {"ephemeris_id": 399, "orientation_id": 399}}], "epoch_tai_s": 0}"#,
        )
        .await;
        assert_eq!(status.code(), tonic::Code::Internal);
        assert!(
            status.message().contains("SPK") || status.message().contains("GM"),
            "{status}"
        );
    }

    #[tokio::test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    async fn test_query_orbits_replies_with_orbit_schema_ipc() {
        let almanac = anise::prelude::MetaAlmanac::latest().unwrap();
        let state =
            ServerState::new(almanac, None, None, None, "entity_id".to_string(), None).await;
        let service = SolocFlightService::new(Arc::new(state));
        let body = r#"{"orbits": [
            {"body": {"ephemeris_id": 399, "orientation_id": 399}},
            {"body": {"ephemeris_id": 301, "orientation_id": 301}}
        ], "epoch_tai_s": 0, "samples": 11}"#;
        let response = service
            .do_action(Request::new(Action {
                r#type: "query_orbits".to_string(),
                body: body.as_bytes().to_vec().into(),
            }))
            .await
            .expect("query_orbits should succeed with kernels");

        let batch = batch_from_ipc_bytes(&action_body(response).await).unwrap();
        assert_eq!(batch.schema(), soloc_ledger::ephemeris::orbit_schema());
        assert_eq!(batch.num_rows(), 2);
    }

    fn wire(json: &str) -> Result<PrescribedId, String> {
        serde_json::from_str::<WireId>(json)
            .map_err(|e| e.to_string())
            .and_then(|w| w.mint())
    }

    /// Each wire form must reach the id its client meant, and the three must agree with the
    /// constructors they stand in for.
    #[test]
    fn test_wire_id_forms_mint_their_constructors() {
        assert_eq!(
            wire(r#"{"authority": "acme.com", "name": "truck_A"}"#).unwrap(),
            PrescribedId::new("acme.com", "truck_A").unwrap()
        );
        assert_eq!(
            wire(r#"{"ephemeris_id": 0, "orientation_id": 1}"#).unwrap(),
            PrescribedId::astronomical(0, 1).unwrap()
        );
    }

    /// An unrecognised frame pair is rejected at the wire boundary, not stored as garbage.
    #[test]
    fn test_wire_astronomical_rejects_an_unrecognised_pair() {
        assert!(wire(r#"{"ephemeris_id": 399, "orientation_id": 499}"#).is_err());
    }

    /// The reserved authority must not imply the astronomical kind. A soloc id minted under
    /// authority `astro` with name `ICRF` and the real astronomical ICRF frame are different
    /// ids — the kind is part of the identity
    #[test]
    fn test_astro_authority_does_not_mint_the_astronomical_id() {
        let squatter = wire(r#"{"authority": "astro", "name": "ICRF"}"#).unwrap();
        let real = wire(r#"{"ephemeris_id": 0, "orientation_id": 1}"#).unwrap();
        assert_ne!(squatter, real);
        assert!(squatter.is_soloc());
        assert!(real.is_astro());
    }

    /// A client that read raw bytes out of a query result holds no name, so the hex form has
    /// to round-trip
    #[test]
    fn test_hex_wire_id_round_trips_hyphenated_and_bare() {
        let id = demo("truck_A");
        let hyphenated = id.to_hyphenated();
        let bare: String = hyphenated.chars().filter(|c| *c != '-').collect();

        assert_eq!(wire(&format!(r#"{{"id": "{hyphenated}"}}"#)).unwrap(), id);
        assert_eq!(wire(&format!(r#"{{"id": "{bare}"}}"#)).unwrap(), id);
    }

    /// Malformed ids must fail at the wire boundary
    #[test]
    fn test_malformed_hex_wire_ids_are_rejected() {
        assert!(wire(r#"{"id": "abcd"}"#).is_err(), "too short");
        assert!(
            wire(r#"{"id": "zz2233445566778899aabbccddeeff00"}"#).is_err(),
            "not hex"
        );
        // 32 valid hex digits, but version nibble 0x4
        assert!(
            wire(r#"{"id": "00112233445566778899aabbccddeeff"}"#).is_err(),
            "version and variant bits must be checked"
        );
    }

    /// The name half of federation, mirroring the topology round trip: a peer's exported
    /// bindings must survive the wire and land in the importer's registry.
    #[tokio::test]
    async fn test_export_import_names_round_trip() {
        let exporter = make_service().await;
        let truck = demo("truck_A");
        exporter
            .state
            .ledger
            .write()
            .unwrap()
            .register_name(truck, "demo", "truck_A")
            .unwrap();

        let exported = exporter
            .do_action(Request::new(Action {
                r#type: "export_names".to_string(),
                body: Default::default(),
            }))
            .await
            .expect("export_names should succeed");
        let bytes = action_body(exported).await;

        let importer = make_service().await;
        // Before importing, the id has no name and renders as hex.
        assert_eq!(
            importer.state.ledger.read().unwrap().names().display(truck),
            truck.to_hyphenated()
        );

        let imported = importer
            .do_action(Request::new(Action {
                r#type: "import_names".to_string(),
                body: bytes.into(),
            }))
            .await
            .expect("import_names should succeed");
        let msg = String::from_utf8(action_body(imported).await).unwrap();
        assert!(msg.starts_with("merged "), "{msg}");

        assert_eq!(
            importer.state.ledger.read().unwrap().names().display(truck),
            "truck_A"
        );
    }

    /// An astronomical root resolves and displays straight from its embedded pair,
    /// with an empty registry Aliased frames collapse to their mapped name
    /// (`(399,399)` → `Earth`, `(0,1)` → `ICRF`).
    #[tokio::test]
    async fn test_astronomical_frames_display_from_the_pair_with_no_registry() {
        let service = make_service().await;
        let ledger = service.state.ledger.read().unwrap();
        assert!(ledger.names().is_empty(), "no astro names are registered");
        for (name, canonical) in [("IAU_EARTH", "Earth"), ("J2000", "ICRF"), ("Mars", "Mars")] {
            let id = PrescribedId::astronomical_from_name(name).unwrap();
            assert_eq!(ledger.names().display(id), canonical);
        }
    }
}
