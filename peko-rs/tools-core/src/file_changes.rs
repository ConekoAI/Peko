//! Where a tool reports the files it writes (ADR-117).
//!
//! The dispatcher hands a recorder to calls of tools that change files;
//! the tool names the exact path it writes just before and just after,
//! so the runtime can keep what the file was and undo the change.

use std::path::Path;

/// Receives the files a call writes.
pub trait FileChangeSink: Send + Sync {
    /// `path` is about to be written (or created, or removed).
    fn before(&self, path: &Path);
    /// The write to `path` is done (or failed: the file is compared).
    fn after(&self, path: &Path);
}
