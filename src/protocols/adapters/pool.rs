//! Connection pool adapter for protocol handlers
//!
//! This module provides a thread-safe connection pool implementation
//! that manages database connections efficiently.

use crate::{Error, Result, Config, StorageEngine};
use std::sync::Arc;
use parking_lot::{RwLock, Mutex};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Connection wrapper
///
/// Represents a managed connection in the pool. When dropped, the connection
/// is automatically returned to the pool.
pub struct PooledConnection {
    engine: Option<Arc<StorageEngine>>,
    pool: Arc<ConnectionPoolInner>,
    id: usize,
}

impl PooledConnection {
    /// Get a reference to the storage engine
    ///
    /// This method returns the storage engine associated with this pooled connection.
    /// The engine is guaranteed to be present while the connection is held and not dropped.
    ///
    /// # Safety
    /// The `Option<Arc<StorageEngine>>` internal structure ensures the engine is always
    /// `Some` until `Drop` is called. Rust's ownership rules prevent access after drop.
    pub fn engine(&self) -> &StorageEngine {
        // SAFETY: The engine is only set to None in Drop::drop.
        // Since we have &self (a borrow), the value cannot have been dropped.
        // This is guaranteed by Rust's ownership and borrowing rules.
        match self.engine.as_ref() {
            Some(engine) => engine,
            None => {
                // This branch is unreachable by Rust's ownership rules.
                // If we're here, something has gone catastrophically wrong
                // with memory safety (which Rust prevents).
                unreachable!("PooledConnection engine accessed after drop - this indicates a memory safety violation")
            }
        }
    }

    /// Get the connection ID
    pub fn id(&self) -> usize {
        self.id
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.pool.return_connection(engine);
        }
    }
}

/// Connection statistics
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Total number of connections in the pool
    pub total_connections: usize,
    /// Number of available connections
    pub available_connections: usize,
    /// Number of active connections
    pub active_connections: usize,
    /// Total connections created
    pub created_connections: u64,
    /// Total connection requests
    pub connection_requests: u64,
    /// Total wait time for connections (milliseconds)
    pub total_wait_time_ms: u64,
}

/// Inner connection pool state
struct ConnectionPoolInner {
    /// Available connections
    available: Mutex<VecDeque<Arc<StorageEngine>>>,
    /// Pool configuration
    config: PoolConfig,
    /// Statistics
    stats: RwLock<PoolStats>,
    /// Next connection ID
    next_id: Mutex<usize>,
}

impl ConnectionPoolInner {
    fn new(config: PoolConfig) -> Self {
        Self {
            available: Mutex::new(VecDeque::with_capacity(config.max_size)),
            config,
            stats: RwLock::new(PoolStats {
                total_connections: 0,
                available_connections: 0,
                active_connections: 0,
                created_connections: 0,
                connection_requests: 0,
                total_wait_time_ms: 0,
            }),
            next_id: Mutex::new(0),
        }
    }

    fn get_connection(&self) -> Result<Arc<StorageEngine>> {
        let start = Instant::now();

        // Update request count
        {
            let mut stats = self.stats.write();
            stats.connection_requests += 1;
        }

        // Try to get from pool first
        {
            let mut available = self.available.lock();
            if let Some(conn) = available.pop_front() {
                let mut stats = self.stats.write();
                stats.available_connections = available.len();
                stats.active_connections += 1;
                stats.total_wait_time_ms += start.elapsed().as_millis() as u64;
                return Ok(conn);
            }
        }

        // Check if we can create a new connection
        let can_create = {
            let stats = self.stats.read();
            stats.total_connections < self.config.max_size
        };

        if can_create {
            // Create new connection
            let engine = if let Some(ref path) = self.config.db_path {
                StorageEngine::open(path, &self.config.db_config)?
            } else {
                StorageEngine::open_in_memory(&self.config.db_config)?
            };

            let engine = Arc::new(engine);

            // Update stats
            {
                let mut stats = self.stats.write();
                stats.total_connections += 1;
                stats.active_connections += 1;
                stats.created_connections += 1;
                stats.total_wait_time_ms += start.elapsed().as_millis() as u64;
            }

            Ok(engine)
        } else {
            // Wait for a connection to become available
            self.wait_for_connection(start)
        }
    }

    fn wait_for_connection(&self, start: Instant) -> Result<Arc<StorageEngine>> {
        let timeout = self.config.connection_timeout;
        let check_interval = Duration::from_millis(10);

        loop {
            // Check timeout
            if start.elapsed() > timeout {
                return Err(Error::protocol("Connection pool timeout"));
            }

            // Try to get a connection
            {
                let mut available = self.available.lock();
                if let Some(conn) = available.pop_front() {
                    let mut stats = self.stats.write();
                    stats.available_connections = available.len();
                    stats.active_connections += 1;
                    stats.total_wait_time_ms += start.elapsed().as_millis() as u64;
                    return Ok(conn);
                }
            }

            // Sleep briefly before checking again
            std::thread::sleep(check_interval);
        }
    }

    fn return_connection(&self, engine: Arc<StorageEngine>) {
        let mut available = self.available.lock();
        available.push_back(engine);

        let mut stats = self.stats.write();
        stats.available_connections = available.len();
        stats.active_connections = stats.active_connections.saturating_sub(1);
    }
}

/// Connection pool configuration
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Minimum number of connections to maintain
    pub min_size: usize,
    /// Maximum number of connections
    pub max_size: usize,
    /// Connection timeout
    pub connection_timeout: Duration,
    /// Database path (None for in-memory)
    pub db_path: Option<String>,
    /// Database configuration
    pub db_config: Config,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_size: 2,
            max_size: 10,
            connection_timeout: Duration::from_secs(30),
            db_path: None,
            db_config: Config::in_memory(),
        }
    }
}

impl PoolConfig {
    /// Create a new pool configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Set minimum pool size
    pub fn with_min_size(mut self, size: usize) -> Self {
        self.min_size = size;
        self
    }

    /// Set maximum pool size
    pub fn with_max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Set connection timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = timeout;
        self
    }

    /// Set database path
    pub fn with_db_path(mut self, path: impl Into<String>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Set database configuration
    pub fn with_db_config(mut self, config: Config) -> Self {
        self.db_config = config;
        self
    }
}

/// Connection pool
///
/// Thread-safe connection pool that manages storage engine instances.
pub struct ConnectionPool {
    inner: Arc<ConnectionPoolInner>,
}

impl ConnectionPool {
    /// Create a new connection pool
    pub fn new(config: PoolConfig) -> Result<Self> {
        let pool = Self {
            inner: Arc::new(ConnectionPoolInner::new(config)),
        };

        // Pre-create minimum connections
        pool.init_min_connections()?;

        Ok(pool)
    }

    /// Create a connection pool with default configuration
    pub fn with_defaults() -> Result<Self> {
        Self::new(PoolConfig::default())
    }

    /// Initialize minimum connections
    fn init_min_connections(&self) -> Result<()> {
        for _ in 0..self.inner.config.min_size {
            let engine = if let Some(ref path) = self.inner.config.db_path {
                StorageEngine::open(path, &self.inner.config.db_config)?
            } else {
                StorageEngine::open_in_memory(&self.inner.config.db_config)?
            };

            self.inner.available.lock().push_back(Arc::new(engine));
        }

        // Update stats
        {
            let mut stats = self.inner.stats.write();
            stats.total_connections = self.inner.config.min_size;
            stats.available_connections = self.inner.config.min_size;
            stats.created_connections = self.inner.config.min_size as u64;
        }

        Ok(())
    }

    /// Get a connection from the pool
    pub fn get(&self) -> Result<PooledConnection> {
        let engine = self.inner.get_connection()?;
        let id = {
            let mut next_id = self.inner.next_id.lock();
            let id = *next_id;
            *next_id += 1;
            id
        };

        Ok(PooledConnection {
            engine: Some(engine),
            pool: Arc::clone(&self.inner),
            id,
        })
    }

    /// Get pool statistics
    pub fn stats(&self) -> PoolStats {
        self.inner.stats.read().clone()
    }

    /// Get current pool size
    pub fn size(&self) -> usize {
        self.inner.stats.read().total_connections
    }

    /// Get number of available connections
    pub fn available(&self) -> usize {
        self.inner.stats.read().available_connections
    }

    /// Get number of active connections
    pub fn active(&self) -> usize {
        self.inner.stats.read().active_connections
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_creation() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(2)
            .with_max_size(5);

        let pool = ConnectionPool::new(config)?;
        assert_eq!(pool.size(), 2);
        assert_eq!(pool.available(), 2);
        Ok(())
    }

    #[test]
    fn test_pool_get_connection() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(2)
            .with_max_size(5);

        let pool = ConnectionPool::new(config)?;

        let conn = pool.get()?;
        assert_eq!(pool.active(), 1);
        assert_eq!(pool.available(), 1);

        drop(conn);
        assert_eq!(pool.active(), 0);
        assert_eq!(pool.available(), 2);
        Ok(())
    }

    #[test]
    fn test_pool_multiple_connections() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(2)
            .with_max_size(5);

        let pool = ConnectionPool::new(config)?;

        let conn1 = pool.get()?;
        let conn2 = pool.get()?;
        let conn3 = pool.get()?;

        assert_eq!(pool.active(), 3);

        drop(conn1);
        assert_eq!(pool.active(), 2);

        drop(conn2);
        drop(conn3);
        assert_eq!(pool.active(), 0);
        Ok(())
    }

    #[test]
    fn test_pool_max_size() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(1)
            .with_max_size(2);

        let pool = ConnectionPool::new(config)?;

        let _conn1 = pool.get()?;
        let _conn2 = pool.get()?;

        // Third connection should timeout
        let config = PoolConfig::new()
            .with_min_size(1)
            .with_max_size(2)
            .with_timeout(Duration::from_millis(100));

        let pool2 = ConnectionPool::new(config)?;
        let _c1 = pool2.get()?;
        let _c2 = pool2.get()?;

        let result = pool2.get();
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_pool_stats() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(2)
            .with_max_size(5);

        let pool = ConnectionPool::new(config)?;

        let _conn1 = pool.get()?;
        let _conn2 = pool.get()?;

        let stats = pool.stats();
        assert_eq!(stats.total_connections, 2);
        assert_eq!(stats.active_connections, 2);
        assert_eq!(stats.available_connections, 0);
        assert_eq!(stats.connection_requests, 2);
        Ok(())
    }

    #[test]
    fn test_pool_connection_reuse() -> Result<()> {
        let config = PoolConfig::new()
            .with_min_size(1)
            .with_max_size(3);

        let pool = ConnectionPool::new(config)?;

        {
            let _conn = pool.get()?;
        } // Connection returned to pool

        let stats = pool.stats();
        assert_eq!(stats.created_connections, 1);

        // Next get should reuse the connection
        let _conn = pool.get()?;
        let stats = pool.stats();
        assert_eq!(stats.created_connections, 1); // No new connection created
        Ok(())
    }
}
