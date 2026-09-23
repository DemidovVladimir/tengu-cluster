//! Chat use cases — one user turn (`service`), flow/session budgeting
//! (`flow`, `prompt_budget`) and the inner tool loop (`tool_loop`).

pub(crate) mod flow;
pub(crate) mod prompt_budget;
pub(crate) mod service;
pub(crate) mod tool_loop;
