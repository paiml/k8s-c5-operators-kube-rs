//! opctl: a Website operator and admission webhook logic built on kube-rs.
//!
//! Everything except `glue` is pure or runs against the `Cluster` trait, so a
//! mock cluster exercises it fully. `glue` is the only module that talks to a
//! real API server.
pub mod admission;
pub mod cli;
pub mod contract;
pub mod crd;
pub mod glue;
pub mod reconcile;

/// Errors surfaced by the reconciler, the admission handlers and the CLI.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// The object cannot be reconciled (for example it has no name).
    Invalid(String),
    /// A call to the cluster failed.
    Cluster(String),
    /// Input could not be parsed.
    Parse(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid object: {m}"),
            Error::Cluster(m) => write!(f, "cluster error: {m}"),
            Error::Parse(m) => write!(f, "parse error: {m}"),
        }
    }
}

impl std::error::Error for Error {}
