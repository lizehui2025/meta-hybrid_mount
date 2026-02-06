// Copyright 2026 https://github.com/KernelSU-Modules-Repo/meta-overlayfs and https://github.com/bmax121/APatch

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[cfg(any(target_os = "linux", target_os = "android"))]
use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", target_os = "android"))]
use loopdev::LoopControl;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::{
    mount::{MountFlags, UnmountFlags, mount, unmount},
    path::Arg,
};

pub struct AutoMountExt4 {
    target: String,
    auto_umount: bool,
}

impl AutoMountExt4 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn try_new<P>(source: P, target: P, auto_umount: bool) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = source.as_ref();
        if !path.exists() {
            println!("Source path does not exist");
        } else {
            let metadata = fs::metadata(path)?;
            let permissions = metadata.permissions();
            let mode = permissions.mode();

            if permissions.readonly() {
                println!("File permissions: {:o} (octal)", mode & 0o777);
            }
        }

        mount_ext4(source.as_ref(), target.as_ref())?;
        Ok(Self {
            target: target.as_ref().as_str()?.to_string(),
            auto_umount,
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn try_new<P>(_src: P, _mnt: P, _auto_umount: bool) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        unimplemented!();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn umount(&self) -> Result<()> {
        unmount(self.target.as_str(), UnmountFlags::DETACH)?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for AutoMountExt4 {
    fn drop(&mut self) {
        log::info!(
            "AutoMountExt4 drop: {}, auto_umount: {}",
            self.target,
            self.auto_umount
        );
        if self.auto_umount {
            let _ = self.umount();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn mount_ext4<P>(source: P, target: P) -> Result<()>
where
    P: AsRef<Path>,
{
    let lc = LoopControl::open().context("Failed to open loop control")?;
    let ld = lc.next_free().context("Failed to find free loop device")?;

    ld.with()
        .read_only(false)
        .autoclear(true)
        .attach(source.as_ref())
        .context("Failed to attach source to loop device")?;

    let device_path = ld.path().context("Could not get loop device path")?;
    log::debug!("loop device path: {}", device_path.display());

    mount(
        &device_path,
        target.as_ref(),
        "ext4",
        MountFlags::NOATIME,
        Some(c""),
    )
    .context(format!(
        "Failed to mount {} to {}",
        device_path.display(),
        target.as_ref().display()
    ))?;

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn umount_dir(src: impl AsRef<Path>) -> Result<()> {
    unmount(src.as_ref(), UnmountFlags::empty())
        .with_context(|| format!("Failed to umount {}", src.as_ref().display()))?;
    Ok(())
}
