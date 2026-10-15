//! Coordinate conversion for ordinary GPS tracking.
//!
//! Inputs are WGS84 geodetic latitude/longitude in degrees and **ellipsoidal height**
//! in metres. Height is required: mean sea level or a GPX elevation of unknown datum
//! must be resolved by the caller before using these functions.
//!
//! [`wgs84_to_ecef_m`] is pure ellipsoid math. [`wgs84_to_frame_m`] additionally maps
//! those coordinates approximately into ITRF93 and rotates into the requested Earth
//! frame at the observation epoch. This approximation is for ordinary tracking, not
//! survey/RTK work: it applies no WGS84-realization, station-motion, or epoch-dependent
//! datum correction and promises no sub-metre accuracy or fixed error bound.
//!
//! These helpers return positions, not measured attitudes or ledger-ready observations.
//! Use [`crate::ephemeris::epoch_to_parts`] with [`crate::TimeScaleCode::TAI`] when
//! encoding the same observation epoch into a row.

use anise::constants::celestial_objects::EARTH;
use anise::constants::frames::EARTH_ITRF93;
use anise::prelude::{Almanac, Epoch};
use nalgebra::Vector3;

use crate::ephemeris::resolve_astronomical_frame;
use crate::transforms::check_itrf93_coverage;

// WGS84 defining ellipsoid parameters, independent of loaded planetary constants.
// https://earth-info.nga.mil/index.php?action=wgs84&dir=wgs84
const WGS84_SEMI_MAJOR_M: f64 = 6_378_137.0;
const WGS84_FLATTENING: f64 = 1.0 / 298.257_223_563;

/// Convert WGS84 geodetic coordinates to Earth-centered, Earth-fixed `[x, y, z]` metres.
///
/// Latitude is north-positive in `[-90, 90]`; longitude is east-positive in
/// `[-180, 180]`. The X axis meets the equator at longitude zero, Y at 90° east,
/// and Z at the north pole. Height is relative to the WGS84 ellipsoid, and may be
/// negative. All inputs must be finite; no missing height, datum correction, or
/// longitude wrapping is inferred. This function requires no kernels.
///
/// The prime-vertical radius is `N = a / sqrt(1 - e² sin²(latitude))`, where
/// `e² = f(2-f)`. Cartesian coordinates then use `(N+h) cos(latitude)` in the
/// equatorial plane and `(N(1-e²)+h) sin(latitude)` along Z.
pub fn wgs84_to_ecef_m(
    latitude_deg: f64,
    longitude_deg: f64,
    ellipsoidal_height_m: f64,
) -> Result<[f64; 3], String> {
    if !latitude_deg.is_finite() || !(-90.0..=90.0).contains(&latitude_deg) {
        return Err(format!(
            "latitude_deg must be finite and within [-90, 90], got {latitude_deg}"
        ));
    }
    if !longitude_deg.is_finite() || !(-180.0..=180.0).contains(&longitude_deg) {
        return Err(format!(
            "longitude_deg must be finite and within [-180, 180], got {longitude_deg}"
        ));
    }
    if !ellipsoidal_height_m.is_finite() {
        return Err(format!(
            "ellipsoidal_height_m must be finite, got {ellipsoidal_height_m}"
        ));
    }

    let (sin_lat, cos_lat) = latitude_deg.to_radians().sin_cos();
    let (sin_lon, cos_lon) = longitude_deg.to_radians().sin_cos();
    let eccentricity_squared = WGS84_FLATTENING * (2.0 - WGS84_FLATTENING);
    let n = WGS84_SEMI_MAJOR_M / (1.0 - eccentricity_squared * sin_lat * sin_lat).sqrt();
    let equatorial_radius = (n + ellipsoidal_height_m) * cos_lat;
    Ok([
        equatorial_radius * cos_lon,
        equatorial_radius * sin_lon,
        (n * (1.0 - eccentricity_squared) + ellipsoidal_height_m) * sin_lat,
    ])
}

/// Convert a WGS84 position approximately into a catalogued Earth frame, in metres.
///
/// `"ITRF93"` uses WGS84 ECEF coordinates unchanged under the ordinary-tracking
/// approximation described in this module. It works with an empty [`Almanac`].
/// `"Earth"` / `"IAU_EARTH"` and `"GCRF"` / `"EME2000"` rotate those coordinates
/// from ITRF93 at **this observation's epoch**. They require an Earth BPC covering
/// the epoch; IAU_EARTH additionally requires planetary constants. An SPK is not
/// needed because the origin stays at Earth's centre.
///
/// Other origins are rejected. For origin-changing projections such as ICRF, use
/// [`crate::transforms::transform_batch`] after building observations in an Earth frame.
/// Missing/invalid kernel data returns an error containing the frame and epoch;
/// coordinates are never merely relabelled as IAU_EARTH or silently given another frame.
///
/// # Example
/// ```
/// use anise::prelude::Almanac;
/// use hifitime::Epoch;
/// use spacetimestamp::{epoch_to_parts, TimeScaleCode};
/// use spacetimestamp::gps::wgs84_to_frame_m;
///
/// let epoch: Epoch = "2025-01-01T00:00:00 UTC".parse()?;
/// let position_m = wgs84_to_frame_m(0.0, 0.0, 0.0, epoch, "ITRF93", &Almanac::default())?;
/// assert_eq!(position_m, [6_378_137.0, 0.0, 0.0]);
/// let (duration_centuries, duration_ns) = epoch_to_parts(epoch);
/// let timescale_id = TimeScaleCode::TAI; // these parts always use J2000 TAI
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn wgs84_to_frame_m(
    latitude_deg: f64,
    longitude_deg: f64,
    ellipsoidal_height_m: f64,
    epoch: Epoch,
    target_frame_name: &str,
    almanac: &Almanac,
) -> Result<[f64; 3], String> {
    let target = resolve_astronomical_frame(target_frame_name).ok_or_else(|| {
        format!(
            "Invalid target_frame_name '{target_frame_name}': not recognized by Soloc's astronomical frame catalog"
        )
    })?;
    if target.ephemeris_id != EARTH {
        return Err(format!(
            "GPS position conversion requires an Earth-centred frame, got '{target_frame_name}'; \
             use transform_batch for changes of origin"
        ));
    }
    let position = wgs84_to_ecef_m(latitude_deg, longitude_deg, ellipsoidal_height_m)?;
    if target == EARTH_ITRF93 {
        return Ok(position);
    }

    let context = |error| {
        format!("GPS rotation error at epoch {epoch} ('ITRF93' → '{target_frame_name}'): {error}")
    };
    check_itrf93_coverage(almanac, EARTH_ITRF93, target, epoch).map_err(&context)?;
    let rotation = almanac
        .rotate(EARTH_ITRF93, target, epoch)
        .map_err(|error| context(error.to_string()))?;
    // Same-origin rotation is unit-independent: keep metres at the GPS boundary.
    let rotated = rotation.rot_mat * Vector3::from(position);
    if !rotated.iter().all(|coordinate| coordinate.is_finite()) {
        return Err(context("rotation produced non-finite coordinates".into()));
    }
    Ok([rotated.x, rotated.y, rotated.z])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_position(actual: [f64; 3], expected: [f64; 3], tolerance_m: f64) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!(
                (actual - expected).abs() <= tolerance_m,
                "{actual} != {expected}"
            );
        }
    }

    #[test]
    fn axes_poles_and_antimeridian_use_metres_and_geodetic_latitude() {
        // Independent exact-axis values from the defining WGS84 ellipsoid.
        for (lat, lon, expected) in [
            (0.0, 0.0, [6_378_137.0, 0.0, 0.0]),
            (0.0, 90.0, [0.0, 6_378_137.0, 0.0]),
            (0.0, -90.0, [0.0, -6_378_137.0, 0.0]),
            (0.0, 180.0, [-6_378_137.0, 0.0, 0.0]),
            (0.0, -180.0, [-6_378_137.0, 0.0, 0.0]),
            (90.0, 47.0, [0.0, 0.0, 6_356_752.314_245_179]),
            (-90.0, -123.0, [0.0, 0.0, -6_356_752.314_245_179]),
        ] {
            assert_position(wgs84_to_ecef_m(lat, lon, 0.0).unwrap(), expected, 1e-8);
        }
    }

    #[test]
    fn agrees_with_iogp_geographic_to_geocentric_worked_example() {
        // IOGP Guidance Note 7-2 (2019), §4.1.1, p96, EPSG method 9602.
        // Published Cartesian values are rounded to the nearest millimetre.
        // https://www.iogp.org/wp-content/uploads/2019/09/373-07-02.pdf#page=96
        let latitude = 53.0 + 48.0 / 60.0 + 33.820 / 3600.0;
        let longitude = 2.0 + 7.0 / 60.0 + 46.380 / 3600.0;
        let expected = [3_771_793.968, 140_253.342, 5_124_304.349];
        assert_position(
            wgs84_to_ecef_m(latitude, longitude, 73.0).unwrap(),
            expected,
            0.001,
        );
        // Southern and western signs follow the same reference point reflected in Y/Z.
        assert_position(
            wgs84_to_ecef_m(-latitude, -longitude, 73.0).unwrap(),
            [expected[0], -expected[1], -expected[2]],
            0.001,
        );
    }

    #[test]
    fn explicit_positive_and_negative_ellipsoidal_heights() {
        assert_position(
            wgs84_to_ecef_m(0.0, 0.0, 1_250.0).unwrap(),
            [6_379_387.0, 0.0, 0.0],
            1e-8,
        );
        assert_position(
            wgs84_to_ecef_m(0.0, 90.0, -100.0).unwrap(),
            [0.0, 6_378_037.0, 0.0],
            1e-8,
        );
        assert_position(
            wgs84_to_ecef_m(-90.0, 0.0, -100.0).unwrap(),
            [0.0, 0.0, -6_356_652.314_245_179],
            1e-8,
        );
    }

    #[test]
    fn rejects_invalid_inputs_without_wrapping_or_defaulting() {
        for lat in [
            -90.000_001,
            90.000_001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert!(
                wgs84_to_ecef_m(lat, 0.0, 0.0)
                    .unwrap_err()
                    .contains("latitude_deg")
            );
        }
        for lon in [
            -180.000_001,
            180.000_001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert!(
                wgs84_to_ecef_m(0.0, lon, 0.0)
                    .unwrap_err()
                    .contains("longitude_deg")
            );
        }
        for height in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                wgs84_to_ecef_m(0.0, 0.0, height)
                    .unwrap_err()
                    .contains("ellipsoidal_height_m")
            );
        }
    }

    #[test]
    fn itrf93_approximation_needs_no_kernels_and_other_frames_never_fall_back() {
        let almanac = Almanac::default();
        let epoch = Epoch::from_gregorian_tai_at_midnight(2025, 1, 1);
        let convert = |target| wgs84_to_frame_m(0.0, 90.0, -100.0, epoch, target, &almanac);
        assert_position(convert("ITRF93").unwrap(), [0.0, 6_378_037.0, 0.0], 1e-8);
        assert!(
            convert("UNKNOWN")
                .unwrap_err()
                .contains("astronomical frame catalog")
        );
        for target in ["ICRF", "Mars"] {
            assert!(convert(target).unwrap_err().contains("Earth-centred frame"));
        }
        for target in ["Earth", "IAU_EARTH", "GCRF", "EME2000"] {
            let error = convert(target).unwrap_err();
            assert!(error.contains("no Earth orientation BPC data"), "{error}");
            assert!(error.contains(target), "{error}");
            assert!(error.contains(&epoch.to_string()), "{error}");
        }
    }
}
