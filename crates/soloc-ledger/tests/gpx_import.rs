//! Local GPX ingestion reports acceptance separately from history retention.
#![cfg(feature = "gpx")]

use std::sync::Arc;

use anise::prelude::Almanac;
use soloc_ledger::gpx::import_gpx_into_ledger;
use soloc_ledger::ledger::Ledger;
use spacetimestamp::gps::gpx::{
    GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxImportReport, GpxMapper, GpxMapping,
    GpxObjectId, GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::identity::{as_id_column, id_at};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::{EntitySchema, entity_schema};
use spacetimestamp::{LengthUnit, PrescribedId, TimeScaleCode};

const SOURCE: &str = "local-journey.gpx";
const EPOCH_NS: u64 = 788_961_637_000_000_000;

fn gps_options() -> GpsOptions<'static> {
    GpsOptions::new(
        PrescribedId::new("gpx-test", "tracked-vessel").unwrap(),
        PrescribedId::abstract_source("gpx-test", "receiver").unwrap(),
    )
}

fn mapper() -> GpxMapper<'static> {
    GpxMapper::new(
        SOURCE,
        [(
            GpxObjectId::Route(1),
            GpxMapping {
                gps: gps_options(),
                height: GpxHeightPolicy::EllipsoidalElevation,
                time: GpxTimePolicy::RequireTimestamp,
                orientation: GpsOrientation::LocalEnu,
            },
        )],
    )
    .unwrap()
}

fn route(count: usize, missing_time: Option<usize>) -> String {
    let mut xml = String::from(
        r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="local-test"><rte>"#,
    );
    for index in 0..count {
        xml.push_str(&format!(
            r#"<rtept lat="0" lon="{index}"><ele>{index}</ele>"#
        ));
        if missing_time != Some(index + 1) {
            xml.push_str(&format!("<time>2025-01-01T00:00:{index:02}Z</time>"));
        }
        xml.push_str("</rtept>");
    }
    xml.push_str("</rte></gpx>");
    xml
}

fn import(ledger: &mut Ledger, xml: &str, policy: GpxPointErrorPolicy) -> GpxImportReport {
    import_gpx_into_ledger(
        ledger,
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        GpxImportOptions::new(2, policy).unwrap(),
        |_| {},
    )
}

fn assert_accounting(report: &GpxImportReport) {
    assert_eq!(
        report.read,
        report.converted + report.skipped + report.failed
    );
    assert_eq!(
        report.converted,
        report.submitted + report.rejected + report.uncertain + report.unsubmitted
    );
}

#[test]
fn full_route_batches_preserve_identity_file_order_and_final_short_batch() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    ledger.set_memory_limit(None).unwrap();
    let xml = route(5, None);
    let mut batches = Vec::new();
    let report = import_gpx_into_ledger(
        &mut ledger,
        GpxReader::new(xml.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper(),
        &Almanac::default(),
        GpxImportOptions::new(2, GpxPointErrorPolicy::Strict).unwrap(),
        |event| {
            if let GpxImportEvent::Submitted(batch) = event {
                batches.push((batch.rows, batch.first.clone(), batch.last.clone()));
            }
        },
    );
    assert!(report.complete, "{report:?}");
    assert_eq!((report.read, report.converted, report.submitted), (5, 5, 5));
    assert_eq!(report.submitted_batches, 3);
    assert_eq!(
        batches.iter().map(|batch| batch.0).collect::<Vec<_>>(),
        [2, 2, 1]
    );
    for (batch, (first, last)) in batches.iter().zip([(1, 2), (3, 4), (5, 5)]) {
        assert_eq!(batch.1.source, SOURCE);
        assert_eq!(batch.1.object, Some(GpxObjectId::Route(1)));
        assert_eq!(batch.1.point, Some(first));
        assert_eq!(batch.2.point, Some(last));
    }
    assert!(report.failure.is_none());
    assert!(report.pending.is_none());
    assert_accounting(&report);

    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    assert_eq!(history.num_rows(), 5);
    let entities = as_id_column(history.column_by_name("entity_id").unwrap(), "entity_id").unwrap();
    let columns = StsColumns::try_new(&history).unwrap();
    for row in 0..5 {
        assert_eq!(id_at(entities, row).unwrap(), gps_options().entity_id);
        assert_eq!(columns.source_at(row).unwrap(), gps_options().source_id);
        assert_eq!(
            columns.frame_at(row).unwrap().astro_frame(),
            Some((399, 3000))
        );
        assert_eq!(columns.timescale_at(row).unwrap(), TimeScaleCode::TAI);
        assert_eq!(columns.units_at(row).unwrap(), LengthUnit::m);
        assert_eq!(
            columns.epoch_parts_at(row),
            (0, EPOCH_NS + row as u64 * 1_000_000_000)
        );
        let [x, y, z] = columns.position_at(row);
        let longitude = (row as f64).to_radians();
        let radius = 6_378_137.0 + row as f64;
        assert!((x - radius * longitude.cos()).abs() < 1e-8);
        assert!((y - radius * longitude.sin()).abs() < 1e-8);
        assert_eq!(z, 0.0);
    }

    let reloaded =
        Ledger::load_ipc_from_bytes(&ledger.save_ipc_to_bytes().unwrap(), "entity_id").unwrap();
    assert_eq!(
        reloaded.query(&SpatiotemporalFilter::new()).unwrap(),
        history
    );
}

#[test]
fn exact_batch_boundary_does_not_append_an_empty_batch() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    let report = import(&mut ledger, &route(4, None), GpxPointErrorPolicy::Strict);
    assert!(report.complete, "{report:?}");
    assert_eq!(report.submitted_batches, 2);
    assert_eq!(ledger.len(), 2);
    assert_eq!(report.submitted, 4);
    assert_accounting(&report);
}

#[test]
fn strict_failure_reports_accepted_prefix_and_buffered_unsubmitted_point() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    let report = import(&mut ledger, &route(5, Some(4)), GpxPointErrorPolicy::Strict);
    assert!(!report.complete);
    assert_eq!((report.read, report.converted, report.submitted), (4, 3, 2));
    assert_eq!(
        (report.failed, report.skipped, report.unsubmitted),
        (1, 0, 1)
    );
    assert_eq!((report.rejected, report.uncertain), (0, 0));
    assert_eq!(report.last_submitted.as_ref().unwrap().last.point, Some(2));
    assert_eq!(report.pending.as_ref().unwrap().first.point, Some(3));
    assert!(report.failure.is_some());
    assert_eq!(
        ledger
            .query(&SpatiotemporalFilter::new())
            .unwrap()
            .num_rows(),
        2
    );
    assert_accounting(&report);
}

#[test]
fn skipping_bad_point_preserves_remaining_observations_and_context() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    let report = import(
        &mut ledger,
        &route(5, Some(4)),
        GpxPointErrorPolicy::SkipInvalid,
    );
    assert!(report.complete, "{report:?}");
    assert_eq!((report.read, report.converted, report.submitted), (5, 4, 4));
    assert_eq!(
        (report.skipped, report.failed, report.unsubmitted),
        (1, 0, 0)
    );
    assert_eq!(report.last_skipped.as_ref().unwrap().context.point, Some(4));
    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    assert_eq!(history.num_rows(), 4);
    let columns = StsColumns::try_new(&history).unwrap();
    for (row, original) in [0, 1, 2, 4].into_iter().enumerate() {
        assert_eq!(
            columns.epoch_parts_at(row),
            (0, EPOCH_NS + original * 1_000_000_000)
        );
    }
    assert_accounting(&report);
}

#[test]
fn malformed_suffix_preserves_confirmed_batches_and_does_not_flush_pending_rows() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    let xml = route(3, None).replace("</rte></gpx>", "</gpx>");
    let report = import(&mut ledger, &xml, GpxPointErrorPolicy::SkipInvalid);
    assert!(!report.complete);
    assert_eq!((report.read, report.converted, report.submitted), (3, 3, 2));
    assert_eq!(
        (report.failed, report.skipped, report.unsubmitted),
        (0, 0, 1)
    );
    assert!(report.failure.is_some());
    assert_eq!(
        ledger
            .query(&SpatiotemporalFilter::new())
            .unwrap()
            .num_rows(),
        2
    );
    assert_accounting(&report);
}

#[test]
fn schema_rejection_is_definite_and_leaves_the_existing_ledger_unchanged() {
    let schema = Arc::new(entity_schema().project(&[0, 1]).unwrap());
    let mut ledger = Ledger::new(&schema, "entity_id").unwrap();
    let report = import(&mut ledger, &route(5, None), GpxPointErrorPolicy::Strict);
    assert!(!report.complete);
    assert_eq!((report.read, report.converted, report.submitted), (2, 2, 0));
    assert_eq!(
        (report.rejected, report.uncertain, report.unsubmitted),
        (2, 0, 0)
    );
    assert!(report.failure.is_some());
    assert!(ledger.is_empty());
    assert_accounting(&report);
}

#[test]
fn reimport_appends_duplicates_without_automatic_retry_or_deduplication() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    let xml = route(5, None);
    for _ in 0..2 {
        let report = import(&mut ledger, &xml, GpxPointErrorPolicy::Strict);
        assert!(report.complete, "{report:?}");
        assert_eq!(report.submitted, 5);
        assert_accounting(&report);
    }
    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    assert_eq!(history.num_rows(), 10);
    assert_eq!(history.slice(0, 5), history.slice(5, 5));
    assert_eq!(ledger.current_state(None, None).unwrap().num_rows(), 1);
}

#[test]
fn successful_submissions_do_not_guarantee_full_route_retention() {
    let mut ledger = Ledger::for_schema::<EntitySchema>().unwrap();
    ledger.set_memory_limit(Some(1)).unwrap();
    let report = import(&mut ledger, &route(5, None), GpxPointErrorPolicy::Strict);
    assert!(report.complete, "{report:?}");
    assert_eq!(report.submitted, 5);
    let history = ledger.query(&SpatiotemporalFilter::new()).unwrap();
    assert!(history.num_rows() < report.submitted);
    let latest = ledger.current_state(None, None).unwrap();
    assert_eq!(latest.num_rows(), 1);
    assert_eq!(
        StsColumns::try_new(&latest).unwrap().epoch_parts_at(0),
        (0, EPOCH_NS + 4_000_000_000)
    );
    // Removing the cap afterwards does not bring the evicted journey back.
    ledger.set_memory_limit(None).unwrap();
    assert_eq!(ledger.query(&SpatiotemporalFilter::new()).unwrap(), history);
    assert_accounting(&report);
}
