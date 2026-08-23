#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackNodeId(pub usize);

impl From<usize> for CallbackNodeId {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl From<CallbackNodeId> for usize {
    fn from(value: CallbackNodeId) -> Self {
        value.0
    }
}

pub trait ReadyNodeSink {
    fn schedule(&mut self, node: CallbackNodeId);
}

#[derive(Default)]
pub struct NoopReadyNodeSink;

impl ReadyNodeSink for NoopReadyNodeSink {
    fn schedule(&mut self, _node: CallbackNodeId) {}
}
