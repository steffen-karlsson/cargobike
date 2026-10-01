//! Version schemes (F-4, F-72, F-95).
//!
//! `spec.version` is an opaque string; the application's `versioning`
//! block decides what it must look like and how versions order for
//! supersession (F-72). Validation per scheme:
//!
//! - [`VersionScheme::SemVer`] — the `semver` crate; scores replace
//!   zero-day; both semver and calver. prerelease ordering applies (F-27a
//!   gates read `semver(release.version).prerelease()`).
//! - [`VersionScheme::CalVer`] — a fixed layout of the tokens
//!   `YYYY`, `0Y`, `MM`, `0M`, `DD`, `0D`, `MICRO`; pure numeric ordering.
//! - [`VersionScheme::Opaque`] — printable, slash-free, match-empty-never;
//!   ordering falls back to creation time (F-72), so [`VersionScheme::order`]
//!   on opaque strings is lexicographic (a stable, deterministic fallback
//!   used when no timestamps exist in tests).

use std::cmp::Ordering;

use semver::Version as SemVerVersion;
use serde::{Deserialize, Serialize};

/// The application's versioning scheme (F-95).
///
/// The registry stores the `versioning` block (`scheme`, `tag_format`,
/// `require_tag`); this enum is the `scheme` discriminate of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "kebab-case")]
pub enum VersionScheme {
    /// Validation and ordering via the `semver` crate.
    Semver,
    /// CalVer with a configurable layout; see [`CALVER_TOKENS`].
    Calver {
        /// Layout, e.g. `"YYYY.MM.MICRO"`; defaults to `DEFAULT_CALVER_FORMAT`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calver_format: Option<String>,
    },
    /// No structure; ordering falls back to creation time (F-72).
    Opaque,
}

/// The default CalVer layout (F-95 suggests `YYYY.MM.MICRO`).
pub const DEFAULT_CALVER_FORMAT: &str = "YYYY.MM.MICRO";

/// Version validation errors (F-4).
#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    /// SemVer parse failed.
    #[error("failed to validate version `{version}`: invalid semver")]
    Semver {
        /// Candidate that failed to parse.
        version: String,
    },
    /// CalVer layout mismatch.
    #[error("failed to validate version `{version}`: mismatch against calver format `{format}`")]
    Calver {
        /// Candidate that failed.
        version: String,
        /// Layout that did not match.
        format: String,
    },
    /// Either scheme-restricted charset violation (non-opaque schemes allow
    /// a superset of semver/calver characters; opaque allows printable,
    /// slash-free).
    #[error("failed to validate version `{version}`: unsupported characters")]
    Charset {
        /// Candidate with unsupported characters.
        version: String,
    },
    /// The empty string is not a version anywhere.
    #[error("failed to parse version: version must not be empty")]
    Empty,
}

/// Layout tokens CalVer understands (F-95).
pub const CALVER_TOKENS: [&str; 7] = ["YYYY", "0Y", "MM", "0M", "DD", "0D", "MICRO"];

impl VersionScheme {
    /// The effective CalVer layout for this scheme.
    pub fn calver_format(&self) -> Option<&str> {
        match self {
            VersionScheme::Calver { calver_format } => {
                calver_format.as_deref().or(Some(DEFAULT_CALVER_FORMAT))
            }
            _ => None,
        }
    }

    /// Validates a candidate version against this scheme (F-4).
    ///
    /// - Semver: `semver::Version::parse` (build metadata and prerelease allowed).
    /// - Calver: every `.`-separated segment must match its layout token
    ///   exactly (`YYYY` ⇒ 4 digits, `0Y`/`0M`/`0D` ⇒ 2 digits, `MM`/`DD`
    ///   ⇒ 1 or 2 digits, `MICRO` ⇒ 1+ digits).
    /// - Opaque: non-empty, printable ASCII, no `/` (the version flows into
    ///   branch names and CR titles).
    pub fn validate(&self, version: &str) -> Result<(), VersionError> {
        if version.is_empty() {
            return Err(VersionError::Empty);
        }
        match self {
            VersionScheme::Semver => {
                if SemVerVersion::parse(version).is_ok() {
                    Ok(())
                } else {
                    Err(VersionError::Semver {
                        version: version.to_owned(),
                    })
                }
            }
            VersionScheme::Opaque => {
                if !version.bytes().all(is_printable_ascii) || version.contains('/') {
                    Err(VersionError::Charset {
                        version: version.to_owned(),
                    })
                } else {
                    Ok(())
                }
            }
            VersionScheme::Calver { .. } => {
                let format = self.calver_format().unwrap_or(DEFAULT_CALVER_FORMAT);
                calver_valid(version, format).ok_or_else(|| VersionError::Calver {
                    version: version.to_owned(),
                    format: format.to_owned(),
                })
            }
        }
    }

    /// Ordering used for supersession (F-72). `Ordering::Less` means the
    /// single argument comes first, i.e. `order(a, b)`.
    ///
    /// - Semver: the `semver` crate's total ordering (prerelease < release,
    ///   identifiers counted, build metadata ignored).
    /// - CalVer: numeric segment-by-segment comparison.
    /// - Opaque: lexicographic on bytes — used only as a deterministic
    ///   tie-break for demonstration; production code uses creation time
    ///   (F-72: opaque falls back to creation time).
    pub fn order(&self, a: &str, b: &str) -> Result<Ordering, VersionError> {
        self.validate(a)?;
        self.validate(b)?;
        match self {
            VersionScheme::Semver => {
                let av = SemVerVersion::parse(a).map_err(|_| VersionError::Semver {
                    version: a.to_owned(),
                })?;
                let bv = SemVerVersion::parse(b).map_err(|_| VersionError::Semver {
                    version: b.to_owned(),
                })?;
                Ok(av.cmp(&bv))
            }
            VersionScheme::Calver { .. } => Ok(calver_segments(a).cmp(&calver_segments(b))),
            VersionScheme::Opaque => Ok(a.cmp(b)),
        }
    }

    /// Whether the scheme supports a distinct prerelease component (F-27a):
    /// only `Calver` (and `Opaque`) gate users away from
    /// `semver(release.version).prerelease()`.
    pub fn has_prerelease_component(&self) -> bool {
        !matches!(self, VersionScheme::Semver)
    }
}

fn is_printable_ascii(c: u8) -> bool {
    (0x21..=0x7E).contains(&c)
}

fn segment_matches(token: &str, segment: &str) -> Option<u64> {
    let as_u64_opt = |s: &str| -> Option<u64> { s.parse::<u64>().ok() };
    match token {
        "YYYY" => (segment.len() == 4 && segment.bytes().all(|b| b.is_ascii_digit()))
            .then(|| as_u64_opt(segment).unwrap_or(0)),
        "0Y" | "0M" | "0D" => (segment.len() == 2 && segment.bytes().all(|b| b.is_ascii_digit()))
            .then(|| as_u64_opt(segment).unwrap_or(0)),
        "MM" | "DD" => (!segment.is_empty()
            && segment.len() <= 2
            && segment.bytes().all(|b| b.is_ascii_digit()))
        .then(|| as_u64_opt(segment).unwrap_or(0)),
        "MICRO" => (!segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()))
            .then(|| as_u64_opt(segment).unwrap_or(0)),
        _ => None,
    }
}

/// Validates candidate segments against the format tokens; every token must
/// match and candidate must have the same segment count.
fn calver_valid(version: &str, format: &str) -> Option<()> {
    let mut tokens = calver_tokens(format);
    let segments: Vec<&str> = version.split('.').collect();
    let tokens: Vec<&str> = tokens.by_ref().collect();
    if segments.len() != tokens.len() {
        return None;
    }
    for (token, segment) in std::iter::zip(&tokens, &segments) {
        segment_matches(token, segment)?;
    }
    Some(())
}

fn calver_tokens(fmt: &str) -> impl Iterator<Item = &'static str> {
    fn map_token(seg: &str) -> Option<&'static str> {
        CALVER_TOKENS.iter().copied().find(|t| *t == seg)
    }
    fmt.split('.').filter_map(map_token)
}

/// Numeric segments in the order they appear, for ordering.
fn calver_segments(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|seg| seg.parse::<u64>().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case::semver_ok(VersionScheme::Semver, "1.2.3", true)]
    #[case::semver_prerelease_ok(VersionScheme::Semver, "1.2.3-rc.1", true)]
    #[case::semver_build_metadata_ok(VersionScheme::Semver, "1.2.3+abc.1", true)]
    #[case::semver_rejects_calver_shape(VersionScheme::Semver, "2026.10.01", false)]
    #[case::semver_rejects_non_numeric(VersionScheme::Semver, "not-a-version", false)]
    #[case::opaque_ok(VersionScheme::Opaque, "2026.10.01", true)]
    #[case::opaque_ok_arbitrary(VersionScheme::Opaque, "nightly-20261001", true)]
    #[case::opaque_rejects_slash(VersionScheme::Opaque, "2026.10.01/2", false)]
    #[case::opaque_rejects_space(VersionScheme::Opaque, "1 2", false)]
    #[case::opaque_rejects_empty(VersionScheme::Opaque, "", false)]
    #[case::calver_ok(VersionScheme::Calver { calver_format: None }, "2026.10.1", true)]
    #[case::calver_rejects_wrong_layout(
        VersionScheme::Calver { calver_format: Some("YYYY.MM".to_owned()) },
        "2026.10.1",
        false,
    )]
    fn test_validation_per_scheme(
        #[case] scheme: VersionScheme,
        #[case] version: &str,
        #[case] valid: bool,
    ) {
        assert_eq!(scheme.validate(version).is_ok(), valid, "{version}");
    }

    #[rstest]
    #[case::semver_less(VersionScheme::Semver, "1.2.3", "1.2.4")]
    #[case::semver_prerelease_is_lower(VersionScheme::Semver, "1.2.3-rc.1", "1.2.3")]
    #[case::calver_less(
        VersionScheme::Calver { calver_format: Some("YYYY.MM.MICRO".to_owned()) },
        "2026.09.1",
        "2026.10.1",
    )]
    #[case::opaque_lexicographic(VersionScheme::Opaque, "1.2.3", "2.0.0")]
    fn test_ordering_follows_f72(#[case] scheme: VersionScheme, #[case] a: &str, #[case] b: &str) {
        assert_eq!(scheme.order(a, b).expect("both valid"), Ordering::Less);
    }

    #[test]
    fn test_prerelease_component_only_in_semver_gates_f27a() {
        assert_eq!(VersionScheme::Semver.has_prerelease_component(), false);
        assert_eq!(
            VersionScheme::Calver {
                calver_format: None
            }
            .has_prerelease_component(),
            true
        );
        assert_eq!(VersionScheme::Opaque.has_prerelease_component(), true);
    }
}
