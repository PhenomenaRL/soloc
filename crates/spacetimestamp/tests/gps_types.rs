//! Public API checks for T02. Samples are built explicitly: these tests do not
//! claim GPX parsing, datum/time resolution, ECEF conversion, or publication.

use std::error::Error;

use spacetimestamp::geodesy::{
    CoordinateError, CoordinateField, EllipsoidalHeight, ResolvedGeodeticPosition,
};
use spacetimestamp::gps::{
    FramePolicy, GeoidSeparation, GpsError, GpsFix, HeightPolicy, ImportCounts, ImportMode,
    ImportReport, ImportWarning, RawGpsSample, SampleDisposition, SampleError, SampleIssue,
    SampleLocation, SampleOutcome, SampleWarning, SourceDocument, UnsupportedGeometry,
    VerticalDatum,
};

#[test]
fn source_bytes_and_digest_remain_exact_and_incomplete_data_stays_raw() {
    let source = SourceDocument::new(b"abc".as_slice());
    assert_eq!(source.bytes(), b"abc");
    // Published SHA-256 check vector, independent of the implementation's hash call.
    assert_eq!(
        source.sha256(),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ]
    );
    let location = SampleLocation::new(0, 0, 0);
    let mut sample = RawGpsSample::new(source.sample_key(location));
    assert_eq!(sample.latitude_deg, None);
    assert_eq!(sample.longitude_deg, None);
    assert_eq!(sample.elevation_m, None);
    assert_eq!(sample.timestamp, None);
    assert_eq!(sample.quality.fix, None);

    // Even an invalid coordinate can be retained for indexed diagnostics.
    sample.latitude_deg = Some(91.0);
    sample.longitude_deg = Some(180.0);
    sample.timestamp = Some("2000-01-01T11:59:28".into());
    sample.geoid_separation = Some(GeoidSeparation {
        metres: 30.0,
        model: None,
        reference_datum: None,
        source: Some("receiver geoidheight field".into()),
    });
    assert_eq!(sample.vertical_datum, VerticalDatum::Unknown);
    assert_eq!(sample.datum_evidence, None);
    assert_eq!(sample.elevation_m, None);
    assert_eq!(sample.longitude_deg, Some(180.0));
    assert_eq!(sample.timestamp.as_deref(), Some("2000-01-01T11:59:28"));
    assert!(ResolvedGeodeticPosition::new(sample.latitude_deg.unwrap(), 0.0, 0.0).is_err());
    sample.quality.fix = Some(GpsFix::NoFix);
    assert_ne!(sample.quality.fix, None);

    assert_eq!(source.clone(), source);
    assert_eq!(
        source.sha256(),
        SourceDocument::new(b"abc".as_slice()).sha256()
    );
    assert_ne!(
        source.sha256(),
        SourceDocument::new(b"abc\n".as_slice()).sha256()
    );
}

#[test]
fn named_acceptance_trace_retains_five_samples_and_two_segments_without_a_ledger() {
    // The exact values selected by equatorial-two-segment-v1. GPX serialization,
    // epoch conversion, and forward/inverse mathematics belong to later tasks.
    let rows = [
        (0, 0, "2000-01-01T11:59:28Z", 0.0, 0.0),
        (0, 1, "2000-01-01T11:59:29Z", 0.0, 0.00001),
        (0, 2, "2000-01-01T11:59:30Z", 0.0, 0.00002),
        (1, 0, "2000-01-01T11:59:40Z", 0.0, 0.00010),
        (1, 1, "2000-01-01T11:59:41Z", 0.00001, 0.00010),
    ];
    let source = SourceDocument::new(b"equatorial-two-segment-v1: manual test inputs".as_slice());
    let samples: Vec<_> = rows
        .into_iter()
        .map(|(segment, point, timestamp, latitude, longitude)| {
            let mut sample =
                RawGpsSample::new(source.sample_key(SampleLocation::new(0, segment, point)));
            sample.latitude_deg = Some(latitude);
            sample.longitude_deg = Some(longitude);
            sample.elevation_m = Some(0.0);
            sample.timestamp = Some(timestamp.into());
            sample.vertical_datum = VerticalDatum::Wgs84Ellipsoidal;
            sample.datum_evidence = Some("synthetic fixture definition".into());
            sample
        })
        .collect();
    let mut report = ImportReport::new(source.sha256(), ImportMode::default());
    for sample in &samples {
        let position = ResolvedGeodeticPosition::new(
            sample.latitude_deg.unwrap(),
            sample.longitude_deg.unwrap(),
            sample.elevation_m.unwrap(),
        )
        .unwrap();
        assert_eq!(position.ellipsoidal_height_m(), 0.0);
        assert!(sample.quality.fix.is_none());
        report
            .record(SampleOutcome {
                sample: sample.key,
                issues: vec![],
                warnings: vec![],
            })
            .unwrap();
    }
    assert_eq!(report.mode(), ImportMode::Strict);
    assert_eq!(report.source_sha256(), source.sha256());
    assert_eq!(
        report.counts(),
        ImportCounts {
            accepted: 5,
            rejected: 0,
            deferred: 0
        }
    );
    assert_eq!(
        report
            .outcomes()
            .iter()
            .map(|o| o.sample)
            .collect::<Vec<_>>(),
        samples.iter().map(|s| s.key).collect::<Vec<_>>()
    );
    assert_ne!(samples[0].key, samples[3].key); // Same point index, different segment.
    assert_eq!(samples[2].key.location.segment_index, 0);
    assert_eq!(samples[3].key.location.segment_index, 1);
    assert_eq!(
        samples[0].timestamp.as_deref(),
        Some("2000-01-01T11:59:28Z")
    );
}

#[test]
fn indexed_errors_keep_machine_readable_causes_and_source_context() {
    let source = SourceDocument::new(b"receiver trace".as_slice());
    let key = source.sample_key(SampleLocation::new(2, 3, 4));
    let coordinate_error = ResolvedGeodeticPosition::new(91.0, 0.0, 0.0).unwrap_err();
    let error = SampleError::new(key, coordinate_error);
    assert_eq!(error.sample, key);
    assert!(matches!(
        error.issue,
        SampleIssue::Coordinate(CoordinateError::OutOfRange {
            field: CoordinateField::Latitude,
            ..
        })
    ));
    let message = error.to_string();
    assert!(message.contains("source sha256:"));
    assert!(message.contains("track 2, segment 3, point 4 (zero-based)"));
    assert!(message.contains("latitude"));
    let error = GpsError::from(error);
    let indexed = error.source().unwrap();
    let issue = indexed.source().unwrap();
    assert!(issue.source().unwrap().is::<CoordinateError>());
    assert!(matches!(error, GpsError::Sample(_)));
}

#[test]
fn reports_retain_all_issues_and_rejections_take_precedence_over_deferrals() {
    let source = SourceDocument::new(b"mixed trace".as_slice());
    let mut report = ImportReport::new(source.sha256(), ImportMode::Strict);
    report.mark_track_unselected(1).unwrap();
    report.mark_track_unselected(1).unwrap();
    report.warnings.push(ImportWarning::UnprocessedGeometry {
        kind: UnsupportedGeometry::Waypoint,
        count: 2,
    });
    let issues = [
        vec![],
        vec![SampleIssue::MissingHeight, SampleIssue::MissingEpoch],
        vec![SampleIssue::MissingHeight, SampleIssue::NoValidFix],
    ];
    for (point, issues) in issues.into_iter().enumerate() {
        report
            .record(SampleOutcome {
                sample: source.sample_key(SampleLocation::new(0, 0, point)),
                issues,
                warnings: vec![SampleWarning::DuplicateEpoch],
            })
            .unwrap();
    }
    assert_eq!(
        report.counts(),
        ImportCounts {
            accepted: 1,
            rejected: 1,
            deferred: 1
        }
    );
    assert_eq!(
        report.outcomes()[0].disposition(),
        SampleDisposition::Accepted
    );
    assert_eq!(report.outcomes()[1].issues.len(), 2);
    let mut reversed = report.outcomes()[2].clone();
    reversed.issues.reverse();
    assert_eq!(reversed.disposition(), SampleDisposition::Rejected);
    assert_eq!(report.unselected_tracks(), [1]);
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(
        report.outcomes()[0].warnings,
        [SampleWarning::DuplicateEpoch]
    );

    // The counts are eligibility data even in Strict mode, not a publication claim.
    let before = report.outcomes().to_vec();
    assert_eq!(
        report.mark_track_unselected(0),
        Err(GpsError::ConflictingTrackSelection { track_index: 0 })
    );
    assert_eq!(
        report.record(SampleOutcome {
            sample: source.sample_key(SampleLocation::new(1, 0, 0)),
            issues: vec![],
            warnings: vec![],
        }),
        Err(GpsError::ConflictingTrackSelection { track_index: 1 })
    );
    assert_eq!(report.unselected_tracks(), [1]);
    assert_eq!(
        report.record(before[0].clone()),
        Err(GpsError::DuplicateReportSample {
            location: SampleLocation::new(0, 0, 0),
        })
    );
    let other = SourceDocument::new(b"different trace".as_slice());
    assert_eq!(
        report.record(SampleOutcome {
            sample: other.sample_key(SampleLocation::new(0, 0, 3)),
            issues: vec![],
            warnings: vec![],
        }),
        Err(GpsError::ReportSourceMismatch)
    );
    assert_eq!(report.outcomes(), before);
    // A failed source check must not reserve the location in this report.
    report
        .record(SampleOutcome {
            sample: source.sample_key(SampleLocation::new(0, 0, 3)),
            issues: vec![],
            warnings: vec![],
        })
        .unwrap();
    assert_eq!(report.counts().accepted, 2);
}

#[test]
fn policies_require_explicit_assumptions_and_incomplete_fields_are_deferrals() {
    assert_eq!(ImportMode::default(), ImportMode::Strict);
    assert_eq!(HeightPolicy::default(), HeightPolicy::RequireResolved);
    let policy = HeightPolicy::AssumeEllipsoidalHeight(EllipsoidalHeight::new(0.0).unwrap());
    let HeightPolicy::AssumeEllipsoidalHeight(height) = policy else {
        panic!("explicit fallback missing")
    };
    assert_eq!(height.metres(), 0.0);
    assert!(EllipsoidalHeight::new(f64::NAN).is_err());
    let _frame = FramePolicy::ApproximateIauEarth; // Caller selects this explicitly.

    for issue in [
        SampleIssue::MissingHeight,
        SampleIssue::UnresolvedHeightDatum,
        SampleIssue::MissingGeoidSeparation,
        SampleIssue::IncompatibleGeoidSeparation,
        SampleIssue::MissingEpoch,
        SampleIssue::MissingTimezone,
    ] {
        assert_eq!(issue.disposition(), SampleDisposition::Deferred);
    }
    for issue in [
        SampleIssue::MissingLatitude,
        SampleIssue::MissingLongitude,
        SampleIssue::InvalidField {
            field: "geoidheight".into(),
            value: "NaN".into(),
        },
        SampleIssue::InvalidGpxLongitude { value: 180.0 },
        SampleIssue::InvalidTimestamp {
            value: "invalid".into(),
        },
        SampleIssue::UnsupportedTimePrecision,
        SampleIssue::UnsupportedEpoch {
            reason: "unrepresentable leap second".into(),
        },
        SampleIssue::NoValidFix,
    ] {
        assert_eq!(issue.disposition(), SampleDisposition::Rejected);
    }
}
