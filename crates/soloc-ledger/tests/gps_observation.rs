//! Public GPS conversion through the standard entity ledger, without kernel downloads.

use anise::prelude::Almanac;
use arrow::array::{Array, StructArray};
use hifitime::{Duration, Epoch};
use soloc_ledger::ledger::Ledger;
use spacetimestamp::gps::{
    GpsObservation, GpsOptions, GpsOrientation, convert_gps_observation, gps_observation_to_batch,
};
use spacetimestamp::identity::{as_id_column, id_at};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::{EntityBuilder, EntitySchema, entity_schema};
use spacetimestamp::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode};

// 2025-01-01 00:00:00 UTC = 00:00:37 TAI, relative to J2000 TAI.
const EPOCH_NS: u64 = 788_961_637_000_000_000;

fn observation(longitude_deg: f64, epoch: Epoch) -> GpsObservation {
    GpsObservation {
        latitude_deg: 0.0,
        longitude_deg,
        ellipsoidal_height_m: 0.0,
        epoch,
        orientation: GpsOrientation::LocalEnu,
    }
}

fn utc_epoch() -> Epoch {
    Epoch::from_gregorian_utc(2025, 1, 1, 0, 0, 0, 0)
}

fn options() -> GpsOptions<'static> {
    GpsOptions::new(
        PrescribedId::new("gps-test", "tracked-vehicle").unwrap(),
        PrescribedId::abstract_source("gps-test", "receiver").unwrap(),
    )
}

#[test]
fn default_observation_is_a_complete_entity_row_with_unknown_state_left_null() {
    let options = options();
    let batch = gps_observation_to_batch(
        &observation(0.0, utc_epoch()),
        &options,
        &Almanac::default(),
    )
    .unwrap();
    assert_eq!(batch.schema(), entity_schema());
    assert_eq!(batch.num_rows(), 1);
    let entities = as_id_column(batch.column_by_name("entity_id").unwrap(), "entity_id").unwrap();
    assert_eq!(id_at(entities, 0).unwrap(), options.entity_id);
    let sts = StsColumns::try_new(&batch).unwrap();
    assert_eq!(sts.source_at(0).unwrap(), options.source_id);
    assert_eq!(sts.frame_at(0).unwrap().astro_frame(), Some((399, 3000)));
    assert_eq!(sts.units_at(0).unwrap(), LengthUnit::m);
    assert_eq!(sts.timescale_at(0).unwrap(), TimeScaleCode::TAI);
    assert_eq!(sts.estimate_at(0).unwrap(), EstimateType::ESTIMATED);
    assert_eq!(sts.position_at(0), [6_378_137.0, 0.0, 0.0]);
    assert_eq!(sts.epoch_parts_at(0), (0, EPOCH_NS));
    // At (0°, 0°), ENU maps x→Earth Y, y→Earth Z, z→Earth X.
    // Either quaternion sign represents this same cyclic-axis rotation.
    let quaternion = sts.quaternion_at(0);
    let sign = quaternion[0].signum();
    for component in quaternion {
        assert!((component * sign - 0.5).abs() < 1e-12);
    }

    for field in [
        "velocity",
        "angular_velocity",
        "acceleration",
        "mass_kg",
        "state_covariance",
        "dimensions",
    ] {
        assert!(batch.column_by_name(field).unwrap().is_null(0), "{field}");
    }
    let nested = batch
        .column_by_name("spacetimestamp")
        .unwrap()
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    for field in ["position_covariance", "orientation_covariance"] {
        assert!(nested.column_by_name(field).unwrap().is_null(0), "{field}");
    }
}

#[test]
fn successive_observations_preserve_identity_and_query_at_the_same_utc_or_tai_instant() {
    let options = options();
    let almanac = Almanac::default();
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    for (longitude, epoch) in [
        (0.0, utc_epoch()),
        (90.0, utc_epoch() + Duration::from_seconds(1.0)),
    ] {
        ledger
            .append(
                gps_observation_to_batch(&observation(longitude, epoch), &options, &almanac)
                    .unwrap(),
            )
            .unwrap();
    }
    assert_eq!(ledger.len(), 2);

    for instant in [
        utc_epoch(),
        Epoch::from_gregorian_tai(2025, 1, 1, 0, 0, 37, 0),
    ] {
        let first = ledger
            .query(&SpatiotemporalFilter::new().with_time_range(instant, instant))
            .unwrap();
        assert_eq!(first.num_rows(), 1);
        assert_eq!(
            StsColumns::try_new(&first).unwrap().position_at(0),
            [6_378_137.0, 0.0, 0.0]
        );

        let cutoff = instant + Duration::from_seconds(1.0);
        let latest = ledger
            .current_state(Some(&[options.entity_id]), Some(cutoff))
            .unwrap();
        assert_eq!(latest.num_rows(), 1);
        let sts = StsColumns::try_new(&latest).unwrap();
        let [x, y, z] = sts.position_at(0);
        assert!(x.abs() < 1e-8 && (y - 6_378_137.0).abs() < 1e-8 && z == 0.0);
        assert_eq!(sts.source_at(0).unwrap(), options.source_id);
        assert_eq!(sts.epoch_parts_at(0), (0, EPOCH_NS + 1_000_000_000));
        assert_eq!(
            ledger
                .current_state(
                    Some(&[options.entity_id]),
                    Some(cutoff + Duration::from_parts(0, 1))
                )
                .unwrap()
                .num_rows(),
            0
        );
    }
}

#[test]
fn converted_observations_can_share_an_existing_entity_builder() {
    let mut options = options();
    // A physical receiver is also valid provenance, distinct from the tracked entity.
    options.source_id = PrescribedId::new("gps-test", "physical-receiver").unwrap();
    options.estimate_type = EstimateType::MEASURED;
    let almanac = Almanac::default();
    let mut builder = EntityBuilder::new(2);
    for second in 0..2 {
        let mut fix = observation(0.0, utc_epoch() + Duration::from_seconds(second as f64));
        fix.orientation = GpsOrientation::QuaternionInEnu([1.0, 0.0, 0.0, 0.0]);
        let converted = convert_gps_observation(&fix, &options, &almanac).unwrap();
        assert_eq!(converted.entity_id(), options.entity_id);
        assert_eq!(converted.source_id(), options.source_id);
        assert_eq!(converted.frame_id().astro_frame(), Some((399, 3000)));
        assert_eq!(converted.estimate_type(), EstimateType::MEASURED);
        assert_eq!(converted.position_m(), [6_378_137.0, 0.0, 0.0]);
        assert_eq!(
            converted.epoch_parts(),
            (0, EPOCH_NS + second * 1_000_000_000)
        );
        let one = converted.to_entity_batch();
        assert_eq!(
            StsColumns::try_new(&one).unwrap().quaternion_at(0),
            converted.quaternion_wxyz()
        );
        converted.append_to(&mut builder);
    }
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    ledger.append(builder.flush()).unwrap();
    assert_eq!(
        ledger
            .query(&SpatiotemporalFilter::new())
            .unwrap()
            .num_rows(),
        2
    );
    assert_eq!(ledger.current_state(None, None).unwrap().num_rows(), 1);
}

#[test]
fn conversion_and_append_failures_leave_existing_ledger_observations_unchanged() {
    let almanac = Almanac::default();
    let valid =
        gps_observation_to_batch(&observation(0.0, utc_epoch()), &options(), &almanac).unwrap();
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    ledger.append(valid.clone()).unwrap();

    for case in ["coordinate", "entity", "source", "orientation", "kernel"] {
        let mut fix = observation(0.0, utc_epoch());
        let mut options = options();
        match case {
            "coordinate" => fix.latitude_deg = 91.0,
            "entity" => options.entity_id = PrescribedId::astronomical_from_name("Earth").unwrap(),
            "source" => options.source_id = PrescribedId::astronomical_from_name("Earth").unwrap(),
            "orientation" => fix.orientation = GpsOrientation::QuaternionInEnu([0.0; 4]),
            "kernel" => options.target_frame = "GCRF",
            _ => unreachable!(),
        }
        let result = gps_observation_to_batch(&fix, &options, &almanac)
            .map_err(|error| error.to_string())
            .and_then(|batch| ledger.append(batch));
        assert!(result.is_err(), "{case} must fail before append");
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.current_state(None, None).unwrap(), valid);
    }

    // The complete GPS batch must fail cleanly if a caller chose a different ledger schema.
    let custom_row = valid.project(&[0, 1]).unwrap();
    let mut custom_ledger = Ledger::new(&custom_row.schema(), "entity_id").unwrap();
    custom_ledger.append(custom_row.clone()).unwrap();
    assert!(
        custom_ledger
            .append(valid.clone())
            .unwrap_err()
            .contains("does not match the ledger schema")
    );
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger.current_state(None, None).unwrap(), valid);
    assert_eq!(custom_ledger.len(), 1);
    assert_eq!(custom_ledger.current_state(None, None).unwrap(), custom_row);
}
