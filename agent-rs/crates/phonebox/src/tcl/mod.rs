//! TCL: the owner's TCL air conditioners through TCL's cloud and AWS IoT, as the tcl-server dashboard reaches them.

pub mod client;
pub mod sigv4;

#[cfg(test)]
pub(crate) mod fake;
