//! Dateiformat-Adapter für das Open Knowledge Format (OKF) v0.2 — ein
//! Verzeichnis von Markdown-Dateien mit YAML-Frontmatter.
//!
//! Enthält bewusst einen eigenen, minimalen YAML-Adapter ([`yaml`]) statt
//! einer Fremd-Dependency: das Crate hat keine Speicher-Abhängigkeit und
//! keinen C-Compiler im Build (siehe `agentkit_graph/README.md`), eine
//! YAML-Crate mit eigenem Parser-Unterbau würde diese Null-Dependency-
//! Eigenschaft — und damit den statischen musl-Build — aufgeben.

pub mod bundle;
pub mod doc;
pub mod instant;
pub mod markdown;
pub mod yaml;
