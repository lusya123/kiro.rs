//! Bounded, in-memory state for the OpenAI Responses compatibility endpoint.
//!
//! The store intentionally contains only replayable, visible conversation
//! messages. System/developer instructions and private reasoning items are not
//! retained, so `previous_response_id` cannot accidentally inherit either.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use super::types::Message;

const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAX_ENTRIES: usize = 256;
const DEFAULT_MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct StoredConversation {
    pub model: String,
    pub session_id: String,
    pub messages: Vec<Message>,
}

impl StoredConversation {
    fn encoded_len(&self) -> usize {
        self.model
            .len()
            .saturating_add(self.session_id.len())
            .saturating_add(
                serde_json::to_vec(&self.messages)
                    .map(|encoded| encoded.len())
                    .unwrap_or(usize::MAX),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreError {
    EntryTooLarge { max_bytes: usize },
}

#[derive(Debug)]
struct Entry {
    conversation: StoredConversation,
    expires_at: Instant,
    last_access: u64,
    encoded_len: usize,
}

#[derive(Debug, Default)]
struct StoreInner {
    entries: HashMap<String, Entry>,
    total_bytes: usize,
    access_clock: u64,
}

/// One store is owned by one [`super::middleware::AppState`]. Because the
/// authentication middleware also authenticates against that state, response
/// ids cannot cross API-key/router boundaries.
#[derive(Debug)]
pub(crate) struct ResponseStore {
    inner: Mutex<StoreInner>,
    ttl: Duration,
    max_entries: usize,
    max_total_bytes: usize,
    max_entry_bytes: usize,
}

impl Default for ResponseStore {
    fn default() -> Self {
        Self::new(
            DEFAULT_TTL,
            DEFAULT_MAX_ENTRIES,
            DEFAULT_MAX_TOTAL_BYTES,
            DEFAULT_MAX_ENTRY_BYTES,
        )
    }
}

impl ResponseStore {
    fn new(
        ttl: Duration,
        max_entries: usize,
        max_total_bytes: usize,
        max_entry_bytes: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            ttl,
            max_entries: max_entries.max(1),
            max_total_bytes: max_total_bytes.max(1),
            max_entry_bytes: max_entry_bytes.min(max_total_bytes).max(1),
        }
    }

    pub(crate) fn validate_size(
        &self,
        conversation: &StoredConversation,
    ) -> Result<(), StoreError> {
        let encoded_len = conversation.encoded_len();
        if encoded_len > self.max_entry_bytes {
            Err(StoreError::EntryTooLarge {
                max_bytes: self.max_entry_bytes,
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn insert(
        &self,
        response_id: String,
        conversation: StoredConversation,
    ) -> Result<(), StoreError> {
        self.insert_at(response_id, conversation, Instant::now())
    }

    pub(crate) fn get(&self, response_id: &str) -> Option<StoredConversation> {
        self.get_at(response_id, Instant::now())
    }

    fn insert_at(
        &self,
        response_id: String,
        conversation: StoredConversation,
        now: Instant,
    ) -> Result<(), StoreError> {
        let encoded_len = conversation.encoded_len().saturating_add(response_id.len());
        if encoded_len > self.max_entry_bytes {
            return Err(StoreError::EntryTooLarge {
                max_bytes: self.max_entry_bytes,
            });
        }

        let mut inner = self.inner.lock();
        Self::purge_expired(&mut inner, now);
        if let Some(replaced) = inner.entries.remove(&response_id) {
            inner.total_bytes = inner.total_bytes.saturating_sub(replaced.encoded_len);
        }

        while inner.entries.len() >= self.max_entries
            || inner.total_bytes.saturating_add(encoded_len) > self.max_total_bytes
        {
            let Some(oldest_id) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&oldest_id) {
                inner.total_bytes = inner.total_bytes.saturating_sub(evicted.encoded_len);
            }
        }

        inner.access_clock = inner.access_clock.wrapping_add(1);
        let last_access = inner.access_clock;
        inner.total_bytes = inner.total_bytes.saturating_add(encoded_len);
        inner.entries.insert(
            response_id,
            Entry {
                conversation,
                expires_at: now + self.ttl,
                last_access,
                encoded_len,
            },
        );
        Ok(())
    }

    fn get_at(&self, response_id: &str, now: Instant) -> Option<StoredConversation> {
        let mut inner = self.inner.lock();
        Self::purge_expired(&mut inner, now);
        inner.access_clock = inner.access_clock.wrapping_add(1);
        let access_clock = inner.access_clock;
        let entry = inner.entries.get_mut(response_id)?;
        entry.last_access = access_clock;
        Some(entry.conversation.clone())
    }

    fn purge_expired(inner: &mut StoreInner, now: Instant) {
        let expired = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(entry) = inner.entries.remove(&id) {
                inner.total_bytes = inner.total_bytes.saturating_sub(entry.encoded_len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversation(label: &str) -> StoredConversation {
        StoredConversation {
            model: "gpt-5.6-sol".to_string(),
            session_id: "00000000-0000-4000-8000-000000000001".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: json!(label),
            }],
        }
    }

    #[test]
    fn entries_expire_without_sliding_the_ttl() {
        let store = ResponseStore::new(Duration::from_secs(5), 4, 4096, 4096);
        let started = Instant::now();
        store
            .insert_at("resp_one".to_string(), conversation("one"), started)
            .unwrap();
        assert!(
            store
                .get_at("resp_one", started + Duration::from_secs(4))
                .is_some()
        );
        assert!(
            store
                .get_at("resp_one", started + Duration::from_secs(5))
                .is_none()
        );
    }

    #[test]
    fn entry_count_uses_lru_eviction() {
        let store = ResponseStore::new(Duration::from_secs(60), 2, 4096, 4096);
        let started = Instant::now();
        store
            .insert_at("resp_one".to_string(), conversation("one"), started)
            .unwrap();
        store
            .insert_at("resp_two".to_string(), conversation("two"), started)
            .unwrap();
        assert!(store.get_at("resp_one", started).is_some());
        store
            .insert_at("resp_three".to_string(), conversation("three"), started)
            .unwrap();
        assert!(store.get_at("resp_one", started).is_some());
        assert!(store.get_at("resp_two", started).is_none());
        assert!(store.get_at("resp_three", started).is_some());
    }

    #[test]
    fn byte_limits_reject_oversized_entries_and_bound_total_state() {
        let small = conversation("small");
        let small_len = small.encoded_len();
        let retained_entry_len = small_len + "resp_three".len();
        let store = ResponseStore::new(
            Duration::from_secs(60),
            8,
            retained_entry_len * 2,
            retained_entry_len,
        );
        store.insert("resp_one".to_string(), small.clone()).unwrap();
        store.insert("resp_two".to_string(), small.clone()).unwrap();
        store
            .insert("resp_three".to_string(), small.clone())
            .unwrap();
        assert!(store.get("resp_one").is_none());
        assert!(store.get("resp_two").is_some());
        assert!(store.get("resp_three").is_some());

        let oversized = conversation(&"x".repeat(small_len * 2));
        assert!(matches!(
            store.insert("resp_large".to_string(), oversized),
            Err(StoreError::EntryTooLarge { .. })
        ));
    }

    #[test]
    fn separate_stores_cannot_read_each_others_response_ids() {
        let first = ResponseStore::default();
        let second = ResponseStore::default();
        first
            .insert("resp_private".to_string(), conversation("secret"))
            .unwrap();
        assert!(first.get("resp_private").is_some());
        assert!(second.get("resp_private").is_none());
    }
}
