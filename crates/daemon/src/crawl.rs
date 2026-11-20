//! The registry task keeps the index of the ARD registry up to date (ARCHITECTURE 18).
//!
//! Phase 1 has one step: each tick replaces the rows of the server's own catalogs. Phase 2 adds
//! the operator's seeds as sources of the kind `seed`: a tick fetches the sources that are due,
//! one at a time, and stores each result through the same engine call.

use bunsin_engine::Engine;
use std::time::Duration;

/// How often the registry task runs.
pub const CRAWL_TICK: Duration = Duration::from_secs(60);

/// One run of the registry task. A failure is written to stderr and does not stop the task.
pub fn tick(engine: &Engine) {
    if let Err(error) = engine.refresh_own_catalogs() {
        eprintln!("the registry could not index the catalogs of this server: {error}");
    }
}
