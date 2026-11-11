//! Minimal SemVer 2.0.0 comparison for the updater downgrade guard.
//!
//! The updater plugin already refuses manifests whose version is not
//! greater than the running version, but the MSI path reads the raw
//! manifest and the frontend keeps a pending `Update` object across
//! re-checks, so both sides re-check here instead of trusting one
//! layer. Pure functions, no dependencies: `semver` is only a
//! transitive dependency of Tauri and adding it for two comparisons
//! would be heavier than the ~60 lines below.
//!
//! Rules implemented (SemVer 2.0.0 §11):
//! - core `MAJOR.MINOR.PATCH` compared numerically; a missing
//!   component counts as 0, so `0.18` == `0.18.0`.
//! - a leading `v` is stripped (`v0.18.13` == `0.18.13`).
//! - build metadata (`+...`) is ignored.
//! - a version WITH a pre-release is lower than the same version
//!   without one (`0.18.14-beta` < `0.18.14`).
//! - pre-release identifiers compare per spec: numeric identifiers
//!   numerically (and below alphanumeric ones), alphanumeric ones
//!   lexically, and a shorter list is lower when it is a prefix of
//!   the longer one.
//!
//! Anything that is not a number in the core (e.g. `0.18.x`) compares
//! as 0 for that component rather than erroring: a malformed manifest
//! version must fail closed in the caller (`is_newer_version` returns
//! false), never panic. The TypeScript twin of this logic lives in
//! `src/semver.ts` and the two test suites mirror each other — keep
//! them in sync.

// Windows-only in practice: the sole caller (`msi_update.rs`, the MSI
// downgrade guard) is `#[cfg(windows)]`, so on Linux every item here
// reads as dead code. Same `cfg_attr` pattern as `msi_file_name` in
// that file — one line, no behavior change.
#![cfg_attr(not(windows), allow(dead_code))]

use std::cmp::Ordering;

/// Compare two version strings. Returns `Ordering::Less` when `a` is
/// older than `b`, `Equal` when they denote the same release,
/// `Greater` when `a` is newer.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let pa = Parsed::parse(a);
    let pb = Parsed::parse(b);
    let max = pa.core.len().max(pb.core.len());
    for i in 0..max {
        let x = pa.core.get(i).copied().unwrap_or(0);
        let y = pb.core.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }
    match (&pa.pre, &pb.pre) {
        (None, None) => Ordering::Equal,
        // A release outranks its own pre-release.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_prerelease(x, y),
    }
}

/// True when `candidate` is strictly newer than `current`: equal
/// versions and pre-releases of the running version are NOT updates.
pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current) == Ordering::Greater
}

#[derive(Debug, PartialEq)]
enum Ident {
    Num(u64),
    Str(String),
}

#[derive(Debug)]
struct Parsed {
    core: Vec<u64>,
    pre: Option<Vec<Ident>>,
}

impl Parsed {
    fn parse(v: &str) -> Self {
        let v = v.trim().strip_prefix(['v', 'V']).unwrap_or(v.trim());
        let v = v.split('+').next().unwrap_or(v);
        let (core_part, pre_part) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let core = core_part
            .split('.')
            .map(|s| s.parse::<u64>().unwrap_or(0))
            .collect();
        let pre = pre_part.map(|p| {
            p.split('.')
                .map(|id| {
                    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                        Ident::Num(id.parse::<u64>().unwrap_or(u64::MAX))
                    } else {
                        Ident::Str(id.to_string())
                    }
                })
                .collect()
        });
        Parsed { core, pre }
    }
}

fn compare_prerelease(a: &[Ident], b: &[Ident]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x, y) {
            (Ident::Num(m), Ident::Num(n)) => m.cmp(n),
            // Numeric identifiers have lower precedence than alphanumeric.
            (Ident::Num(_), Ident::Str(_)) => Ordering::Less,
            (Ident::Str(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Str(m), Ident::Str(n)) => m.cmp(n),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_versions_are_not_newer() {
        assert_eq!(compare_versions("0.18.13", "0.18.13"), Ordering::Equal);
        assert!(!is_newer_version("0.18.13", "0.18.13"));
        assert_eq!(compare_versions("v0.18.13", "0.18.13"), Ordering::Equal);
        assert_eq!(compare_versions("0.18", "0.18.0"), Ordering::Equal);
    }

    #[test]
    fn newer_and_older() {
        assert!(is_newer_version("0.18.14", "0.18.13"));
        assert!(!is_newer_version("0.18.13", "0.18.14"));
        assert!(is_newer_version("0.19.0", "0.18.13"));
        assert!(is_newer_version("1.0.0", "0.18.13"));
        assert!(!is_newer_version("0.18.12", "0.18.13"));
    }

    #[test]
    fn downgrade_guard_never_offers_equal_or_older() {
        // The guard's whole job: <= current is never an update.
        for candidate in ["0.18.13", "0.18.12", "0.18.0", "0.17.99"] {
            assert!(
                !is_newer_version(candidate, "0.18.13"),
                "{candidate} must not count as newer than 0.18.13"
            );
        }
    }

    #[test]
    fn prerelease_is_older_than_its_release() {
        assert_eq!(
            compare_versions("0.18.14-beta", "0.18.14"),
            Ordering::Less
        );
        assert!(!is_newer_version("0.18.14-beta", "0.18.14"));
        // ...but a pre-release of a NEWER version still is one.
        assert!(is_newer_version("0.18.14-beta", "0.18.13"));
        assert!(!is_newer_version("0.18.13-beta", "0.18.13"));
    }

    #[test]
    fn prerelease_identifier_ordering() {
        // SemVer 2.0.0 §11 example ordering.
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert_eq!(
                compare_versions(pair[0], pair[1]),
                Ordering::Less,
                "{} should be older than {}",
                pair[0],
                pair[1]
            );
        }
        // Numeric identifiers compare numerically, not lexically.
        assert_eq!(
            compare_versions("1.0.0-beta.2", "1.0.0-beta.11"),
            Ordering::Less
        );
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(
            compare_versions("0.18.13+build.5", "0.18.13"),
            Ordering::Equal
        );
        assert!(!is_newer_version("0.18.13+build.5", "0.18.13"));
    }
}
