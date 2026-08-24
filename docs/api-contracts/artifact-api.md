# API Contract — Artifact and Provenance API

CAS write/read/metadata/provenance/retention and typed presentation.

## Types

### `ArtifactId`
sha256 digest

### `ArtifactMeta`
type,mime,size,producer,trust,retention

### `ProvenanceEdge`
source/derived/change/verify relationships

## Operations

- `artifact.put/read/meta/pin/unpin`
- `artifact.provenance`
- `artifact.verify_digest`
- `artifact.export`

## Error/recovery semantics

Partial writes staged then atomically committed; digest mismatch is corruption and invalidates dependent evidence.

## Versioning/compatibility

Artifact metadata schema additive; immutable bytes never mutate under same digest.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
