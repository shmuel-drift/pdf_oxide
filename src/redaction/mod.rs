//! True / destructive redaction (#231).
//!
//! Replaces the prior *cosmetic* redaction (a filled rectangle drawn over
//! content whose underlying bytes survived) with physical content removal
//! per ISO 32000-1:2008 §12.5.6.23: *"shall remove all traces of the
//! specified content … clipping or image masks shall not be used to hide
//! that data."*
//!
//! `apply_redactions_destructive` removes intersecting vector text and
//! burns JPEG/Flate image pixels under each rectangle, then paints an
//! opaque overlay. Path prune is still planning-only (`path_prune`).
//! Document sanitization (`sanitize_catalog` / `sanitize_document`) is a
//! **separate** API — apply does not run it.
//!
//! One responsibility per submodule (SRP). The geometric region model is
//! the shared input to text, image, and overlay.

#![forbid(unsafe_code)]

pub mod classify;
pub mod engine;
pub mod font_scrub;
pub mod image_burn;
pub mod image_prune;
pub mod image_walk;
pub mod options;
pub mod overlay;
pub mod path_prune;
pub mod region;
pub mod sanitize;
pub mod serialize;
pub mod text_engine;
pub mod text_prune;

pub use classify::Classification;
pub use engine::{redact_content_stream, FontInfoMetrics};
pub use options::{OcgPolicy, RedactionOptions, RedactionReport};
pub use region::{RedactionRegion, RegionSet, DEFAULT_EDGE_PADDING};
pub use sanitize::{sanitize_catalog, CatalogScrub, SanitizeCounts};
