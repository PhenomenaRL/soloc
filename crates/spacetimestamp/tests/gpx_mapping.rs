#![cfg(feature = "gpx")]

//! Public GPX reader → mapping → standard EntitySchema regression tests.
//! No kernels, network, ledger, or parser-private helpers are needed.

use std::cell::Cell;

use anise::prelude::{Almanac, Epoch};
use arrow::array::{Array, FixedSizeBinaryArray, StructArray};
use hifitime::Duration;
use spacetimestamp::gps::gpx::{
    GpxContext, GpxError, GpxErrorKind, GpxEvent, GpxHeightPolicy, GpxMapper, GpxMapping,
    GpxObjectId, GpxPoint, GpxReader, GpxTimeOrigin, GpxTimePolicy, WaypointPolicy, parse_gpx_time,
};
use spacetimestamp::gps::{GpsError, GpsOptions, GpsOrientation};
use spacetimestamp::identity::id_at;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::{EntityBuilder, entity_schema};
use spacetimestamp::validation::validate_spacetimestamp_batch;
use spacetimestamp::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode, epoch_to_parts};

const MULTIPLE_OBJECTS: &str = include_str!("fixtures/gpx/multiple_objects.gpx");
const UNTIMED_ROUTE: &str = include_str!("fixtures/gpx/untimed_route.gpx");
const GEOID_HEIGHT: &str = include_str!("fixtures/gpx/geoid_height.gpx");

fn mapping(entity: &str) -> GpxMapping<'static> {
    GpxMapping {
        gps: GpsOptions::new(
            PrescribedId::new("gpx-test", entity).unwrap(),
            PrescribedId::abstract_source("gpx-test", "receiver-log").unwrap(),
        ),
        height: GpxHeightPolicy::EllipsoidalElevation,
        time: GpxTimePolicy::RequireTimestamp,
        orientation: GpsOrientation::LocalEnu,
    }
}

fn events(xml: &str, source: &str) -> Vec<GpxEvent> {
    GpxReader::new(xml.as_bytes(), source, WaypointPolicy::Reject)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn points(xml: &str, source: &str) -> Vec<GpxPoint> {
    events(xml, source)
        .into_iter()
        .filter_map(|event| match event {
            GpxEvent::Point(point) => Some(point),
            _ => None,
        })
        .collect()
}

fn document(contents: &str) -> String {
    format!(
        r#"<gpx version="1.1" creator="soloc-test" xmlns="http://www.topografix.com/GPX/1/1">{contents}</gpx>"#
    )
}

fn track_point(fields: &str) -> String {
    document(&format!(
        r#"<trk><trkseg><trkpt lat="0" lon="0">{fields}</trkpt></trkseg></trk>"#
    ))
}

fn assert_point_context(
    error: &GpxError,
    source: &str,
    object: GpxObjectId,
    segment: Option<usize>,
    point: usize,
) {
    assert_eq!(
        error.context,
        GpxContext {
            source: source.into(),
            object: Some(object),
            segment,
            point: Some(point),
        }
    );
    let display = error.to_string();
    assert!(display.contains(source), "{display}");
    assert!(display.contains(&object.to_string()), "{display}");
    assert!(display.contains(&format!("point {point}")), "{display}");
    if let Some(segment) = segment {
        assert!(display.contains(&format!("segment {segment}")), "{display}");
    }
}

#[test]
fn tracks_routes_segments_and_duplicate_points_keep_their_original_order_and_identity() {
    let source = "multiple_objects.gpx";
    let events = events(MULTIPLE_OBJECTS, source);
    let object_order: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            GpxEvent::ObjectStart { context } => context.object,
            _ => None,
        })
        .collect();
    assert_eq!(
        object_order,
        [
            GpxObjectId::Route(1),
            GpxObjectId::Track(1),
            GpxObjectId::Track(2)
        ]
    );
    let segment_order: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            GpxEvent::SegmentStart { context } => {
                Some((context.object.unwrap(), context.segment.unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        segment_order,
        [
            (GpxObjectId::Track(1), 1),
            (GpxObjectId::Track(1), 2), // An empty segment still occupies its original ordinal.
            (GpxObjectId::Track(1), 3),
            (GpxObjectId::Track(2), 1),
        ]
    );
    assert!(events.windows(2).any(|pair| matches!(pair,
        [GpxEvent::SegmentStart { context: start }, GpxEvent::SegmentEnd { context: end }]
            if start == end && start.object == Some(GpxObjectId::Track(1)) && start.segment == Some(2)
    )));

    let settings = [
        mapping("planned-object"),
        mapping("vehicle-a"),
        mapping("vehicle-b"),
    ];
    let mapper = GpxMapper::new(
        source,
        [
            (GpxObjectId::Route(1), settings[0]),
            (GpxObjectId::Track(1), settings[1]),
            (GpxObjectId::Track(2), settings[2]),
        ],
    )
    .unwrap();
    let mut builder = EntityBuilder::new(6);
    let mut mapped = Vec::new();
    for event in events {
        if let GpxEvent::Point(point) = event {
            let result = mapper.convert(&point, &Almanac::default()).unwrap();
            assert_eq!(
                result.point, point,
                "original fields and context remain available"
            );
            result.converted.append_to(&mut builder);
            mapped.push(result);
        }
    }
    assert_eq!(mapped.len(), 6);
    assert_eq!(
        mapped[1].observation, mapped[2].observation,
        "duplicates are retained"
    );
    assert!(
        mapped[3].observation.epoch < mapped[2].observation.epoch,
        "timestamps do not reorder the file"
    );
    assert_eq!(mapped[3].point.context.point, Some(3));
    assert_eq!(mapped[4].point.context.segment, Some(3));
    assert_eq!(mapped[4].point.context.point, Some(1));
    assert!(
        mapped
            .iter()
            .all(|point| point.time_origin == GpxTimeOrigin::Recorded)
    );

    let batch = builder.flush();
    assert_eq!(batch.schema(), entity_schema());
    validate_spacetimestamp_batch(&batch).unwrap();
    let ids = batch
        .column_by_name("entity_id")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .unwrap();
    let expected_entities = [
        settings[0].gps.entity_id,
        settings[1].gps.entity_id,
        settings[1].gps.entity_id,
        settings[1].gps.entity_id,
        settings[1].gps.entity_id,
        settings[2].gps.entity_id,
    ];
    let sts = StsColumns::try_new(&batch).unwrap();
    let frame = PrescribedId::astronomical_from_name("ITRF93").unwrap();
    for (row, expected_entity) in expected_entities.into_iter().enumerate() {
        assert_eq!(id_at(ids, row).unwrap(), expected_entity);
        assert_eq!(sts.frame_at(row).unwrap(), frame);
        assert_eq!(sts.source_at(row).unwrap(), settings[0].gps.source_id);
        assert_eq!(sts.units_at(row).unwrap(), LengthUnit::m);
        assert_eq!(sts.timescale_at(row).unwrap(), TimeScaleCode::TAI);
        assert_eq!(sts.estimate_at(row).unwrap(), EstimateType::ESTIMATED);
        assert_eq!(
            sts.epoch_parts_at(row),
            epoch_to_parts(mapped[row].observation.epoch)
        );
        assert!((sts.quaternion_at(row).iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-14);
    }
    // Independent UTC → TAI golden value, including all nine fractional digits.
    assert_eq!(sts.epoch_parts_at(1), (0, 788_961_639_123_456_789));
    assert_eq!(sts.position_at(1), [6_378_147.0, 0.0, 0.0]);
    for field in [
        "velocity",
        "angular_velocity",
        "acceleration",
        "mass_kg",
        "state_covariance",
        "dimensions",
    ] {
        assert_eq!(
            batch.column_by_name(field).unwrap().null_count(),
            batch.num_rows()
        );
    }
    let nested = batch
        .column_by_name("spacetimestamp")
        .unwrap()
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    for field in ["position_covariance", "orientation_covariance"] {
        assert_eq!(
            nested.column_by_name(field).unwrap().null_count(),
            batch.num_rows()
        );
    }
}

#[test]
fn matching_display_names_never_supply_implicit_object_mappings() {
    let source = "multiple_objects.gpx";
    let points = points(MULTIPLE_OBJECTS, source);
    let mapper =
        GpxMapper::new(source, [(GpxObjectId::Route(1), mapping("explicit-route"))]).unwrap();
    assert!(mapper.convert(&points[0], &Almanac::default()).is_ok());
    let error = mapper.convert(&points[1], &Almanac::default()).unwrap_err();
    assert_eq!(error.kind, GpxErrorKind::MissingMapping);
    assert_point_context(&error, source, GpxObjectId::Track(1), Some(1), 1);

    for objects in [
        vec![(GpxObjectId::Track(0), mapping("invalid"))],
        vec![(GpxObjectId::Route(0), mapping("invalid"))],
        vec![
            (GpxObjectId::Track(1), mapping("first")),
            (GpxObjectId::Track(1), mapping("second")),
        ],
    ] {
        assert!(matches!(
            GpxMapper::new(source, objects),
            Err(GpxError {
                kind: GpxErrorKind::InvalidPolicy(_),
                ..
            })
        ));
    }
}

#[test]
fn untimed_routes_require_an_explicit_schedule_and_simulated_estimate_type() {
    let source = "untimed_route.gpx";
    let points = points(UNTIMED_ROUTE, source);
    let baseline = mapping("planned-route");
    let mapper = GpxMapper::new(source, [(GpxObjectId::Route(1), baseline)]).unwrap();
    let error = mapper.convert(&points[0], &Almanac::default()).unwrap_err();
    assert_eq!(error.kind, GpxErrorKind::MissingField("time"));
    assert_point_context(&error, source, GpxObjectId::Route(1), None, 1);

    let start: Epoch = "2025-01-01T00:00:00 TAI".parse().unwrap();
    let schedule = |point: &GpxPoint| {
        Ok(start + Duration::from_seconds(10.0 * (point.context.point.unwrap() - 1) as f64))
    };
    let mut scheduled = GpxMapping {
        time: GpxTimePolicy::FillMissing(&schedule),
        ..baseline
    };
    let mapper = GpxMapper::new(source, [(GpxObjectId::Route(1), scheduled)]).unwrap();
    assert!(matches!(
        mapper.convert(&points[0], &Almanac::default()),
        Err(GpxError {
            kind: GpxErrorKind::InvalidPolicy(_),
            ..
        })
    ));
    scheduled.gps.estimate_type = EstimateType::SIMULATED;
    let mapper = GpxMapper::new(source, [(GpxObjectId::Route(1), scheduled)]).unwrap();
    for (index, point) in points.iter().enumerate() {
        let mapped = mapper.convert(point, &Almanac::default()).unwrap();
        assert_eq!(mapped.time_origin, GpxTimeOrigin::Scheduled);
        assert_eq!(mapped.point.time, None);
        assert_eq!(
            mapped.observation.epoch,
            start + Duration::from_seconds(index as f64 * 10.0)
        );
        assert_eq!(mapped.converted.estimate_type(), EstimateType::SIMULATED);
    }
}

#[test]
fn a_schedule_only_fills_missing_time_and_never_overrides_invalid_recorded_text() {
    let source = "schedule.gpx";
    let xml = track_point("<ele>0</ele><time>2025-01-01T00:00:00Z</time>");
    let mut point = points(&xml, source).remove(0);
    let calls = Cell::new(0);
    let schedule = |_: &GpxPoint| {
        calls.set(calls.get() + 1);
        Err("no supplied schedule entry".into())
    };
    let mut settings = mapping("vehicle");
    settings.time = GpxTimePolicy::FillMissing(&schedule);
    settings.gps.estimate_type = EstimateType::SIMULATED;
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), settings)]).unwrap();
    let recorded = mapper.convert(&point, &Almanac::default()).unwrap();
    assert_eq!(recorded.time_origin, GpxTimeOrigin::Recorded);
    assert_eq!(calls.get(), 0);
    point.time = Some("2016-12-31T23:59:60Z".into());
    assert!(matches!(
        mapper.convert(&point, &Almanac::default()),
        Err(GpxError {
            kind: GpxErrorKind::InvalidField { field: "time", .. },
            ..
        })
    ));
    assert_eq!(calls.get(), 0);
    point.time = None;
    let error = mapper.convert(&point, &Almanac::default()).unwrap_err();
    assert!(matches!(
        error.kind,
        GpxErrorKind::InvalidField {
            field: "schedule",
            ..
        }
    ));
    assert!(error.to_string().contains("no supplied schedule entry"));
    assert_point_context(&error, source, GpxObjectId::Track(1), Some(1), 1);
    assert_eq!(calls.get(), 1);
}

#[test]
fn elevation_and_geoid_values_change_height_only_under_the_declared_policy() {
    let source = "geoid_height.gpx";
    let points = points(GEOID_HEIGHT, source);
    for (policy, expected_heights) in [
        (GpxHeightPolicy::EllipsoidalElevation, [100.0, 50.0]),
        (GpxHeightPolicy::ElevationPlusGeoidHeight, [125.0, 20.0]),
        (
            GpxHeightPolicy::ElevationWithGeoidOffset(10.0),
            [110.0, 60.0],
        ),
        (GpxHeightPolicy::FixedEllipsoidalHeight(-7.0), [-7.0, -7.0]),
    ] {
        let settings = GpxMapping {
            height: policy,
            ..mapping("vehicle")
        };
        let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), settings)]).unwrap();
        for (index, point) in points.iter().enumerate() {
            let mapped = mapper.convert(point, &Almanac::default()).unwrap();
            assert_eq!(
                mapped.observation.ellipsoidal_height_m,
                expected_heights[index]
            );
            assert_eq!(mapped.height_policy, policy);
            assert_eq!(mapped.point, *point);
            assert!(
                (mapped.converted.position_m()[index] - (6_378_137.0 + expected_heights[index]))
                    .abs()
                    < 1e-8
            );
        }
    }
}

#[test]
fn absent_elevation_or_geoid_is_never_silently_replaced() {
    let source = "incomplete-height.gpx";
    let xml = track_point("<time>2025-01-01T00:00:00Z</time>");
    let mut point = points(&xml, source).remove(0);
    let settings = mapping("vehicle");
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), settings)]).unwrap();
    let error = mapper.convert(&point, &Almanac::default()).unwrap_err();
    assert_eq!(error.kind, GpxErrorKind::MissingField("ele"));
    assert_point_context(&error, source, GpxObjectId::Track(1), Some(1), 1);

    let explicit = GpxMapping {
        height: GpxHeightPolicy::FixedEllipsoidalHeight(-12.0),
        ..settings
    };
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), explicit)]).unwrap();
    assert_eq!(
        mapper
            .convert(&point, &Almanac::default())
            .unwrap()
            .observation
            .ellipsoidal_height_m,
        -12.0
    );
    point.elevation_m = Some(5.0);
    let geoid = GpxMapping {
        height: GpxHeightPolicy::ElevationPlusGeoidHeight,
        ..settings
    };
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), geoid)]).unwrap();
    assert_eq!(
        mapper
            .convert(&point, &Almanac::default())
            .unwrap_err()
            .kind,
        GpxErrorKind::MissingField("geoidheight")
    );
}

#[test]
fn assumed_heights_cannot_claim_a_measured_pose_and_nonfinite_policies_fail_early() {
    for height in [
        GpxHeightPolicy::FixedEllipsoidalHeight(f64::NAN),
        GpxHeightPolicy::FixedEllipsoidalHeight(f64::INFINITY),
        GpxHeightPolicy::ElevationWithGeoidOffset(f64::NEG_INFINITY),
    ] {
        let settings = GpxMapping {
            height,
            ..mapping("vehicle")
        };
        assert!(matches!(
            GpxMapper::new("policy.gpx", [(GpxObjectId::Track(1), settings)]),
            Err(GpxError {
                kind: GpxErrorKind::InvalidPolicy(_),
                ..
            })
        ));
    }
    let mut settings = mapping("vehicle");
    settings.height = GpxHeightPolicy::FixedEllipsoidalHeight(0.0);
    settings.orientation = GpsOrientation::QuaternionInEnu([1.0, 0.0, 0.0, 0.0]);
    settings.gps.estimate_type = EstimateType::MEASURED;
    assert!(matches!(
        GpxMapper::new("policy.gpx", [(GpxObjectId::Track(1), settings)]),
        Err(GpxError {
            kind: GpxErrorKind::InvalidPolicy(_),
            ..
        })
    ));
}

#[test]
fn timezones_preserve_nanoseconds_and_physical_instants_across_a_leap_boundary() {
    let instant = parse_gpx_time("2017-01-01T00:00:00.123456789Z").unwrap();
    for text in [
        "2016-12-31T19:00:00.123456789-05:00",
        "2017-01-01T01:00:00.123456789+01:00",
        "2017-01-01T14:00:00.123456789+14:00",
        "2017-01-01T00:00:00.123456789-00:00",
    ] {
        assert_eq!(parse_gpx_time(text).unwrap(), instant, "{text}");
    }
    let before = parse_gpx_time("2016-12-31T23:59:59.123456789Z").unwrap();
    assert_eq!(
        instant.to_tai_duration() - before.to_tai_duration(),
        Duration::from_seconds(2.0)
    );
    let tai: Epoch = "2017-01-01T00:00:37.123456789 TAI".parse().unwrap();
    assert_eq!(instant.to_tai_duration(), tai.to_tai_duration());
    assert_eq!(epoch_to_parts(instant), (0, 536_500_837_123_456_789));
}

#[test]
fn invalid_or_ambiguous_times_report_the_original_point_context() {
    let source = "invalid-time.gpx";
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), mapping("vehicle"))]).unwrap();
    for time in [
        "2016-12-31T23:59:60Z",
        "2017-01-01T00:59:60+01:00",
        "2025-01-01T00:00:00",
        "2025-01-01T00:00:00.1234567890Z",
        "2025-02-29T00:00:00Z",
        "2025-01-01T24:00:00Z",
        "2025-01-01T00:00:00+14:01",
        "2025-01-01T00:00:00+01:60",
        "0000-01-01T00:00:00Z",
        "2025-01-01t00:00:00z",
        "\u{a0}2025-01-01T00:00:00Z",
        "2025-01-01T00:00:00Z\u{a0}",
        "not-a-time",
    ] {
        let xml = track_point(&format!("<ele>0</ele><time>{time}</time>"));
        let point = points(&xml, source).remove(0);
        let error = mapper.convert(&point, &Almanac::default()).unwrap_err();
        assert!(
            matches!(error.kind, GpxErrorKind::InvalidField { field: "time", .. }),
            "{time}: {error}"
        );
        assert_point_context(&error, source, GpxObjectId::Track(1), Some(1), 1);
    }
}

#[test]
fn failed_conversion_cannot_append_a_partial_point_to_an_existing_builder() {
    let source = "append.gpx";
    let xml = track_point("<ele>0</ele><time>2025-01-01T00:00:00Z</time>");
    let point = points(&xml, source).remove(0);
    let baseline = mapping("vehicle");
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), baseline)]).unwrap();
    let first = mapper.convert(&point, &Almanac::default()).unwrap();
    let mut builder = EntityBuilder::new(2);
    first.converted.append_to(&mut builder);

    let mut rotated = baseline;
    rotated.gps.target_frame = "GCRF";
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), rotated)]).unwrap();
    let result = mapper.convert(&point, &Almanac::default());
    let error = match result {
        Ok(mapped) => {
            mapped.converted.append_to(&mut builder);
            panic!("a GCRF transform cannot succeed without Earth orientation kernels")
        }
        Err(error) => error,
    };
    assert!(matches!(
        error.kind,
        GpxErrorKind::Conversion(ref cause) if matches!(cause.as_ref(), GpsError::Transform { .. })
    ));
    assert_point_context(&error, source, GpxObjectId::Track(1), Some(1), 1);
    assert_eq!(builder.len(), 1);
    let batch = builder.flush();
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(
        StsColumns::try_new(&batch).unwrap().position_at(0),
        first.converted.position_m()
    );
}

#[test]
fn mapper_rejects_points_from_a_different_import_source() {
    let xml = track_point("<ele>0</ele><time>2025-01-01T00:00:00Z</time>");
    let point = points(&xml, "actual.gpx").remove(0);
    let mapper = GpxMapper::new(
        "different.gpx",
        [(GpxObjectId::Track(1), mapping("vehicle"))],
    )
    .unwrap();
    let error = mapper.convert(&point, &Almanac::default()).unwrap_err();
    assert!(matches!(error.kind, GpxErrorKind::InvalidPolicy(_)));
    assert_point_context(&error, "actual.gpx", GpxObjectId::Track(1), Some(1), 1);
}

#[test]
fn names_quality_fields_and_ignored_extensions_remain_explicit_import_context() {
    let source = "metadata.gpx";
    let xml = document(
        r#"<metadata><name>Journey &amp; context</name></metadata>
        <trk><name>Receiver track</name><desc>Two fields of context</desc><trkseg>
          <trkpt lat="0" lon="0">
            <ele>10</ele><time>2025-01-01T00:00:00Z</time><name>First fix</name>
            <fix>3d</fix><sat>8</sat><hdop>0.8</hdop>
            <extensions><foreign:trkpt xmlns:foreign="urn:other" lat="90" lon="0"/></extensions>
          </trkpt>
        </trkseg></trk>"#,
    );
    let events = events(&xml, source);
    assert!(events.iter().any(|event| matches!(event,
        GpxEvent::Metadata { context, field, value }
            if context.object.is_none() && field == "name" && value == "Journey & context"
    )));
    assert!(events.iter().any(|event| matches!(event,
        GpxEvent::Metadata { context, field, value }
            if context.object == Some(GpxObjectId::Track(1)) && field == "name" && value == "Receiver track"
    )));
    assert!(events.iter().any(|event| matches!(event,
        GpxEvent::Ignored { context, element }
            if element == "extensions" && context.segment == Some(1) && context.point == Some(1)
    )));
    let points: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            GpxEvent::Point(point) => Some(point),
            _ => None,
        })
        .collect();
    assert_eq!(
        points.len(),
        1,
        "GPX-like extension contents cannot create observations"
    );
    let mapper = GpxMapper::new(source, [(GpxObjectId::Track(1), mapping("vehicle"))]).unwrap();
    let mapped = mapper.convert(&points[0], &Almanac::default()).unwrap();
    for (field, value) in [
        ("name", "First fix"),
        ("fix", "3d"),
        ("sat", "8"),
        ("hdop", "0.8"),
    ] {
        assert_eq!(
            mapped.point.metadata.get(field).map(String::as_str),
            Some(value)
        );
    }
    assert_eq!(mapped.observation.orientation, GpsOrientation::LocalEnu);
    assert_eq!(mapped.converted.estimate_type(), EstimateType::ESTIMATED);
}

#[test]
fn an_empty_document_has_boundaries_but_no_points_and_invalid_input_has_no_successful_end() {
    let source = "empty.gpx";
    assert_eq!(
        events(&document(""), source),
        [
            GpxEvent::DocumentStart {
                creator: "soloc-test".into()
            },
            GpxEvent::DocumentEnd,
        ]
    );
    for xml in [
        "",
        "not XML",
        r#"<gpx version="1.0" creator="test" xmlns="http://www.topografix.com/GPX/1/0"/>"#,
        r#"<gpx version="1.1" creator="test"/>"#,
        r#"<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1"><trk>"#,
    ] {
        let mut reader = GpxReader::new(xml.as_bytes(), source, WaypointPolicy::Reject);
        let mut error = None;
        for event in reader.by_ref() {
            match event {
                Err(found) => {
                    error = Some(found);
                    break;
                }
                Ok(GpxEvent::DocumentEnd) => panic!("invalid XML reached DocumentEnd: {xml}"),
                _ => {}
            }
        }
        let error = error.expect("an empty, unsupported, or incomplete XML input must fail");
        assert_eq!(error.context.source, source);
        assert!(
            reader.next().is_none(),
            "reader stops after its first error"
        );
    }
}

#[test]
fn standalone_waypoints_require_an_explicit_ignore_policy() {
    let source = "waypoints.gpx";
    let xml =
        document(r#"<wpt lat="0" lon="0"><ele>0</ele><time>2025-01-01T00:00:00Z</time></wpt>"#);
    let error = GpxReader::new(xml.as_bytes(), source, WaypointPolicy::Reject)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_err();
    assert!(matches!(error.kind, GpxErrorKind::Unsupported(_)));
    assert_eq!(error.context.source, source);
    assert_eq!(error.context.point, Some(1));
    let ignored = GpxReader::new(xml.as_bytes(), source, WaypointPolicy::Ignore)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(ignored.iter().any(|event| matches!(event,
        GpxEvent::Ignored { context, element } if element == "wpt" && context.point == Some(1)
    )));
    assert!(
        !ignored
            .iter()
            .any(|event| matches!(event, GpxEvent::Point(_)))
    );
    assert_eq!(ignored.last(), Some(&GpxEvent::DocumentEnd));
}

#[test]
fn missing_coordinates_and_malformed_point_xml_identify_the_later_segment_and_point() {
    let source = "broken-later.gpx";
    for broken_point in [
        r#"<trkpt lon="0"/>"#,
        r#"<trkpt lat="91" lon="0"/>"#,
        r#"<trkpt lat="0" lon="0"><ele>NaN</ele></trkpt>"#,
        r#"<trkpt lat="0" lon="0"><ele>10</time></trkpt>"#,
    ] {
        let xml = document(&format!(
            r#"<trk><trkseg/><trkseg>
              <trkpt lat="0" lon="0"/><trkpt lat="0" lon="1"/>{broken_point}
            </trkseg></trk>"#
        ));
        let error = GpxReader::new(xml.as_bytes(), source, WaypointPolicy::Reject)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_err();
        assert_point_context(&error, source, GpxObjectId::Track(1), Some(2), 3);
    }
}
