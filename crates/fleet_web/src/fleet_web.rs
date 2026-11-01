//! Fleet web (plan 051): thread history and rig queues behind a Fastly edge
//! with GitHub login. This crate is the origin side; `edge/` is the Fastly app.

pub mod origin;
pub mod publish;
pub mod signature;
pub mod thread_export;
