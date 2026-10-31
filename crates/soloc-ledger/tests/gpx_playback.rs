//! Playback export contracts, with opt-in local-kernel coverage.
#![cfg(feature = "gpx")]

use anise::prelude::{Almanac, Epoch};
use arrow::array::{Array, StringArray, UInt64Array};
use soloc_ledger::gpx::playback::{GpxPlaybackOptions, import_gpx_for_playback};
use soloc_ledger::ledger::{Ledger, names_sibling_path};
use spacetimestamp::gps::gpx::{
    GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxMapper, GpxMapping, GpxObjectId,
    GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::identity::{as_id_column, id_at};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::{PrescribedId, TimeScaleCode, epoch_to_parts};

const SOURCE: &str = "playback.gpx";
const HEADER: &str =
    r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="playback-test">"#;

fn gps_options() -> GpsOptions<'static> {
    GpsOptions::new(
        PrescribedId::new("playback-test", "receiver").unwrap(),
        PrescribedId::abstract_source("playback-test", "import").unwrap(),
    )
}

fn mapper(gps: GpsOptions<'static>) -> GpxMapper<'static> {
    let mapping = GpxMapping {
        gps,
        height: GpxHeightPolicy::EllipsoidalElevation,
        time: GpxTimePolicy::RequireTimestamp,
        orientation: GpsOrientation::LocalEnu,
    };
    GpxMapper::new(
        SOURCE,
        [
            (GpxObjectId::Route(1), mapping),
            (GpxObjectId::Track(1), mapping),
            (GpxObjectId::Track(2), mapping),
        ],
    )
    .unwrap()
}

fn options(policy: GpxPointErrorPolicy) -> GpxPlaybackOptions {
    GpxPlaybackOptions::new(GpxImportOptions::new(2, policy).unwrap(), 60.0, 100).unwrap()
}

fn route(times: &[Option<&str>]) -> String {
    let mut xml = format!("{HEADER}<rte>");
    for time in times {
        xml.push_str(r#"<rtept lat="0" lon="90"><ele>10</ele>"#);
        if let Some(time) = time {
            xml.push_str(&format!("<time>2025-01-01T00:{time}Z</time>"));
        }
        xml.push_str("</rtept>");
    }
    xml.push_str("</rte></gpx>");
    xml
}

fn reader(xml: &str) -> GpxReader<&[u8]> {
    GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject)
}

#[test]
fn validates_frame_cadence_and_memory_guard() {
    let import = GpxImportOptions::default();
    for cadence in [0.0, -1.0, 1e-12, 300.1, f64::INFINITY, f64::NAN] {
        assert!(GpxPlaybackOptions::new(import, cadence, 10).is_err());
    }
    for count in [0, 1_000_001] {
        assert!(GpxPlaybackOptions::new(import, 60.0, count).is_err());
    }
    assert!(GpxPlaybackOptions::new(import, 1e-9, 1).is_ok());
    assert!(GpxPlaybackOptions::new(import, 300.0, 1_000_000).is_ok());
    assert_eq!(GpxPlaybackOptions::default().frame_step_seconds(), 60.0);
}

#[test]
fn incomplete_input_keeps_precise_report_without_exposing_partial_export() {
    let xml = route(&[Some("00:00"), Some("00:10"), Some("00:20"), None]);
    let mut accepted = 0;
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &Almanac::default(),
        options(GpxPointErrorPolicy::Strict),
        |event| {
            if let GpxImportEvent::Submitted(batch) = event {
                accepted += batch.rows;
            }
        },
    )
    .unwrap_err();
    let report = error.report.unwrap();
    assert!(!report.complete);
    assert_eq!(
        (
            accepted,
            report.submitted,
            report.unsubmitted,
            report.failed
        ),
        (2, 2, 1, 1)
    );
    assert_eq!(report.pending.unwrap().first.point, Some(3));
}

#[test]
fn missing_kernels_never_return_an_unposed_itrf93_artifact() {
    let xml = route(&[Some("00:00"), None, Some("00:20")]);
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &Almanac::default(),
        options(GpxPointErrorPolicy::SkipInvalid),
        |_| {},
    )
    .unwrap_err();
    assert!(error.message.contains("BPC"), "{error}");
    let report = error.report.unwrap();
    assert!(report.complete);
    assert_eq!((report.submitted, report.skipped), (2, 1));
}

#[test]
fn empty_input_and_excess_observation_epochs_have_explicit_failures() {
    let xml = format!("{HEADER}<trk><trkseg/></trk></gpx>");
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &Almanac::default(),
        GpxPlaybackOptions::default(),
        |_| {},
    )
    .unwrap_err();
    assert!(error.message.contains("at least one"));
    assert!(error.report.unwrap().complete);

    let xml = route(&[Some("00:00"), Some("00:10")]);
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &Almanac::default(),
        GpxPlaybackOptions::new(GpxImportOptions::default(), 60.0, 1).unwrap(),
        |_| {},
    )
    .unwrap_err();
    assert!(error.message.contains("max_frame_samples"));
    let report = error.report.unwrap();
    assert_eq!((report.submitted, report.rejected), (0, 2));
}

fn local_almanac() -> Almanac {
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .expect("set SOLOC_KERNEL_PATHS to local Earth BPC paths; no downloads occur");
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        assert!(
            path.is_file(),
            "kernel is not a local file: {}",
            path.display()
        );
        almanac = almanac.load(path.to_str().unwrap()).unwrap();
    }
    almanac
}

const COMPLEX: &str = r#"
<rte>
 <rtept lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:02:05Z</time></rtept>
 <rtept lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:01:00Z</time></rtept>
 <rtept lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:01:00Z</time></rtept>
</rte>
<trk><trkseg>
 <trkpt lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:00:00Z</time></trkpt>
 <trkpt lat="0" lon="90"><ele>10</ele></trkpt>
 <trkpt lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:00:30Z</time></trkpt>
</trkseg><trkseg/><trkseg>
 <trkpt lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:02:30Z</time></trkpt>
 <trkpt lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:03:00Z</time></trkpt>
</trkseg></trk>
<trk><trkseg><trkpt lat="0" lon="90"><ele>10</ele><time>2025-01-01T00:00:10Z</time></trkpt></trkseg></trk>
"#;

#[test]
#[ignore = "requires local Earth .bpc files in SOLOC_KERNEL_PATHS"]
fn export_preserves_rows_and_gap_provenance_with_complete_frame_coverage_and_ipc_names() {
    let almanac = local_almanac();
    let xml = format!("{HEADER}{COMPLEX}</gpx>");
    let mut export = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &almanac,
        options(GpxPointErrorPolicy::SkipInvalid),
        |_| {},
    )
    .unwrap();
    export
        .register_name(gps_options().entity_id, "playback-test", "receiver")
        .unwrap();
    assert_eq!((export.report().submitted, export.report().skipped), (8, 1));
    assert_eq!(export.frame_samples(), 8);
    assert_eq!(export.ledger().row_count(), 16);
    let history = export.ledger().query(&SpatiotemporalFilter::new()).unwrap();
    assert_eq!(history.schema().metadata()["soloc:gpx_playback"], "1");
    let ids = as_id_column(history.column_by_name("entity_id").unwrap(), "entity_id").unwrap();
    let sts = StsColumns::try_new(&history).unwrap();
    let source = history
        .column_by_name("gpx_source")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let object = history
        .column_by_name("gpx_object")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let segment = history
        .column_by_name("gpx_segment")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let point = history
        .column_by_name("gpx_point")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    for (row, (object_name, segment_number, point_number)) in [
        ("route:1", None, 1),
        ("route:1", None, 2),
        ("route:1", None, 3),
        ("track:1", Some(1), 1),
        ("track:1", Some(1), 3),
        ("track:1", Some(3), 1),
        ("track:1", Some(3), 2),
        ("track:2", Some(1), 1),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(id_at(ids, row).unwrap(), gps_options().entity_id);
        assert_eq!(source.value(row), SOURCE);
        assert_eq!(object.value(row), object_name);
        assert_eq!(segment.is_null(row), segment_number.is_none());
        if let Some(number) = segment_number {
            assert_eq!(segment.value(row), number);
        }
        assert_eq!(point.value(row), point_number);
        assert_eq!(sts.frame_at(row).unwrap().astro_frame(), Some((399, 3000)));
        assert_eq!(sts.timescale_at(row).unwrap(), TimeScaleCode::TAI);
        assert!((sts.position_at(row)[1] - 6_378_147.0).abs() < 1e-8);
    }
    let anchor_times = [
        "00:00", "00:10", "00:30", "01:00", "02:00", "02:05", "02:30", "03:00",
    ];
    for (row, time) in (8..16).zip(anchor_times) {
        assert_eq!(id_at(ids, row).unwrap().astro_frame(), Some((399, 3000)));
        assert_eq!(sts.frame_at(row).unwrap().astro_frame(), Some((399, 1)));
        assert_eq!(sts.position_at(row), [0.0; 3]);
        assert!(
            source.is_null(row)
                && object.is_null(row)
                && segment.is_null(row)
                && point.is_null(row)
        );
        let epoch: Epoch = format!("2025-01-01T00:{time} UTC").parse().unwrap();
        let (centuries, ns) = epoch_to_parts(epoch);
        assert_eq!(sts.epoch_parts_at(row), (centuries, ns));
    }
    let path =
        std::env::temp_dir().join(format!("soloc-gpx-playback-{}.arrow", std::process::id()));
    export.save_ipc(&path).unwrap();
    let loaded = Ledger::load_ipc(&path, "entity_id").unwrap();
    assert_eq!(loaded.row_count(), 16);
    assert_eq!(loaded.schema(), export.ledger().schema());
    assert_eq!(loaded.names().len(), export.ledger().names().len());
    assert_eq!(
        loaded.names().name_of(&gps_options().entity_id),
        Some(("playback-test", "receiver"))
    );
    assert_eq!(loaded.query(&SpatiotemporalFilter::new()).unwrap(), history);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(names_sibling_path(&path)).unwrap();
}

#[test]
#[ignore = "requires local Earth .bpc files in SOLOC_KERNEL_PATHS"]
fn rejects_other_display_frames_and_caps_regular_frame_sampling() {
    let almanac = local_almanac();
    let xml = route(&[Some("00:00"), Some("01:00")]);
    let mut gps = gps_options();
    gps.target_frame = "GCRF";
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps),
        &almanac,
        options(GpxPointErrorPolicy::Strict),
        |_| {},
    )
    .unwrap_err();
    assert!(error.message.contains("requires ITRF93"));
    assert_eq!(error.report.unwrap().rejected, 2);

    let options = GpxPlaybackOptions::new(GpxImportOptions::default(), 10.0, 2).unwrap();
    let error = import_gpx_for_playback(
        reader(&xml),
        &mapper(gps_options()),
        &almanac,
        options,
        |_| {},
    )
    .unwrap_err();
    assert!(error.message.contains("max_frame_samples"));
    assert!(error.report.unwrap().complete);
}
