use std::path::Path;

use anyhow::Result;

use crate::{
    conf::config::Config,
    core::{
        inventory,
        inventory::model as modules,
        ops::{backup as granary, executor, planner, sync},
        state, storage,
        storage::{StorageHandle, get_usage},
    },
};

use crate::core::ops::merge;

impl MountController<ModulesReady> {
    pub fn generate_plan(mut self) -> Result<MountController<Planned>> {
        // === 新增：合并策略判断 ===
        // 你可以在 Config 中添加一个字段 use_merge_strategy 来控制
        let use_merge_strategy = true; // 或者从 self.config 读取

        let modules_to_plan = if use_merge_strategy {
            // 定义合并目录，例如 /data/adb/mountify/merged 或 tmpfs挂载点
            let merge_dir = self.state.handle.mount_point.join("merged_workspace");
            
            // 执行合并
            // 注意：inventory::scan 返回的顺序通常可能需要调整，确保 merge_modules 里的覆盖顺序正确
            // 假设 self.state.modules 里的顺序是：[高优先级, ..., 低优先级]
            // 合并时我们需要：先拷贝低优先级，再拷贝高优先级。
            let mut sorted_modules = self.state.modules.clone();
            sorted_modules.reverse(); // 翻转为：低 -> 高

            let super_module = merge::merge_modules(&sorted_modules, &merge_dir)?;
            
            // 返回只包含超级模块的列表
            vec![super_module]
        } else {
            // 传统模式：直接使用所有模块
            self.state.modules.clone()
        };
        // ========================

        // 使用处理过的 modules_to_plan 生成计划
        let plan = planner::generate(
            &self.config,
            &modules_to_plan, // 传入 "超级模块" 或 原始列表
            &self.state.handle.mount_point,
        )?;

        // 注意：这里保存到 state 的 modules 最好还是原始的 modules，
        // 这样后续 finalize 更新描述时还能知道有多少个真实模块被加载。
        // 但是 plan 里面使用的是合并后的路径。
        Ok(MountController {
            config: self.config,
            state: Planned {
                handle: self.state.handle,
                modules: self.state.modules, // 保持原始模块列表用于记录状态
                plan,
            },
        })
    }
}

pub struct Init;

pub struct StorageReady {
    pub handle: StorageHandle,
}

pub struct ModulesReady {
    pub handle: StorageHandle,
    pub modules: Vec<inventory::Module>,
}

pub struct Planned {
    pub handle: StorageHandle,
    pub modules: Vec<inventory::Module>,
    pub plan: planner::MountPlan,
}

pub struct Executed {
    pub handle: StorageHandle,
    #[allow(dead_code)]
    pub modules: Vec<inventory::Module>,
    pub plan: planner::MountPlan,
    pub result: executor::ExecutionResult,
}

pub struct MountController<S> {
    config: Config,
    state: S,
}

impl MountController<Init> {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: Init,
        }
    }

    pub fn init_storage(
        self,
        mnt_base: &Path,
        img_path: &Path,
    ) -> Result<MountController<StorageReady>> {
        let handle = storage::setup(
            mnt_base,
            img_path,
            &self.config.moduledir,
            matches!(
                self.config.overlay_mode,
                crate::conf::config::OverlayMode::Ext4
            ),
            matches!(
                self.config.overlay_mode,
                crate::conf::config::OverlayMode::Erofs
            ),
            &self.config.mountsource,
            self.config.disable_umount,
        )?;

        log::info!(">> Storage Backend: [{}]", handle.mode.to_uppercase());

        Ok(MountController {
            config: self.config,
            state: StorageReady { handle },
        })
    }
}

impl MountController<StorageReady> {
    pub fn scan_and_sync(mut self) -> Result<MountController<ModulesReady>> {
        let modules = inventory::scan(&self.config.moduledir, &self.config)?;

        log::info!(
            ">> Inventory Scan: Found {} enabled modules.",
            modules.len()
        );

        sync::perform_sync(&modules, &self.state.handle.mount_point)?;

        if self.state.handle.mode == "erofs_staging" {
            let needs_magic = modules.iter().any(|m| {
                m.rules.default_mode == inventory::MountMode::Magic
                    || m.rules
                        .paths
                        .values()
                        .any(|v| *v == inventory::MountMode::Magic)
            });

            if needs_magic {
                let magic_ws = self.state.handle.mount_point.join("magic_workspace");
                if !magic_ws.exists() {
                    let _ = std::fs::create_dir(magic_ws);
                }
            }
        }

        self.state.handle.commit(self.config.disable_umount)?;

        Ok(MountController {
            config: self.config,
            state: ModulesReady {
                handle: self.state.handle,
                modules,
            },
        })
    }
}

impl MountController<ModulesReady> {
    pub fn generate_plan(self) -> Result<MountController<Planned>> {
        let plan = planner::generate(
            &self.config,
            &self.state.modules,
            &self.state.handle.mount_point,
        )?;

        Ok(MountController {
            config: self.config,
            state: Planned {
                handle: self.state.handle,
                modules: self.state.modules,
                plan,
            },
        })
    }
}

impl MountController<Planned> {
    pub fn execute(self) -> Result<MountController<Executed>> {
        log::info!(">> Link Start! Executing mount plan...");

        let result = executor::execute(&self.state.plan, &self.config)?;

        Ok(MountController {
            config: self.config,
            state: Executed {
                handle: self.state.handle,
                modules: self.state.modules,
                plan: self.state.plan,
                result,
            },
        })
    }
}

impl MountController<Executed> {
    pub fn finalize(self) -> Result<()> {
        modules::update_description(
            &self.state.handle.mode,
            self.state.result.overlay_module_ids.len(),
            self.state.result.magic_module_ids.len(),
        );

        let storage_stats = get_usage(&self.state.handle.mount_point);

        let mut active_mounts: Vec<String> = self
            .state
            .plan
            .overlay_ops
            .iter()
            .map(|op| op.partition_name.clone())
            .collect();

        active_mounts.sort();
        active_mounts.dedup();

        let state = state::RuntimeState::new(
            self.state.handle.mode,
            self.state.handle.mount_point,
            self.state.result.overlay_module_ids,
            self.state.result.magic_module_ids,
            active_mounts,
            storage_stats,
        );

        if let Err(e) = state.save() {
            log::error!("Failed to save runtime state: {:#}", e);
        }

        granary::reset_recovery_state();

        log::info!(">> System operational. Mount sequence complete.");

        Ok(())
    }
}
