//! Source-preserving GPS sample types, explicit policies, and indexed diagnostics.
//!
//! Raw samples may contain missing or invalid values. They are not observations:
//! height/time resolution must succeed before later adapters can materialize STS.
//! Validated mathematical inputs live separately in [`crate::geodesy`]. This
//! module does not parse GPX, resolve timestamps, convert coordinates, or publish
//! Arrow/ledger data.
//!
//! ```
//! use spacetimestamp::gps::{RawGpsSample, SampleLocation, SourceDocument};
//! use spacetimestamp::geodesy::ResolvedGeodeticPosition;
//!
//! let document = SourceDocument::new(b"original receiver data".as_slice());
//! let mut raw = RawGpsSample::new(document.sample_key(SampleLocation::new(0, 0, 0)));
//! raw.latitude_deg = Some(0.0);
//! raw.longitude_deg = Some(180.0);
//! // Raw data stays unchanged when a separately resolved position is normalized.
//! let position = ResolvedGeodeticPosition::new(0.0, 180.0, 0.0)?;
//! assert_eq!(position.longitude_deg(), -180.0);
//! assert_eq!(raw.longitude_deg, Some(180.0));
//! assert!(raw.elevation_m.is_none());
//! assert!(raw.timestamp.is_none());
//! # Ok::<(), spacetimestamp::geodesy::CoordinateError>(())
//! ```

mod error;
mod model;
mod report;

pub use error::{GpsError, SampleError, SampleIssue};
pub use model::{
    FramePolicy, GeoidSeparation, GpsFix, GpsQuality, HeightPolicy, ImportMode, RawGpsSample,
    SampleLocation, SourceDocument, SourceSampleKey, SourceSha256, VerticalDatum,
};
pub use report::{
    ImportCounts, ImportReport, ImportWarning, SampleDisposition, SampleOutcome, SampleWarning,
    UnsupportedGeometry,
};
