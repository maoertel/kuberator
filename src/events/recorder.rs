use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::events::types::{EventData, Reason};
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
            namespace: Some(namespace.clone()),
            uid: object.meta().uid.clone(),
            resource_version: object.meta().resource_version.clone(),
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

        let k8s_event = K8sEvent {
            metadata: ObjectMeta {
                name: Some(event_name.clone()),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            event_time: Some(MicroTime(now)),
            reporting_controller: Some(self.component.to_string()),
            reporting_instance: Some(self.component.to_string()),
            regarding: Some(regarding),
            action: event.action,
            reason: Some(event.reason.to_string()),
            note: Some(event.message),
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
