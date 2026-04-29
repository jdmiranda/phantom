//! [`Disposition`] — intent classification for an agent spawn.

/// Intent classification for an agent spawn.
///
/// The default is [`Disposition::Chat`] (zero-side-effect) so existing call
/// sites that don't set a disposition are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Disposition {
    Chat,
    Feature,
    BugFix,
    Refactor,
    Chore,
    Synthesize,
    Decompose,
    Audit,
}

impl Disposition {
    #[must_use]
    pub fn creates_branch(self) -> bool {
        matches!(self, Self::Feature | Self::BugFix | Self::Refactor | Self::Chore)
    }

    #[must_use]
    pub fn requires_plan_gate(self) -> bool {
        matches!(self, Self::Feature | Self::BugFix | Self::Refactor)
    }

    #[must_use]
    pub fn runs_hooks(self) -> bool {
        self.creates_branch()
    }

    #[must_use]
    pub fn auto_approve(self) -> bool {
        matches!(self, Self::Chat | Self::Synthesize | Self::Decompose | Self::Audit)
    }

    #[must_use]
    pub fn skill(self) -> &'static str {
        match self {
            Self::Chat => "",
            Self::Feature => "feature",
            Self::BugFix => "bugfix",
            Self::Refactor => "refactor",
            Self::Chore => "chore",
            Self::Synthesize => "synthesize",
            Self::Decompose => "decompose",
            Self::Audit => "",
        }
    }
}

impl Default for Disposition {
    fn default() -> Self {
        Self::Chat
    }
}
