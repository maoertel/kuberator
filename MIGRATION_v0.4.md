# Migration Guide: v0.3 → v0.4

## Breaking Change: Event API migrated to `events.k8s.io/v1` with deduplication

Kuberator v0.4 replaces the legacy `core/v1/Event` API with `events.k8s.io/v1` and adds built-in event deduplication. Identical events are now collapsed into a single Event object with an incrementing series count instead of flooding the API server with duplicates.

## Why This Change?

Operators that emit events on every reconciliation cycle (e.g., every 15 seconds for persistent errors) create ~240 duplicate events per hour. This floods `kubectl describe` output, loads the API server, and makes it hard to find meaningful events.

**Benefits you gain:**
- **Deduplication**: Identical events within a 6-minute window are PATCHed (incrementing `series.count`) instead of CREATEd
- **Clean output**: `kubectl events` shows `(x52 over 13m)` instead of 52 separate lines
- **Reduced API load**: One PATCH vs one CREATE per repeated event
- **Note truncation**: Messages exceeding the 1024-character `events.k8s.io/v1` limit are automatically truncated with `...`
- **Action defaulting**: The required `action` field defaults to the reason string when not explicitly set

## Migration Steps

### Step 1: Update import

Change the Event type in your `StaticApiProvider` type signatures:

```rust
// Before
use k8s_openapi::api::core::v1::Event;

// After
use k8s_openapi::api::events::v1::Event;
```

This typically affects:
- Your operator setup code where `StaticApiProvider<Event>` is constructed
- Type aliases or struct fields referencing `EventRecorder<StaticApiProvider<Event>>`

### Step 2: Update RBAC / ClusterRole

The `events.k8s.io/v1` API requires its own RBAC rules. Add the new rule alongside the existing one during transition:

```yaml
rules:
  # Legacy core/v1 events (keep during transition)
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["create", "patch"]
  # New events.k8s.io/v1 (required for kuberator v0.4)
  - apiGroups: ["events.k8s.io"]
    resources: ["events"]
    verbs: ["create", "patch"]
```

After all deployments are running v0.4, you can remove the legacy `core/v1` rule.

### Step 3: Update test code

If you use `MockEventRecorder` in tests, update field access:

```rust
// Before (tuple access)
let events = recorder.events();
assert_eq!(events[0].0, "resource-name");  // name
assert_eq!(events[0].1, MyReason::Created); // reason
assert_eq!(events[0].2, "message");         // message

// After (struct fields)
let events = recorder.events();
assert_eq!(events[0].resource_name, "resource-name");
assert_eq!(events[0].reason, MyReason::Created);
assert_eq!(events[0].message, "message");
assert_eq!(events[0].count, 1); // dedup count
```

The mock now deduplicates events the same way the real recorder does: same `resource_name` + `reason` increments `count` instead of creating a new entry.

## New Features

### Event Deduplication

Events with the same (object UID, event type, reason, action) are automatically deduplicated within a 6-minute window. The first occurrence creates a new Event; subsequent identical events PATCH the existing Event's `series.count`.

This happens transparently - no code changes needed in your event emission logic.

### Customizing the Dedup Window

```rust
use std::time::Duration;

let event_recorder = EventRecorder::new(event_api_provider, "my-operator")
    .with_cache_ttl(Duration::from_secs(10 * 60)); // 10 minutes
```

After the TTL expires, the next emission creates a fresh Event object.

### Action Field

The `events.k8s.io/v1` API requires the `action` field. If you don't set it via `with_action()`, it defaults to the reason string (e.g., `"DriftDetected"`, `"ReconciliationFailed"`).

To set a custom action:

```rust
EventData::normal(MyReason::ResourceCreated, "Resource created")
    .with_action("Reconcile")
```

### Note Truncation

Messages longer than 1024 characters are automatically truncated with a `...` suffix, respecting UTF-8 character boundaries. No action needed.

## Viewing Events

Events created via `events.k8s.io/v1` are best viewed with:

```bash
# Recommended - shows events.k8s.io/v1, sorted by time, with dedup counts
kubectl events -n <namespace>

# Filter for a specific resource
kubectl events -n <namespace> --for <kind>/<name>

# Also works - kubectl describe aggregates from both APIs
kubectl describe <kind> <name> -n <namespace>
```

Note: `kubectl get events` only shows legacy `core/v1` events. Use `kubectl events` (without `get`) for the new API.

## Troubleshooting

### "Forbidden" errors in operator logs

Your ClusterRole is missing the `events.k8s.io` RBAC rule. See Step 2 above.

### "action: Required value" errors

You're running kuberator v0.4.0 code but the `action` field isn't being set. This was fixed in the initial release - make sure you're on the latest commit.

### Events not showing in `kubectl get events`

Use `kubectl events` instead. The `get events` command only queries the legacy `core/v1` API.

### Old duplicate events still visible

Events created before the migration (via `core/v1`) remain until they expire (default TTL is 1 hour in most clusters). New events will be deduplicated.
