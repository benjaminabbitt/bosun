//! bosun I/O: read the manifest and user config through `annals-yaml`
//! (strict YAML, comment-preserving, versioned loader, §4.4); read and write
//! the lockfile (§4.5) and machine state (§4.6) as JSON; and resolve the
//! per-platform config/state paths (§4.4, §4.6).
