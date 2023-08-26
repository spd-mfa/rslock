mod lock;

#[cfg(any(feature = "async-std", feature = "tokio"))]
pub use crate::lock::{Lock, LockError, LockManager};
#[cfg(all(feature = "async-std", not(feature = "tokio")))]
pub use crate::lock::LockGuard;
