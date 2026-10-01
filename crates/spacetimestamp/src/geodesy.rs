//! Validated WGS84 coordinates, independent of storage, identities, and frame mapping.
//!
//! Geographic coordinates use degrees and ellipsoidal metres (EPSG:4979 semantics).
//! Cartesian coordinates use Earth-centred, Earth-fixed metres (EPSG:4978 semantics):
//! X meets the equator at zero longitude, Y at 90 degrees east, and Z points north.
//! These types do not assign a soloc frame or perform coordinate conversion.

use std::{error::Error, fmt};

/// Lowest supported WGS84 ellipsoidal height, in metres, inclusive.
pub const MIN_ELLIPSOIDAL_HEIGHT_M: f64 = -10_000.0;
/// Highest supported WGS84 ellipsoidal height, in metres, inclusive.
pub const MAX_ELLIPSOIDAL_HEIGHT_M: f64 = 100_000_000.0;

/// The coordinate component that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateField {
    Latitude,
    Longitude,
    EllipsoidalHeight,
    EcefX,
    EcefY,
    EcefZ,
}

impl fmt::Display for CoordinateField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Latitude => "latitude (degrees)",
            Self::Longitude => "longitude (degrees)",
            Self::EllipsoidalHeight => "ellipsoidal height (metres)",
            Self::EcefX => "ECEF X (metres)",
            Self::EcefY => "ECEF Y (metres)",
            Self::EcefZ => "ECEF Z (metres)",
        })
    }
}

/// A coordinate fails the finite-value, supported-domain, or non-centre contract.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoordinateError {
    /// The input is NaN or infinite; `value` retains that input for diagnostics.
    NonFinite { field: CoordinateField, value: f64 },
    /// The input lies outside an inclusive interval, in the field's declared units.
    OutOfRange {
        field: CoordinateField,
        value: f64,
        min: f64,
        max: f64,
    },
    /// All three ECEF components are zero; geographic coordinates are undefined.
    EarthCentre,
}

impl fmt::Display for CoordinateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { field, value } => {
                write!(f, "{field} must be finite, got {value}")
            }
            Self::OutOfRange {
                field,
                value,
                min,
                max,
            } => write!(f, "{field} must be in [{min}, {max}], got {value}"),
            Self::EarthCentre => f.write_str("the Earth centre has no geographic coordinates"),
        }
    }
}

impl Error for CoordinateError {}

fn validate_finite(field: CoordinateField, value: f64) -> Result<(), CoordinateError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(CoordinateError::NonFinite { field, value })
    }
}

fn validate_range(
    field: CoordinateField,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), CoordinateError> {
    validate_finite(field, value)?;
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(CoordinateError::OutOfRange {
            field,
            value,
            min,
            max,
        })
    }
}

/// A finite WGS84 ellipsoidal height within the supported conversion domain.
///
/// A numeric value alone cannot establish an elevation's datum. Callers must
/// resolve that datum before using this type, including for explicit assumptions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EllipsoidalHeight {
    metres: f64,
}

impl EllipsoidalHeight {
    /// Validates an ellipsoidal height in the inclusive supported metre interval.
    pub fn new(metres: f64) -> Result<Self, CoordinateError> {
        validate_range(
            CoordinateField::EllipsoidalHeight,
            metres,
            MIN_ELLIPSOIDAL_HEIGHT_M,
            MAX_ELLIPSOIDAL_HEIGHT_M,
        )?;
        Ok(Self { metres })
    }

    /// Returns the ellipsoidal height in metres.
    pub fn metres(self) -> f64 {
        self.metres
    }
}

/// WGS84 geodetic coordinates whose height datum has already been resolved.
///
/// Latitude is north-positive in `[-90, 90]` degrees. Longitude is east-positive
/// in `[-180, 180)` degrees after canonicalization. Height is ellipsoidal, not
/// mean-sea-level or orthometric. An epoch is not needed for this representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedGeodeticPosition {
    latitude_deg: f64,
    longitude_deg: f64,
    ellipsoidal_height: EllipsoidalHeight,
}

impl ResolvedGeodeticPosition {
    /// Validates coordinates in degrees and metres, normalizing `+180` to `-180`.
    ///
    /// The mathematical API accepts both antimeridian endpoints. GPX parsing
    /// must separately enforce its stricter `[-180, 180)` input interval and
    /// preserve original source values before calling this constructor.
    pub fn new(
        latitude_deg: f64,
        longitude_deg: f64,
        ellipsoidal_height_m: f64,
    ) -> Result<Self, CoordinateError> {
        validate_range(CoordinateField::Latitude, latitude_deg, -90.0, 90.0)?;
        validate_range(CoordinateField::Longitude, longitude_deg, -180.0, 180.0)?;
        let ellipsoidal_height = EllipsoidalHeight::new(ellipsoidal_height_m)?;
        let longitude_deg = if longitude_deg == 180.0 {
            -180.0
        } else {
            longitude_deg
        };
        Ok(Self {
            latitude_deg,
            longitude_deg,
            ellipsoidal_height,
        })
    }

    /// Returns north-positive latitude in degrees.
    pub fn latitude_deg(self) -> f64 {
        self.latitude_deg
    }

    /// Returns east-positive longitude in degrees, with `+180` represented as `-180`.
    pub fn longitude_deg(self) -> f64 {
        self.longitude_deg
    }

    /// Returns WGS84 ellipsoidal height in metres.
    pub fn ellipsoidal_height_m(self) -> f64 {
        self.ellipsoidal_height.metres()
    }
}

/// A finite, non-centre WGS84 Earth-centred, Earth-fixed position in metres.
///
/// Construction does not prove that inverse conversion yields a height within
/// the supported domain. The inverse converter (T03) must check that separately.
/// No automatic equivalence to a soloc astronomical frame is implied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wgs84EcefPosition {
    x_m: f64,
    y_m: f64,
    z_m: f64,
}

impl Wgs84EcefPosition {
    /// Validates finite components and rejects the exact Earth centre.
    pub fn new(x_m: f64, y_m: f64, z_m: f64) -> Result<Self, CoordinateError> {
        validate_finite(CoordinateField::EcefX, x_m)?;
        validate_finite(CoordinateField::EcefY, y_m)?;
        validate_finite(CoordinateField::EcefZ, z_m)?;
        if x_m == 0.0 && y_m == 0.0 && z_m == 0.0 {
            return Err(CoordinateError::EarthCentre);
        }
        Ok(Self { x_m, y_m, z_m })
    }

    /// Returns the zero-longitude equatorial component in metres.
    pub fn x_m(self) -> f64 {
        self.x_m
    }

    /// Returns the 90-degree-east equatorial component in metres.
    pub fn y_m(self) -> f64 {
        self.y_m
    }

    /// Returns the north-pole component in metres.
    pub fn z_m(self) -> f64 {
        self.z_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geodetic_domain_includes_boundaries_and_canonicalizes_antimeridian() {
        for latitude in [-90.0, 0.0, 90.0] {
            for longitude in [-180.0, 0.0, 180.0] {
                for height in [MIN_ELLIPSOIDAL_HEIGHT_M, 0.0, MAX_ELLIPSOIDAL_HEIGHT_M] {
                    let position =
                        ResolvedGeodeticPosition::new(latitude, longitude, height).unwrap();
                    assert_eq!(position.latitude_deg(), latitude);
                    assert_eq!(position.ellipsoidal_height_m(), height);
                    assert_eq!(
                        position.longitude_deg(),
                        if longitude == 180.0 {
                            -180.0
                        } else {
                            longitude
                        }
                    );
                }
            }
        }
        let position = ResolvedGeodeticPosition::new(43.6532, -79.3832, 123.45).unwrap();
        assert_eq!(position.latitude_deg(), 43.6532);
        assert_eq!(position.longitude_deg(), -79.3832);
        assert_eq!(position.ellipsoidal_height_m(), 123.45);
    }

    #[test]
    fn out_of_range_coordinates_are_rejected_with_field_and_bounds() {
        for latitude in [(-90.0_f64).next_down(), 90.0_f64.next_up()] {
            assert_eq!(
                ResolvedGeodeticPosition::new(latitude, 0.0, 0.0),
                Err(CoordinateError::OutOfRange {
                    field: CoordinateField::Latitude,
                    value: latitude,
                    min: -90.0,
                    max: 90.0,
                })
            );
        }
        for longitude in [(-180.0_f64).next_down(), 180.0_f64.next_up()] {
            assert!(matches!(
                ResolvedGeodeticPosition::new(0.0, longitude, 0.0),
                Err(CoordinateError::OutOfRange {
                    field: CoordinateField::Longitude,
                    ..
                })
            ));
        }
        for height in [
            MIN_ELLIPSOIDAL_HEIGHT_M.next_down(),
            MAX_ELLIPSOIDAL_HEIGHT_M.next_up(),
        ] {
            let expected = CoordinateError::OutOfRange {
                field: CoordinateField::EllipsoidalHeight,
                value: height,
                min: MIN_ELLIPSOIDAL_HEIGHT_M,
                max: MAX_ELLIPSOIDAL_HEIGHT_M,
            };
            assert_eq!(EllipsoidalHeight::new(height), Err(expected));
            assert_eq!(
                ResolvedGeodeticPosition::new(0.0, 0.0, height),
                Err(expected)
            );
        }
    }

    #[test]
    fn every_component_rejects_nonfinite_values() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let cases = [
                (
                    CoordinateField::Latitude,
                    ResolvedGeodeticPosition::new(value, 0.0, 0.0).unwrap_err(),
                ),
                (
                    CoordinateField::Longitude,
                    ResolvedGeodeticPosition::new(0.0, value, 0.0).unwrap_err(),
                ),
                (
                    CoordinateField::EllipsoidalHeight,
                    ResolvedGeodeticPosition::new(0.0, 0.0, value).unwrap_err(),
                ),
                (
                    CoordinateField::EllipsoidalHeight,
                    EllipsoidalHeight::new(value).unwrap_err(),
                ),
                (
                    CoordinateField::EcefX,
                    Wgs84EcefPosition::new(value, 1.0, 1.0).unwrap_err(),
                ),
                (
                    CoordinateField::EcefY,
                    Wgs84EcefPosition::new(1.0, value, 1.0).unwrap_err(),
                ),
                (
                    CoordinateField::EcefZ,
                    Wgs84EcefPosition::new(1.0, 1.0, value).unwrap_err(),
                ),
            ];
            for (expected_field, error) in cases {
                let CoordinateError::NonFinite {
                    field,
                    value: retained,
                } = error
                else {
                    panic!("expected a nonfinite error, got {error:?}");
                };
                assert_eq!(field, expected_field);
                assert_eq!(retained.to_bits(), value.to_bits());
            }
        }
    }

    #[test]
    fn ecef_rejects_signed_zero_centre_without_rejecting_other_finite_positions() {
        for x in [0.0, -0.0] {
            for y in [0.0, -0.0] {
                for z in [0.0, -0.0] {
                    assert_eq!(
                        Wgs84EcefPosition::new(x, y, z),
                        Err(CoordinateError::EarthCentre)
                    );
                }
            }
        }
        let position = Wgs84EcefPosition::new(6_378_137.0, -2.5, 4.0).unwrap();
        assert_eq!(position.x_m(), 6_378_137.0);
        assert_eq!(position.y_m(), -2.5);
        assert_eq!(position.z_m(), 4.0);

        // These are finite non-centre points; supported inverse height is a
        // separate check, and squared-norm underflow/overflow must not decide it.
        assert!(Wgs84EcefPosition::new(f64::from_bits(1), 0.0, 0.0).is_ok());
        assert!(Wgs84EcefPosition::new(f64::MAX, f64::MAX, f64::MAX).is_ok());
    }

    #[test]
    fn coordinate_errors_support_diagnostics_without_string_parsing() {
        let error = EllipsoidalHeight::new(-10_001.0).unwrap_err();
        let error: &dyn Error = &error;
        assert!(error.to_string().contains("ellipsoidal height (metres)"));
        assert!(error.source().is_none());
    }
}
