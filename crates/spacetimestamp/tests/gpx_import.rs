#![cfg(feature = "gpx")]

//! Public streaming-import tests: accounting, bounded submissions and provenance.

use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::task::{Context, Poll, Waker};

use anise::prelude::{Almanac, Epoch};
use arrow::array::FixedSizeBinaryArray;
use arrow::record_batch::RecordBatch;
use spacetimestamp::gps::gpx::{
    GpxContext, GpxEvent, GpxHeightPolicy, GpxImportEvent, GpxImportFailure, GpxImportOptions,
    GpxImportReport, GpxMapper, GpxMapping, GpxObjectId, GpxPoint, GpxPointErrorPolicy, GpxReader,
    GpxSubmitError, GpxTimeOrigin, GpxTimePolicy, WaypointPolicy, import_gpx, import_gpx_async,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::identity::id_at;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::validation::validate_spacetimestamp_batch;
use spacetimestamp::{EstimateType, PrescribedId};

const SOURCE: &str = "streamed-route.gpx";
const MULTIPLE_OBJECTS: &str = include_str!("fixtures/gpx/multiple_objects.gpx");

fn mapping(entity: &str) -> GpxMapping<'static> {
    GpxMapping {
        gps: GpsOptions::new(
            PrescribedId::new("gpx-import-test", entity).unwrap(),
            PrescribedId::abstract_source("gpx-import-test", "receiver-log").unwrap(),
        ),
        height: GpxHeightPolicy::EllipsoidalElevation,
        time: GpxTimePolicy::RequireTimestamp,
        orientation: GpsOrientation::LocalEnu,
    }
}

fn mapper() -> GpxMapper<'static> {
    GpxMapper::new(SOURCE, [(GpxObjectId::Track(1), mapping("vehicle"))]).unwrap()
}

fn document(contents: &str) -> String {
    format!(
        r#"<gpx version="1.1" creator="soloc-test" xmlns="http://www.topografix.com/GPX/1/1">{contents}</gpx>"#
    )
}

fn point(index: usize) -> String {
    format!(
        r#"<trkpt lat="0" lon="{index}"><ele>12</ele><time>2025-01-01T00:00:{index:02}Z</time></trkpt>"#
    )
}

fn track(points: &str) -> String {
    document(&format!("<trk><trkseg>{points}</trkseg></trk>"))
}

fn options(policy: GpxPointErrorPolicy) -> GpxImportOptions {
    GpxImportOptions::new(2, policy).unwrap()
}

fn assert_accounting(report: &GpxImportReport) {
    assert_eq!(
        report.read,
        report.converted + report.skipped + report.failed,
        "every complete parsed point has exactly one conversion outcome"
    );
    assert_eq!(
        report.converted,
        report.submitted + report.rejected + report.uncertain + report.unsubmitted,
        "every converted point has exactly one submission outcome"
    );
    assert_eq!(report.source, SOURCE);
}

fn context(point: usize) -> GpxContext {
    GpxContext {
        source: SOURCE.into(),
        object: Some(GpxObjectId::Track(1)),
        segment: Some(1),
        point: Some(point),
    }
}

#[test]
fn batch_limits_reject_zero_and_excessive_preallocation() {
    assert!(GpxImportOptions::new(0, GpxPointErrorPolicy::Strict).is_err());
    assert!(GpxImportOptions::new(1, GpxPointErrorPolicy::Strict).is_ok());
    assert!(GpxImportOptions::new(65_536, GpxPointErrorPolicy::Strict).is_ok());
    assert!(GpxImportOptions::new(65_537, GpxPointErrorPolicy::Strict).is_err());
    assert!(GpxImportOptions::new(usize::MAX, GpxPointErrorPolicy::Strict).is_err());
}

#[test]
fn empty_exact_and_partial_final_batches_have_consistent_counts() {
    for count in 0..=5 {
        let xml = track(&(0..count).map(point).collect::<String>());
        let mut batch_sizes = Vec::new();
        let mut notifications = Vec::new();
        let report = import_gpx(
            GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
            &mapper(),
            &Almanac::default(),
            options(GpxPointErrorPolicy::Strict),
            |batch| {
                validate_spacetimestamp_batch(&batch).unwrap();
                batch_sizes.push(batch.num_rows());
                Ok(())
            },
            |event| {
                if let GpxImportEvent::Submitted(info) = event {
                    notifications.push(info.clone());
                }
            },
        );
        assert_accounting(&report);
        assert!(report.complete, "{report:?}");
        assert!(report.failure.is_none());
        assert!(report.pending.is_none());
        assert_eq!(report.read, count);
        assert_eq!(report.converted, count);
        assert_eq!(report.submitted, count);
        assert_eq!(report.submitted_batches, count.div_ceil(2));
        assert_eq!(report.unsubmitted, 0);
        let expected: Vec<_> = (0..count).step_by(2).map(|n| (count - n).min(2)).collect();
        assert_eq!(batch_sizes, expected);
        for (index, info) in notifications.iter().enumerate() {
            assert_eq!(info.index, index + 1);
            assert_eq!(info.rows, expected[index]);
            assert_eq!(info.first, context(index * 2 + 1));
            assert_eq!(info.last, context(index * 2 + info.rows));
        }
        assert_eq!(report.last_submitted, notifications.last().cloned());
    }
}

#[test]
fn batching_keeps_explicit_ids_duplicates_and_file_order_across_objects() {
    let mappings = [mapping("route"), mapping("vehicle-a"), mapping("vehicle-b")];
    let mapper = GpxMapper::new(
        SOURCE,
        [
            (GpxObjectId::Route(1), mappings[0]),
            (GpxObjectId::Track(1), mappings[1]),
            (GpxObjectId::Track(2), mappings[2]),
        ],
    )
    .unwrap();
    let mut batches = Vec::new();
    let mut originals = Vec::new();
    let report = import_gpx(
        GpxReader::new(MULTIPLE_OBJECTS.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper,
        &Almanac::default(),
        options(GpxPointErrorPolicy::Strict),
        |batch| {
            batches.push(batch);
            Ok(())
        },
        |event| {
            if let GpxImportEvent::Mapped { point, .. } = event {
                originals.push(point.clone());
            }
        },
    );
    assert_accounting(&report);
    assert!(report.complete);
    assert_eq!(report.submitted, 6);
    assert_eq!(
        batches
            .iter()
            .map(RecordBatch::num_rows)
            .collect::<Vec<_>>(),
        [2, 2, 2]
    );
    let expected_entities = [
        mappings[0].gps.entity_id,
        mappings[1].gps.entity_id,
        mappings[1].gps.entity_id,
        mappings[1].gps.entity_id,
        mappings[1].gps.entity_id,
        mappings[2].gps.entity_id,
    ];
    for (batch_index, batch) in batches.iter().enumerate() {
        let ids = batch
            .column_by_name("entity_id")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        let sts = StsColumns::try_new(batch).unwrap();
        for row in 0..batch.num_rows() {
            let index = 2 * batch_index + row;
            assert_eq!(id_at(ids, row).unwrap(), expected_entities[index]);
            assert_eq!(sts.source_at(row).unwrap(), mappings[0].gps.source_id);
            assert_eq!(
                sts.position_at(row),
                originals[index].converted.position_m()
            );
            assert_eq!(
                sts.epoch_parts_at(row),
                originals[index].converted.epoch_parts()
            );
        }
    }
    assert_eq!(originals[1].converted, originals[2].converted);
    assert!(originals[3].observation.epoch < originals[2].observation.epoch);
    assert_eq!(originals[4].point.context.segment, Some(3));
    assert_eq!(originals[4].point.context.point, Some(1));
}

#[test]
fn strict_mapping_failure_preserves_accepted_prefix_and_reports_pending_rows() {
    let xml = track(&format!(
        "{}{}{}<trkpt lat=\"0\" lon=\"3\"><ele>12</ele></trkpt>{}",
        point(0),
        point(1),
        point(2),
        point(4)
    ));
    let mut sizes = Vec::new();
    let mut failures = 0;
    let report = import_gpx(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        options(GpxPointErrorPolicy::Strict),
        |batch| {
            sizes.push(batch.num_rows());
            Ok(())
        },
        |event| {
            if matches!(event, GpxImportEvent::Failed(_)) {
                failures += 1;
            }
        },
    );
    assert_accounting(&report);
    assert!(!report.complete);
    assert_eq!(sizes, [2]);
    assert_eq!(report.read, 4);
    assert_eq!(report.converted, 3);
    assert_eq!(report.submitted, 2);
    assert_eq!(report.failed, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.unsubmitted, 1);
    assert_eq!(failures, 1);
    let pending = report.pending.unwrap();
    assert_eq!(pending.index, 2);
    assert_eq!(pending.rows, 1);
    assert_eq!(pending.first, context(3));
    assert_eq!(pending.last, context(3));
    match report.failure.unwrap() {
        GpxImportFailure::Mapping(error) => assert_eq!(error.context, context(4)),
        other => panic!("expected mapping failure, got {other:?}"),
    }
    assert_eq!(report.last_submitted.unwrap().last, context(2));
}

#[test]
fn skip_invalid_records_mapping_failure_then_continues_without_filling_a_row() {
    let xml = track(&format!(
        "{}<trkpt lat=\"0\" lon=\"1\"><ele>12</ele></trkpt>{}{}{}",
        point(0),
        point(2),
        point(3),
        point(4)
    ));
    let mut sizes = Vec::new();
    let mut skipped = Vec::new();
    let mut submitted = Vec::new();
    let report = import_gpx(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        options(GpxPointErrorPolicy::SkipInvalid),
        |batch| {
            sizes.push(batch.num_rows());
            Ok(())
        },
        |event| match event {
            GpxImportEvent::Skipped(error) => skipped.push(error.clone()),
            GpxImportEvent::Submitted(info) => submitted.push(info.clone()),
            _ => {}
        },
    );
    assert_accounting(&report);
    assert!(report.complete);
    assert_eq!(report.read, 5);
    assert_eq!(report.converted, 4);
    assert_eq!(report.submitted, 4);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(sizes, [2, 2]);
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].context, context(2));
    assert_eq!(report.last_skipped.as_ref(), skipped.last());
    assert!(report.failure.is_none());
    assert_eq!(submitted[0].first, context(1));
    assert_eq!(submitted[0].last, context(3));
    assert_eq!(submitted[1].first, context(4));
    assert_eq!(submitted[1].last, context(5));
}

#[test]
fn parser_failure_is_fatal_even_when_mapping_failures_may_be_skipped() {
    let xml = track(&format!(
        "{}{}{}<trkpt lat=\"not-a-number\" lon=\"3\"/>{}",
        point(0),
        point(1),
        point(2),
        point(4)
    ));
    let mut calls = 0;
    let report = import_gpx(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        options(GpxPointErrorPolicy::SkipInvalid),
        |_| {
            calls += 1;
            Ok(())
        },
        |_| {},
    );
    assert_accounting(&report);
    assert!(!report.complete);
    assert_eq!(calls, 1);
    assert_eq!(
        report.read, 3,
        "incomplete malformed point is not counted as parsed"
    );
    assert_eq!(report.converted, 3);
    assert_eq!(report.submitted, 2);
    assert_eq!(report.unsubmitted, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);
    assert!(report.last_skipped.is_none());
    assert_eq!(report.pending.unwrap().first, context(3));
    match report.failure.unwrap() {
        GpxImportFailure::Read(error) => assert_eq!(error.context, context(4)),
        other => panic!("expected parser failure, got {other:?}"),
    }
}

#[test]
fn final_short_batch_waits_for_validated_eof() {
    let xml = format!(
        "{}<extra/>",
        track(&format!("{}{}{}", point(0), point(1), point(2)))
    );
    let mut sizes = Vec::new();
    let mut saw_document_end = false;
    let report = import_gpx(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        options(GpxPointErrorPolicy::Strict),
        |batch| {
            sizes.push(batch.num_rows());
            Ok(())
        },
        |event| {
            saw_document_end |= matches!(event, GpxImportEvent::Input(GpxEvent::DocumentEnd));
        },
    );
    assert_accounting(&report);
    assert!(!report.complete);
    assert!(!saw_document_end);
    assert_eq!(sizes, [2]);
    assert_eq!(report.read, 3);
    assert_eq!(report.submitted, 2);
    assert_eq!(report.unsubmitted, 1);
    assert!(matches!(report.failure, Some(GpxImportFailure::Read(_))));
}

#[test]
fn rejected_or_uncertain_submission_stops_without_reading_later_points_or_retrying() {
    for unknown in [false, true] {
        let xml = track(&(0..6).map(point).collect::<String>());
        let calls = Cell::new(0);
        let parsed = Cell::new(0);
        let mut submitted_events = 0;
        let mut failed_events = 0;
        let report = import_gpx(
            GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
            &mapper(),
            &Almanac::default(),
            options(GpxPointErrorPolicy::Strict),
            |batch| {
                assert_eq!(batch.num_rows(), 2);
                calls.set(calls.get() + 1);
                assert_eq!(
                    parsed.get(),
                    calls.get() * 2,
                    "no later point was parsed ahead"
                );
                if calls.get() == 1 {
                    Ok(())
                } else if unknown {
                    Err(GpxSubmitError::unknown(
                        "connection closed before acknowledgment",
                    ))
                } else {
                    Err(GpxSubmitError::rejected("batch validation failed"))
                }
            },
            |event| match event {
                GpxImportEvent::Input(GpxEvent::Point(_)) => parsed.set(parsed.get() + 1),
                GpxImportEvent::Submitted(_) => submitted_events += 1,
                GpxImportEvent::Failed(_) => failed_events += 1,
                _ => {}
            },
        );
        assert_accounting(&report);
        assert!(!report.complete);
        assert_eq!(
            calls.get(),
            2,
            "failed submissions must not be retried implicitly"
        );
        assert_eq!(parsed.get(), 4);
        assert_eq!(report.read, 4);
        assert_eq!(report.converted, 4);
        assert_eq!(report.submitted, 2);
        assert_eq!(report.submitted_batches, 1);
        assert_eq!(report.rejected, if unknown { 0 } else { 2 });
        assert_eq!(report.uncertain, if unknown { 2 } else { 0 });
        assert_eq!(report.unsubmitted, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(submitted_events, 1);
        assert_eq!(failed_events, 1);
        assert_eq!(report.last_submitted.unwrap().last, context(2));
        match report.failure.unwrap() {
            GpxImportFailure::Submit { batch, .. } => {
                assert_eq!(batch.index, 2);
                assert_eq!(batch.rows, 2);
                assert_eq!(batch.first, context(3));
                assert_eq!(batch.last, context(4));
            }
            other => panic!("expected submission failure, got {other:?}"),
        }
    }
}

#[test]
fn observer_exposes_empty_segments_ignored_metadata_and_explicit_assumptions() {
    let xml = document(
        r#"<metadata><name>Recorded source</name></metadata><trk><name>Boat</name>
        <trkseg/><trkseg><trkpt lat="0" lon="0"><hdop>2</hdop></trkpt></trkseg>
        <extensions><extra xmlns="urn:example">ignored payload</extra></extensions></trk>"#,
    );
    let schedule = |_: &GpxPoint| Ok(Epoch::from_gregorian_utc_at_midnight(2025, 1, 1));
    let mut settings = mapping("vehicle");
    settings.gps.estimate_type = EstimateType::SIMULATED;
    settings.height = GpxHeightPolicy::FixedEllipsoidalHeight(7.0);
    settings.time = GpxTimePolicy::FillMissing(&schedule);
    let mapper = GpxMapper::new(SOURCE, [(GpxObjectId::Track(1), settings)]).unwrap();
    let mut input = Vec::new();
    let mut mapped_count = 0;
    let report = import_gpx(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper,
        &Almanac::default(),
        options(GpxPointErrorPolicy::Strict),
        |_| Ok(()),
        |event| match event {
            GpxImportEvent::Input(event) => input.push(event.clone()),
            GpxImportEvent::Mapped { point, mapping } => {
                mapped_count += 1;
                assert_eq!(point.time_origin, GpxTimeOrigin::Scheduled);
                assert_eq!(
                    point.height_policy,
                    GpxHeightPolicy::FixedEllipsoidalHeight(7.0)
                );
                assert_eq!(point.observation.ellipsoidal_height_m, 7.0);
                assert_eq!(point.point.context.segment, Some(2));
                assert_eq!(
                    point.point.metadata.get("hdop").map(String::as_str),
                    Some("2")
                );
                assert_eq!(mapping.gps.entity_id, settings.gps.entity_id);
                assert_eq!(mapping.gps.source_id, settings.gps.source_id);
                assert_eq!(mapping.gps.target_frame, "ITRF93");
                assert_eq!(mapping.gps.estimate_type, EstimateType::SIMULATED);
                assert_eq!(mapping.height, settings.height);
                assert_eq!(mapping.orientation, GpsOrientation::LocalEnu);
                assert!(matches!(mapping.time, GpxTimePolicy::FillMissing(_)));
            }
            _ => {}
        },
    );
    assert_accounting(&report);
    assert!(report.complete);
    assert_eq!(mapped_count, 1);
    assert!(input.windows(2).any(|events| matches!(events,
        [GpxEvent::SegmentStart { context: first }, GpxEvent::SegmentEnd { context: last }]
            if first == last && first.segment == Some(1)
    )));
    assert!(input.iter().any(
        |event| matches!(event, GpxEvent::Metadata { field, value, .. }
        if field == "name" && value == "Boat")
    ));
    assert!(
        input
            .iter()
            .any(|event| matches!(event, GpxEvent::Ignored { .. }))
    );
    assert!(matches!(input.last(), Some(GpxEvent::DocumentEnd)));
}

#[test]
fn explicit_reimport_keeps_duplicate_rows_and_stable_identity() {
    let xml = track(&(0..3).map(point).collect::<String>());
    let mapper = mapper();
    let mut appended = Vec::new();
    for _ in 0..2 {
        let report = import_gpx(
            GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
            &mapper,
            &Almanac::default(),
            options(GpxPointErrorPolicy::Strict),
            |batch| {
                appended.push(batch);
                Ok(())
            },
            |_| {},
        );
        assert_accounting(&report);
        assert!(report.complete);
        assert_eq!(report.submitted, 3);
    }
    assert_eq!(appended.len(), 4);
    assert_eq!(appended.iter().map(RecordBatch::num_rows).sum::<usize>(), 6);
    assert_eq!(appended[0], appended[2]);
    assert_eq!(appended[1], appended[3]);
}

#[test]
fn mismatched_source_fails_before_reading_even_an_empty_document() {
    for xml in [document(""), track(&point(0))] {
        let mapper = GpxMapper::new(
            "different-file.gpx",
            [(GpxObjectId::Track(1), mapping("vehicle"))],
        )
        .unwrap();
        let mut input_events = 0;
        let mut failure_events = 0;
        let report = import_gpx(
            GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
            &mapper,
            &Almanac::default(),
            options(GpxPointErrorPolicy::SkipInvalid),
            |_| panic!("a mismatched source must not reach the sink"),
            |event| match event {
                GpxImportEvent::Input(_) => input_events += 1,
                GpxImportEvent::Failed(_) => failure_events += 1,
                _ => {}
            },
        );
        assert_accounting(&report);
        assert!(!report.complete);
        assert_eq!(report.read, 0);
        assert_eq!(report.failed, 0, "a source mismatch is not a point failure");
        assert_eq!(report.skipped, 0);
        assert_eq!(input_events, 0);
        assert_eq!(failure_events, 1);
        assert!(report.pending.is_none());
        match report.failure.unwrap() {
            GpxImportFailure::Mapping(error) => {
                assert_eq!(error.context.source, SOURCE);
                assert_eq!(error.context.object, None);
                assert_eq!(error.context.point, None);
            }
            other => panic!("expected source mapping failure, got {other:?}"),
        }
    }
}

#[test]
fn consumed_reader_cannot_report_a_partial_file_as_a_complete_import() {
    let xml = track(&format!("{}{}", point(0), point(1)));
    for exhausted in [false, true] {
        let mut reader = GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject);
        if exhausted {
            for event in reader.by_ref() {
                event.unwrap();
            }
        } else {
            assert!(matches!(
                reader.next().unwrap().unwrap(),
                GpxEvent::DocumentStart { .. }
            ));
        }
        let mut input_events = 0;
        let report = import_gpx(
            reader,
            &mapper(),
            &Almanac::default(),
            options(GpxPointErrorPolicy::Strict),
            |_| panic!("a previously consumed reader must not submit rows"),
            |event| {
                if matches!(event, GpxImportEvent::Input(_)) {
                    input_events += 1;
                }
            },
        );
        assert_accounting(&report);
        assert!(!report.complete);
        assert_eq!(report.read, 0);
        assert_eq!(report.submitted, 0);
        assert_eq!(input_events, 0);
        assert!(report.pending.is_none());
        match report.failure.unwrap() {
            GpxImportFailure::Read(error) => {
                assert_eq!(error.context.source, SOURCE);
                assert!(error.to_string().contains("fresh"));
            }
            other => panic!("expected reader reuse failure, got {other:?}"),
        }
    }
}

#[test]
fn async_submission_waits_for_acknowledgment_before_reading_the_next_batch() {
    let xml = track(&(0..4).map(point).collect::<String>());
    let mapper = mapper();
    let almanac = Almanac::default();
    let ready = Cell::new(false);
    let calls = Cell::new(0);
    let parsed = Cell::new(0);
    let acknowledged = Cell::new(0);
    let mut task = std::pin::pin!(import_gpx_async(
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper,
        &almanac,
        options(GpxPointErrorPolicy::Strict),
        |batch| {
            assert_eq!(batch.num_rows(), 2);
            calls.set(calls.get() + 1);
            let ready = &ready;
            poll_fn(move |_| {
                if ready.replace(false) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
        },
        |event| match event {
            GpxImportEvent::Input(GpxEvent::Point(_)) => parsed.set(parsed.get() + 1),
            GpxImportEvent::Submitted(_) => acknowledged.set(acknowledged.get() + 1),
            _ => {}
        },
    ));
    // Manually poll a deliberately gated sink; no timer, runtime or network is needed.
    let mut cx = Context::from_waker(Waker::noop());
    assert!(task.as_mut().poll(&mut cx).is_pending());
    assert_eq!((calls.get(), parsed.get(), acknowledged.get()), (1, 2, 0));
    assert!(task.as_mut().poll(&mut cx).is_pending());
    assert_eq!((calls.get(), parsed.get(), acknowledged.get()), (1, 2, 0));

    ready.set(true);
    assert!(task.as_mut().poll(&mut cx).is_pending());
    assert_eq!((calls.get(), parsed.get(), acknowledged.get()), (2, 4, 1));

    ready.set(true);
    let Poll::Ready(report) = task.as_mut().poll(&mut cx) else {
        panic!("second acknowledgment should complete the import");
    };
    assert_accounting(&report);
    assert!(report.complete);
    assert_eq!(report.submitted, 4);
    assert_eq!(report.submitted_batches, 2);
    assert_eq!(acknowledged.get(), 2);
}
