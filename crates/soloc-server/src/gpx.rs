//! Bounded GPX batch submission through the existing Flight `do_put` contract.
//!
//! Pass [`submit_batch`] as the sink of
//! [`spacetimestamp::gps::gpx::import_gpx_async`]. Clone a configured client into
//! each callback future; the importer awaits it before preparing the next batch.
//! One request contains one unsplit record batch, so each successful response
//! confirms that batch. Encoding and size errors occur before submission and are
//! rejected; every RPC error has an unknown outcome because the protocol provides
//! no commit receipt. No request is retried automatically.
//!
//! The supplied limit must match or be smaller than the server's
//! `server.max_message_size`. Every encoded protobuf message, including the schema,
//! is checked. A limit too small for a batch rejects that batch; reduce the import
//! row limit explicitly. A successful submission means append acknowledgement,
//! not durable persistence or a guarantee that retention preserves every row.

use arrow::record_batch::RecordBatch;
use arrow_flight::{
    encode::FlightDataEncoderBuilder, error::FlightError,
    flight_service_client::FlightServiceClient, FlightData, FlightDescriptor,
};
use futures::TryStreamExt;
use prost::Message;
use spacetimestamp::gps::gpx::GpxSubmitError;
use tonic::transport::Channel;

/// Submit exactly one batch and drain the server's complete response before
/// confirming it. The client is cheap to clone and shares its existing channel.
///
/// All transport/server errors are conservatively classified as unknown, even
/// when a particular status might suggest rejection. Callers must reconcile an
/// uncertain batch before manually retrying; this append-only path is not
/// idempotent. Earlier acknowledged batches remain accepted after a later error.
pub async fn submit_batch(
    client: FlightServiceClient<Channel>,
    batch: RecordBatch,
    max_message_size: usize,
) -> Result<(), GpxSubmitError> {
    let messages = encode_batch(batch, max_message_size).await?;
    let mut client = client
        .max_encoding_message_size(max_message_size)
        .max_decoding_message_size(max_message_size);
    let mut response = client
        .do_put(futures::stream::iter(messages))
        .await
        .map_err(|error| GpxSubmitError::unknown(format!("Flight do_put failed: {error}")))?
        .into_inner();
    // This server returns an empty stream on success. Still consume trailers:
    // receiving response headers alone does not prove a completed append.
    while response
        .message()
        .await
        .map_err(|error| GpxSubmitError::unknown(format!("Flight response failed: {error}")))?
        .is_some()
    {}
    Ok(())
}

async fn encode_batch(
    batch: RecordBatch,
    max_message_size: usize,
) -> Result<Vec<FlightData>, GpxSubmitError> {
    if max_message_size == 0 {
        return Err(GpxSubmitError::rejected(
            "Flight max_message_size must be greater than zero",
        ));
    }
    let messages: Vec<_> = FlightDataEncoderBuilder::new()
        // The encoder's ordinary size limit is approximate and may split one
        // input batch into multiple server appends. Keep this request atomic at
        // the existing Ledger::append boundary, then check exact message sizes.
        .with_max_flight_data_size(usize::MAX)
        .with_flight_descriptor(Some(FlightDescriptor::new_path(vec!["entities".into()])))
        .build(futures::stream::iter([Ok::<_, FlightError>(batch)]))
        .try_collect()
        .await
        .map_err(|error| GpxSubmitError::rejected(format!("Flight encoding failed: {error}")))?;
    for (index, message) in messages.iter().enumerate() {
        let bytes = message.encoded_len();
        if bytes > max_message_size {
            return Err(GpxSubmitError::rejected(format!(
                "Flight message {} is {bytes} bytes, exceeding max_message_size \
                 {max_message_size}; reduce the import max_rows or configure a larger \
                 limit on both client and server",
                index + 1
            )));
        }
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_flight::decode::FlightRecordBatchStream;
    use spacetimestamp::gps::gpx::GpxSubmitOutcome;
    use spacetimestamp::schemas::entity::EntityBuilder;
    use spacetimestamp::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode};

    fn entity_batch(rows: usize) -> RecordBatch {
        let mut builder = EntityBuilder::new(rows);
        for index in 0..rows {
            builder.append_entity(
                PrescribedId::new("flight-test", "vessel").unwrap(),
                PrescribedId::astronomical_from_name("ITRF93").unwrap(),
                LengthUnit::m,
                TimeScaleCode::TAI,
                PrescribedId::abstract_source("flight-test", "gps").unwrap(),
                EstimateType::ESTIMATED,
                [6_378_137.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                index as u64,
                None,
                None,
                None,
                None,
                None,
                None,
            );
        }
        builder.flush()
    }

    #[tokio::test]
    async fn exact_message_cap_includes_schema_and_protobuf_overhead() {
        let batch = entity_batch(1);
        let messages = encode_batch(batch.clone(), usize::MAX).await.unwrap();
        let cap = messages.iter().map(Message::encoded_len).max().unwrap();
        assert!(messages.iter().all(|m| m.data_body.len() < cap));
        assert!(encode_batch(batch.clone(), cap).await.is_ok());
        let error = encode_batch(batch, cap - 1).await.unwrap_err();
        assert_eq!(error.outcome, GpxSubmitOutcome::Rejected);
        assert!(error.message.contains("exceeding max_message_size"));
    }

    #[tokio::test]
    async fn batches_larger_than_encoder_default_are_not_split() {
        let batch = entity_batch(20_000);
        let messages = encode_batch(batch.clone(), 64 * 1024 * 1024).await.unwrap();
        assert!(messages.iter().any(|m| m.data_body.len() > 2 * 1024 * 1024));
        let decoded: Vec<_> = FlightRecordBatchStream::new_from_flight_data(futures::stream::iter(
            messages.into_iter().map(Ok),
        ))
        .try_collect()
        .await
        .unwrap();
        assert_eq!(decoded, vec![batch]);
    }

    #[tokio::test]
    async fn zero_limit_is_rejected_before_rpc() {
        let error = encode_batch(entity_batch(1), 0).await.unwrap_err();
        assert_eq!(error.outcome, GpxSubmitOutcome::Rejected);
    }
}
