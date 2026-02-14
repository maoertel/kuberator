use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::api::events::v1::Event as K8sEvent;
use k8s_openapi::api::events::v1::EventSeries;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::Patch;
use kube::api::PatchParams;
use kube::api::PostParams;
use kube::Resource;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::cache::ProvideApi;
use crate::error::Result;
use crate::events::types::EventData;
use crate::events::types::Reason;
use crate::events::EmitEvent;
use crate::TryResource;

/// Implementation of EmitEvent that creates Kubernetes Event resources using the
/// `events.k8s.io/v1` API with built-in deduplication.
///
/// First occurrence of an event creates a new Event object. Subsequent identical
/// events (same object UID, type, reason, and action) increment the series count
/// via a PATCH instead of creating duplicate events.
///
/// Cache entries expire after a configurable TTL (default: 6 minutes), after which
/// the next emission creates a fresh Event.
pub struct EventRecorder<P>
where
    P: ProvideApi<K8sEvent> + Send + Sync,
{
    api_provider: Arc<P>,
    component: Cow<'static, str>,
    cache: Mutex<HashMap<EventKey, CachedEvent>>,
    cache_ttl: Duration,
}

#[async_trait]
impl<P, R> EmitEvent<R> for EventRecorder<P>
where
    P: ProvideApi<K8sEvent> + Send + Sync,
    R: Reason,
{
    #[tracing::instrument(
        skip(self, object),
        fields(
            object_kind = %K::kind(&()),
            object_name = %object.try_name().unwrap_or_default(),
            object_namespace = %object.try_namespace().unwrap_or_default(),
            event_type = %event.type_,
            event_reason = %event.reason,
        )
    )]
    async fn try_emit<K>(&self, object: &K, event: EventData<R>) -> Result<()>
    where
        K: Resource<DynamicType = ()> + TryResource + Clone + Send + Sync,
    {
        let namespace = object.try_namespace()?;
        let name = object.try_name()?;
        let uid = object.meta().uid.to_owned().unwrap_or_default();
        let key = EventKey::new(uid, &event);

        let cached = self.lookup_cached(&key).await;
        let events_api = self.api_provider.get(&namespace)?;

        if let Some(cached) = cached {
            return self.patch_existing(&events_api, &key, &cached).await;
        }

        let regarding = ObjectReference {
            api_version: Some(K::api_version(&()).to_string()),
            kind: Some(K::kind(&()).to_string()),
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            uid: object.meta().uid.to_owned(),
            resource_version: object.meta().resource_version.to_owned(),
            ..Default::default()
        };
        self.create_new(&events_api, key, event, name, &namespace, regarding)
            .await
    }
}

impl<P> EventRecorder<P>
where
    P: ProvideApi<K8sEvent> + Send + Sync,
{
    const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(6 * 60);
    /// Maximum length for the `note` field in `events.k8s.io/v1` Events.
    const MAX_NOTE_LENGTH: usize = 1024;

    /// Create a new EventRecorder
    ///
    /// # Arguments
    /// * `api_provider` - API provider for Event resources
    /// * `component` - Component name that will appear in events (e.g., "my-operator")
    pub fn new(api_provider: Arc<P>, component: impl Into<Cow<'static, str>>) -> Self {
        Self {
            api_provider,
            component: component.into(),
            cache: Mutex::new(HashMap::new()),
            cache_ttl: Self::DEFAULT_CACHE_TTL,
        }
    }

    /// Set a custom cache TTL for deduplication.
    ///
    /// Events with the same key emitted within this window are deduplicated.
    /// After expiry, the next emission creates a new Event object.
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Look up a cached event by key, evicting expired entries first.
    async fn lookup_cached(&self, key: &EventKey) -> Option<CachedEvent> {
        let mut cache = self.cache.lock().await;
        let ttl = self.cache_ttl;
        cache.retain(|_, v| v.last_emitted.elapsed() < ttl);
        cache.get(key).cloned()
    }

    async fn patch_existing(
        &self,
        events_api: &kube::Api<K8sEvent>,
        key: &EventKey,
        cached: &CachedEvent,
    ) -> Result<()> {
        let new_count = cached.count + 1;
        let now = Utc::now();

        let patch = EventSeriesPatch {
            series: EventSeries {
                count: new_count,
                last_observed_time: MicroTime(now),
            },
        };

        events_api
            .patch(&cached.event_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;

        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.get_mut(key) {
            entry.count = new_count;
            entry.last_emitted = Instant::now();
        }

        Ok(())
    }

    async fn create_new<R: Reason>(
        &self,
        events_api: &kube::Api<K8sEvent>,
        key: EventKey,
        event: EventData<R>,
        name: &str,
        namespace: &str,
        regarding: ObjectReference,
    ) -> Result<()> {
        let now = Utc::now();
        let event_name = format!("{name}.{:x}", now.timestamp_micros() as u64);

        let action = event.action.unwrap_or_else(|| event.reason.to_string());
        let note = truncate_note(event.message, Self::MAX_NOTE_LENGTH);

        let k8s_event = K8sEvent {
            metadata: ObjectMeta {
                name: Some(event_name.to_owned()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            event_time: Some(MicroTime(now)),
            reporting_controller: Some(self.component.to_string()),
            reporting_instance: Some(self.component.to_string()),
            regarding: Some(regarding),
            action: Some(action),
            reason: Some(event.reason.to_string()),
            note: Some(note),
            type_: Some(event.type_.to_string()),
            ..Default::default()
        };

        events_api.create(&PostParams::default(), &k8s_event).await?;

        let mut cache = self.cache.lock().await;
        cache.insert(
            key,
            CachedEvent {
                event_name,
                count: 1,
                last_emitted: Instant::now(),
            },
        );

        Ok(())
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct EventKey {
    object_uid: String,
    event_type: Cow<'static, str>,
    reason: String,
    action: Option<String>,
}

impl EventKey {
    fn new<R: Reason>(uid: String, event: &EventData<R>) -> Self {
        let event_type: &'static str = event.type_.into();
        Self {
            object_uid: uid,
            event_type: Cow::Borrowed(event_type),
            reason: event.reason.to_string(),
            action: event.action.to_owned(),
        }
    }
}

#[derive(Clone)]
struct CachedEvent {
    event_name: String,
    count: i32,
    last_emitted: Instant,
}

/// Patch payload containing only the series field for merge-patching existing events.
#[derive(Debug, Serialize)]
struct EventSeriesPatch {
    series: EventSeries,
}

/// Truncate a message to fit within the `events.k8s.io/v1` note field limit.
/// Appends "..." when truncation occurs, preserving char boundaries.
fn truncate_note(message: String, max_len: usize) -> String {
    if message.len() <= max_len {
        return message;
    }

    let suffix = "...";
    let truncate_at = max_len - suffix.len();
    let boundary = message
        .char_indices()
        .take_while(|(i, _)| *i <= truncate_at)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);

    format!("{}{suffix}", &message[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_note_short_message_unchanged() {
        // Given a message within the limit
        let message = "Short message".to_string();

        // When truncating with a generous limit
        let result = truncate_note(message.clone(), 1024);

        // Then the message is unchanged
        assert_eq!(result, message);
    }

    #[test]
    fn truncate_note_exact_limit_unchanged() {
        // Given a message exactly at the limit
        let message = "a".repeat(1024);

        // When truncating
        let result = truncate_note(message.clone(), 1024);

        // Then the message is unchanged
        assert_eq!(result, message);
    }

    #[test]
    fn truncate_note_over_limit_adds_ellipsis() {
        // Given a message exceeding the limit
        let message = "a".repeat(1025);

        // When truncating
        let result = truncate_note(message, 1024);

        // Then the result is truncated with "..." and fits within limit
        assert_eq!(result.len(), 1024);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_note_preserves_char_boundaries() {
        // Given a message with multi-byte characters near the boundary
        let message = format!("{}ä{}", "a".repeat(1022), "b".repeat(10));

        // When truncating
        let result = truncate_note(message, 1024);

        // Then the result is valid UTF-8 and within limit
        assert!(result.len() <= 1024);
        assert!(result.ends_with("..."));
    }
}
