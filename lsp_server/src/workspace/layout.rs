//! Remapping ownership rules for dependencies: which package's remapping
//! table a dependency compiles against, and how the owner's and the
//! dependency's tables merge.

use camino::{Utf8Path, Utf8PathBuf};

use super::Remapping;

/// Remappings that apply to a dependency: those of the most specific
/// non-dependency package enclosing it, or the workspace-root fallback when the
/// dependency is hoisted outside every package.
pub(super) fn owner_remappings<'a>(
    dependency_root: &Utf8Path,
    owners: &'a [(Utf8PathBuf, Vec<Remapping>)],
    fallback: &'a [Remapping],
) -> &'a [Remapping] {
    owners
        .iter()
        .filter(|(owner_root, _)| dependency_root.starts_with(owner_root))
        .max_by_key(|(owner_root, _)| owner_root.components().count())
        .map(|(_, remappings)| remappings.as_slice())
        .unwrap_or(fallback)
}

pub(super) fn merge_remappings(root: &[Remapping], dependency: Vec<Remapping>) -> Vec<Remapping> {
    let mut remappings = root.to_vec();
    for remapping in dependency {
        let claimed = root.iter().any(|root| {
            root.prefix == remapping.prefix
                && (root.context.is_none() || root.context == remapping.context)
        });
        if !claimed && !remappings.contains(&remapping) {
            remappings.push(remapping);
        }
    }
    remappings
}

