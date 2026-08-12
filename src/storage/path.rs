//! Validated filesystem path relationships.

use std::path::Path;

use crate::{Result, WombatError};

pub(crate) fn parent(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        WombatError::configuration(format!("path `{}` has no parent", path.display()))
    })
}
