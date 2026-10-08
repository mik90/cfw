//! Simulation exposes an explicit stepping session. Real worker batches are
//! scoped to each step, so neither workers nor callback borrows escape a call.
pub use crate::state::SimulationState as SimulationExecutor;
