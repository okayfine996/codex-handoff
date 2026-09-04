use crate::{HandoffError, LocalHealth, ProfileMetadata, ProfileName};
use std::{fs, path::Path};

#[derive(Clone)]
pub(crate) struct InventoryEntry {
    pub(crate) name: ProfileName,
    pub(crate) metadata: Option<ProfileMetadata>,
    pub(crate) active: bool,
    pub(crate) health: LocalHealth,
}

pub(crate) fn scan(
    profiles_dir: &Path,
    active: Option<&ProfileName>,
    mut inspect: impl FnMut(&ProfileName) -> (Option<ProfileMetadata>, LocalHealth),
) -> Result<Vec<InventoryEntry>, HandoffError> {
    let mut profiles = Vec::new();
    if !profiles_dir.exists() {
        return Ok(profiles);
    }
    for entry in fs::read_dir(profiles_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| ProfileName::parse(name).ok())
        else {
            continue;
        };
        let (metadata, health) = inspect(&name);
        profiles.push(InventoryEntry {
            active: active == Some(&name),
            name,
            metadata,
            health,
        });
    }
    profiles.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    Ok(profiles)
}
