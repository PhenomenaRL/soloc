use std::collections::HashSet;

use super::{GpsError, ImportMode, SampleIssue, SampleLocation, SourceSampleKey, SourceSha256};

/// Admission classification, not a statement that an observation was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleDisposition {
    Accepted,
    Rejected,
    Deferred,
}

/// A warning does not reject a sample. Timestamp-order warnings require a
/// playback break; they must not be repaired by inventing different epochs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleWarning {
    DuplicateEpoch,
    ReversedEpoch,
    UnprocessedExtension { namespace: String, name: String },
}

/// All issues for one selected point. No issues means eligible/accepted. Any
/// rejection takes precedence over deferrals, regardless of issue ordering;
/// retain every issue so an invalid coordinate cannot hide missing time or height.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleOutcome {
    pub sample: SourceSampleKey,
    pub issues: Vec<SampleIssue>,
    pub warnings: Vec<SampleWarning>,
}

impl SampleOutcome {
    pub fn disposition(&self) -> SampleDisposition {
        if self
            .issues
            .iter()
            .any(|issue| issue.disposition() == SampleDisposition::Rejected)
        {
            SampleDisposition::Rejected
        } else if self.issues.is_empty() {
            SampleDisposition::Accepted
        } else {
            SampleDisposition::Deferred
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub accepted: usize,
    pub rejected: usize,
    pub deferred: usize,
}

/// GPX geometry preserved in the original file but outside version-1 conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedGeometry {
    Route,
    Waypoint,
}

/// Document-level diagnostics, including content without a trackpoint location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportWarning {
    UnprocessedGeometry {
        kind: UnsupportedGeometry,
        count: usize,
    },
    UnprocessedExtension {
        namespace: String,
        name: String,
    },
}

/// In-memory report for one source and import mode. Outcomes retain insertion
/// order and reject duplicate keys or another document's samples.
///
/// Counts describe selected-point eligibility only: even a nonzero accepted
/// count does not imply publication, especially after a Strict failure. Later
/// importers must classify every selected point, enforce the mode, and handle
/// global failures before publishing. This type alone does none of that I/O.
#[derive(Debug, Clone)]
pub struct ImportReport {
    source_sha256: SourceSha256,
    mode: ImportMode,
    outcomes: Vec<SampleOutcome>,
    locations: HashSet<SampleLocation>,
    unselected_tracks: Vec<usize>,
    pub warnings: Vec<ImportWarning>,
}

impl ImportReport {
    pub fn new(source_sha256: SourceSha256, mode: ImportMode) -> Self {
        Self {
            source_sha256,
            mode,
            outcomes: Vec::new(),
            locations: HashSet::new(),
            unselected_tracks: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn source_sha256(&self) -> SourceSha256 {
        self.source_sha256
    }

    pub fn mode(&self) -> ImportMode {
        self.mode
    }

    pub fn outcomes(&self) -> &[SampleOutcome] {
        &self.outcomes
    }

    /// Tracks excluded by caller selection, in declaration order. These are not
    /// rejected or deferred samples and do not contribute to the counts.
    pub fn unselected_tracks(&self) -> &[usize] {
        &self.unselected_tracks
    }

    /// Records an excluded track once. A track with outcomes cannot be excluded.
    pub fn mark_track_unselected(&mut self, track_index: usize) -> Result<(), GpsError> {
        if self
            .outcomes
            .iter()
            .any(|outcome| outcome.sample.location.track_index == track_index)
        {
            return Err(GpsError::ConflictingTrackSelection { track_index });
        }
        if !self.unselected_tracks.contains(&track_index) {
            self.unselected_tracks.push(track_index);
        }
        Ok(())
    }

    /// Adds one classification. Failure leaves the report unchanged.
    pub fn record(&mut self, outcome: SampleOutcome) -> Result<(), GpsError> {
        if outcome.sample.source_sha256 != self.source_sha256 {
            return Err(GpsError::ReportSourceMismatch);
        }
        if self
            .unselected_tracks
            .contains(&outcome.sample.location.track_index)
        {
            return Err(GpsError::ConflictingTrackSelection {
                track_index: outcome.sample.location.track_index,
            });
        }
        if !self.locations.insert(outcome.sample.location) {
            return Err(GpsError::DuplicateReportSample {
                location: outcome.sample.location,
            });
        }
        self.outcomes.push(outcome);
        Ok(())
    }

    pub fn counts(&self) -> ImportCounts {
        let mut counts = ImportCounts::default();
        for outcome in &self.outcomes {
            match outcome.disposition() {
                SampleDisposition::Accepted => counts.accepted += 1,
                SampleDisposition::Rejected => counts.rejected += 1,
                SampleDisposition::Deferred => counts.deferred += 1,
            }
        }
        counts
    }
}
