# Capability Broker and Policy API

## Normalize → decide → approve if needed → lease → execute → audit

```rust
pub struct ActionRequest {
    pub principal: PrincipalRef,
    pub session_id: SessionId,
    pub capability: Capability,
    pub resource: ResourceDescriptor,
    pub normalized_action: CanonicalAction,
    pub reason: String,
}

pub enum Decision { Allow(LeaseConstraints), Ask(ApprovalSpec), Deny(DenyReason) }

#[async_trait]
pub trait CapabilityBroker {
    async fn evaluate(&self, req: ActionRequest) -> Result<Decision, PolicyError>;
    async fn issue(&self, approved: ApprovedAction) -> Result<CapabilityLease, PolicyError>;
    async fn validate_use(&self, lease: &CapabilityLease, actual: &CanonicalAction) -> Result<(), PolicyError>;
}
```

The canonical action resolves command executable/path, normalized argv, effective cwd, resolved filesystem targets, URL origin/IP class, environment variable names, target MCP server/tool and sandbox tier. The action hash is calculated only after normalization.

A lease default is one use and <=60 seconds. Approval UI may issue narrower/broader repeated leases only from predefined safe scopes. Executors fail closed if broker is unavailable or policy revision changed incompatibly.
