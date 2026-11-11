//! Memory estimation: GGUF header parsing, the weights + KV cache + overhead + margin formula
//! (spec §8), target detection and the fit verdict. Later: app profiles, history-based estimates
//! and the community catalog. Every result is labelled as an estimate.

pub mod apps;
pub mod fit;
pub mod gguf;
pub mod model;
pub mod target;
