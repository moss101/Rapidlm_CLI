# LLM Provider and Router Contract

Provider adapters expose capabilities; the router owns policy.

```rust
pub trait ProviderAdapter {
    fn capabilities(&self) -> ProviderCapabilities;
    async fn invoke(&self, req: CanonicalModelRequest, cancel: CancellationToken) -> Result<ModelStream, ProviderError>;
}

pub struct RouteRequest {
    pub purpose: ModelPurpose,
    pub privacy: PrivacyClass,
    pub min_context: u32,
    pub required: ModelCapabilities,
    pub latency_slo_ms: Option<u64>,
    pub budget_remaining_usd: Option<Decimal>,
    pub user_pin: Option<ModelRef>,
}
```

Routing first applies hard filters (availability, capability, privacy/region, context, user pin, policy), then scores eligible models using a versioned formula. Every selection records `router_policy_version` and score components. Fallback may not violate a hard constraint and stops on auth/config/safety failures unless an explicitly configured alternative is permitted.

Usage accounting distinguishes cached input, uncached input, output, reasoning where exposed, and provider-reported cost or locally calculated estimate.
