# Kernel API Contract

All frontends depend on `KernelClient`; no TUI-only mutation path exists.

```rust
#[async_trait]
pub trait KernelClient {
    async fn create_session(&self, req: CreateSession) -> Result<SessionSnapshot, ApiError>;
    async fn get_session(&self, id: SessionId) -> Result<SessionSnapshot, ApiError>;
    async fn submit_turn(&self, req: SubmitTurn) -> Result<TurnHandle, ApiError>;
    async fn interrupt(&self, req: Interrupt) -> Result<(), ApiError>;
    async fn subscribe(&self, req: SubscribeEvents) -> Result<EventStream, ApiError>;
    async fn approve(&self, req: ResolveApproval) -> Result<(), ApiError>;
    async fn fork_session(&self, req: ForkSession) -> Result<SessionSnapshot, ApiError>;
    async fn rewind(&self, req: RewindSession) -> Result<RewindResult, ApiError>;
}
```

## Concurrency

`submit_turn` uses `expected_seq` and fails with `session.conflict` if another writer advanced the session. Only one foreground turn runs per session. Interrupt is idempotent.

## Transport

In-process, local IPC, WebSocket and SDK transports preserve the same request/response semantics. Transport disconnect does not cancel daemon-owned jobs unless the call explicitly selected `lifetime=client`.
