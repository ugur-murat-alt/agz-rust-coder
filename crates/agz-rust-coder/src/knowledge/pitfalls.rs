//! Text and identity data for the source audit.
//!
//! Keeping this table separate from the scanner makes the audit logic usable by
//! the server without mixing untrusted source text with advisory guidance.

#[derive(Debug, Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PatternId {
    CloneTax,
    Unwrap,
    StringParam,
    VecParam,
    PathBufParam,
    IndexLoop,
    ArcMutexStack,
    StdMutexAwait,
    UnsafeBlock,
    CasualSafetyComment,
}

impl PatternId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CloneTax => "clone-tax",
            Self::Unwrap => "unwrap",
            Self::StringParam => "string-param",
            Self::VecParam => "vec-param",
            Self::PathBufParam => "pathbuf-param",
            Self::IndexLoop => "index-loop",
            Self::ArcMutexStack => "arc-mutex-stack",
            Self::StdMutexAwait => "std-mutex-await",
            Self::UnsafeBlock => "unsafe-block",
            Self::CasualSafetyComment => "casual-safety-comment",
        }
    }

    pub const fn all() -> &'static [Self] {
        &[
            Self::CloneTax,
            Self::Unwrap,
            Self::StringParam,
            Self::VecParam,
            Self::PathBufParam,
            Self::IndexLoop,
            Self::ArcMutexStack,
            Self::StdMutexAwait,
            Self::UnsafeBlock,
            Self::CasualSafetyComment,
        ]
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Warn,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PitfallDefinition {
    pub id: PatternId,
    pub label: &'static str,
    pub fix: &'static str,
    pub severity: Severity,
}

pub type AuditPattern = PitfallDefinition;
pub type AuditPatternId = PatternId;
pub type AuditSeverity = Severity;

/// Advisory metadata is deliberately data-only; source text never enters this table.
pub const AUDIT_PATTERNS: &[PitfallDefinition] = &[
    PitfallDefinition {
        id: PatternId::CloneTax,
        label: "clone() call that may paper over a borrow issue",
        fix: "Redesign ownership first: borrow (&str, &[T]), restructure, or clone only when a second independent owner truly exists.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::Unwrap,
        label: "unwrap()/expect() on possibly-fallible code",
        fix: "Propagate with ? or handle with match/if let/unwrap_or; if genuinely infallible, add a comment stating the invariant.",
        severity: Severity::Error,
    },
    PitfallDefinition {
        id: PatternId::StringParam,
        label: "&String parameter (should be &str)",
        fix: "Accept &str: it accepts literals, String, and &String without forcing ownership.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::VecParam,
        label: "&Vec<T> parameter (should be &[T])",
        fix: "Accept &[T]; it accepts Vec, arrays, and slices.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::PathBufParam,
        label: "&PathBuf parameter (should be &Path)",
        fix: "Accept &Path; it accepts Path, PathBuf, and &str.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::IndexLoop,
        label: "index-based loop over 0..len",
        fix: "Iterate directly; use enumerate, zip, windows, or chunks when index or pairing matters.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::ArcMutexStack,
        label: "Arc<Mutex<..>> wrapper (shared mutable state)",
        fix: "Prefer ownership redesign, RwLock for read-heavy state, message passing via channels, or sharding; Mutex serializes all access through one lock.",
        severity: Severity::Warn,
    },
    PitfallDefinition {
        id: PatternId::StdMutexAwait,
        label: "std::sync::Mutex combined with async code in the same file",
        fix: "Never hold a std Mutex across .await. Use tokio::sync::Mutex in async code, or move the lock out of the await span.",
        severity: Severity::Error,
    },
    PitfallDefinition {
        id: PatternId::UnsafeBlock,
        label: "unsafe block",
        fix: "Safe Rust should compile first; unsafe is a last resort. Every unsafe block needs a precise, enforced invariant, not a courtesy comment.",
        severity: Severity::Error,
    },
    PitfallDefinition {
        id: PatternId::CasualSafetyComment,
        label: "vague SAFETY comment (\"guaranteed by the caller\")",
        fix: "State how the invariant is established and maintained; point to the code that enforces it.",
        severity: Severity::Warn,
    },
];

pub fn pattern_by_id(id: PatternId) -> &'static PitfallDefinition {
    // The table is static and complete by construction; keep the fallback
    // explicit so callers do not need to handle an impossible absence.
    AUDIT_PATTERNS
        .iter()
        .find(|pattern| pattern.id == id)
        .unwrap_or(&AUDIT_PATTERNS[0])
}

pub fn pattern_by_name(name: &str) -> Option<&'static PitfallDefinition> {
    AUDIT_PATTERNS
        .iter()
        .find(|pattern| pattern.id.as_str() == name)
}
