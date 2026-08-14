# Context Engine API Contract

```rust
#[async_trait]
pub trait ContextEngine {
    async fn ensure_indexed(&self, scope: RepoScope) -> Result<IndexStatus, ContextError>;
    async fn search(&self, q: ContextQuery) -> Result<Vec<ContextHit>, ContextError>;
    async fn read(&self, locator: ContextLocator) -> Result<ContextDocument, ContextError>;
    async fn compile(&self, req: CompileContext) -> Result<ContextPacket, ContextError>;
    async fn record_read(&self, read: ReadObservation) -> Result<(), ContextError>;
    async fn write_memory(&self, req: MemoryWrite) -> Result<MemoryRecord, ContextError>;
}
```

`ContextQuery` includes repo/path filters, lexical query, optional embedding vector, symbol hints, desired languages and max candidates. `CompileContext` includes task, goal, provider context limit, output reserve, static-prefix estimate, current diff/error evidence, read-set and explicit pins.

Every `ContextBlock` MUST contain: stable locator, content hash, trust label, token estimate, reason for inclusion, source and freshness. Retrieval caches are invalidated by content hash, never solely by mtime.

Failure of LSP or vector services degrades to lexical/structural retrieval and emits a health event; it is not a total context-engine outage.
