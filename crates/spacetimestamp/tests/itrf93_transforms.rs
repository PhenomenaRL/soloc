//! Production transforms with explicitly supplied local Earth orientation data.
//!
//! These tests never download kernels. Supply a platform-separated path list containing
//! a planetary constants `.pca` (for IAU_EARTH) and an Earth `.bpc` covering 2020 and 2025:
//!
//! ```text
//! SOLOC_KERNEL_PATHS=/path/pck11.pca:/path/earth_latest_high_prec.bpc \
//!   cargo test -p spacetimestamp --test itrf93_transforms -- --ignored
//! ```
//!
//! Direct ANISE rotations check Soloc's frame resolution, units and per-row composition;
//! they are not an independent validation of the underlying Earth orientation model.

use anise::constants::frames::{EARTH_ITRF93, EARTH_J2000, IAU_EARTH_FRAME};
use anise::constants::orientations::ITRF93;
use anise::prelude::{Almanac, Epoch, Frame};
use arrow::array::RecordBatch;
use hifitime::Duration;
use nalgebra::{Quaternion, UnitQuaternion, Vector3};

use spacetimestamp::ephemeris::{epoch_from_parts, epoch_to_parts};
use spacetimestamp::identity::PrescribedId;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::EntityBuilder;
use spacetimestamp::transforms::transform_batch;
use spacetimestamp::validation::validate_spacetimestamp_batch;
use spacetimestamp::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};

const POSITION_KM: [f64; 3] = [6378.137, 0.0, 0.0];
const QUATERNION: [f64; 4] = [0.5, 0.5, 0.5, 0.5];
const FRAME_PAIRS: [(&str, &str); 4] = [
    ("ITRF93", "IAU_EARTH"),
    ("IAU_EARTH", "ITRF93"),
    ("ITRF93", "GCRF"),
    ("GCRF", "ITRF93"),
];

fn local_almanac() -> Almanac {
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS").expect(
        "set SOLOC_KERNEL_PATHS to local pck11.pca and Earth .bpc paths; no downloads occur",
    );
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        assert!(
            path.is_file(),
            "kernel is not a local file: {}",
            path.display()
        );
        assert!(
            matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("pca" | "bpc")
            ),
            "these same-origin tests require only .pca/.bpc files: {}",
            path.display()
        );
        let name = path.to_str().expect("kernel path must be valid UTF-8");
        almanac = almanac
            .load(name)
            .unwrap_or_else(|error| panic!("cannot load kernel {name}: {error}"));
    }
    almanac
        .bpc_domain(ITRF93)
        .expect("SOLOC_KERNEL_PATHS must include an Earth BPC containing ITRF93");
    almanac
}

fn epoch(year: i32) -> Epoch {
    Epoch::from_gregorian_tai_at_midnight(year, 1, 1)
}

fn batch(frame: &str, samples: &[(Epoch, LengthUnit)]) -> RecordBatch {
    let frame = PrescribedId::astronomical_from_name(frame).unwrap();
    let entity = PrescribedId::new("itrf93-test", "receiver").unwrap();
    let mut builder = EntityBuilder::new(samples.len());
    for (row, &(epoch, units)) in samples.iter().enumerate() {
        let source =
            PrescribedId::abstract_source("itrf93-test", &format!("source-{row}")).unwrap();
        let (centuries, nanos) = epoch_to_parts(epoch);
        builder.append_entity(
            entity,
            frame,
            units,
            TimeScaleCode::TAI,
            source,
            EstimateType::MEASURED,
            POSITION_KM.map(|coordinate| units.from_km(coordinate)),
            QUATERNION,
            centuries,
            nanos,
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

fn quaternion(components: [f64; 4]) -> UnitQuaternion<f64> {
    let [w, x, y, z] = components;
    UnitQuaternion::from_quaternion(Quaternion::new(w, x, y, z))
}

fn reference_frame(name: &str) -> Frame {
    // Use ANISE's constants independently of Soloc's frame-name lookup.
    match name {
        "ITRF93" => EARTH_ITRF93,
        "IAU_EARTH" => IAU_EARTH_FRAME,
        "GCRF" => EARTH_J2000,
        _ => panic!("unexpected test frame: {name}"),
    }
}

#[test]
#[ignore = "requires local .pca and Earth .bpc files in SOLOC_KERNEL_PATHS"]
fn itrf93_transforms_use_each_rows_epoch_and_units_and_preserve_metadata() {
    let almanac = local_almanac();
    let samples = [
        (epoch(2020), LengthUnit::m),
        (epoch(2020), LengthUnit::km),
        (epoch(2025), LengthUnit::m),
        (epoch(2025), LengthUnit::km),
    ];

    for (from, to) in FRAME_PAIRS {
        let mut almanac = almanac.clone();
        if from == "GCRF" || to == "GCRF" {
            // Earth-centred inertial transforms require the Earth BPC alone; neither
            // planetary constants nor an SPK is needed for a change of axes.
            almanac.planetary_data.clear();
        }
        let input = batch(from, &samples);
        validate_spacetimestamp_batch(&input).unwrap();
        let original = StsColumns::try_new(&input).unwrap();
        for output_unit in [LengthUnit::m, LengthUnit::km] {
            let output = transform_batch(&input, to, &almanac, output_unit, None).unwrap();
            validate_spacetimestamp_batch(&output).unwrap();
            let actual = StsColumns::try_new(&output).unwrap();
            for (row, &(at, _)) in samples.iter().enumerate() {
                let rotation = almanac
                    .rotate(reference_frame(from), reference_frame(to), at)
                    .unwrap();
                let expected = rotation.rot_mat * Vector3::from(POSITION_KM);
                let position = Vector3::from(actual.position_at(row).map(|v| output_unit.to_km(v)));
                assert!(
                    (position - expected).norm() < 1e-8,
                    "{from} -> {to}, row {row}"
                );
                // Both frames have Earth's centre: a rotation preserves radius, and changing
                // the label alone would leave this equatorial point several kilometres off.
                assert!((position.norm() - Vector3::from(POSITION_KM).norm()).abs() < 1e-8);
                assert!((position - Vector3::from(POSITION_KM)).norm() > 0.01);

                let components = actual.quaternion_at(row);
                assert!((components.iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-12);
                for axis in [Vector3::x(), Vector3::y(), Vector3::z()] {
                    let expected_axis = rotation.rot_mat * (quaternion(QUATERNION) * axis);
                    assert!((quaternion(components) * axis - expected_axis).norm() < 1e-12);
                }
                assert_eq!(
                    actual.frame_at(row).unwrap(),
                    PrescribedId::astronomical_from_name(to).unwrap()
                );
                assert_eq!(actual.units_at(row).unwrap(), output_unit);
                assert_eq!(
                    actual.source_at(row).unwrap(),
                    original.source_at(row).unwrap()
                );
                assert_eq!(
                    actual.timescale_at(row).unwrap(),
                    original.timescale_at(row).unwrap()
                );
                assert_eq!(
                    actual.estimate_at(row).unwrap(),
                    original.estimate_at(row).unwrap()
                );
                assert_eq!(actual.epoch_parts_at(row), original.epoch_parts_at(row));
            }
            // Parent schema fields are preserved when the nested spacetimestamp changes.
            for (index, field) in input.schema().fields().iter().enumerate() {
                if field.name() != "spacetimestamp" {
                    assert_eq!(
                        input.column(index).to_data(),
                        output.column(index).to_data()
                    );
                }
            }

            let round_trip =
                transform_batch(&output, from, &almanac, LengthUnit::km, None).unwrap();
            let recovered = StsColumns::try_new(&round_trip).unwrap();
            for row in 0..samples.len() {
                assert!(
                    (Vector3::from(recovered.position_at(row)) - Vector3::from(POSITION_KM)).norm()
                        < 1e-8
                );
                assert!(
                    quaternion(recovered.quaternion_at(row)).angle_to(&quaternion(QUATERNION))
                        < 1e-12
                );
            }
        }
    }
}

#[test]
#[ignore = "requires local .pca and Earth .bpc files in SOLOC_KERNEL_PATHS"]
fn missing_earth_orientation_data_identifies_the_required_kernel_and_row() {
    let almanac = local_almanac();
    let mut without_bpc = almanac.clone();
    without_bpc.bpc_data.clear();
    for (from, to) in FRAME_PAIRS {
        let input = batch(from, &[(epoch(2020), LengthUnit::m)]);
        let error = transform_batch(&input, to, &without_bpc, LengthUnit::m, None).unwrap_err();
        assert!(
            error.contains("no Earth orientation BPC data for ITRF93"),
            "{error}"
        );
        assert!(error.contains("row 0"), "{error}");
        assert!(error.contains(&epoch(2020).to_string()), "{error}");
    }

    let mut without_planetary_constants = almanac;
    without_planetary_constants.planetary_data.clear();
    for (from, to) in [("ITRF93", "IAU_EARTH"), ("IAU_EARTH", "ITRF93")] {
        let input = batch(from, &[(epoch(2020), LengthUnit::m)]);
        let error = transform_batch(
            &input,
            to,
            &without_planetary_constants,
            LengthUnit::m,
            None,
        )
        .unwrap_err();
        assert!(error.contains("Anise rotate error"), "{error}");
        assert!(error.contains("row 0"), "{error}");
        assert!(error.contains(&epoch(2020).to_string()), "{error}");
    }
}

#[test]
#[ignore = "requires local .pca and Earth .bpc files in SOLOC_KERNEL_PATHS"]
fn earth_bpc_coverage_is_checked_for_the_observation_row_in_both_directions() {
    let almanac = local_almanac();
    let (start, end) = almanac.bpc_domain(ITRF93).unwrap();
    for outside in [
        start - Duration::from_seconds(1.0),
        end + Duration::from_seconds(1.0),
    ] {
        // Use the same TAI representation as the input row when checking the diagnostic.
        let (centuries, nanos) = epoch_to_parts(outside);
        let at = epoch_from_parts(centuries, nanos, TimeScaleCode::TAI.into());
        for (from, to) in FRAME_PAIRS {
            let input = batch(from, &[(epoch(2020), LengthUnit::m), (at, LengthUnit::m)]);
            let error = transform_batch(&input, to, &almanac, LengthUnit::m, None).unwrap_err();
            assert!(
                error.contains("no loaded ITRF93 BPC segment covers this epoch"),
                "{error}"
            );
            assert!(error.contains("row 1"), "{error}");
            assert!(error.contains(&at.to_string()), "{error}");
        }
    }
}
