//! Kernel-backed checks of the public Earth-frame playback API.
//!
//! Never downloads kernels. Run with an Earth BPC supplied explicitly:
//! `SOLOC_KERNEL_PATHS=/path/earth.bpc cargo test -p spacetimestamp --test gps_playback -- --ignored`
//! Direct ANISE/GPS comparisons check composition, not independent Earth-model accuracy.

use anise::constants::orientations::ITRF93;
use anise::prelude::{Almanac, Epoch};
use arrow::array::{Array, FixedSizeBinaryArray};
use hifitime::{Duration, TimeScale};
use nalgebra::{Quaternion, UnitQuaternion, Vector3};
use spacetimestamp::gps::{
    GpsObservation, GpsOptions, GpsOrientation, check_itrf93_interval, convert_gps_observation,
    itrf93_frame_snapshot,
};
use spacetimestamp::identity::id_at;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::validation::validate_spacetimestamp_batch;
use spacetimestamp::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode, epoch_from_parts};

fn local_almanac() -> Almanac {
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .expect("set SOLOC_KERNEL_PATHS to local Earth .bpc; no downloads occur");
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        assert!(path.is_file(), "not a local kernel: {}", path.display());
        almanac = almanac
            .load(path.to_str().expect("kernel path must be UTF-8"))
            .unwrap();
    }
    // Deliberately retain only BPC data: Earth-centred playback must not acquire
    // a hidden requirement for SPK, planetary constants, GM or an IAU model.
    almanac.spk_data.clear();
    almanac.planetary_data.clear();
    almanac.bpc_domain(ITRF93).unwrap();
    almanac
}

fn quaternion(parts: [f64; 4]) -> UnitQuaternion<f64> {
    UnitQuaternion::from_quaternion(Quaternion::new(parts[0], parts[1], parts[2], parts[3]))
}

fn frame_rotation(almanac: &Almanac, epoch: Epoch) -> UnitQuaternion<f64> {
    let batch = itrf93_frame_snapshot(almanac, epoch).unwrap();
    quaternion(StsColumns::try_new(&batch).unwrap().quaternion_at(0))
}

#[test]
#[ignore = "requires a local Earth BPC in SOLOC_KERNEL_PATHS"]
fn frame_snapshot_preserves_identity_units_time_and_unknown_state() {
    let almanac = local_almanac();
    let utc: Epoch = "2025-01-01T00:00:00 UTC".parse().unwrap();
    let tai: Epoch = "2025-01-01T00:00:37 TAI".parse().unwrap();
    let utc_batch = itrf93_frame_snapshot(&almanac, utc).unwrap();
    let tai_batch = itrf93_frame_snapshot(&almanac, tai).unwrap();
    assert_eq!(utc_batch, tai_batch);
    validate_spacetimestamp_batch(&utc_batch).unwrap();
    assert_eq!(utc_batch.num_rows(), 1);
    let cols = StsColumns::try_new(&utc_batch).unwrap();
    let entities = utc_batch
        .column_by_name("entity_id")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .unwrap();
    assert_eq!(
        id_at(entities, 0).unwrap(),
        PrescribedId::astronomical(399, 3000).unwrap()
    );
    assert_eq!(
        cols.frame_at(0).unwrap(),
        PrescribedId::astronomical(399, 1).unwrap()
    );
    assert_eq!(
        cols.source_at(0).unwrap(),
        PrescribedId::abstract_source("anise", "almanac").unwrap()
    );
    assert_eq!(cols.units_at(0).unwrap(), LengthUnit::m);
    assert_eq!(cols.timescale_at(0).unwrap(), TimeScaleCode::TAI);
    assert_eq!(cols.estimate_at(0).unwrap(), EstimateType::ESTIMATED);
    assert_eq!(cols.position_at(0), [0.0; 3]);
    let (centuries, nanos) = cols.epoch_parts_at(0);
    assert_eq!(epoch_from_parts(centuries, nanos, TimeScale::TAI), tai);
    for name in [
        "velocity",
        "angular_velocity",
        "acceleration",
        "mass_kg",
        "state_covariance",
        "dimensions",
    ] {
        assert!(utc_batch.column_by_name(name).unwrap().is_null(0), "{name}");
    }
}

#[test]
#[ignore = "requires a local Earth BPC in SOLOC_KERNEL_PATHS"]
fn native_gps_observations_composed_with_anchor_match_direct_inertial_conversion() {
    let almanac = local_almanac();
    let options = GpsOptions::new(
        PrescribedId::new("gps-playback-test", "receiver").unwrap(),
        PrescribedId::abstract_source("gps-playback-test", "receiver-log").unwrap(),
    );
    for date in [
        "2020-01-01T00:00:00 UTC",
        "2025-01-01T06:00:00 UTC",
        "2025-01-01T12:00:00 UTC",
    ] {
        let at: Epoch = date.parse().unwrap();
        let anchor = frame_rotation(&almanac, at);
        for (latitude, longitude, height) in [
            (0.0, 0.0, 0.0),
            (0.0, 90.0, 123.0),
            (44.6488, -63.5752, 10.0),
            (90.0, 0.0, 100.0),
        ] {
            let fix = GpsObservation {
                latitude_deg: latitude,
                longitude_deg: longitude,
                ellipsoidal_height_m: height,
                epoch: at,
                orientation: GpsOrientation::LocalEnu,
            };
            let native = convert_gps_observation(&fix, &options, &Almanac::default()).unwrap();
            let direct = convert_gps_observation(
                &fix,
                &GpsOptions {
                    target_frame: "GCRF",
                    ..options
                },
                &almanac,
            )
            .unwrap();
            let world = anchor * Vector3::from(native.position_m());
            assert!((world - Vector3::from(direct.position_m())).norm() < 1e-8);
            let world_q = anchor * quaternion(native.quaternion_wxyz());
            assert!(world_q.angle_to(&quaternion(direct.quaternion_wxyz())) < 1e-12);
            assert_eq!(
                native.frame_id(),
                PrescribedId::astronomical(399, 3000).unwrap()
            );
            assert_eq!(native.epoch_parts(), direct.epoch_parts());
            assert!((world.norm() - Vector3::from(native.position_m()).norm()).abs() < 1e-8);
            // Independent exact-axis ellipsoid checks prevent units/height or
            // longitude-handedness errors from hiding in a shared rotation.
            if latitude == 0.0 && longitude == 90.0 {
                assert!(native.position_m()[0].abs() < 1e-8);
                assert!((native.position_m()[1] - 6_378_260.0).abs() < 1e-8);
                assert_eq!(native.position_m()[2], 0.0);
            }
            if latitude == 90.0 {
                assert!((native.position_m()[2] - 6_356_852.314_245_179).abs() < 1e-8);
            }
        }
    }
}

#[test]
#[ignore = "requires a local Earth BPC in SOLOC_KERNEL_PATHS"]
fn anchor_rotation_is_eastward_and_short_sample_interpolation_tracks_kernels() {
    let almanac = local_almanac();
    for date in ["2020-01-01T00:00:00 UTC", "2025-01-01T00:00:00 UTC"] {
        let start: Epoch = date.parse().unwrap();
        let end = start + Duration::from_seconds(21_600.0);
        check_itrf93_interval(&almanac, start, end).unwrap();
        let first = frame_rotation(&almanac, start);
        let last = frame_rotation(&almanac, end);
        let angle = first.angle_to(&last);
        assert!((1.56..1.59).contains(&angle), "6-hour rotation {angle}");
        let x0 = first * Vector3::x();
        let x1 = last * Vector3::x();
        let north = first * Vector3::z();
        assert!(
            x0.cross(&x1).dot(&north) > 0.99,
            "Earth must rotate eastward"
        );

        // Measured integration check at the exporter's maximum supported
        // cadence. This is not a universal kernel interpolation error bound.
        let after = frame_rotation(&almanac, start + Duration::from_seconds(300.0));
        let midpoint = frame_rotation(&almanac, start + Duration::from_seconds(150.0));
        let interpolated = first.slerp(&after, 0.5);
        let surface = Vector3::new(6_378_137.0, 0.0, 0.0);
        assert!((interpolated * surface - midpoint * surface).norm() < 0.05);
    }
}

#[test]
#[ignore = "requires a local Earth BPC in SOLOC_KERNEL_PATHS"]
fn interval_checks_reject_outside_loaded_data_and_accept_a_covered_instant() {
    let almanac = local_almanac();
    let at: Epoch = "2025-01-01T00:00:00 UTC".parse().unwrap();
    check_itrf93_interval(&almanac, at, at).unwrap();
    let (first, last) = almanac.bpc_domain(ITRF93).unwrap();
    let one_second = Duration::from_seconds(1.0);
    for (start, end) in [(first - one_second, at), (at, last + one_second)] {
        let error = check_itrf93_interval(&almanac, start, end).unwrap_err();
        assert!(
            error.contains("no continuous ITRF93 BPC coverage"),
            "{error}"
        );
    }
    for outside in [first - one_second, last + one_second] {
        let error = itrf93_frame_snapshot(&almanac, outside).unwrap_err();
        assert!(
            error.contains("no loaded ITRF93 BPC segment covers"),
            "{error}"
        );
    }
}
