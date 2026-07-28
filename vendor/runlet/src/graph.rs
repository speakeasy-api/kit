use crate::{CanonicalValue, Span};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Semantic role of a node in an execution graph.
pub enum NodeKind {
    /// Program return root.
    Root,
    /// Host tool or intrinsic invocation.
    Call,
    /// Pure expression computation.
    Compute,
    /// Schema-directed value conversion.
    Convert,
    /// Member or index projection.
    Project,
    /// Structured composite value.
    Composite,
    /// Conditional branch selection.
    Branch,
    /// Dynamic loop container.
    Loop,
    /// One dynamic loop iteration.
    Iteration,
    /// Retry and recovery boundary.
    Boundary,
    /// Value supplied by the host.
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Lifecycle state of an execution graph node.
pub enum NodeState {
    /// Created during planning but not yet considered for execution.
    Planned,
    /// Waiting for a dependency or retry decision.
    Blocked,
    /// Eligible to start.
    Ready,
    /// Being handed to a tool implementation.
    Dispatching,
    /// Actively executing.
    Running,
    /// Completed with a value.
    Succeeded,
    /// Completed with an error.
    Failed,
    /// Cancellation has been requested.
    Cancelling,
    /// Stopped due to cancellation.
    Cancelled,
    /// Excluded because it cannot contribute to a root.
    Pruned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// A unit of work or value in an execution graph.
pub struct Node {
    /// Stable identifier within this graph.
    pub id: String,
    /// Semantic role of the node.
    pub kind: NodeKind,
    /// Human-readable operation label.
    pub label: String,
    /// Source expression associated with the node.
    pub span: Span,
    /// Current lifecycle state.
    pub state: NodeState,
    /// Zero-based retry attempt.
    pub attempt: u32,
    /// Successful output, when available.
    pub output: Option<CanonicalValue>,
    /// Failure description, when available.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Relationship between two execution graph nodes.
pub enum EdgeKind {
    /// A value flows from a producer to a consumer.
    Data {
        /// Path selected from the producer output.
        producer_path: String,
        /// Path populated on the consumer input.
        consumer_path: String,
    },
    /// The destination is gated by a condition.
    Control {
        /// Human-readable condition label.
        condition: String,
    },
    /// The source structurally owns the destination.
    Contains,
    /// The source must complete before the destination.
    Orders,
    /// The destination is another attempt of the source operation.
    RetryOf,
    /// The destination handles failure of the source.
    FallbackOf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Directed relationship in an execution graph.
pub struct Edge {
    /// Source node identifier.
    pub from: String,
    /// Destination node identifier.
    pub to: String,
    /// Meaning of the relationship.
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
/// Snapshot of nodes and edges produced by one execution.
pub struct Graph {
    /// Graph nodes in creation order.
    pub nodes: Vec<Node>,
    /// Graph edges in creation order.
    pub edges: Vec<Edge>,
}

/// An ordered, incremental change to a running execution graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEvent {
    /// Monotonically increasing event number for a run.
    pub sequence: u64,
    /// Mutation represented by this event.
    pub change: GraphChange,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
/// Mutation emitted while an execution graph is built.
pub enum GraphChange {
    /// A node was created.
    NodeAdded(Node),
    /// A node's state or data changed.
    NodeUpdated(Node),
    /// An edge was created.
    EdgeAdded(Edge),
}

impl Graph {
    pub(crate) fn begin(
        &mut self,
        kind: NodeKind,
        label: impl Into<String>,
        span: Span,
        attempt: u32,
    ) -> usize {
        let i = self.nodes.len();
        self.nodes.push(Node {
            id: format!("n{i:08x}"),
            kind,
            label: label.into(),
            span,
            state: NodeState::Ready,
            attempt,
            output: None,
            error: None,
        });
        i
    }
    pub(crate) fn running(&mut self, i: usize) {
        self.nodes[i].state = NodeState::Running
    }
    pub(crate) fn success(&mut self, i: usize, v: CanonicalValue) {
        self.nodes[i].state = NodeState::Succeeded;
        self.nodes[i].output = Some(v)
    }
    pub(crate) fn fail(&mut self, i: usize, e: impl Into<String>) {
        self.nodes[i].state = NodeState::Failed;
        self.nodes[i].error = Some(e.into())
    }
    pub(crate) fn contains(&mut self, parent: usize, child: usize) {
        let from = self.nodes[parent].id.clone();
        let to = self.nodes[child].id.clone();
        self.edges.push(Edge {
            from,
            to,
            kind: EdgeKind::Contains,
        })
    }
}
