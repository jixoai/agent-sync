//! agents-sync 的核心逻辑：把混乱的活目录翻译成干净、可合并、可校验的仓库。
//!
//! 分层（自下而上，上层只依赖下层）：
//!   paths   —— 磁盘位置与设备身份
//!   config  —— 本机配置与设备注册表
//!   exclude —— 准入规则（唯一的安全边界）
//!   links   —— 符号链接的登记与重建
//!   checks  —— 凭据门与树哈希（可复算的判定）
//!   mirror  —— capture / apply（活目录 ↔ payload）
//!   vcs     —— jj / git 命令封装
//!   sync    —— 编排（publish / pull / align）
//!
//! 设计约束：本 crate 不依赖 UI、不依赖网络、不落任何全局状态，
//! 因此可以在测试里用临时 HOME 完整演一遍两节点同步。

pub mod checks;
pub mod config;
pub mod exclude;
pub mod links;
pub mod lock;
pub mod mirror;
pub mod paths;
pub mod settings;
pub mod sync;
pub mod update;
pub mod vcs;

pub use config::{Device, DeviceRegistry, SyncConf};
pub use paths::{local_device_id, SyncPaths};
