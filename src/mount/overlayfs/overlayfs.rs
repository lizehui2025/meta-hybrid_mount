// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::HashMap,
    ffi::CString,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use procfs::process::Process;
use rustix::{
    fs::CWD,
    mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, fsconfig_create,
        fsconfig_set_string, fsmount, fsopen, mount, move_mount,
    },
};

use crate::{mount::overlayfs::utils::umount_dir, try_umount::send_unmountable};

pub fn mount_overlayfs(
    lower_dirs: &[String],
    lowest: &str,
    upperdir: Option<PathBuf>,
    workdir: Option<PathBuf>,
    dest: impl AsRef<Path>,
    mount_source: &str,
) -> Result<()> {
    let lowerdir_config = lower_dirs
        .iter()
        .map(|s| s.as_ref())
        .chain(std::iter::once(lowest))
        .collect::<Vec<_>>()
        .join(":");
    log::info!(
        "mount overlayfs on {:?}, lowerdir={}, upperdir={:?}, workdir={:?}, source={}",
        dest.as_ref(),
        lowerdir_config,
        upperdir,
        workdir,
        mount_source
    );

    let upperdir = upperdir
        .filter(|up| up.exists())
        .map(|e| e.display().to_string());
    let workdir = workdir
        .filter(|wd| wd.exists())
        .map(|e| e.display().to_string());

    let result = (|| {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        let fs = fs.as_fd();
        fsconfig_set_string(fs, "lowerdir", &lowerdir_config)?;
        if let (Some(upperdir), Some(workdir)) = (&upperdir, &workdir) {
            fsconfig_set_string(fs, "upperdir", upperdir)?;
            fsconfig_set_string(fs, "workdir", workdir)?;
        }
        fsconfig_set_string(fs, "source", mount_source)?;
        fsconfig_create(fs)?;
        let mount = fsmount(fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(
            mount.as_fd(),
            "",
            CWD,
            dest.as_ref(),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })();

    if let Err(e) = result {
        log::warn!("fsopen mount failed: {:#}, fallback to mount", e);
        let mut data = format!("lowerdir={lowerdir_config}");
        if let (Some(upperdir), Some(workdir)) = (upperdir, workdir) {
            data = format!("{data},upperdir={upperdir},workdir={workdir}");
        }
        mount(
            mount_source,
            dest.as_ref(),
            "overlay",
            MountFlags::empty(),
            Some(CString::new(data)?.as_c_str()),
        )?;
    }
    Ok(())
}

pub fn bind_mount(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    log::info!(
        "bind mount {} -> {}",
        from.as_ref().display(),
        to.as_ref().display()
    );
    use rustix::mount::{OpenTreeFlags, open_tree};
    match open_tree(
        CWD,
        from.as_ref(),
        OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_RECURSIVE,
    ) {
        Result::Ok(tree) => {
            move_mount(
                tree.as_fd(),
                "",
                CWD,
                to.as_ref(),
                MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
            )?;
        }
        _ => {
            mount(
                from.as_ref(),
                to.as_ref(),
                "",
                MountFlags::BIND | MountFlags::REC,
                None,
            )?;
        }
    }
    Ok(())
}

fn mount_overlay_child(
    mount_point: &str,
    relative: &String,
    module_roots: &Vec<String>,
    stock_root: &String,
    mount_source: &str,
) -> Result<()> {
    // Check if any module provides modifications for this path
    if !module_roots
        .iter()
        .any(|lower| Path::new(&format!("{lower}{relative}")).exists())
    {
        // If no modifications, just bind mount the stock root back to the mount point.
        // This ensures that even if /system overlay masked the original directory/symlink,
        // we explicitly restore the original content here.
        return bind_mount(stock_root, mount_point);
    }

    if !Path::new(&stock_root).is_dir() {
        // If stock_root is not a directory, we can't use it as a lowerdir base for overlayfs normally,
        // unless it's a file-overlay which is rare here.
        return Ok(());
    }

    let mut lower_dirs: Vec<String> = vec![];
    for lower in module_roots {
        let lower_dir = format!("{lower}{relative}");
        let path = Path::new(&lower_dir);
        if path.is_dir() {
            lower_dirs.push(lower_dir);
        } else if path.exists() {
            return Ok(());
        }
    }
    if lower_dirs.is_empty() {
        return Ok(());
    }

    // stock_root here is the "lowest" directory.
    // By passing the resolved absolute path (e.g. /vendor), we bypass the /system overlay.
    if let Err(e) = mount_overlayfs(
        &lower_dirs,
        stock_root,
        None,
        None,
        mount_point,
        mount_source,
    ) {
        log::warn!("failed: {:#}, fallback to bind mount", e);
        bind_mount(stock_root, mount_point)?;
    }
    let _ = send_unmountable(mount_point);
    Ok(())
}

pub fn mount_overlay(
    root: &String,
    module_roots: &Vec<String>,
    workdir: Option<PathBuf>,
    upperdir: Option<PathBuf>,
    mount_source: &str,
) -> Result<()> {
    log::info!("mount overlay for {}", root);
    std::env::set_current_dir(root).with_context(|| format!("failed to chdir to {root}"))?;
    let stock_root = ".";

    let mounts = Process::myself()?
        .mountinfo()
        .with_context(|| "get mountinfo")?;
    
    // Collect all child mount points under 'root'
    let mut mount_seq = mounts
        .0
        .iter()
        .filter(|m| {
            m.mount_point.starts_with(root) && !Path::new(&root).starts_with(&m.mount_point)
        })
        .map(|m| m.mount_point.clone())
        .collect::<Vec<_>>();
    mount_seq.sort();
    mount_seq.dedup();

    // [Fix Strategy] Pre-resolve paths to handle Self-Masking (e.g. /system/vendor -> /vendor)
    // Before we mount the overlay on 'root' (which might hide symlinks), we detect them
    // and resolve them to their absolute physical paths.
    let mut resolved_lower_paths = HashMap::new();
    for mount_point in &mount_seq {
        let path = Path::new(mount_point);
        // Try to read link to see if it's a symlink (like /system/vendor -> /vendor)
        if let Ok(target) = std::fs::read_link(path) {
            let mut target_path = target;
            // If relative, resolve it against the parent directory
            if target_path.is_relative() {
                if let Some(parent) = path.parent() {
                    target_path = parent.join(target_path);
                }
            }
            // Use canonicalize to get the clean absolute path, ensuring we bypass any future masking.
            // If canonicalize fails (e.g. broken link), we fall back to the raw target string.
            let resolved = std::fs::canonicalize(&target_path)
                .unwrap_or(target_path)
                .to_string_lossy()
                .to_string();
            
            resolved_lower_paths.insert(mount_point.clone(), resolved);
        }
    }

    // Mount the main overlay on root (e.g. /system)
    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)
        .with_context(|| "mount overlayfs for root failed")?;
        
    // Handle children (e.g. /system/vendor)
    for mount_point in mount_seq.iter() {
        let mount_point_str = mount_point.to_string_lossy().to_string();
        let relative = mount_point_str.replacen(root, "", 1);
        
        // Determine the "lowest" directory for the child overlay.
        // 1. If we found it was a symlink earlier, use the pre-resolved absolute path (e.g. "/vendor").
        //    This guarantees we are not looking at the masked path inside the new /system overlay.
        // 2. Otherwise, fall back to the relative path logic (old behavior), 
        //    which is usually fine for standard sub-directories.
        let stock_root_path = if let Some(resolved) = resolved_lower_paths.get(mount_point) {
             resolved.clone()
        } else {
             format!("{stock_root}{relative}")
        };

        // Check existence. Note: If stock_root_path is absolute (like /vendor), checking it works fine
        // even if CWD is /system, provided /vendor is accessible globally.
        if !Path::new(&stock_root_path).exists() {
            continue;
        }
        
        if let Err(e) = mount_overlay_child(
            &mount_point_str,
            &relative,
            module_roots,
            &stock_root_path,
            mount_source,
        ) {
            log::warn!(
                "failed to mount overlay for child {}: {:#}, revert",
                mount_point.display(),
                e
            );
            umount_dir(root).with_context(|| format!("failed to revert {root}"))?;
            bail!(e);
        }
    }
    Ok(())
}
