use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use lazy_static::lazy_static;
use tokio::sync::Mutex;
use tracing::instrument;

use super::filesystem::FileSystem;
use super::util::unmount;
use crate::Error;

pub const TMP_MOUNTPOINT: &'static str = "/media/embassy-os/tmp";

#[async_trait::async_trait]
pub trait GenericMountGuard: AsRef<Path> + std::fmt::Debug + Send + Sync + 'static {
    async fn unmount(mut self) -> Result<(), Error>;
}

#[derive(Debug)]
pub struct MountGuard {
    mountpoint: PathBuf,
    mounted: bool,
}
impl MountGuard {
    pub async fn mount(
        filesystem: &impl FileSystem,
        mountpoint: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let mountpoint = mountpoint.as_ref().to_owned();
        filesystem.mount(&mountpoint).await?;
        Ok(MountGuard {
            mountpoint,
            mounted: true,
        })
    }
    pub async fn unmount(mut self) -> Result<(), Error> {
        if self.mounted {
            unmount(&self.mountpoint).await?;
            self.mounted = false;
        }
        Ok(())
    }
}
impl AsRef<Path> for MountGuard {
    fn as_ref(&self) -> &Path {
        &self.mountpoint
    }
}
impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.mounted {
            let mountpoint = std::mem::take(&mut self.mountpoint);
            tokio::spawn(async move { unmount(mountpoint).await.unwrap() });
        }
    }
}
#[async_trait::async_trait]
impl GenericMountGuard for MountGuard {
    async fn unmount(mut self) -> Result<(), Error> {
        MountGuard::unmount(self).await
    }
}

async fn tmp_mountpoint(source: &impl FileSystem) -> Result<PathBuf, Error> {
    Ok(Path::new(TMP_MOUNTPOINT).join(base32::encode(
        base32::Alphabet::RFC4648 { padding: false },
        &source.source_hash().await?,
    )))
}

lazy_static! {
    static ref TMP_MOUNTS: Mutex<BTreeMap<PathBuf, Weak<MountGuard>>> = Mutex::new(BTreeMap::new());
}

#[derive(Debug)]
pub struct TmpMountGuard {
    guard: Arc<MountGuard>,
}
impl TmpMountGuard {
    #[instrument(skip(filesystem))]
    pub async fn mount(filesystem: &impl FileSystem) -> Result<Self, Error> {
        let mountpoint = tmp_mountpoint(filesystem).await?;
        let mut tmp_mounts = TMP_MOUNTS.lock().await;
        if !tmp_mounts.contains_key(&mountpoint) {
            tmp_mounts.insert(mountpoint.clone(), Weak::new());
        }
        let weak_slot = tmp_mounts.get_mut(&mountpoint).unwrap();
        if let Some(guard) = weak_slot.upgrade() {
            Ok(TmpMountGuard { guard })
        } else {
            let guard = Arc::new(MountGuard::mount(filesystem, &mountpoint).await?);
            *weak_slot = Arc::downgrade(&guard);
            Ok(TmpMountGuard { guard })
        }
    }
    pub async fn unmount(self) -> Result<(), Error> {
        if let Ok(guard) = Arc::try_unwrap(self.guard) {
            guard.unmount().await?;
        }
        Ok(())
    }
}
impl AsRef<Path> for TmpMountGuard {
    fn as_ref(&self) -> &Path {
        (&*self.guard).as_ref()
    }
}
#[async_trait::async_trait]
impl GenericMountGuard for TmpMountGuard {
    async fn unmount(mut self) -> Result<(), Error> {
        TmpMountGuard::unmount(self).await
    }
}
