//! Run an external agent, classify what came back, produce a typed result.
//!
//! What both phases need before a CLI can be invoked at all: the model and build to ask
//! for, the shell recipe that asks, the process exit it left behind, and the isolated tree
//! it ran in. All of it lived in `translate.rs`, which is why `verify.rs` had to name the
//! translation module to reach its OWN work tree, and why three of the module cycles ran
//! through that one file.

pub mod exit;
pub mod invocation;
pub mod opencode;
pub mod session;
pub mod work;

use std::sync::{Condvar, Mutex};

pub struct Semaphore {
    state: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl Semaphore {
    pub fn new(max: usize) -> Self {
        Self {
            state: Mutex::new(0),
            cvar: Condvar::new(),
            max,
        }
    }
    /// Poison is recovered, not propagated: a `usize` is never half-updated and the
    /// panicking worker's guard still ran, so the count is sound — while propagating
    /// would panic every sibling worker that next acquires.
    pub fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count += 1;
        SemaphoreGuard(self)
    }
}

pub struct SemaphoreGuard<'a>(&'a Semaphore);

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        // Runs while unwinding, where a second panic aborts the process; see `acquire`.
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.cvar.notify_one();
    }
}
