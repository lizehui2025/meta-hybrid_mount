// Copyright 2025 Meta-Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
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
    
    // 收集需要处理的子挂载点
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

    // 【新增】在主挂载发生前，预先打开子挂载点，获取 FD
    // 这样可以避免主挂载覆盖后，软链接解析到错误的路径
    let mut submount_fds = HashMap::new();
    for mount_point in &mount_seq {
        match std::fs::File::open(mount_point) {
            Ok(f) => {
                submount_fds.insert(mount_point.clone(), f);
            },
            Err(e) => {
                log::warn!("Failed to pre-open submount {}: {}", mount_point, e);
            }
        }
    }

    // 执行主目录的 overlay 挂载
    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)
        .with_context(|| "mount overlayfs for root failed")?;

    // 处理子挂载点
    for mount_point in mount_seq.iter() {
        let relative = mount_point.replacen(root, "", 1);
        
        // 【修改】优先使用 /proc/self/fd/N 路径
        let stock_root_path = if let Some(fd) = submount_fds.get(mount_point) {
             format!("/proc/self/fd/{}", fd.as_raw_fd())
        } else {
             // 如果打开失败，回退到原有的相对路径逻辑
             format!("{stock_root}{relative}")
        };

        // 检查路径是否存在（/proc/self/fd/N 也是有效的路径）
        if !Path::new(&stock_root_path).exists() {
            continue;
        }
        
        // 传入计算好的 stock_root_path
        if let Err(e) = mount_overlay_child(
            mount_point,
            &relative,
            module_roots,
            &stock_root_path, 
            mount_source,
        ) {
            log::warn!(
                "failed to mount overlay for child {}: {:#}, revert",
                mount_point,
                e
            );
            umount_dir(root).with_context(|| format!("failed to revert {root}"))?;
            bail!(e);
        }
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
    if !module_roots
        .iter()
        .any(|lower| Path::new(&format!("{lower}{relative}")).exists())
    {
        return bind_mount(stock_root, mount_point);
    }
    if !Path::new(&stock_root).is_dir() {
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
    let mut mount_seq = mounts
        .0
        .iter()
        .filter(|m| {
            m.mount_point.starts_with(root) && !Path::new(&root).starts_with(&m.mount_point)
        })
        .map(|m| m.mount_point.to_str())
        .collect::<Vec<_>>();
    mount_seq.sort();
    mount_seq.dedup();

    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)
        .with_context(|| "mount overlayfs for root failed")?;
    for mount_point in mount_seq.iter() {
        let Some(mount_point) = mount_point else {
            continue;
        };
        let relative = mount_point.replacen(root, "", 1);
        let stock_root: String = format!("{stock_root}{relative}");
        if !Path::new(&stock_root).exists() {
            continue;
        }
        if let Err(e) = mount_overlay_child(
            mount_point,
            &relative,
            module_roots,
            &stock_root,
            mount_source,
        ) {
            log::warn!(
                "failed to mount overlay for child {}: {:#}, revert",
                mount_point,
                e
            );
            umount_dir(root).with_context(|| format!("failed to revert {root}"))?;
            bail!(e);
        }
    }
    Ok(())
}
