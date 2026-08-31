//! The three closed vocabularies a spacetimestamp carries, stored as `UInt8` codes.
//!
//! [`LengthUnit`] governs `units_pos`, [`TimeScaleCode`] governs `timescale_id`, and
//! [`EstimateType`] governs `estimate_type`. Every other physical quantity in the workspace
//! is fixed SI and declares no unit at all.
//!
//! Code `0` is reserved and never valid in data, so a zero-filled column fails validation
//! loudly instead of silently meaning km / TAI / MEASURED. Codes are **append-only**: a new
//! member takes the next free code and existing codes never move, because renumbering
//! reinterprets every row already written.
//!
//! Each column carries its own decode table in field metadata (see [`vocabulary_field`]),
//! so a reader in any language recovers the strings with
//! `metadata["ARROW:extension:metadata"].split(",")[code]` and needs no hardcoded table.

use arrow::datatypes::{DataType, Field};
use hifitime::TimeScale;
use std::collections::HashMap;

use crate::identity::{ARROW_EXTENSION_KEY, ARROW_EXTENSION_METADATA_KEY};

/// A closed vocabulary stored as a `UInt8` code with its decode table in field metadata.
///
/// Implementors supply [`CANONICAL`](Self::CANONICAL) and [`ALL`](Self::ALL) in code order;
/// everything else is derived from them so the two can never disagree with the stored codes.
pub trait Vocabulary: Copy + 'static {
    /// The Arrow extension name identifying this vocabulary's column.
    const EXTENSION_NAME: &'static str;

    /// Canonical token per code. Index `0` is the reserved placeholder `-`.
    const CANONICAL: &'static [&'static str];

    /// Every member, in code order, so index `i` holds code `i + 1`.
    const ALL: &'static [Self];

    /// This member's stored code.
    fn code(self) -> u8;

    /// The highest code this vocabulary currently defines.
    fn max_code() -> u8 {
        Self::ALL.len() as u8
    }

    /// Decodes a stored code, rejecting the reserved `0` and anything out of range.
    fn from_code(code: u8) -> Result<Self, String> {
        Self::ALL
            .get((code as usize).wrapping_sub(1))
            .copied()
            .ok_or_else(|| {
                format!(
                    "invalid {} code {code}, expected 1..={}",
                    Self::EXTENSION_NAME,
                    Self::max_code()
                )
            })
    }

    /// This member's canonical token.
    fn as_str(self) -> &'static str {
        Self::CANONICAL[self.code() as usize]
    }

    /// The decode table as it is written into field metadata.
    fn vocabulary() -> String {
        Self::CANONICAL.join(",")
    }
}

/// Builds a non-nullable `UInt8` column named `name` carrying `V`'s extension name and
/// decode table.
pub fn vocabulary_field<V: Vocabulary>(name: &str) -> Field {
    Field::new(name, DataType::UInt8, false).with_metadata(HashMap::from([
        (
            ARROW_EXTENSION_KEY.to_string(),
            V::EXTENSION_NAME.to_string(),
        ),
        (ARROW_EXTENSION_METADATA_KEY.to_string(), V::vocabulary()),
    ]))
}

// ---------------------------------------------------------------------------
// Length units
// ---------------------------------------------------------------------------

/// The unit a row's `position` is expressed in.
///
/// Variants are spelled exactly as they are stored, so `in` needs the `r#in` raw form.
#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LengthUnit {
    km = 1,
    m = 2,
    cm = 3,
    mm = 4,
    au = 5,
    r#in = 6,
    ft = 7,
    mi = 8,
    nmi = 9,
}

impl Vocabulary for LengthUnit {
    const EXTENSION_NAME: &'static str = "soloc.length_unit";
    const CANONICAL: &'static [&'static str] =
        &["-", "km", "m", "cm", "mm", "au", "in", "ft", "mi", "nmi"];
    const ALL: &'static [Self] = &[
        Self::km,
        Self::m,
        Self::cm,
        Self::mm,
        Self::au,
        Self::r#in,
        Self::ft,
        Self::mi,
        Self::nmi,
    ];

    fn code(self) -> u8 {
        self as u8
    }
}

impl LengthUnit {
    /// `(numerator, denominator)` where one of this unit is `numerator / denominator` km.
    ///
    /// Both halves are integers exactly representable in f64, so dividing is correctly
    /// rounded where multiplying by the equivalent decimal is not: `0.001` has no exact
    /// binary form, `1.0 / 1000.0` does.
    const fn ratio(self) -> (f64, f64) {
        match self {
            Self::km => (1.0, 1.0),
            Self::m => (1.0, 1_000.0),
            Self::cm => (1.0, 100_000.0),
            Self::mm => (1.0, 1_000_000.0),
            // IAU 2012 defines the au as exactly 149 597 870 700 m.
            Self::au => (149_597_870_700.0, 1_000.0),
            Self::r#in => (254.0, 10_000_000.0),
            Self::ft => (3_048.0, 10_000_000.0),
            Self::mi => (1_609_344.0, 1_000_000.0),
            Self::nmi => (1_852.0, 1_000.0),
        }
    }

    /// Converts `v`, expressed in this unit, to kilometres.
    pub fn to_km(self, v: f64) -> f64 {
        let (n, d) = self.ratio();
        v * n / d
    }

    /// Converts `v`, expressed in kilometres, to this unit.
    pub fn from_km(self, v: f64) -> f64 {
        let (n, d) = self.ratio();
        v * d / n
    }

    /// Converts `v` between two units, returning it untouched when they are the same.
    ///
    /// The short-circuit is what makes a metres-in/metres-out reprojection bit-exact; going
    /// through km would round twice.
    pub fn convert(v: f64, from: Self, to: Self) -> f64 {
        if from == to {
            v
        } else {
            to.from_km(from.to_km(v))
        }
    }
}

// ---------------------------------------------------------------------------
// Timescales
// ---------------------------------------------------------------------------

/// The timescale a row's `(duration_centuries, duration_ns)` offset is measured on.
///
/// Members mirror [`hifitime::TimeScale`]'s declaration order, but the codes are ours: a
/// rename or reorder upstream cannot reinterpret stored data.
#[repr(u8)]
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeScaleCode {
    TAI = 1,
    TT = 2,
    ET = 3,
    TDB = 4,
    UTC = 5,
    GPST = 6,
    GST = 7,
    BDT = 8,
    QZSST = 9,
    TCG = 10,
    TCB = 11,
    TL = 12,
    TCL = 13,
}

impl Vocabulary for TimeScaleCode {
    const EXTENSION_NAME: &'static str = "soloc.timescale";
    const CANONICAL: &'static [&'static str] = &[
        "-", "TAI", "TT", "ET", "TDB", "UTC", "GPST", "GST", "BDT", "QZSST", "TCG", "TCB", "TL",
        "TCL",
    ];
    const ALL: &'static [Self] = &[
        Self::TAI,
        Self::TT,
        Self::ET,
        Self::TDB,
        Self::UTC,
        Self::GPST,
        Self::GST,
        Self::BDT,
        Self::QZSST,
        Self::TCG,
        Self::TCB,
        Self::TL,
        Self::TCL,
    ];

    fn code(self) -> u8 {
        self as u8
    }
}

impl From<TimeScaleCode> for TimeScale {
    fn from(code: TimeScaleCode) -> Self {
        match code {
            TimeScaleCode::TAI => TimeScale::TAI,
            TimeScaleCode::TT => TimeScale::TT,
            TimeScaleCode::ET => TimeScale::ET,
            TimeScaleCode::TDB => TimeScale::TDB,
            TimeScaleCode::UTC => TimeScale::UTC,
            TimeScaleCode::GPST => TimeScale::GPST,
            TimeScaleCode::GST => TimeScale::GST,
            TimeScaleCode::BDT => TimeScale::BDT,
            TimeScaleCode::QZSST => TimeScale::QZSST,
            TimeScaleCode::TCG => TimeScale::TCG,
            TimeScaleCode::TCB => TimeScale::TCB,
            TimeScaleCode::TL => TimeScale::TL,
            TimeScaleCode::TCL => TimeScale::TCL,
        }
    }
}

/// `TimeScale` is `#[non_exhaustive]`, so a variant added upstream has no code here yet.
impl TryFrom<TimeScale> for TimeScaleCode {
    type Error = String;

    fn try_from(ts: TimeScale) -> Result<Self, Self::Error> {
        Ok(match ts {
            TimeScale::TAI => Self::TAI,
            TimeScale::TT => Self::TT,
            TimeScale::ET => Self::ET,
            TimeScale::TDB => Self::TDB,
            TimeScale::UTC => Self::UTC,
            TimeScale::GPST => Self::GPST,
            TimeScale::GST => Self::GST,
            TimeScale::BDT => Self::BDT,
            TimeScale::QZSST => Self::QZSST,
            TimeScale::TCG => Self::TCG,
            TimeScale::TCB => Self::TCB,
            TimeScale::TL => Self::TL,
            TimeScale::TCL => Self::TCL,
            other => {
                return Err(format!(
                    "hifitime TimeScale {other:?} has no {} code",
                    Self::EXTENSION_NAME
                ));
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Estimate types
// ---------------------------------------------------------------------------

/// How a row was arrived at.
#[repr(u8)]
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EstimateType {
    MEASURED = 1,
    ESTIMATED = 2,
    SIMULATED = 3,
}

impl Vocabulary for EstimateType {
    const EXTENSION_NAME: &'static str = "soloc.estimate_type";
    const CANONICAL: &'static [&'static str] = &["-", "MEASURED", "ESTIMATED", "SIMULATED"];
    const ALL: &'static [Self] = &[Self::MEASURED, Self::ESTIMATED, Self::SIMULATED];

    fn code(self) -> u8 {
        self as u8
    }
}

impl EstimateType {
    /// Tie-break rank when two rows share a timestamp; lower wins.
    ///
    /// Deliberately not the stored code: coupling the two would force every future member to
    /// rank worst, since codes are append-only.
    pub fn priority(self) -> u8 {
        match self {
            Self::MEASURED => 0,
            Self::ESTIMATED => 1,
            Self::SIMULATED => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes are the stored wire format: a reorder must fail here, not silently reinterpret
    /// every row already written.
    #[test]
    fn frozen_codes() {
        for (unit, code) in [
            (LengthUnit::km, 1u8),
            (LengthUnit::m, 2),
            (LengthUnit::cm, 3),
            (LengthUnit::mm, 4),
            (LengthUnit::au, 5),
            (LengthUnit::r#in, 6),
            (LengthUnit::ft, 7),
            (LengthUnit::mi, 8),
            (LengthUnit::nmi, 9),
        ] {
            assert_eq!(unit.code(), code, "{unit:?}");
        }

        for (ts, code) in [
            (TimeScaleCode::TAI, 1u8),
            (TimeScaleCode::TT, 2),
            (TimeScaleCode::ET, 3),
            (TimeScaleCode::TDB, 4),
            (TimeScaleCode::UTC, 5),
            (TimeScaleCode::GPST, 6),
            (TimeScaleCode::GST, 7),
            (TimeScaleCode::BDT, 8),
            (TimeScaleCode::QZSST, 9),
            (TimeScaleCode::TCG, 10),
            (TimeScaleCode::TCB, 11),
            (TimeScaleCode::TL, 12),
            (TimeScaleCode::TCL, 13),
        ] {
            assert_eq!(ts.code(), code, "{ts:?}");
        }

        for (est, code) in [
            (EstimateType::MEASURED, 1u8),
            (EstimateType::ESTIMATED, 2),
            (EstimateType::SIMULATED, 3),
        ] {
            assert_eq!(est.code(), code, "{est:?}");
        }
    }

    /// The vocabulary strings are the cross-language decode table, so they are frozen too.
    #[test]
    fn frozen_vocabulary_strings() {
        assert_eq!(LengthUnit::vocabulary(), "-,km,m,cm,mm,au,in,ft,mi,nmi");
        assert_eq!(
            TimeScaleCode::vocabulary(),
            "-,TAI,TT,ET,TDB,UTC,GPST,GST,BDT,QZSST,TCG,TCB,TL,TCL"
        );
        assert_eq!(EstimateType::vocabulary(), "-,MEASURED,ESTIMATED,SIMULATED");
    }

    fn assert_round_trips<V: Vocabulary + PartialEq + std::fmt::Debug>() {
        assert_eq!(
            V::CANONICAL.len(),
            V::ALL.len() + 1,
            "CANONICAL must hold one token per member plus the reserved slot"
        );
        assert_eq!(V::CANONICAL[0], "-");

        for (i, &member) in V::ALL.iter().enumerate() {
            let code = i as u8 + 1;
            assert_eq!(member.code(), code);
            assert_eq!(V::from_code(code).unwrap(), member);
            assert_eq!(member.as_str(), V::CANONICAL[code as usize]);
        }
    }

    #[test]
    fn codes_and_tokens_agree() {
        assert_round_trips::<LengthUnit>();
        assert_round_trips::<TimeScaleCode>();
        assert_round_trips::<EstimateType>();
    }

    fn assert_rejects_reserved_and_out_of_range<V: Vocabulary + std::fmt::Debug>() {
        let zero = V::from_code(0).unwrap_err();
        assert!(zero.contains(V::EXTENSION_NAME), "{zero}");

        assert!(V::from_code(V::max_code() + 1).is_err());
        assert!(V::from_code(u8::MAX).is_err());
    }

    #[test]
    fn code_zero_and_out_of_range_are_rejected() {
        assert_rejects_reserved_and_out_of_range::<LengthUnit>();
        assert_rejects_reserved_and_out_of_range::<TimeScaleCode>();
        assert_rejects_reserved_and_out_of_range::<EstimateType>();
    }

    /// Each factor against the definition that fixes it.
    #[test]
    fn to_km_matches_the_defining_constants() {
        assert_eq!(LengthUnit::km.to_km(1.0), 1.0);
        assert_eq!(LengthUnit::m.to_km(1.0), 0.001);
        assert_eq!(LengthUnit::cm.to_km(1.0), 1e-5);
        assert_eq!(LengthUnit::mm.to_km(1.0), 1e-6);
        assert_eq!(LengthUnit::au.to_km(1.0), 149_597_870.7);
        assert_eq!(LengthUnit::r#in.to_km(1.0), 2.54e-5);
        assert_eq!(LengthUnit::ft.to_km(1.0), 3.048e-4);
        assert_eq!(LengthUnit::mi.to_km(1.0), 1.609344);
        assert_eq!(LengthUnit::nmi.to_km(1.0), 1.852);
    }

    #[test]
    fn from_km_inverts_to_km_on_the_exact_decimal_units() {
        assert_eq!(LengthUnit::m.from_km(1.0), 1000.0);
        assert_eq!(LengthUnit::cm.from_km(1.0), 100_000.0);
        assert_eq!(LengthUnit::mm.from_km(1.0), 1_000_000.0);
        assert_eq!(LengthUnit::km.from_km(42.0), 42.0);
    }

    /// The README's case: a millimetre measurement read as km is a 10⁶ error.
    #[test]
    fn millimetres_convert_to_kilometres() {
        // Not bit-exact: mm -> m pivots through km, so the value rounds twice.
        let mm_to_m = LengthUnit::convert(350.25, LengthUnit::mm, LengthUnit::m);
        assert!(
            (mm_to_m - 0.35025).abs() <= f64::EPSILON * 0.35025,
            "{mm_to_m}"
        );
        assert_eq!(
            LengthUnit::convert(1.0, LengthUnit::km, LengthUnit::mm),
            1e6
        );
    }

    /// Same unit in and out must return the input untouched, not a rounded round-trip.
    #[test]
    fn identity_conversion_is_bit_exact() {
        for &unit in LengthUnit::ALL {
            for v in [0.0, 1.0, -350.25, 1.0 / 3.0, 6.371e3, f64::MIN_POSITIVE] {
                let out = LengthUnit::convert(v, unit, unit);
                assert_eq!(out.to_bits(), v.to_bits(), "{unit:?} {v}");
            }
        }
    }

    #[test]
    fn imperial_units_match_their_si_definitions() {
        let approx = |a: f64, b: f64| assert!((a - b).abs() <= 1e-12 * b.abs(), "{a} vs {b}");
        approx(
            LengthUnit::convert(1.0, LengthUnit::r#in, LengthUnit::m),
            0.0254,
        );
        approx(
            LengthUnit::convert(1.0, LengthUnit::ft, LengthUnit::m),
            0.3048,
        );
        approx(
            LengthUnit::convert(1.0, LengthUnit::mi, LengthUnit::m),
            1609.344,
        );
        approx(
            LengthUnit::convert(1.0, LengthUnit::nmi, LengthUnit::m),
            1852.0,
        );
        approx(
            LengthUnit::convert(1.0, LengthUnit::au, LengthUnit::m),
            149_597_870_700.0,
        );
    }

    #[test]
    fn every_timescale_round_trips_through_hifitime() {
        for &code in TimeScaleCode::ALL {
            let ts: TimeScale = code.into();
            assert_eq!(TimeScaleCode::try_from(ts).unwrap(), code);
            assert_eq!(format!("{ts:?}"), code.as_str());
        }
    }

    #[test]
    fn estimate_priority_ranks_measured_first() {
        assert!(EstimateType::MEASURED.priority() < EstimateType::ESTIMATED.priority());
        assert!(EstimateType::ESTIMATED.priority() < EstimateType::SIMULATED.priority());
    }

    /// Mirrors `identity::tests::id_field_carries_the_uuid_extension`: assert the metadata
    /// directly rather than by rebuilding it with the constructor under test.
    #[test]
    fn vocabulary_field_carries_both_metadata_keys() {
        let f = vocabulary_field::<LengthUnit>("units_pos");

        assert_eq!(f.name(), "units_pos");
        assert_eq!(f.data_type(), &DataType::UInt8);
        assert!(!f.is_nullable());
        assert_eq!(
            f.metadata().get("ARROW:extension:name").map(String::as_str),
            Some("soloc.length_unit")
        );
        assert_eq!(
            f.metadata()
                .get("ARROW:extension:metadata")
                .map(String::as_str),
            Some("-,km,m,cm,mm,au,in,ft,mi,nmi")
        );
    }

    /// A reader in any language decodes with `split(",")[code]`.
    #[test]
    fn metadata_decodes_positionally() {
        let f = vocabulary_field::<TimeScaleCode>("timescale_id");
        let table = f.metadata().get("ARROW:extension:metadata").unwrap();
        let tokens: Vec<&str> = table.split(',').collect();

        for &code in TimeScaleCode::ALL {
            assert_eq!(tokens[code.code() as usize], code.as_str());
        }
        assert_eq!(tokens[0], "-");
    }
}
