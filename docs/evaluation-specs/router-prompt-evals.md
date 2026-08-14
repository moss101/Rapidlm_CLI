# LLM Router and Prompt Evaluation Specification

## Router tests
Use a synthetic model catalog with deterministic cost, latency, region, context and capability metadata. Property-test that hard constraints are never violated and score changes only affect eligible models. Verify fallback order and policy-version recording.

## Prompt bundle tests
Compile system prompts with fixture roles/capabilities and snapshot the structured sections. Validate that untrusted repository/web/MCP content never lands in system/developer instruction slots. Tool descriptions remain byte-stable for a session unless provider negotiation forces a version change.

## Model experiments
For prompt changes run paired A/B on fixed eval cases using the same model snapshot when available. Compare success, false completion, unnecessary questions, tool calls, context tokens and policy incidents. A prompt change is not accepted based on anecdotal examples.
