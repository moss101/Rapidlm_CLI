# Bounded image regions — REF-014 complete

Pinned Kimi supports regions in original-image pixels and full-resolution requests, with bounded compression and animation handling. RapidLM `workspace_read` supports region, full-resolution, maximum-edge and byte-budget options while retaining its path permission check and inline vision result. Normal static images over the byte or edge budget use the transform path; unsupported animation is reported rather than silently flattened.

Source bytes, dimensions, decoded pixels and output are bounded. PNG, JPEG and static WebP transforms apply EXIF orientation before region coordinates and retain alpha in PNG output. Full-resolution requests fail explicitly when the output exceeds budget; ordinary requests try bounded quality and size steps. Decoded-pixel, orientation, animation, budget and configured-binary vision/non-vision tests passed, along with formatting and Rapid all-target clippy. Full workspace and platform acceptance remains REF-027.
