pub mod actor;
pub mod config;
pub mod grid;
pub mod history;
pub mod images;
pub mod link;
pub mod process;
pub mod protocol;
pub mod search;
pub mod terminal_input;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::fmt::Display;
use std::sync::Arc;

/// Rune's `Waker`, spelled out so core doesn't depend on Rune.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub trait LogErr<T> {
    fn log_err(self, context: &str) -> Option<T>;
}

impl<T, E: Display> LogErr<T> for std::result::Result<T, E> {
    fn log_err(self, context: &str) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("benches: {context}: {error:#}");
                None
            }
        }
    }
}
