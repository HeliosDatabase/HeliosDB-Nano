//! Pub/Sub adapter layer for PostgreSQL LISTEN/NOTIFY
//!
//! This module provides an in-memory implementation of PostgreSQL's
//! LISTEN/NOTIFY mechanism for real-time notifications.

use crate::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use parking_lot::RwLock;
use uuid::Uuid;

/// A notification message
#[derive(Debug, Clone)]
pub struct Notification {
    /// The channel name
    pub channel: String,
    /// The notification payload
    pub payload: String,
    /// The process ID that sent the notification (simulated)
    pub pid: u32,
}

impl Notification {
    /// Create a new notification
    pub fn new(channel: String, payload: String, pid: u32) -> Self {
        Self {
            channel,
            payload,
            pid,
        }
    }
}

/// A subscription handle
///
/// Represents an active subscription to a channel. When dropped,
/// the subscription is automatically unsubscribed.
pub struct Subscription {
    id: Uuid,
    channel: String,
    manager: Arc<PubSubManager>,
}

impl Subscription {
    /// Get the subscription ID
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Get the channel name
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Check for new notifications
    ///
    /// Returns all pending notifications for this subscription.
    pub fn poll(&self) -> Result<Vec<Notification>> {
        self.manager.poll_subscription(self.id)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Best-effort unsubscribe on drop
        let _ = self.manager.unsubscribe_by_id(self.id);
    }
}

/// Internal subscription state
struct SubscriptionState {
    channel: String,
    pending_notifications: Vec<Notification>,
}

/// Pub/Sub manager
///
/// Thread-safe in-memory implementation of PostgreSQL LISTEN/NOTIFY.
pub struct PubSubManager {
    /// Active subscriptions
    subscriptions: Arc<RwLock<HashMap<Uuid, SubscriptionState>>>,
    /// Channel -> Subscription ID mapping
    channels: Arc<RwLock<HashMap<String, HashSet<Uuid>>>>,
    /// Simulated process ID
    pid: u32,
}

impl PubSubManager {
    /// Create a new Pub/Sub manager
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            channels: Arc::new(RwLock::new(HashMap::new())),
            pid: std::process::id(),
        }
    }

    /// Subscribe to a channel
    ///
    /// # Arguments
    /// * `channel` - The channel name to subscribe to
    ///
    /// # Returns
    /// A subscription handle that receives notifications
    pub fn subscribe(&self, channel: impl Into<String>) -> Result<Subscription> {
        let channel = channel.into();
        let id = Uuid::new_v4();

        // Add subscription
        {
            let mut subs = self.subscriptions.write();
            subs.insert(id, SubscriptionState {
                channel: channel.clone(),
                pending_notifications: Vec::new(),
            });
        }

        // Add to channel mapping
        {
            let mut channels = self.channels.write();
            channels.entry(channel.clone())
                .or_insert_with(HashSet::new)
                .insert(id);
        }

        Ok(Subscription {
            id,
            channel,
            manager: Arc::new(PubSubManager {
                subscriptions: Arc::clone(&self.subscriptions),
                channels: Arc::clone(&self.channels),
                pid: self.pid,
            }),
        })
    }

    /// Unsubscribe from a channel
    ///
    /// # Arguments
    /// * `channel` - The channel name to unsubscribe from
    ///
    /// # Returns
    /// The number of subscriptions removed
    pub fn unsubscribe(&self, channel: &str) -> Result<usize> {
        let sub_ids = {
            let mut channels = self.channels.write();
            channels.remove(channel).unwrap_or_default()
        };

        let count = sub_ids.len();

        // Remove all subscriptions for this channel
        {
            let mut subs = self.subscriptions.write();
            for id in sub_ids {
                subs.remove(&id);
            }
        }

        Ok(count)
    }

    /// Unsubscribe by subscription ID (internal)
    fn unsubscribe_by_id(&self, id: Uuid) -> Result<()> {
        let channel = {
            let mut subs = self.subscriptions.write();
            subs.remove(&id).map(|s| s.channel)
        };

        if let Some(channel) = channel {
            let mut channels = self.channels.write();
            if let Some(ids) = channels.get_mut(&channel) {
                ids.remove(&id);
                if ids.is_empty() {
                    channels.remove(&channel);
                }
            }
        }

        Ok(())
    }

    /// Send a notification to a channel
    ///
    /// # Arguments
    /// * `channel` - The channel name
    /// * `payload` - The notification payload (up to 8000 bytes in PostgreSQL)
    ///
    /// # Returns
    /// The number of subscribers that received the notification
    pub fn notify(&self, channel: impl Into<String>, payload: impl Into<String>) -> Result<usize> {
        let channel = channel.into();
        let payload = payload.into();

        // Validate payload size (PostgreSQL limit is 8000 bytes)
        if payload.len() > 8000 {
            return Err(Error::protocol(
                "Notification payload exceeds maximum size of 8000 bytes"
            ));
        }

        let notification = Notification::new(channel.clone(), payload, self.pid);

        // Get all subscribers for this channel
        let sub_ids = {
            let channels = self.channels.read();
            channels.get(&channel).cloned().unwrap_or_default()
        };

        let count = sub_ids.len();

        // Deliver notification to all subscribers
        {
            let mut subs = self.subscriptions.write();
            for id in sub_ids {
                if let Some(state) = subs.get_mut(&id) {
                    state.pending_notifications.push(notification.clone());
                }
            }
        }

        Ok(count)
    }

    /// Poll for notifications on a subscription (internal)
    fn poll_subscription(&self, id: Uuid) -> Result<Vec<Notification>> {
        let mut subs = self.subscriptions.write();
        let state = subs.get_mut(&id)
            .ok_or_else(|| Error::protocol("Invalid subscription"))?;

        // Take all pending notifications
        Ok(std::mem::take(&mut state.pending_notifications))
    }

    /// Get list of active channels
    pub fn list_channels(&self) -> Vec<String> {
        let channels = self.channels.read();
        channels.keys().cloned().collect()
    }

    /// Get subscriber count for a channel
    pub fn subscriber_count(&self, channel: &str) -> usize {
        let channels = self.channels.read();
        channels.get(channel).map(|ids| ids.len()).unwrap_or(0)
    }

    /// Get total number of active subscriptions
    pub fn subscription_count(&self) -> usize {
        let subs = self.subscriptions.read();
        subs.len()
    }
}

impl Default for PubSubManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Pub/Sub adapter trait
///
/// Provides a unified interface for pub/sub operations.
pub trait PubSubAdapter: Send + Sync {
    /// Subscribe to a channel
    fn subscribe(&self, channel: &str) -> Result<Box<dyn SubscriptionHandle>>;

    /// Unsubscribe from a channel
    fn unsubscribe(&self, channel: &str) -> Result<()>;

    /// Send a notification
    fn notify(&self, channel: &str, payload: &str) -> Result<()>;
}

/// Subscription handle trait
pub trait SubscriptionHandle: Send + Sync {
    /// Check for new notifications
    fn poll(&self) -> Result<Vec<Notification>>;

    /// Get the channel name
    fn channel(&self) -> &str;
}

impl SubscriptionHandle for Subscription {
    fn poll(&self) -> Result<Vec<Notification>> {
        Subscription::poll(self)
    }

    fn channel(&self) -> &str {
        Subscription::channel(self)
    }
}

impl PubSubAdapter for PubSubManager {
    fn subscribe(&self, channel: &str) -> Result<Box<dyn SubscriptionHandle>> {
        let sub = PubSubManager::subscribe(self, channel)?;
        Ok(Box::new(sub))
    }

    fn unsubscribe(&self, channel: &str) -> Result<()> {
        PubSubManager::unsubscribe(self, channel)?;
        Ok(())
    }

    fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        PubSubManager::notify(self, channel, payload)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_pubsub_subscribe_notify() -> Result<()> {
        let manager = PubSubManager::new();

        // Subscribe to channel
        let sub = manager.subscribe("test_channel")?;

        // Send notification
        let count = manager.notify("test_channel", "Hello, World!")?;
        assert_eq!(count, 1);

        // Poll for notifications
        let notifications = sub.poll()?;
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].payload, "Hello, World!");
        assert_eq!(notifications[0].channel, "test_channel");
        Ok(())
    }

    #[test]
    fn test_pubsub_multiple_subscribers() -> Result<()> {
        let manager = PubSubManager::new();

        // Create multiple subscriptions
        let sub1 = manager.subscribe("test_channel")?;
        let sub2 = manager.subscribe("test_channel")?;

        // Send notification
        let count = manager.notify("test_channel", "Broadcast")?;
        assert_eq!(count, 2);

        // Both subscribers should receive it
        let notifications1 = sub1.poll()?;
        let notifications2 = sub2.poll()?;

        assert_eq!(notifications1.len(), 1);
        assert_eq!(notifications2.len(), 1);
        assert_eq!(notifications1[0].payload, "Broadcast");
        assert_eq!(notifications2[0].payload, "Broadcast");
        Ok(())
    }

    #[test]
    fn test_pubsub_unsubscribe() -> Result<()> {
        let manager = PubSubManager::new();

        // Subscribe
        let _sub = manager.subscribe("test_channel")?;

        // Verify subscription exists
        assert_eq!(manager.subscriber_count("test_channel"), 1);

        // Unsubscribe
        manager.unsubscribe("test_channel")?;

        // Verify no subscribers
        assert_eq!(manager.subscriber_count("test_channel"), 0);
        Ok(())
    }

    #[test]
    fn test_pubsub_drop_unsubscribes() -> Result<()> {
        let manager = PubSubManager::new();

        {
            let _sub = manager.subscribe("test_channel")?;
            assert_eq!(manager.subscriber_count("test_channel"), 1);
        } // sub dropped here

        // Subscription should be removed
        assert_eq!(manager.subscriber_count("test_channel"), 0);
        Ok(())
    }

    #[test]
    fn test_pubsub_payload_size_limit() {
        let manager = PubSubManager::new();

        // Create a payload larger than 8000 bytes
        let large_payload = "x".repeat(8001);

        let result = manager.notify("test_channel", large_payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_pubsub_multiple_channels() -> Result<()> {
        let manager = PubSubManager::new();

        let sub1 = manager.subscribe("channel1")?;
        let sub2 = manager.subscribe("channel2")?;

        // Send to different channels
        manager.notify("channel1", "Message 1")?;
        manager.notify("channel2", "Message 2")?;

        // Each subscriber only gets their channel's messages
        let notifications1 = sub1.poll()?;
        let notifications2 = sub2.poll()?;

        assert_eq!(notifications1.len(), 1);
        assert_eq!(notifications2.len(), 1);
        assert_eq!(notifications1[0].payload, "Message 1");
        assert_eq!(notifications2[0].payload, "Message 2");
        Ok(())
    }

    #[test]
    fn test_pubsub_list_channels() -> Result<()> {
        let manager = PubSubManager::new();

        let _sub1 = manager.subscribe("channel1")?;
        let _sub2 = manager.subscribe("channel2")?;

        let channels = manager.list_channels();
        assert_eq!(channels.len(), 2);
        assert!(channels.contains(&"channel1".to_string()));
        assert!(channels.contains(&"channel2".to_string()));
        Ok(())
    }
}
