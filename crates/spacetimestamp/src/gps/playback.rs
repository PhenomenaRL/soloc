//! Kernel-derived frame poses for an Earth-centred playback view.

use anise::constants::frames::EARTH_J2000;
use anise::constants::orientations::ITRF93;
use anise::naif::daf::NAIFSummaryRecord;
use anise::prelude::{Almanac, Epoch};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, TimeScale};
use nalgebra::UnitQuaternion;

use crate::ephemeris::{epoch_from_parts, epoch_to_parts};
use crate::schemas::entity::EntityBuilder;
use crate::{EstimateType, LengthUnit, PrescribedId, TimeScaleCode};

/// Pose the ITRF93 frame relative to GCRF at one physical epoch.
///
/// The returned entity batch contains one row: `entity_id = ITRF93`,
/// `frame_id = GCRF`, zero translation in metres, and the ANISE ITRF93→GCRF
/// rotation as a `[w,x,y,z]` quaternion. The row uses J2000 TAI,
/// `source_id = anise:almanac`, and `ESTIMATED`; optional state is unknown.
/// No SPK or planetary constants are needed, only an Earth BPC covering `epoch`.
/// Missing or unusable orientation data is an error, never an identity fallback.
///
/// These are additional display frame rows; observations remain in their stored
/// frame. A viewer can resolve `receiver → ITRF93 → GCRF`, with GCRF the explicit
/// Earth-centred inertial root. This does not place Earth in a solar-system scene.
/// Sample throughout the playback interval, including observation epochs, rather
/// than holding a single rotation for an entire route. Interpolation between
/// samples is a display approximation, not a replacement for a kernel query.
pub fn itrf93_frame_snapshot(almanac: &Almanac, epoch: Epoch) -> Result<RecordBatch, String> {
    let (centuries, nanos) = checked_epoch_parts(epoch)?;
    let rotation = super::itrf93_rotation(epoch, "GCRF", EARTH_J2000, almanac)?;
    let quaternion = UnitQuaternion::from_rotation_matrix(&rotation);
    let mut builder = EntityBuilder::new(1);
    builder.append_entity(
        PrescribedId::astronomical(399, 3000)?,
        PrescribedId::astronomical(399, 1)?,
        LengthUnit::m,
        TimeScaleCode::TAI,
        PrescribedId::abstract_source("anise", "almanac")?,
        EstimateType::ESTIMATED,
        [0.0; 3],
        [quaternion.w, quaternion.i, quaternion.j, quaternion.k],
        centuries,
        nanos,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    Ok(builder.flush())
}

/// Require continuous loaded ITRF93 BPC coverage over the closed interval.
///
/// Unlike an aggregate kernel date range, this checks the union of individual
/// segment intervals, including gaps between them. It also checks ANISE's actual
/// segment selection at boundaries and between boundaries: overlapping kernels
/// retain ANISE's load-order priority and are never reordered by this function.
/// Reversed intervals, unrepresentable epochs, missing BPC data and gaps fail.
/// A zero-length interval is valid when its epoch is covered.
///
/// This checks temporal coverage, not the accuracy or observational/predictive
/// status of the Earth model. Each subsequent [`itrf93_frame_snapshot`] still
/// evaluates the selected kernel and reports corrupt or unsupported data.
pub fn check_itrf93_interval(almanac: &Almanac, start: Epoch, end: Epoch) -> Result<(), String> {
    checked_epoch_parts(start)?;
    checked_epoch_parts(end)?;
    if start > end {
        return Err(format!(
            "ITRF93 playback interval is reversed: {start} to {end}"
        ));
    }
    let summaries = almanac.bpc_summaries(ITRF93).map_err(|error| {
        format!(
            "no Earth orientation BPC data for ITRF93 (3000) is loaded for \
             playback interval {start} to {end}: {error}"
        )
    })?;
    let mut intervals: Vec<_> = summaries
        .iter()
        .map(|summary| (summary.start_epoch(), summary.end_epoch()))
        .collect();
    intervals.sort_unstable_by_key(|&(first, last)| (first, last));
    require_coverage(&intervals, start, end)?;

    // A union alone would overlook a loader selecting an overlapping segment
    // differently from an interval-merging algorithm. Probe every partition on
    // which the set of covering summaries is constant, using the live almanac.
    let mut boundaries = vec![start, end];
    for &(first, last) in &intervals {
        for boundary in [first, last] {
            if boundary > start && boundary < end {
                boundaries.push(boundary);
            }
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let selected = |epoch| {
        almanac
            .bpc_summary_at_epoch(ITRF93, epoch)
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "ITRF93 BPC selection cannot cover playback epoch {epoch} \
                 in interval {start} to {end}: {error}"
                )
            })
    };
    for &boundary in &boundaries {
        selected(boundary)?;
    }
    for pair in boundaries.windows(2) {
        let first = pair[0].to_tai_duration();
        let last = pair[1].to_tai_duration();
        selected(Epoch::from_tai_duration(first + (last - first) / 2))?;
    }
    Ok(())
}

fn checked_epoch_parts(epoch: Epoch) -> Result<(i16, u64), String> {
    let tai = epoch.to_tai_duration();
    let parts = epoch_to_parts(epoch);
    if epoch.duration == Duration::MIN
        || epoch.duration == Duration::MAX
        || tai == Duration::MIN
        || tai == Duration::MAX
        || epoch_from_parts(parts.0, parts.1, TimeScale::TAI).to_tai_duration() != tai
    {
        return Err(format!(
            "ITRF93 playback epoch cannot be represented in J2000 TAI: {epoch}"
        ));
    }
    Ok(parts)
}

/// `intervals` must be sorted by start epoch; overlaps and touching endpoints are valid.
fn require_coverage(intervals: &[(Epoch, Epoch)], start: Epoch, end: Epoch) -> Result<(), String> {
    let mut covered_to = start;
    for &(first, last) in intervals {
        if first > last {
            return Err(format!(
                "invalid ITRF93 BPC segment interval: {first} to {last}"
            ));
        }
        if last < covered_to {
            continue;
        }
        if first > covered_to {
            break;
        }
        covered_to = covered_to.max(last);
        if covered_to >= end {
            return Ok(());
        }
    }
    Err(format!(
        "no continuous ITRF93 BPC coverage for playback interval {start} to {end}; \
         coverage is missing at or immediately after {covered_to}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(seconds: f64) -> Epoch {
        Epoch::from_tai_seconds(seconds)
    }

    #[test]
    fn interval_union_accepts_touching_nested_and_overlapping_segments() {
        let intervals = [(0.0, 5.0), (1.0, 2.0), (4.0, 8.0), (8.0, 10.0)]
            .map(|(first, last)| (epoch(first), epoch(last)));
        require_coverage(&intervals, epoch(0.0), epoch(10.0)).unwrap();
        require_coverage(&intervals, epoch(3.0), epoch(9.0)).unwrap();
        require_coverage(&intervals, epoch(8.0), epoch(8.0)).unwrap();
        require_coverage(&intervals, epoch(10.0), epoch(10.0)).unwrap();
    }

    #[test]
    fn interval_union_rejects_internal_gaps_and_uncovered_endpoints() {
        let intervals = [(0.0, 5.0), (6.0, 10.0)].map(|(first, last)| (epoch(first), epoch(last)));
        for (first, last) in [(-1.0, 0.0), (0.0, 10.0), (10.0, 11.0), (5.5, 5.5)] {
            let error = require_coverage(&intervals, epoch(first), epoch(last)).unwrap_err();
            assert!(
                error.contains("no continuous ITRF93 BPC coverage"),
                "{error}"
            );
        }
        assert!(require_coverage(&[], epoch(0.0), epoch(0.0)).is_err());
    }

    #[test]
    fn missing_kernels_and_reversed_intervals_are_explicit() {
        let almanac = Almanac::default();
        let at: Epoch = "2025-01-01T00:00:00 UTC".parse().unwrap();
        let error = itrf93_frame_snapshot(&almanac, at).unwrap_err();
        assert!(error.contains("no Earth orientation BPC data"), "{error}");
        assert!(error.contains(&at.to_string()), "{error}");
        let error = check_itrf93_interval(&almanac, at, at).unwrap_err();
        assert!(error.contains("no Earth orientation BPC data"), "{error}");
        let error =
            check_itrf93_interval(&almanac, at + Duration::from_seconds(1.0), at).unwrap_err();
        assert!(error.contains("interval is reversed"), "{error}");
    }

    #[test]
    fn unrepresentable_epochs_fail_before_kernel_lookup() {
        for duration in [Duration::MIN, Duration::MAX] {
            let at = Epoch::from_tai_duration(duration);
            let error = itrf93_frame_snapshot(&Almanac::default(), at).unwrap_err();
            assert!(error.contains("cannot be represented"), "{error}");
        }
    }
}
