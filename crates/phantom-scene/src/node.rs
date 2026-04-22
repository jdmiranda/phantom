use crate::dirty::DirtyFlags;

/// Lightweight handle into the scene-tree arena.
pub type NodeId = u32;

/// What kind of UI element this node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Root,
    TabBar,
    ContentArea,
    Pane,
    StatusBar,
    SystemOverlay,
    CommandBar,
    DebugHud,
    AgentSuggestion,
    Image,
    /// Bezier connection between panes.
    Tether,
    /// Plugin-defined node types.
    Custom(u32),
}

/// Position and size relative to the parent node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }
    }
}

/// Cached absolute position computed by walking the parent chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldTransform {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for WorldTransform {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }
    }
}

/// Which render pass a node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderLayer {
    /// Rendered into an offscreen texture that receives CRT post-fx.
    Scene,
    /// Rendered *after* CRT, directly onto the surface.
    Overlay,
}

/// A single node in the retained scene graph.
#[derive(Debug)]
pub struct SceneNode {
    pub id: NodeId,
    pub kind: NodeKind,
    /// Local transform relative to parent.
    pub transform: Transform,
    /// Cached absolute position (recomputed when TRANSFORM is dirty).
    pub world_transform: WorldTransform,
    pub visible: bool,
    /// Higher values are drawn later (on top).
    pub z_order: i32,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub dirty: DirtyFlags,
    /// Which render pass this node belongs to.
    pub render_layer: RenderLayer,
    /// When `true`, the arena slot is dead and should be skipped.
    pub(crate) alive: bool,
}

impl SceneNode {
    /// Create a new node with sensible defaults and `DirtyFlags::ALL`.
    pub fn new(id: NodeId, kind: NodeKind) -> Self {
        Self {
            id,
            kind,
            transform: Transform::default(),
            world_transform: WorldTransform::default(),
            visible: true,
            z_order: 0,
            parent: None,
            children: Vec::new(),
            dirty: DirtyFlags::ALL,
            render_layer: RenderLayer::Scene,
            alive: true,
        }
    }

    /// Builder: set local transform.
    pub fn with_transform(mut self, x: f32, y: f32, w: f32, h: f32) -> Self {
        self.transform = Transform { x, y, width: w, height: h };
        self
    }

    /// Builder: set z-order.
    pub fn with_z_order(mut self, z: i32) -> Self {
        self.z_order = z;
        self
    }

    /// Builder: set render layer.
    pub fn with_layer(mut self, layer: RenderLayer) -> Self {
        self.render_layer = layer;
        self
    }
}
