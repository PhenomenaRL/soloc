//! Acceptance through the public single-fix, GPX, ledger, transform and IPC APIs.
#![cfg(feature = "gpx")]

use anise::prelude::{Almanac, Epoch};
use arrow::array::{Array, RecordBatch, StructArray};
use hifitime::Duration;
use nalgebra::{Quaternion, UnitQuaternion, Vector3};
use soloc_ledger::gpx::import_gpx_into_ledger;
use soloc_ledger::ledger::{Ledger, names_sibling_path};
use spacetimestamp::gps::gpx::{
    GpxHeightPolicy, GpxImportOptions, GpxMapper, GpxMapping, GpxObjectId, GpxPointErrorPolicy,
    GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsObservation, GpsOptions, GpsOrientation, gps_observation_to_batch};
use spacetimestamp::identity::{as_id_column, id_at};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::EntitySchema;
use spacetimestamp::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode};

const JOURNEY: &[u8] = include_bytes!("fixtures/gpx/halifax.gpx");
const SOURCE: &str = "halifax.gpx";
const FIRST_GPX_TAI_NS: u64 = 789_004_837_000_000_000;
const OFFSETS: [i64; 6] = [-10, 0, 10, 20, 300, 310];
const GEODETIC: [[f64; 3]; 6] = [
    [44.6486, -63.5754, 9.0],
    [44.6488, -63.5752, 10.0],
    [44.6490, -63.5750, 11.0],
    [44.6492, -63.5748, 12.0],
    [44.6500, -63.5740, 13.0],
    [44.6502, -63.5738, 14.0],
];
// Independent WGS84 Cartesian values from PROJ 9.7.1's `cct -d 9
// +proj=cart +ellps=WGS84`, input longitude latitude height for GEODETIC above.
// No datum transformation is claimed: the production approximation labels these
// WGS84 Cartesian coordinates ITRF93. PROJ is not needed to run this test.
const ECEF_M: [[f64; 3]; 6] = [
    [
        2_022_670.359_380_248,
        -4_070_254.128_979_568,
        4_459_657.358_423_173,
    ],
    [
        2_022_677.933_111_124,
        -4_070_233.718_659_135,
        4_459_673.872_701_35,
    ],
    [
        2_022_685.506_697_163,
        -4_070_213.308_281_112,
        4_459_690.386_930_528,
    ],
    [
        2_022_693.080_138_368,
        -4_070_192.897_845_498,
        4_459_706.901_110_711,
    ],
    [
        2_022_722.422_629_653,
        -4_070_109.344_294_931,
        4_459_770.849_019_024,
    ],
    [
        2_022_729.995_343_277,
        -4_070_088.933_581_288,
        4_459_787.362_946_764,
    ],
];

fn gps_options(single_fix: bool) -> GpsOptions<'static> {
    let mut options = GpsOptions::new(
        PrescribedId::new("soloc.example", "halifax-vessel").unwrap(),
        PrescribedId::abstract_source(
            "soloc.example",
            if single_fix {
                "synthetic-fix"
            } else {
                "synthetic-gpx"
            },
        )
        .unwrap(),
    );
    options.estimate_type = EstimateType::SIMULATED;
    options
}

fn epoch(offset: i64) -> Epoch {
    Epoch::from_gregorian_utc(2025, 1, 1, 12, 0, 0, 0) + Duration::from_seconds(offset as f64)
}

fn observation(row: usize) -> GpsObservation {
    let [latitude_deg, longitude_deg, ellipsoidal_height_m] = GEODETIC[row];
    GpsObservation {
        latitude_deg,
        longitude_deg,
        ellipsoidal_height_m,
        epoch: epoch(OFFSETS[row]),
        orientation: GpsOrientation::LocalEnu,
    }
}

fn ingest() -> Ledger {
    let almanac = Almanac::default();
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    ledger.set_memory_limit(None).unwrap();
    ledger
        .register_name(
            gps_options(false).entity_id,
            "soloc.example",
            "halifax-vessel",
        )
        .unwrap();
    ledger
        .append(gps_observation_to_batch(&observation(0), &gps_options(true), &almanac).unwrap())
        .unwrap();
    let mapper = GpxMapper::new(
        SOURCE,
        [(
            GpxObjectId::Track(1),
            GpxMapping {
                gps: gps_options(false),
                height: GpxHeightPolicy::EllipsoidalElevation,
                time: GpxTimePolicy::RequireTimestamp,
                orientation: GpsOrientation::LocalEnu,
            },
        )],
    )
    .unwrap();
    let report = import_gpx_into_ledger(
        &mut ledger,
        GpxReader::new(JOURNEY, SOURCE, WaypointPolicy::Reject),
        &mapper,
        &almanac,
        GpxImportOptions::new(2, GpxPointErrorPolicy::Strict).unwrap(),
        |_| {},
    );
    assert!(report.complete, "{report:?}");
    assert_eq!((report.read, report.converted, report.submitted), (5, 5, 5));
    assert_eq!(report.submitted_batches, 3);
    assert_eq!(
        (
            report.skipped,
            report.failed,
            report.uncertain,
            report.unsubmitted
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(ledger.row_count(), 6);
    ledger
}

fn assert_identity_time_and_nulls(batch: &RecordBatch, frame: &str, units: LengthUnit) {
    assert_eq!(batch.num_rows(), 6);
    let columns = StsColumns::try_new(batch).unwrap();
    let ids = as_id_column(batch.column_by_name("entity_id").unwrap(), "entity_id").unwrap();
    let nested = batch
        .column_by_name("spacetimestamp")
        .unwrap()
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    for (row, offset) in OFFSETS.into_iter().enumerate() {
        let options = gps_options(row == 0);
        assert_eq!(id_at(ids, row).unwrap(), options.entity_id);
        assert_eq!(columns.source_at(row).unwrap(), options.source_id);
        assert_eq!(
            columns.frame_at(row).unwrap(),
            PrescribedId::astronomical_from_name(frame).unwrap()
        );
        assert_eq!(columns.units_at(row).unwrap(), units);
        assert_eq!(columns.timescale_at(row).unwrap(), TimeScaleCode::TAI);
        assert_eq!(columns.estimate_at(row).unwrap(), EstimateType::SIMULATED);
        assert_eq!(
            columns.epoch_parts_at(row),
            (
                0,
                FIRST_GPX_TAI_NS
                    .checked_add_signed(offset * 1_000_000_000)
                    .unwrap()
            )
        );
        for field in [
            "velocity",
            "angular_velocity",
            "acceleration",
            "mass_kg",
            "state_covariance",
            "dimensions",
        ] {
            assert!(
                batch.column_by_name(field).unwrap().is_null(row),
                "{field}, row {row}"
            );
        }
        for field in ["position_covariance", "orientation_covariance"] {
            assert!(
                nested.column_by_name(field).unwrap().is_null(row),
                "{field}, row {row}"
            );
        }
    }
}

fn assert_itrf93(batch: &RecordBatch, units: LengthUnit) {
    assert_identity_time_and_nulls(batch, "ITRF93", units);
    let columns = StsColumns::try_new(batch).unwrap();
    for row in 0..6 {
        let actual_m = Vector3::from(
            columns
                .position_at(row)
                .map(|value| units.to_km(value) * 1000.0),
        );
        assert!(
            (actual_m - Vector3::from(ECEF_M[row])).norm() < 1e-6,
            "row {row}: {actual_m:?}"
        );
        let [w, x, y, z] = columns.quaternion_at(row);
        assert!((w * w + x * x + y * y + z * z - 1.0).abs() < 1e-12);
        let orientation = UnitQuaternion::from_quaternion(Quaternion::new(w, x, y, z));
        let [latitude, longitude, _] = GEODETIC[row];
        let (sin_lat, cos_lat) = latitude.to_radians().sin_cos();
        let (sin_lon, cos_lon) = longitude.to_radians().sin_cos();
        let east = Vector3::new(-sin_lon, cos_lon, 0.0);
        let up = Vector3::new(cos_lat * cos_lon, cos_lat * sin_lon, sin_lat);
        assert!((orientation * Vector3::x() - east).norm() < 1e-12);
        assert!((orientation * Vector3::z() - up).norm() < 1e-12);
    }
}

#[test]
fn single_fix_and_halifax_gpx_survive_query_transform_and_file_reload() {
    let ledger = ingest();
    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    assert_itrf93(&history, LengthUnit::m);
    let segment = ledger
        .query(&SpatiotemporalFilter::new().with_time_range(epoch(0), epoch(20)))
        .unwrap();
    assert_eq!(segment, history.slice(1, 3));
    assert_eq!(
        ledger
            .query(&SpatiotemporalFilter::new().with_time_range(epoch(21), epoch(299)))
            .unwrap()
            .num_rows(),
        0
    );
    let latest = ledger
        .current_state(Some(&[gps_options(false).entity_id]), None)
        .unwrap();
    assert_eq!(latest, history.slice(5, 1));
    // current_state's second argument is a freshness cutoff, not an as-of time.
    assert_eq!(
        ledger
            .current_state(None, Some(epoch(311)))
            .unwrap()
            .num_rows(),
        0
    );

    let kilometres = ledger
        .transform(&history, "ITRF93", LengthUnit::km, &Almanac::default())
        .unwrap();
    assert_itrf93(&kilometres, LengthUnit::km);
    assert!(
        ledger
            .transform(&history, "GCRF", LengthUnit::m, &Almanac::default())
            .is_err()
    );
    assert_eq!(ledger.query(&SpatiotemporalFilter::new()).unwrap(), history);

    // Persist the transformed result and registry, then exercise the rebuilt query
    // and current-state indexes. The original metre ledger remains unchanged.
    let mut exported = Ledger::for_schema::<EntitySchema>().unwrap();
    exported.append(kilometres.clone()).unwrap();
    exported
        .merge_names_from_ipc_bytes(&ledger.names_to_ipc_bytes().unwrap())
        .unwrap();
    let path =
        std::env::temp_dir().join(format!("soloc-gps-workflow-{}.arrow", std::process::id()));
    exported.save_ipc(&path).unwrap();
    let reloaded = Ledger::load_ipc(&path, "entity_id").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(names_sibling_path(&path)).unwrap();
    assert_eq!(reloaded.row_count(), 6);
    let loaded_history = reloaded.query(&SpatiotemporalFilter::new()).unwrap();
    assert_eq!(loaded_history, kilometres);
    assert_itrf93(&loaded_history, LengthUnit::km);
    assert_eq!(
        reloaded.current_state(None, None).unwrap(),
        kilometres.slice(5, 1)
    );
    assert_eq!(
        reloaded.export_names().unwrap(),
        ledger.export_names().unwrap()
    );
    assert_itrf93(
        &reloaded
            .transform(
                &loaded_history,
                "ITRF93",
                LengthUnit::m,
                &Almanac::default(),
            )
            .unwrap(),
        LengthUnit::m,
    );
}

#[test]
#[ignore = "requires a local Earth .bpc covering January 1, 2025 in SOLOC_KERNEL_PATHS"]
fn retained_halifax_history_transforms_at_each_epoch_and_survives_ipc() {
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .expect("set SOLOC_KERNEL_PATHS to local Earth .bpc paths; no downloads occur");
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        assert!(
            path.is_file(),
            "kernel is not a local file: {}",
            path.display()
        );
        almanac = almanac.load(path.to_str().unwrap()).unwrap();
    }
    // Rotation to Earth-centred inertial axes needs no planetary ephemeris.
    almanac.spk_data.clear();
    almanac.planetary_data.clear();
    let ledger = ingest();
    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    let inertial = ledger
        .transform(&history, "GCRF", LengthUnit::m, &almanac)
        .unwrap();
    assert_identity_time_and_nulls(&inertial, "GCRF", LengthUnit::m);
    let columns = StsColumns::try_new(&inertial).unwrap();
    for (row, position) in ECEF_M.into_iter().enumerate() {
        // This checks agreement between two public ingestion paths. Independent
        // Earth-rotation reference tests live in spacetimestamp's kernel suite.
        let mut options = gps_options(row == 0);
        options.target_frame = "GCRF";
        let direct = gps_observation_to_batch(&observation(row), &options, &almanac).unwrap();
        let expected = StsColumns::try_new(&direct).unwrap();
        assert!(
            (Vector3::from(columns.position_at(row)) - Vector3::from(expected.position_at(0)))
                .norm()
                < 1e-6
        );
        let actual_q = columns.quaternion_at(row);
        let expected_q = expected.quaternion_at(0);
        let dot: f64 = actual_q
            .into_iter()
            .zip(expected_q)
            .map(|(a, b)| a * b)
            .sum();
        assert!((dot.abs() - 1.0).abs() < 1e-12);
        assert!(
            (Vector3::from(columns.position_at(row)).norm() - Vector3::from(position).norm()).abs()
                < 1e-6
        );
        assert!(
            (Vector3::from(columns.position_at(row)) - Vector3::from(position)).norm()
                > 1_000_000.0
        );
    }
    let mut exported = Ledger::for_schema::<EntitySchema>().unwrap();
    exported.append(inertial.clone()).unwrap();
    let reloaded =
        Ledger::load_ipc_from_bytes(&exported.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();
    assert_eq!(
        reloaded.query(&SpatiotemporalFilter::new()).unwrap(),
        inertial
    );
    assert_eq!(
        reloaded.current_state(None, None).unwrap(),
        inertial.slice(5, 1)
    );
    assert_itrf93(
        &reloaded
            .transform(&inertial, "ITRF93", LengthUnit::m, &almanac)
            .unwrap(),
        LengthUnit::m,
    );
    assert_eq!(ledger.query(&SpatiotemporalFilter::new()).unwrap(), history);
}
