use crate::{HandoffError, ProfileName, private_dir, private_file};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

/// A typed RAII guard for a profile runtime lock.
///
/// Shared leases are held by isolated Codex runs. Mutating or refreshing a
/// profile requires an exclusive lease, so dropping this value is the only
/// way to release that activity claim.
pub(crate) struct ActivityLease {
    _file: File,
}

pub(crate) fn acquire_shared(
    locks_dir: &Path,
    lock_path: &Path,
    profile: &ProfileName,
) -> Result<ActivityLease, HandoffError> {
    let file = open(locks_dir, lock_path)?;
    file.try_lock_shared()
        .map_err(|_| HandoffError::ProfileBusy(profile.as_str().into()))?;
    Ok(ActivityLease { _file: file })
}

pub(crate) fn acquire_exclusive(
    locks_dir: &Path,
    lock_path: &Path,
    profile: &ProfileName,
) -> Result<ActivityLease, HandoffError> {
    let file = open(locks_dir, lock_path)?;
    file.try_lock_exclusive()
        .map_err(|_| HandoffError::ProfileBusy(profile.as_str().into()))?;
    Ok(ActivityLease { _file: file })
}

fn open(locks_dir: &Path, lock_path: &Path) -> Result<File, HandoffError> {
    private_dir(locks_dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    private_file(&file)?;
    Ok(file)
}
