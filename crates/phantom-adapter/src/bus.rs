//! Pub/sub event bus.
//!
//! Apps publish typed data streams (topics) and other apps subscribe
//! to them. Messages are queued and drained per-subscriber each frame.
//! Inspired by ROS topics and Yahoo Pipes.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::adapter::AppId;

/// Opaque topic identifier.
pub type TopicId = u32;

/// Declares a topic an app publishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicDeclaration {
    pub name: String,
    pub data_type: DataType,
}

/// The kind of data flowing through a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    /// Plain text stream.
    Text,
    /// Structured JSON objects.
    Json,
    /// Audio stream (e.g. for speech-to-text).
    Audio,
    /// Image frames.
    Image,
    /// Parsed terminal output.
    TerminalOutput,
    /// Plugin-defined data type.
    Custom(String),
}

/// A message on the event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub topic_id: TopicId,
    pub sender: AppId,
    pub payload: serde_json::Value,
    pub timestamp: u64,
}

/// A registered topic on the bus.
pub struct Topic {
    pub id: TopicId,
    pub name: String,
    pub data_type: DataType,
    pub publisher: AppId,
}

/// The central event bus for inter-app communication.
pub struct EventBus {
    topics: Vec<Topic>,
    subscriptions: HashMap<TopicId, Vec<AppId>>,
    queue: VecDeque<BusMessage>,
    next_topic_id: TopicId,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            topics: Vec::new(),
            subscriptions: HashMap::new(),
            queue: VecDeque::new(),
            next_topic_id: 1,
        }
    }

    /// Register a new topic. Returns the assigned `TopicId`.
    pub fn create_topic(
        &mut self,
        publisher: AppId,
        name: &str,
        data_type: DataType,
    ) -> TopicId {
        let id = self.next_topic_id;
        self.next_topic_id += 1;
        self.topics.push(Topic {
            id,
            name: name.to_string(),
            data_type,
            publisher,
        });
        self.subscriptions.entry(id).or_default();
        id
    }

    /// Remove a topic and all of its subscriptions.
    pub fn remove_topic(&mut self, topic_id: TopicId) {
        self.topics.retain(|t| t.id != topic_id);
        self.subscriptions.remove(&topic_id);
        // Drain any queued messages for this topic.
        self.queue.retain(|m| m.topic_id != topic_id);
    }

    /// Subscribe `subscriber` to `topic_id`.
    pub fn subscribe(&mut self, subscriber: AppId, topic_id: TopicId) {
        let subs = self.subscriptions.entry(topic_id).or_default();
        if !subs.contains(&subscriber) {
            subs.push(subscriber);
        }
    }

    /// Unsubscribe `subscriber` from `topic_id`.
    pub fn unsubscribe(&mut self, subscriber: AppId, topic_id: TopicId) {
        if let Some(subs) = self.subscriptions.get_mut(&topic_id) {
            subs.retain(|&id| id != subscriber);
        }
    }

    /// Remove `app_id` from all subscriptions.
    pub fn unsubscribe_all(&mut self, app_id: AppId) {
        for subs in self.subscriptions.values_mut() {
            subs.retain(|&id| id != app_id);
        }
    }

    /// Enqueue a message.
    pub fn emit(&mut self, message: BusMessage) {
        self.queue.push_back(message);
    }

    /// Drain and return all queued messages whose topic `subscriber` is
    /// subscribed to. Messages are removed from the queue only once all
    /// subscribers have received them (but for simplicity in this initial
    /// implementation we clone for each subscriber and remove when the
    /// last one drains).
    ///
    /// Current strategy: collect matching messages, leave non-matching ones.
    pub fn drain_for(&mut self, subscriber: AppId) -> Vec<BusMessage> {
        // Build the set of topics this subscriber cares about.
        let subscribed_topics: Vec<TopicId> = self
            .subscriptions
            .iter()
            .filter_map(|(&tid, subs)| {
                if subs.contains(&subscriber) {
                    Some(tid)
                } else {
                    None
                }
            })
            .collect();

        if subscribed_topics.is_empty() {
            return Vec::new();
        }

        let mut result = Vec::new();
        let mut remaining = VecDeque::new();

        while let Some(msg) = self.queue.pop_front() {
            if subscribed_topics.contains(&msg.topic_id) {
                result.push(msg);
            } else {
                remaining.push_back(msg);
            }
        }

        self.queue = remaining;
        result
    }

    /// All registered topics.
    pub fn topics(&self) -> &[Topic] {
        &self.topics
    }

    /// Topics filtered by data type.
    pub fn topics_by_type(&self, data_type: &DataType) -> Vec<&Topic> {
        self.topics
            .iter()
            .filter(|t| &t.data_type == data_type)
            .collect()
    }

    /// Subscribers for a given topic.
    pub fn subscribers(&self, topic_id: TopicId) -> Vec<AppId> {
        self.subscriptions
            .get(&topic_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Number of registered topics.
    pub fn topic_count(&self) -> usize {
        self.topics.len()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
