//! Lambda client pool for HTTP/2 connection distribution.
//!
//! The AWS SDK's Lambda client uses HTTP/2, which multiplexes requests over a single
//! TCP connection. However, HTTP/2 servers limit concurrent streams per connection
//! (typically ~250 for AWS Lambda). When this limit is reached, requests queue up
//! waiting for stream slots.
//!
//! This module provides a pool of Lambda clients, each with its own HTTP/2 connection,
//! allowing us to scale beyond the single-connection stream limit.

use aws_sdk_lambda::Client as LambdaClient;
use aws_types::region::Region;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::aws_client;

/// Default number of Lambda clients in the pool. Each client maintains its own
/// HTTP/2 connection, and HTTP/2 servers typically allow ~250 concurrent streams
/// per connection. With 64 clients, we can support ~16,000 concurrent requests.
pub const DEFAULT_POOL_SIZE: usize = 64;

/// A Lambda client with tracking for active requests.
///
/// The `active` counter tracks how many requests are currently using this client,
/// enabling load-balanced client selection.
pub struct PooledClient {
    client: LambdaClient,
    active: AtomicUsize,
}

impl PooledClient {
    /// Create a new pooled client.
    pub fn new(client: LambdaClient) -> Self {
        Self {
            client,
            active: AtomicUsize::new(0),
        }
    }

    /// Get the current number of active requests on this client.
    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

/// RAII guard that holds a reference to a pooled client.
///
/// When the guard is dropped, the client's active count is decremented.
/// This ensures accurate tracking even if the request panics.
pub struct ClientGuard<'a> {
    pooled: &'a PooledClient,
}

impl<'a> ClientGuard<'a> {
    fn new(pooled: &'a PooledClient) -> Self {
        Self { pooled }
    }

    /// Get a reference to the underlying Lambda client.
    pub fn client(&self) -> &LambdaClient {
        &self.pooled.client
    }
}

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.pooled.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl std::ops::Deref for ClientGuard<'_> {
    type Target = LambdaClient;

    fn deref(&self) -> &Self::Target {
        &self.pooled.client
    }
}

/// A pool of Lambda clients for distributing requests across HTTP/2 connections.
///
/// Uses "power of two choices" algorithm for load balancing: randomly select two
/// clients and use the one with fewer active requests. This achieves near-optimal
/// load distribution with O(1) overhead.
pub struct ClientPool {
    clients: Box<[PooledClient]>,
    /// Counter for pseudo-random client selection
    counter: AtomicUsize,
}

impl ClientPool {
    /// Create a new client pool from a vector of Lambda clients.
    ///
    /// # Panics
    /// Panics if the clients vector is empty.
    pub fn new(clients: Vec<LambdaClient>) -> Self {
        assert!(!clients.is_empty(), "ClientPool requires at least one client");

        let pooled: Box<[PooledClient]> = clients
            .into_iter()
            .map(PooledClient::new)
            .collect();

        Self {
            clients: pooled,
            counter: AtomicUsize::new(0),
        }
    }

    /// Build a new client pool with the specified configuration.
    ///
    /// Creates `pool_size` Lambda clients in parallel, each with its own HTTP/2 connection.
    pub async fn build(region: Region, endpoint_url: Option<String>, pool_size: usize) -> Self {
        // Create all clients concurrently for faster initialization
        let futures: Vec<_> = (0..pool_size)
            .map(|_| {
                let r = region.clone();
                let e = endpoint_url.clone();
                async move { aws_client::build_lambda_client(r, e).await }
            })
            .collect();

        let clients = futures::future::join_all(futures).await;
        Self::new(clients)
    }

    /// Acquire a client from the pool using power-of-two choices.
    ///
    /// This selects two random clients and returns the one with fewer active
    /// requests. The returned guard automatically decrements the active count
    /// when dropped.
    pub fn acquire(&self) -> ClientGuard<'_> {
        let len = self.clients.len();

        if len == 1 {
            // Fast path for single client
            self.clients[0].active.fetch_add(1, Ordering::Relaxed);
            return ClientGuard::new(&self.clients[0]);
        }

        // Generate two pseudo-random indices using atomic counter
        // Uses Knuth multiplicative hash for second index to decorrelate
        let r = self.counter.fetch_add(1, Ordering::Relaxed);
        let idx1 = r % len;
        let mut idx2 = r.wrapping_mul(2654435761) % len;
        if idx2 == idx1 {
            idx2 = (idx1 + 1) % len;
        }

        let c1 = &self.clients[idx1];
        let c2 = &self.clients[idx2];

        // Pick the client with fewer active requests
        let chosen = if c1.active.load(Ordering::Relaxed) <= c2.active.load(Ordering::Relaxed) {
            c1
        } else {
            c2
        };

        chosen.active.fetch_add(1, Ordering::Relaxed);
        ClientGuard::new(chosen)
    }

    /// Get the number of clients in the pool.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Check if the pool is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Get the first client directly (useful for probes).
    ///
    /// This does NOT increment the active count - use only for operations
    /// that don't need load balancing (like health probes).
    pub fn first_client(&self) -> &LambdaClient {
        &self.clients[0].client
    }

    /// Get the total number of active requests across all clients.
    #[allow(dead_code)]
    pub fn total_active(&self) -> usize {
        self.clients
            .iter()
            .map(|c| c.active.load(Ordering::Relaxed))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require a mock LambdaClient, which is complex to set up.
    // For now, we test the logic that doesn't require an actual client.

    #[test]
    fn test_power_of_two_indices() {
        // Verify that our index generation produces different values
        let counter = AtomicUsize::new(0);
        let len = 64usize;

        let mut pairs = Vec::new();
        for _ in 0..100 {
            let r = counter.fetch_add(1, Ordering::Relaxed);
            let idx1 = r % len;
            let mut idx2 = r.wrapping_mul(2654435761) % len;
            if idx2 == idx1 {
                idx2 = (idx1 + 1) % len;
            }
            pairs.push((idx1, idx2));
        }

        // Check that we get variety in our indices
        let unique_idx1: std::collections::HashSet<_> = pairs.iter().map(|(a, _)| *a).collect();
        let unique_idx2: std::collections::HashSet<_> = pairs.iter().map(|(_, b)| *b).collect();

        // idx1 is sequential mod 64, so with 100 samples we hit all 64 buckets
        assert_eq!(unique_idx1.len(), 64, "idx1 should cover all buckets");
        // idx2 uses Knuth hash, expect good distribution (at least 40 of 64 buckets)
        assert!(unique_idx2.len() >= 40, "idx2 should have good variety, got {}", unique_idx2.len());
    }

    #[test]
    fn test_indices_never_equal() {
        // Verify that idx1 and idx2 are never the same (our collision fix)
        let counter = AtomicUsize::new(0);
        let len = 64usize;

        for _ in 0..1000 {
            let r = counter.fetch_add(1, Ordering::Relaxed);
            let idx1 = r % len;
            let mut idx2 = r.wrapping_mul(2654435761) % len;
            if idx2 == idx1 {
                idx2 = (idx1 + 1) % len;
            }
            assert_ne!(idx1, idx2, "idx1 and idx2 should never be equal");
        }
    }
}
