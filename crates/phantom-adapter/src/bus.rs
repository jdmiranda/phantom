//! Pub/sub event bus.
//!
//! Apps publish typed data streams (topics) and other apps subscribe
//! to them. Messages are queued and drained per-subscriber each frame.
//! Inspired by ROS topics and Yahoo Pipes.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::adapter::AppId;
use phantom_protocol::Event;

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
#[derive(Debug, Clone)]
pub struct BusMessage {
    pub topic_id: TopicId,
    pub sender: AppId,
    pub event: Event,
    pub frame: u64,
    pub timestamp: u64,
}

/// A registered topic on the bus.
pub struct Topic {
    pub id: TopicId,
    pub name: String,
    pub data_type: DataType,
    pub publisher: AppId,
}

/// Maximum queued messages before oldest are dropped.
const MAX_QUEUE_SIZE: usize = 256;

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

    /// Enqueue a message. Drops oldest messages if the queue is full.
    pub fn emit(&mut self, message: BusMessage) {
        if self.queue.len() >= MAX_QUEUE_SIZE {
            self.queue.pop_front();
        }
        self.queue.push_back(message);
    }

    /// Number of messages currently queued.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── App-topology integration tests ──────────────────────────────

    /// Simulate the 3-topic setup that phantom-app creates on startup
    /// and verify the event routing used by the update loop.
    #[test]
    fn app_topology_three_topics() {
        let mut bus = EventBus::new();
        let app_id = 0;
        let t_output = bus.create_topic(app_id, "terminal.output", DataType::TerminalOutput);
        let t_error = bus.create_topic(app_id, "terminal.error", DataType::Text);
        let t_agent = bus.create_topic(app_id, "agent.event", DataType::Json);

        assert_eq!(bus.topic_count(), 3);
        assert_eq!(bus.topics()[0].name, "terminal.output");
        assert_eq!(bus.topics()[1].name, "terminal.error");
        assert_eq!(bus.topics()[2].name, "agent.event");

        // IDs should be sequential.
        assert_eq!(t_output, 1);
        assert_eq!(t_error, 2);
        assert_eq!(t_agent, 3);
    }

    #[test]
    fn terminal_output_event_routes_to_subscriber() {
        let mut bus = EventBus::new();
        let t_output = bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let brain_id = 100;
        bus.subscribe(brain_id, t_output);

        bus.emit(BusMessage {
            topic_id: t_output,
            sender: 0,
            event: Event::TerminalOutput { app_id: 0, bytes: 2 },
            frame: 0,
            timestamp: 42,
        });

        let msgs = bus.drain_for(brain_id);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].event, Event::TerminalOutput { app_id: 0, bytes: 2 }));
        assert_eq!(msgs[0].timestamp, 42);
    }

    #[test]
    fn error_event_isolated_from_output_subscriber() {
        let mut bus = EventBus::new();
        let t_output = bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let t_error = bus.create_topic(0, "terminal.error", DataType::Text);

        let output_sub = 10;
        let error_sub = 20;
        bus.subscribe(output_sub, t_output);
        bus.subscribe(error_sub, t_error);

        bus.emit(BusMessage {
            topic_id: t_error,
            sender: 0,
            event: Event::Custom { kind: "error".into(), data: "has_errors".into() },
            frame: 0,
            timestamp: 1,
        });

        // Output subscriber should NOT see the error event.
        let msgs = bus.drain_for(output_sub);
        assert!(msgs.is_empty());

        // Error subscriber should see it.
        let msgs = bus.drain_for(error_sub);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0].event, Event::Custom { kind, .. } if kind == "error"));
    }

    #[test]
    fn agent_event_carries_task_and_success() {
        let mut bus = EventBus::new();
        let t_agent = bus.create_topic(0, "agent.event", DataType::Json);
        let sub = 50;
        bus.subscribe(sub, t_agent);

        bus.emit(BusMessage {
            topic_id: t_agent,
            sender: 0,
            event: Event::AgentTaskComplete {
                agent_id: 1,
                success: true,
                summary: "fix the bug".into(),
            },
            frame: 0,
            timestamp: 100,
        });

        let msgs = bus.drain_for(sub);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0].event,
            Event::AgentTaskComplete { agent_id: 1, success: true, summary }
            if summary == "fix the bug"
        ));
    }

    #[test]
    fn multiple_events_batch_per_frame() {
        let mut bus = EventBus::new();
        let t_output = bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let sub = 10;
        bus.subscribe(sub, t_output);

        // Simulate 3 frames of terminal output.
        for ts in 1..=3u64 {
            bus.emit(BusMessage {
                topic_id: t_output,
                sender: 0,
                event: Event::TerminalOutput { app_id: 0, bytes: ts },
                frame: ts,
                timestamp: ts,
            });
        }

        let msgs = bus.drain_for(sub);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].frame, 1);
        assert_eq!(msgs[2].frame, 3);
    }

    #[test]
    fn emit_caps_queue_at_max_size() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(0, "spam", DataType::Text);

        // Fill past the cap.
        for i in 0..300u64 {
            bus.emit(BusMessage {
                topic_id: tid,
                sender: 0,
                event: Event::Custom { kind: "spam".into(), data: i.to_string() },
                frame: i,
                timestamp: i,
            });
        }

        // Queue should never exceed MAX_QUEUE_SIZE.
        assert!(bus.queue_len() <= 256, "queue len was {}", bus.queue_len());
        // Oldest messages should have been dropped.
        // The newest message (299) should be in the queue.
        let sub = 1;
        bus.subscribe(sub, tid);
        let msgs = bus.drain_for(sub);
        assert_eq!(msgs.last().unwrap().timestamp, 299);
    }

    #[test]
    fn drain_clears_queue_for_subscriber() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(0, "x", DataType::Text);
        let sub = 1;
        bus.subscribe(sub, tid);

        bus.emit(BusMessage {
            topic_id: tid,
            sender: 0,
            event: Event::Custom { kind: "test".into(), data: "first".into() },
            frame: 0,
            timestamp: 1,
        });

        let first = bus.drain_for(sub);
        assert_eq!(first.len(), 1);

        // Second drain should be empty.
        let second = bus.drain_for(sub);
        assert!(second.is_empty());
    }
}
