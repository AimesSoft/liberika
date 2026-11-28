//! DFM+ core adapted from the MIT-licensed NipaPlay-Reload Rust implementation.
//!
//! Copyright (c) 2025 MCDFsteve.
//! liberika keeps this module as the layout/filter/retainer core and wraps it in
//! a native renderer-facing adapter instead of carrying NipaPlay's FRB/Flutter
//! presentation layer.

pub mod factory;
pub mod filters;
pub mod measure;
pub mod model;
pub mod retainer;
pub mod timer;
pub mod types;
