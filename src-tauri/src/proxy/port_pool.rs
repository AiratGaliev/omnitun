//! RAII Port Pool — manages a range of TCP ports for parallel sing-box testing.
//!
//! Ports are acquired before generating the N-inbound config and are
//! automatically returned to the pool when `PortGuard` is dropped — even
//! during panics, ensuring no `EADDRINUSE` leaks.
//!
//! Uses `std::sync::Mutex` (NOT `tokio::sync::Mutex`) so that `Drop`
//! never panics from calling `tokio::spawn` on a stopped runtime.
//!
//! Port range: 30000–35000 (5001 ports — enough for 3 batches × ~50 inbounds,
//! system-proxy mixed inbound, and Clash API stats port with margin).
//!
//! `PortPool` uses `Arc<Mutex<Vec<u16>>>` internally, making it cheap to
//! clone. Every `PortGuard` shares a reference to the same port list, so
//! `acquire()` and `acquire_guarded()` both return ports correctly on Drop.

use std::sync::{Arc, Mutex};

/// Default port range for batch testing (above typical ephemeral range).
const PORT_MIN: u16 = 30_000;
const PORT_MAX: u16 = 35_000;

/// RAII port pool. Thread-safe via `Arc<Mutex<Vec<u16>>>`.
///
/// Cloning is cheap — shares the same underlying port list.
#[derive(Clone)]
pub struct PortPool {
    available: Arc<Mutex<Vec<u16>>>,
    capacity: usize,
}

/// A guard that holds one port. The port is returned to the pool on `Drop`.
///
/// Uses `PortPool` directly (not `Arc`) — cloning `PortPool` is cheap
/// since its inner state is already `Arc<Mutex<...>>`.
pub struct PortGuard {
    pub port: u16,
    pool: PortPool,
}

impl PortPool {
    /// Create a new pool covering [`PORT_MIN`, `PORT_MAX`].
    pub fn new() -> Self {
        let ports: Vec<u16> = (PORT_MIN..=PORT_MAX).collect();
        tracing::info!(target: "cache",
            "[PortPool] created with {} ports ({}–{})",
            ports.len(),
            PORT_MIN,
            PORT_MAX
        );
        let capacity = ports.len();
        Self {
            available: Arc::new(Mutex::new(ports)),
            capacity,
        }
    }
}

impl Default for PortPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PortPool {
    /// Create a pool with a custom port range (for testing).
    pub fn with_range(min: u16, max: u16) -> Self {
        let ports: Vec<u16> = (min..=max).collect();
        let capacity = ports.len();
        Self {
            available: Arc::new(Mutex::new(ports)),
            capacity,
        }
    }

    /// Acquire one port from the pool. Returns `None` if the pool is exhausted.
    ///
    /// The returned `PortGuard` shares the SAME pool reference via the internal
    /// `Arc`, so the port is returned correctly on Drop.
    pub fn acquire(&self) -> Option<PortGuard> {
        self.acquire_with(|candidate| {
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, candidate)).is_ok()
        })
    }

    /// Acquire a port using an availability predicate.
    ///
    /// Kept private so production callers cannot bypass the socket check.
    /// Unit tests use it to verify pool accounting independently from ports
    /// occupied by unrelated processes on the developer/CI machine.
    fn acquire_with<F>(&self, mut is_available: F) -> Option<PortGuard>
    where
        F: FnMut(u16) -> bool,
    {
        let mut pool = self.available.lock().unwrap_or_else(|e| e.into_inner());
        // LIFO returns the most-recently-freed port. Try each candidate at
        // most once; unavailable ports stay in the pool for a later retry.
        let attempts = pool.len();
        for _ in 0..attempts {
            let candidate = pool.pop()?;
            if is_available(candidate) {
                return Some(PortGuard {
                    port: candidate,
                    pool: self.clone(),
                });
            }
            pool.insert(0, candidate);
        }
        None
    }

    /// Acquire a guard from an `Arc<PortPool>` reference.
    /// Semantically identical to `acquire()`, just more ergonomic when
    /// the caller already has an Arc.
    pub fn acquire_guarded(self: &Arc<Self>) -> Option<PortGuard> {
        self.acquire()
    }

    /// Number of ports currently available.
    pub fn available_count(&self) -> usize {
        self.available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Total configured capacity, independent from currently available ports.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return a port directly (for use when not holding a guard).
    /// Checks for duplicates to prevent double-return.
    pub fn return_port(&self, port: u16) {
        let mut pool = self.available.lock().unwrap_or_else(|e| e.into_inner());
        if !pool.contains(&port) {
            pool.push(port);
        } else {
            tracing::warn!(target: "cache", "[PortPool] port {port} returned twice — ignoring");
        }
    }
}

impl Drop for PortGuard {
    /// ⚠️ SYNCHRONOUS Drop — uses `std::sync::Mutex`, NOT `tokio::spawn`.
    /// This is safe even when the Tokio runtime is shutting down.
    fn drop(&mut self) {
        let mut pool = self
            .pool
            .available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pool.push(self.port);
    }
}

// ── Debug impls ───────────────────────────────────────────────

impl std::fmt::Debug for PortPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.available_count();
        write!(f, "PortPool({count} available, {PORT_MIN}–{PORT_MAX})")
    }
}

impl std::fmt::Debug for PortGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PortGuard(port={})", self.port)
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn test_new_pool_has_full_capacity() {
        let pool = PortPool::new();
        assert_eq!(pool.available_count(), 5001);
        assert_eq!(pool.capacity(), 5001);
    }

    #[test]
    fn test_acquire_decreases_count() {
        let pool = PortPool::new();
        let before = pool.available_count();
        let guard = pool.acquire_with(|_| true);
        assert!(guard.is_some());
        assert_eq!(pool.available_count(), before - 1);
        assert_eq!(guard.unwrap().port, 35000); // LIFO — last port in range
    }

    #[test]
    fn test_custom_range_reports_its_own_capacity() {
        let pool = PortPool::with_range(32000, 32004);
        assert_eq!(pool.capacity(), 5);
        assert_eq!(pool.available_count(), 5);
    }

    #[test]
    fn test_acquire_skips_unavailable_candidates_without_losing_them() {
        let pool = PortPool::with_range(32000, 32002);
        let guard = pool.acquire_with(|port| port == 32000).unwrap();
        assert_eq!(guard.port, 32000);
        assert_eq!(pool.available_count(), 2);
        drop(guard);
        assert_eq!(pool.available_count(), 3);
    }

    #[test]
    fn test_drop_returns_port() {
        let pool = std::sync::Arc::new(PortPool::new());
        let before = pool.available_count();
        {
            let _guard = pool.acquire_with(|_| true).unwrap();
            assert_eq!(pool.available_count(), before - 1);
        }
        // After guard drop: port should be returned
        assert_eq!(pool.available_count(), before);
    }

    #[test]
    fn test_acquire_all_exhausts_pool() {
        let pool = PortPool::with_range(30000, 30004); // 5 ports
        assert_eq!(pool.available_count(), 5);

        let mut guards: Vec<PortGuard> = Vec::new();
        for _ in 0..5 {
            if let Some(g) = pool.acquire_with(|_| true) {
                guards.push(g);
            }
        }
        assert_eq!(pool.available_count(), 0);

        // Next acquire returns None
        assert!(pool.acquire_with(|_| true).is_none());
    }

    #[test]
    fn test_drop_all_returns_all_ports() {
        let pool = std::sync::Arc::new(PortPool::with_range(30000, 30009)); // 10 ports
        let mut guards: Vec<PortGuard> = Vec::new();
        for _ in 0..10 {
            guards.push(pool.acquire_with(|_| true).unwrap());
        }
        assert_eq!(pool.available_count(), 0);
        drop(guards);
        assert_eq!(pool.available_count(), 10);
    }

    #[test]
    fn test_double_return_is_noop() {
        let pool = std::sync::Arc::new(PortPool::with_range(31000, 31000)); // 1 port
        let guard = pool.acquire_with(|_| true).unwrap();
        let port = guard.port;
        drop(guard); // returns port
        assert_eq!(pool.available_count(), 1);

        // Return the same port again — should be noop (already there)
        pool.return_port(port);
        assert_eq!(
            pool.available_count(),
            1,
            "double-return must not increase count"
        );
    }

    #[test]
    fn test_ports_are_within_range() {
        let pool = PortPool::new();
        let guard = pool.acquire().unwrap();
        assert!(
            guard.port >= PORT_MIN && guard.port <= PORT_MAX,
            "port {} out of range {}-{}",
            guard.port,
            PORT_MIN,
            PORT_MAX
        );
    }

    #[test]
    fn test_acquire_guarded_via_arc() {
        let pool = Arc::new(PortPool::new());
        let before = pool.available_count();
        let guard = pool.acquire_guarded().unwrap();
        assert_eq!(pool.available_count(), before - 1);
        let port = guard.port;
        drop(guard);
        assert_eq!(pool.available_count(), before);
        // Verify the returned port is in the pool
        let guard2 = pool.acquire_guarded().unwrap();
        assert_eq!(guard2.port, port);
    }

    #[test]
    fn test_acquire_guarded_after_regular_acquire() {
        // Both acquire() and acquire_guarded() share the same pool Arc,
        // so all ports are returned correctly on Drop.
        let pool = Arc::new(PortPool::with_range(33000, 33002));
        let g1 = pool.acquire_with(|_| true).unwrap();
        let g2 = pool.acquire_with(|_| true).unwrap();
        assert_ne!(g1.port, g2.port);
        let port1 = g1.port;
        let port2 = g2.port;
        drop(g1); // returned to pool via shared Arc
        drop(g2); // returned to pool via shared Arc
                  // Both ports should be back
        assert_eq!(pool.available_count(), 3);
        // Verify the returned ports re-enter the pool
        let g3 = pool.acquire_with(|_| true).unwrap();
        assert!(g3.port == port1 || g3.port == port2);
        let g4 = pool.acquire_with(|_| true).unwrap();
        assert!(g4.port == port1 || g4.port == port2);
        assert_ne!(g3.port, g4.port);
    }

    #[test]
    fn test_with_range_respects_bounds() {
        let pool = PortPool::with_range(40000, 40002);
        assert_eq!(pool.available_count(), 3);
        let mut ports: Vec<u16> = Vec::new();
        let mut guards: Vec<PortGuard> = Vec::new();
        while let Some(g) = pool.acquire_with(|_| true) {
            ports.push(g.port);
            guards.push(g); // keep guard alive to prevent Drop from returning port
        }
        ports.sort();
        assert_eq!(ports, vec![40000, 40001, 40002]);
        assert_eq!(pool.available_count(), 0);
        drop(guards);
        assert_eq!(pool.available_count(), 3);
    }

    #[test]
    fn test_debug_format() {
        let pool = PortPool::new();
        let debug = format!("{:?}", pool);
        assert!(debug.contains("5001 available"));
        assert!(debug.contains("30000"));
    }

    #[test]
    fn test_guard_debug_format() {
        let pool = PortPool::new();
        let guard = pool.acquire().unwrap();
        let debug = format!("{:?}", guard);
        assert!(debug.contains(&format!("port={}", guard.port)));
    }

    /// Drop safety: PortGuard::Drop must not panic even when called from
    /// a context where the Tokio runtime is gone. We test this by calling
    /// Drop directly on a guard outside any tokio context.
    #[test]
    fn test_drop_safety_no_tokio_runtime() {
        let pool = Arc::new(PortPool::with_range(41000, 41005));
        let guard = pool.acquire_with(|_| true).unwrap();
        assert_eq!(pool.available_count(), 5); // 1 taken
                                               // Drop the guard outside any tokio runtime (this test is sync)
        drop(guard);
        assert_eq!(pool.available_count(), 6); // all returned
    }

    /// Stress test: acquire + drop 1000 ports concurrently across threads.
    /// Thread-safety of the underlying Mutex.
    #[test]
    fn test_concurrent_acquire_drop() {
        let pool = Arc::new(PortPool::new());
        let mut handles = Vec::new();
        for _ in 0..100 {
            let p = pool.clone();
            handles.push(std::thread::spawn(move || {
                let mut guards: Vec<PortGuard> = Vec::new();
                for _ in 0..10 {
                    if let Some(g) = p.acquire_guarded() {
                        guards.push(g);
                    }
                }
                // Simulate brief work
                std::thread::sleep(Duration::from_millis(1));
                drop(guards);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // All ports should be returned
        assert_eq!(pool.available_count(), 5001);
    }

    /// Panic safety: if a tokio::spawn task panics with a PortGuard,
    /// the Drop runs and the port is returned.
    #[test]
    fn test_panic_safety_returns_ports() {
        let pool = Arc::new(PortPool::with_range(42000, 42004));
        let guard = pool.acquire_with(|_| true).unwrap();
        let captured_port = guard.port;

        // Capture the pool reference
        let p = guard.pool.clone();

        // Drop guard manually — simulates what happens when guard is on stack
        // during a panic that's caught
        drop(guard);

        // The port should be back — use acquire() directly
        let guard2 = p.acquire_with(|_| true).unwrap();
        assert_eq!(guard2.port, captured_port);
    }
}
