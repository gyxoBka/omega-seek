//! Ranked code search for agents: one index, two channels, one answer.
//!
//! Files are cut into chunks along block boundaries, each chunk is indexed for
//! BM25 over code-aware terms and embedded with a static model, and a query is
//! answered by fusing the two rankings. The answer names lines, not bytes, and
//! carries the source, so the agent does not have to ask twice.

pub mod access;
pub mod chunk;
pub mod index;
pub mod install;
pub mod mcp;
pub mod model;
pub mod outline;
pub mod paths;
pub mod roots;
pub mod search;
pub mod store;
pub mod tokenize;
pub mod update;
pub mod usages;
