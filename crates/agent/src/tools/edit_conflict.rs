use agent_client_protocol::schema as acp;
use collections::HashMap;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Information about an agent holding an edit lock on a file.
#[derive(Clone, Debug)]
pub struct EditLock {
    /// Which agent session holds the lock.
    pub session_id: acp::SessionId,
    /// When the lock was acquired.
    pub locked_at: Instant,
    /// Most recent edit activity timestamp.
    pub last_heartbeat: Instant,
}

/// Global process-local registry tracking which files are being actively edited
/// by which agent session. Used to prevent concurrent agent edits to the same file.
pub struct EditConflictRegistry {
    locks: Mutex<HashMap<PathBuf, EditLock>>,
    enabled: AtomicBool,
}

static REGISTRY: LazyLock<EditConflictRegistry> = LazyLock::new(EditConflictRegistry::new);

impl EditConflictRegistry {
    fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::default()),
            enabled: AtomicBool::new(true),
        }
    }

    /// Get the global singleton registry.
    pub fn global() -> &'static Self {
        &REGISTRY
    }

    /// Enable or disable conflict detection globally.
    /// When disabled, register/release/check become no-ops.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether conflict detection is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Register that an agent session is actively editing a file.
    pub fn register(&self, path: PathBuf, session_id: acp::SessionId) {
        if !self.is_enabled() {
            return;
        }
        let now = Instant::now();
        let mut locks = self.locks.lock();
        locks.insert(
            path,
            EditLock {
                session_id,
                locked_at: now,
                last_heartbeat: now,
            },
        );
    }

    /// Update the heartbeat for a file being actively edited.
    /// Should be called periodically during long-running edits.
    pub fn heartbeat(&self, path: &PathBuf) {
        if !self.is_enabled() {
            return;
        }
        let mut locks = self.locks.lock();
        if let Some(lock) = locks.get_mut(path) {
            lock.last_heartbeat = Instant::now();
        }
    }

    /// Release the edit lock on a file.
    /// Only releases if the lock is held by the given session.
    pub fn release(&self, path: &PathBuf, session_id: &acp::SessionId) {
        if !self.is_enabled() {
            return;
        }
        let mut locks = self.locks.lock();
        if let Some(lock) = locks.get(path) {
            if &lock.session_id == session_id {
                locks.remove(path);
            }
        }
    }

    /// Check if another agent session is actively editing a file.
    /// Returns the lock info if there's a conflict, None if the file is available.
    /// Stale locks (no heartbeat for longer than `max_idle`) are automatically removed.
    pub fn check_conflict(
        &self,
        path: &PathBuf,
        self_session_id: &acp::SessionId,
        max_idle: Duration,
    ) -> Option<EditLock> {
        if !self.is_enabled() {
            return None;
        }
        let mut locks = self.locks.lock();
        let now = Instant::now();

        if let Some(lock) = locks.get(path) {
            let idle_duration = now.duration_since(lock.last_heartbeat);

            // Stale lock — remove it
            if idle_duration > max_idle {
                locks.remove(path);
                return None;
            }

            // Same session — no conflict
            if &lock.session_id == self_session_id {
                return None;
            }

            // Different session actively editing — conflict
            Some(lock.clone())
        } else {
            None
        }
    }

    /// Check if any agent session is editing a file (without knowing own session_id).
    /// Used by read_file tool which may not have thread context.
    pub fn is_file_busy(&self, path: &PathBuf, max_idle: Duration) -> Option<EditLock> {
        if !self.is_enabled() {
            return None;
        }
        let mut locks = self.locks.lock();
        let now = Instant::now();

        if let Some(lock) = locks.get(path) {
            let idle_duration = now.duration_since(lock.last_heartbeat);

            if idle_duration > max_idle {
                locks.remove(path);
                return None;
            }

            Some(lock.clone())
        } else {
            None
        }
    }

    /// Remove all stale locks (no heartbeat for longer than `max_age`).
    pub fn cleanup_stale(&self, max_age: Duration) {
        let mut locks = self.locks.lock();
        let now = Instant::now();
        locks.retain(|_, lock| now.duration_since(lock.last_heartbeat) <= max_age);
    }

    /// Clear all locks. For use in test teardown.
    #[cfg(test)]
    pub fn clear(&self) {
        self.locks.lock().clear();
    }
}
