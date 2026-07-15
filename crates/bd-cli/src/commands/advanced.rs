//! The advanced families, now split by domain into their own modules.
//!
//! This file used to hold every one of them. As they graduate from stubs into
//! real commands they get their own file — one owner apiece — so that no two
//! implementations ever collide in here. What remains is the one re-export the
//! dispatch table still reaches through.

// `formula …` and `cook` live in `crate::commands::formula`, over the
// `bd_formula` compiler.
pub use crate::commands::formula::{cook, formula};
