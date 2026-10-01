use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::geodesy::EllipsoidalHeight;

/// SHA-256 of the exact source bytes, independent of import options or filename.
pub type SourceSha256 = [u8; 32];

/// Immutable source bytes. Unknown XML fields and original lexical values remain
/// recoverable here even when they are not represented in a normalized sample.
/// Construction hashes bytes only; it does not validate a document or parse GPX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDocument {
    bytes: Arc<[u8]>,
    sha256: SourceSha256,
}

impl SourceDocument {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        let sha256 = Sha256::digest(bytes.as_ref()).into();
        Self { bytes, sha256 }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn sha256(&self) -> SourceSha256 {
        self.sha256
    }

    pub fn sample_key(&self, location: SampleLocation) -> SourceSampleKey {
        SourceSampleKey {
            source_sha256: self.sha256,
            location,
        }
    }
}

/// Zero-based location in the source, preserved independently of time sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SampleLocation {
    pub track_index: usize,
    pub segment_index: usize,
    pub point_index: usize,
}

impl SampleLocation {
    pub const fn new(track_index: usize, segment_index: usize, point_index: usize) -> Self {
        Self {
            track_index,
            segment_index,
            point_index,
        }
    }
}

impl fmt::Display for SampleLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "track {}, segment {}, point {} (zero-based)",
            self.track_index, self.segment_index, self.point_index
        )
    }
}

/// Identity of a source sample, not an entity or an import operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSampleKey {
    pub source_sha256: SourceSha256,
    pub location: SampleLocation,
}

impl fmt::Display for SourceSampleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("source sha256:")?;
        for byte in self.source_sha256 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ", {}", self.location)
    }
}

/// Declared meaning of the source elevation. An absent declaration stays unknown.
/// Evidence for a declaration is retained separately on [`RawGpsSample`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerticalDatum {
    #[default]
    Unknown,
    Wgs84Ellipsoidal,
    Orthometric,
}

/// Raw geoid separation and its own provenance. Presence of a separation does
/// not establish the elevation datum or prove that the two inputs are compatible.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoidSeparation {
    pub metres: f64,
    pub model: Option<String>,
    pub reference_datum: Option<String>,
    pub source: Option<String>,
}

/// GPX fix status. `None` in [`GpsQuality::fix`] means unknown; `NoFix` means an
/// explicit invalid fix and must prevent later observation materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpsFix {
    NoFix,
    TwoDimensional,
    ThreeDimensional,
    Differential,
    Pps,
}

/// Optional receiver quality. DOP values are metadata, not position covariances.
/// Like other raw fields, values are preserved here without admission validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpsQuality {
    pub fix: Option<GpsFix>,
    pub satellites: Option<u32>,
    pub hdop: Option<f64>,
    pub vdop: Option<f64>,
    pub pdop: Option<f64>,
    pub differential_age_s: Option<f64>,
    pub differential_station_id: Option<u16>,
}

/// Incomplete, unvalidated source data. Missing or invalid latitude/longitude is
/// representable so adapters can return indexed errors without losing the file.
/// Numeric fields retain original values; exact text remains in [`SourceDocument`].
/// No field implies a resolved epoch, frame, height, or observed attitude.
#[derive(Debug, Clone, PartialEq)]
pub struct RawGpsSample {
    pub key: SourceSampleKey,
    pub latitude_deg: Option<f64>,
    pub longitude_deg: Option<f64>,
    pub elevation_m: Option<f64>,
    pub timestamp: Option<String>,
    pub vertical_datum: VerticalDatum,
    pub datum_evidence: Option<String>,
    pub geoid_separation: Option<GeoidSeparation>,
    pub quality: GpsQuality,
}

impl RawGpsSample {
    /// Creates a source-linked sample with all measurements absent.
    pub fn new(key: SourceSampleKey) -> Self {
        Self {
            key,
            latitude_deg: None,
            longitude_deg: None,
            elevation_m: None,
            timestamp: None,
            vertical_datum: VerticalDatum::Unknown,
            datum_evidence: None,
            geoid_separation: None,
            quality: GpsQuality::default(),
        }
    }
}

/// Later importers must not publish Strict results if any selected point fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImportMode {
    #[default]
    Strict,
    Partial,
}

/// No height assumption is made by default. A fallback is already validated but
/// must only be applied to unresolved samples and marked estimated by the resolver.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum HeightPolicy {
    #[default]
    RequireResolved,
    AssumeEllipsoidalHeight(EllipsoidalHeight),
}

/// Explicit opt-in for later STS mapping. There is deliberately no `Default`:
/// WGS84 ECEF has no automatic soloc astronomical frame identity. This enum
/// describes a policy; it performs no mapping and implies no kernel availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePolicy {
    ApproximateIauEarth,
}
