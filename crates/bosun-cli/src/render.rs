//! `bosun render` (§8.3): the versions a container build installs, as build
//! args, env lines or a JSON versions file. For this machine they come from
//! its lock, which must be there and current.

use std::path::PathBuf;

use bosun_apply::render::{render, Rendered};
use bosun_core::Target;
use bosun_io::manifest_hash;

use crate::commands::{canonical_manifest, load, manifest_file, this_lock};
use crate::env::Env;
use crate::errmsg;
use crate::error::CliError;

/// The manifest at `path` rendered for this machine, from its lock (§8.3); a
/// missing lock, or one the manifest has moved on from, is an error naming
/// `bosun lock`.
pub fn this_machine(env: &Env, path: Option<PathBuf>) -> Result<(Target, Rendered), CliError> {
    let file = manifest_file(path)?;
    let manifest = load(env, &file)?;
    let claim_path = canonical_manifest(&file);
    let lock = this_lock(env, &claim_path)?.ok_or_else(|| {
        CliError::Resolve(format!(
            "{} {}; {}",
            errmsg::RENDER_NO_LOCK,
            file.display(),
            errmsg::RUN_LOCK
        ))
    })?;
    if lock.manifest_hash != manifest_hash(&manifest) {
        return Err(CliError::Resolve(format!(
            "{} {}; {}",
            file.display(),
            errmsg::RENDER_STALE_LOCK,
            errmsg::RUN_LOCK
        )));
    }
    let target = env.target();
    let rendered =
        render(&manifest, &[(target, &lock)]).map_err(|e| CliError::Resolve(e.to_string()))?;
    Ok((target, rendered))
}

/// The JSON versions file (§8.3).
pub fn json(rendered: &Rendered) -> String {
    serde_json::to_string_pretty(rendered).expect("output serialization is infallible")
}
