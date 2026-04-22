bitflags::bitflags! {
    /// Per-node dirty flags that track what changed since last GPU upload.
    ///
    /// Flags propagate upward: when a node is marked dirty, its ancestors
    /// receive `CHILDREN` so the render traversal knows to descend into
    /// that subtree.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DirtyFlags: u8 {
        /// Position or size changed — world transforms need recomputation.
        const TRANSFORM  = 1 << 0;
        /// Render data changed (text, quads, images) — needs GPU re-upload.
        const CONTENT    = 1 << 1;
        /// At least one descendant has dirty content.
        const CHILDREN   = 1 << 2;
        /// Visibility toggled — may need to add/remove from draw list.
        const VISIBILITY = 1 << 3;
        /// All flags set.
        const ALL = Self::TRANSFORM.bits()
                  | Self::CONTENT.bits()
                  | Self::CHILDREN.bits()
                  | Self::VISIBILITY.bits();
    }
}

impl DirtyFlags {
    /// Returns `true` when no flags are set — node is fully up-to-date.
    pub fn is_clean(self) -> bool {
        self.is_empty()
    }

    /// Returns `true` when the node's render data needs re-upload.
    pub fn needs_upload(self) -> bool {
        self.contains(Self::CONTENT)
    }

    /// Returns `true` when the node's layout / world transform is stale.
    pub fn needs_layout(self) -> bool {
        self.contains(Self::TRANSFORM)
    }
}
