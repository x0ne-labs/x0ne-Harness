//! The `x0ne-manifest` custom section.
//!
//! The manifest is a **claim**, not a proof: the compiler is
//! outside the TCB, so potentially lying. The chain holds because a
//! lying manifest can only claim *patterns*, those patterns all
//! go through `PolicyEngine::evaluate` path by path, and the host
//! constrains on **that same** manifest — extracted from the **same bytes**
//! of the artifact, with no disk re-read between the two (invariant I-B).

use crate::ir::{RegexLit, Template};
use crate::limits::{
    MANIFEST_VERSION, MAX_MANIFEST_ENTRIES, MAX_MANIFEST_REGEXES, MAX_MANIFEST_TEMPLATES,
};
use serde::{Deserialize, Serialize};

/// The name of the custom section.
pub const SECTION_NAME: &str = "x0ne-manifest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Read,
    ReadWrite,
}

impl Capability {
    pub fn may_write(&self) -> bool {
        matches!(self, Capability::ReadWrite)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// **Absolute canonical** pattern, `globset` style.
    pub pattern: String,
    /// SECURITY: per entry. See `each_manifest_entry_carries_its_own_capability`.
    pub capability: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u16,
    pub entries: Vec<ManifestEntry>,
    pub regexes: Vec<RegexLit>,
    pub templates: Vec<Template>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Version {
        found: u16,
        expected: u16,
    },
    RelativePattern {
        pattern: String,
    },
    TooManyEntries {
        found: usize,
        max: usize,
    },
    /// SECURITY: the **number** of patterns, not only their length. See
    /// `limits::MAX_MANIFEST_REGEXES`.
    TooManyRegexes {
        found: usize,
        max: usize,
    },
    TooManyTemplates {
        found: usize,
        max: usize,
    },
    Malformed {
        reason: String,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Version { found, expected } => write!(
                f,
                "manifest version {found}, this build reads version {expected} only"
            ),
            ManifestError::RelativePattern { pattern } => write!(
                f,
                "manifest pattern `{pattern}` is relative; manifests carry absolute canonical paths only"
            ),
            ManifestError::TooManyEntries { found, max } => {
                write!(f, "manifest declares {found} patterns, the limit is {max}")
            }
            ManifestError::TooManyRegexes { found, max } => {
                write!(f, "manifest carries {found} regex patterns, the limit is {max}")
            }
            ManifestError::TooManyTemplates { found, max } => {
                write!(f, "manifest carries {found} replacement templates, the limit is {max}")
            }
            ManifestError::Malformed { reason } => write!(f, "manifest is malformed: {reason}"),
        }
    }
}

/// Encodes after validation. This is the normal path.
pub fn encode(m: &Manifest) -> Result<Vec<u8>, ManifestError> {
    validate(m)?;
    Ok(encode_unchecked(m))
}

/// Encodes without validating. **Reserved for tests** that must fabricate a
/// lying manifest to check that the decoder refuses it.
pub fn encode_unchecked(m: &Manifest) -> Vec<u8> {
    postcard::to_allocvec(m).expect("the manifest is serializable")
}

pub fn decode(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    let (m, rest): (Manifest, &[u8]) =
        postcard::take_from_bytes(bytes).map_err(|e| ManifestError::Malformed {
            reason: e.to_string(),
        })?;
    // SECURITY: nothing trails after. `take_from_bytes` stops at the first
    // complete manifest and returns the rest **without complaining** about
    // it; a custom section could therefore carry two claims of which
    // only one would be read. Verifier and host read the same bytes
    // (invariant I-B) — there must still be only one thing to read in them.
    if !rest.is_empty() {
        return Err(ManifestError::Malformed {
            reason: format!("{} trailing bytes after the manifest", rest.len()),
        });
    }
    validate(&m)?;
    Ok(m)
}

fn validate(m: &Manifest) -> Result<(), ManifestError> {
    if m.version != MANIFEST_VERSION {
        return Err(ManifestError::Version {
            found: m.version,
            expected: MANIFEST_VERSION,
        });
    }
    if m.entries.len() > MAX_MANIFEST_ENTRIES {
        return Err(ManifestError::TooManyEntries {
            found: m.entries.len(),
            max: MAX_MANIFEST_ENTRIES,
        });
    }
    // SECURITY: the number of patterns and templates. A forged manifest does not go
    // through the parser: bounding a pattern's length without bounding how many
    // there are bounds nothing at all (400 × 512 characters fit in 206 KB).
    if m.regexes.len() > MAX_MANIFEST_REGEXES {
        return Err(ManifestError::TooManyRegexes {
            found: m.regexes.len(),
            max: MAX_MANIFEST_REGEXES,
        });
    }
    if m.templates.len() > MAX_MANIFEST_TEMPLATES {
        return Err(ManifestError::TooManyTemplates {
            found: m.templates.len(),
            max: MAX_MANIFEST_TEMPLATES,
        });
    }
    for e in &m.entries {
        // SECURITY: absolute, and without `..`. A relative pattern would depend on the
        // current directory of whoever reads it, so would say two different things
        // to the verifier and to the host.
        if !e.pattern.starts_with('/') || e.pattern.split('/').any(|s| s == "..") {
            return Err(ManifestError::RelativePattern {
                pattern: e.pattern.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{RegexLit, Template, TemplatePart};

    /// SECURITY: the capability is **per entry**, never global.
    ///
    /// The hole closed by construction: a program that reads `src/**` and writes
    /// `out/**` must not be able to write into `src/**` because "the path
    /// is under some prefix of the manifest". Each file id
    /// retains the entry that produced it, and it is **that** entry the host
    /// queries (T5).
    #[test]
    fn each_manifest_entry_carries_its_own_capability() {
        let m = Manifest {
            version: crate::limits::MANIFEST_VERSION,
            entries: vec![
                ManifestEntry {
                    pattern: "/w/src/**/*.rs".into(),
                    capability: Capability::Read,
                },
                ManifestEntry {
                    pattern: "/w/out/**/*.rs".into(),
                    capability: Capability::ReadWrite,
                },
            ],
            regexes: vec![RegexLit {
                pattern: "a".into(),
                case_insensitive: false,
            }],
            templates: vec![Template {
                parts: vec![TemplatePart::Literal("b".into())],
            }],
        };
        assert!(!m.entries[0].capability.may_write());
        assert!(m.entries[1].capability.may_write());
        let bytes = encode(&m).expect("encoding");
        assert_eq!(decode(&bytes).expect("decoding"), m);
    }

    /// A manifest of another version is refused, not interpreted on a best-effort basis.
    /// Verifier and host must read the **same** bytes the **same**
    /// way (invariant I-B): a lenient decoder would make them diverge.
    #[test]
    fn a_manifest_of_another_version_is_refused() {
        let m = Manifest {
            version: crate::limits::MANIFEST_VERSION + 1,
            entries: vec![],
            regexes: vec![],
            templates: vec![],
        };
        let bytes = encode_unchecked(&m);
        assert!(matches!(decode(&bytes), Err(ManifestError::Version { .. })));
        // And the normal path does not let it out either: a manifest
        // that could not be read back has no reason to be written.
        assert!(matches!(encode(&m), Err(ManifestError::Version { .. })));
    }

    /// A relative path in the manifest would be a claim that depends
    /// on the current directory of whoever reads it — hence two different
    /// claims for the verifier and for the host.
    #[test]
    fn a_relative_pattern_is_refused_at_decode() {
        let m = Manifest {
            version: crate::limits::MANIFEST_VERSION,
            entries: vec![ManifestEntry {
                pattern: "src/**/*.rs".into(),
                capability: Capability::Read,
            }],
            regexes: vec![],
            templates: vec![],
        };
        let bytes = encode_unchecked(&m);
        assert!(matches!(
            decode(&bytes),
            Err(ManifestError::RelativePattern { .. })
        ));
    }

    /// SECURITY: an absolute pattern that **climbs up** is refused at decode
    /// too. `/w/tests/../../etc/**` does start with `/`, and a decoder that
    /// only tested the first character would let it through to
    /// expansion — where it would designate `/etc`. The policy would catch it,
    /// but a claim that has to be caught is a claim that
    /// should not have been accepted.
    #[test]
    fn an_absolute_pattern_that_climbs_out_is_refused_at_decode() {
        for pattern in [
            "/w/tests/../../etc/**",
            "/..",
            "/w/../etc/shadow",
            "/w/**/../*.rs",
        ] {
            let m = Manifest {
                version: crate::limits::MANIFEST_VERSION,
                entries: vec![ManifestEntry {
                    pattern: pattern.into(),
                    capability: Capability::Read,
                }],
                regexes: vec![],
                templates: vec![],
            };
            let bytes = encode_unchecked(&m);
            assert!(
                matches!(decode(&bytes), Err(ManifestError::RelativePattern { .. })),
                "`{pattern}` climbs up and must be refused"
            );
        }
    }

    /// Beyond `MAX_MANIFEST_ENTRIES` the manifest is refused. The bound
    /// exists because each entry costs the verifier a disk walk,
    /// and a manifest with a thousand patterns would turn the proof
    /// into a sweep.
    #[test]
    fn a_manifest_beyond_the_entry_limit_is_refused() {
        let entry = ManifestEntry {
            pattern: "/w/a/*.rs".into(),
            capability: Capability::Read,
        };
        let m = Manifest {
            version: crate::limits::MANIFEST_VERSION,
            entries: vec![entry; crate::limits::MAX_MANIFEST_ENTRIES + 1],
            regexes: vec![],
            templates: vec![],
        };
        let bytes = encode_unchecked(&m);
        assert!(matches!(
            decode(&bytes),
            Err(ManifestError::TooManyEntries { .. })
        ));
    }

    /// SECURITY: the **number** of patterns is bounded, not only their length.
    ///
    /// Measured on 2026-08-15: 400 patterns of 512 characters fit in a
    /// 206 KB module, hence under `MAX_MODULE_BYTES`, and passed all the
    /// checks. The host would have compiled them — `REGEX_SIZE_LIMIT` is a budget
    /// **per pattern**. Bounding the length without bounding the count bounds nothing.
    #[test]
    fn a_manifest_carrying_too_many_regexes_or_templates_is_refused() {
        let base = Manifest {
            version: crate::limits::MANIFEST_VERSION,
            entries: vec![],
            regexes: vec![],
            templates: vec![],
        };

        let mut many = base.clone();
        many.regexes = vec![
            RegexLit {
                pattern: "a".into(),
                case_insensitive: false,
            };
            crate::limits::MAX_MANIFEST_REGEXES + 1
        ];
        assert!(matches!(
            decode(&encode_unchecked(&many)),
            Err(ManifestError::TooManyRegexes { .. })
        ));

        let mut many = base.clone();
        many.templates = vec![
            Template {
                parts: vec![TemplatePart::Group(1)],
            };
            crate::limits::MAX_MANIFEST_TEMPLATES + 1
        ];
        assert!(matches!(
            decode(&encode_unchecked(&many)),
            Err(ManifestError::TooManyTemplates { .. })
        ));

        // And the legal count passes: the bound refuses what exceeds it.
        let mut exact = base;
        exact.regexes = vec![
            RegexLit {
                pattern: "a".into(),
                case_insensitive: false,
            };
            crate::limits::MAX_MANIFEST_REGEXES
        ];
        assert!(decode(&encode_unchecked(&exact)).is_ok());
    }

    /// Bytes that are not a manifest do not become an empty
    /// manifest: `Malformed` says what happened, where an `unwrap_or_default`
    /// would have produced a zero-entry claim — i.e. a module
    /// one would believe pure.
    #[test]
    fn arbitrary_bytes_do_not_decode_into_an_empty_manifest() {
        assert!(matches!(
            decode(b"not a manifest"),
            Err(ManifestError::Malformed { .. })
        ));
        assert!(matches!(decode(&[]), Err(ManifestError::Malformed { .. })));
    }

    /// The decoder refuses what **trails after** an otherwise valid
    /// manifest. Without this, the custom section could carry two claims
    /// of which only one would be read — and the verifier would prove the first
    /// while the host, some day, would read the second.
    #[test]
    fn trailing_bytes_after_a_manifest_are_refused() {
        let m = Manifest {
            version: crate::limits::MANIFEST_VERSION,
            entries: vec![],
            regexes: vec![],
            templates: vec![],
        };
        let mut bytes = encode_unchecked(&m);
        assert!(decode(&bytes).is_ok(), "the bare manifest reads back");
        bytes.extend_from_slice(b"and a second one, sneakily");
        assert!(matches!(
            decode(&bytes),
            Err(ManifestError::Malformed { .. })
        ));
    }
}
