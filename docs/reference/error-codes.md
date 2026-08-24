# Error Taxonomy

`UserInput, Conflict, StaleReference, PolicyDenied, ApprovalRequired, CapabilityExpired, ProviderTransient, ProviderPermanent, ToolInvalid, ToolFailed, SandboxUnavailable, ResourceExhausted, ProcessFailed, VerificationRejected, Corruption, Unsupported, Cancelled, Timeout, Internal`.

Wire `ErrorEnvelope` includes stable code, class, retryability, safe message, optional details artifact and trace ID. Raw provider/process errors may be redacted/artifact-spooled rather than blindly inserted into model context.
