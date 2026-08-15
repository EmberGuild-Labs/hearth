//! Where Hearth keeps the key it signs provenance with.
//!
//! Signing is optional throughout. A file with an unsigned provenance chain
//! is still tamper-evident — the entries are hash-linked — it simply cannot
//! prove *who* made each change. Hearth therefore never demands a key and
//! never generates one behind the user's back: it signs if a key is
//! configured and says so plainly when it is not, because a tool that
//! silently creates an identity has made a decision that was not its to make.

use anyhow::{Context, Result};
use libwick::Identity;
use std::path::PathBuf;

/// `$HEARTH_KEY` holds a hex secret key directly, for CI and containers
/// where there is no home directory worth writing to.
const ENV_KEY: &str = "HEARTH_KEY";

pub fn key_path() -> PathBuf {
    if let Ok(p) = std::env::var("HEARTH_KEY_FILE") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("hearth").join("identity.key")
}

/// Load the configured identity, or `None` if there is not one.
pub fn load() -> Result<Option<Identity>> {
    if let Ok(hex) = std::env::var(ENV_KEY) {
        return Ok(Some(
            Identity::from_secret_hex(&hex).context(format!("${ENV_KEY} is not a valid key"))?,
        ));
    }
    let path = key_path();
    if !path.exists() {
        return Ok(None);
    }
    let hex = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    Ok(Some(Identity::from_secret_hex(&hex).with_context(
        || format!("{} is not a valid key", path.display()),
    )?))
}

/// Create a key and store it. Refuses to replace an existing one, because
/// overwriting a signing key silently orphans every signature ever made with
/// it.
pub fn generate() -> Result<(Identity, PathBuf)> {
    let path = key_path();
    anyhow::ensure!(
        !path.exists(),
        "{} already exists. Delete it deliberately if you mean to replace your identity — \
         every signature made with the old key becomes unverifiable",
        path.display()
    );
    let id = Identity::generate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, id.secret_hex())?;
    restrict(&path)?;
    Ok((id, path))
}

/// Owner-only permissions. A signing key readable by every process on the
/// machine is not an identity, it is a shared secret.
#[cfg(unix)]
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
