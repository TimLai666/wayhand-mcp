use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use rustix::{
    fs::{FlockOperation, flock},
    io::Errno,
};

#[derive(Debug)]
pub(crate) enum InstanceLockError {
    AlreadyHeld(PathBuf),
    Other(anyhow::Error),
}

pub(crate) struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub(crate) fn acquire() -> Result<Self, InstanceLockError> {
        let uid = unsafe { libc::geteuid() } as u32;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty());
        Self::acquire_at(lock_path(runtime_dir.as_deref(), uid))
    }

    fn acquire_at(path: PathBuf) -> Result<Self, InstanceLockError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                InstanceLockError::Other(anyhow!("open instance lock {}: {error}", path.display()))
            })?;

        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) if error == Errno::WOULDBLOCK => Err(InstanceLockError::AlreadyHeld(path)),
            Err(error) => Err(InstanceLockError::Other(anyhow!(
                "lock instance file {}: {error}",
                path.display()
            ))),
        }
    }
}

fn lock_path(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    runtime_dir.map_or_else(
        || PathBuf::from(format!("/tmp/wayhand-mcp-{uid}.lock")),
        |runtime_dir| Path::new(runtime_dir).join("wayhand-mcp.lock"),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{InstanceLock, InstanceLockError, lock_path};

    fn temp_lock_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("wayhand-mcp-test-{nonce}.lock"))
    }

    #[test]
    fn chooses_runtime_directory_or_uid_fallback() {
        assert_eq!(
            lock_path(Some(OsStr::new("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/wayhand-mcp.lock")
        );
        assert_eq!(
            lock_path(None, 1000),
            PathBuf::from("/tmp/wayhand-mcp-1000.lock")
        );
    }

    #[test]
    fn second_lock_is_rejected_until_first_lock_is_dropped() {
        let path = temp_lock_path();
        let first = InstanceLock::acquire_at(path.clone()).unwrap();

        assert!(matches!(
            InstanceLock::acquire_at(path.clone()),
            Err(InstanceLockError::AlreadyHeld(_))
        ));

        drop(first);
        assert!(InstanceLock::acquire_at(path.clone()).is_ok());
        fs::remove_file(path).unwrap();
    }
}
