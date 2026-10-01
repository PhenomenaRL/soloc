use std::error::Error;
use std::fmt;

use crate::geodesy::CoordinateError;

use super::{SampleDisposition, SampleLocation, SourceSampleKey};

/// Machine-readable reason a raw sample cannot yet become an observation.
/// Deferrals retain incomplete data; rejections identify invalid data. Parsing
/// and resolution in later tasks produce these issues, not this type definition.
#[derive(Debug, Clone, PartialEq)]
pub enum SampleIssue {
    Coordinate(CoordinateError),
    MissingLatitude,
    MissingLongitude,
    /// Invalid lexical/quality data not represented by a resolved coordinate.
    InvalidField {
        field: String,
        value: String,
    },
    /// GPX alone excludes +180, although the mathematical API normalizes it.
    InvalidGpxLongitude {
        value: f64,
    },
    MissingHeight,
    UnresolvedHeightDatum,
    MissingGeoidSeparation,
    IncompatibleGeoidSeparation,
    MissingEpoch,
    MissingTimezone,
    InvalidTimestamp {
        value: String,
    },
    UnsupportedEpoch {
        reason: String,
    },
    UnsupportedTimePrecision,
    NoValidFix,
}

impl SampleIssue {
    pub fn disposition(&self) -> SampleDisposition {
        match self {
            Self::MissingHeight
            | Self::UnresolvedHeightDatum
            | Self::MissingGeoidSeparation
            | Self::IncompatibleGeoidSeparation
            | Self::MissingEpoch
            | Self::MissingTimezone => SampleDisposition::Deferred,
            _ => SampleDisposition::Rejected,
        }
    }
}

impl fmt::Display for SampleIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordinate(error) => error.fmt(f),
            Self::MissingLatitude => f.write_str("missing latitude"),
            Self::MissingLongitude => f.write_str("missing longitude"),
            Self::InvalidField { field, value } => write!(f, "invalid {field}: {value:?}"),
            Self::InvalidGpxLongitude { value } => {
                write!(
                    f,
                    "GPX longitude {value} must be finite and in [-180, 180) degrees"
                )
            }
            Self::MissingHeight => f.write_str("missing elevation"),
            Self::UnresolvedHeightDatum => f.write_str("elevation datum is unresolved"),
            Self::MissingGeoidSeparation => f.write_str("missing geoid separation"),
            Self::IncompatibleGeoidSeparation => {
                f.write_str("geoid separation is incompatible with the elevation datum")
            }
            Self::MissingEpoch => f.write_str("missing timestamp"),
            Self::MissingTimezone => f.write_str("timestamp has no timezone"),
            Self::InvalidTimestamp { value } => write!(f, "invalid timestamp {value:?}"),
            Self::UnsupportedEpoch { reason } => write!(f, "unsupported epoch: {reason}"),
            Self::UnsupportedTimePrecision => f.write_str("unsupported sub-nanosecond precision"),
            Self::NoValidFix => f.write_str("receiver explicitly reports no valid fix"),
        }
    }
}

impl Error for SampleIssue {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coordinate(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CoordinateError> for SampleIssue {
    fn from(error: CoordinateError) -> Self {
        Self::Coordinate(error)
    }
}

/// A sample issue carrying the original document hash and all source indices.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleError {
    pub sample: SourceSampleKey,
    pub issue: SampleIssue,
}

impl SampleError {
    pub fn new(sample: SourceSampleKey, issue: impl Into<SampleIssue>) -> Self {
        Self {
            sample,
            issue: issue.into(),
        }
    }
}

impl fmt::Display for SampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.sample, self.issue)
    }
}

impl Error for SampleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.issue)
    }
}

/// Workflow errors that are global unless wrapped in `Sample`. Defining a
/// category does not implement GPX parsing, frame transforms, or persistence.
#[derive(Debug, Clone, PartialEq)]
pub enum GpsError {
    Sample(SampleError),
    MalformedGpx {
        message: String,
        byte_offset: Option<usize>,
    },
    UnsupportedGpxVersion {
        version: String,
    },
    NoTrackPoints,
    MissingObjectMapping {
        track_index: usize,
    },
    MissingFramePolicy,
    UnsupportedFramePolicy {
        policy: String,
    },
    /// Human-readable epoch/frame context; no ANISE type is required to report it.
    MissingKernelCoverage {
        frame: String,
        epoch: String,
    },
    IncompatibleOutputSchema {
        message: String,
    },
    ReportSourceMismatch,
    DuplicateReportSample {
        location: SampleLocation,
    },
    ConflictingTrackSelection {
        track_index: usize,
    },
}

impl fmt::Display for GpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sample(error) => error.fmt(f),
            Self::MalformedGpx {
                message,
                byte_offset,
            } => {
                write!(f, "malformed GPX")?;
                if let Some(offset) = byte_offset {
                    write!(f, " at byte {offset}")?;
                }
                write!(f, ": {message}")
            }
            Self::UnsupportedGpxVersion { version } => {
                write!(f, "unsupported GPX version {version:?}")
            }
            Self::NoTrackPoints => f.write_str("GPX contains no trackpoints"),
            Self::MissingObjectMapping { track_index } => {
                write!(f, "missing object mapping for track {track_index}")
            }
            Self::MissingFramePolicy => f.write_str("STS import requires an explicit frame policy"),
            Self::UnsupportedFramePolicy { policy } => {
                write!(f, "unsupported frame policy {policy:?}")
            }
            Self::MissingKernelCoverage { frame, epoch } => {
                write!(f, "missing kernel coverage for {frame} at {epoch}")
            }
            Self::IncompatibleOutputSchema { message } => {
                write!(f, "incompatible GPS output schema: {message}")
            }
            Self::ReportSourceMismatch => {
                f.write_str("sample source does not match the import report source")
            }
            Self::DuplicateReportSample { location } => {
                write!(f, "duplicate outcome for {location}")
            }
            Self::ConflictingTrackSelection { track_index } => {
                write!(
                    f,
                    "track {track_index} cannot have outcomes and be unselected"
                )
            }
        }
    }
}

impl Error for GpsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sample(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SampleError> for GpsError {
    fn from(error: SampleError) -> Self {
        Self::Sample(error)
    }
}
