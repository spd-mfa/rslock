#[cfg(any(feature = "async-std", feature = "tokio"))]
mod lock;

#[cfg(all(feature = "async-std", not(feature = "tokio")))]
pub use crate::lock::LockGuard;
#[cfg(any(feature = "async-std", feature = "tokio"))]
pub use crate::lock::{Lock, LockError, LockManager};
