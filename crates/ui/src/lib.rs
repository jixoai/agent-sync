//! 意图（2026-11-25 Owner）：用 nsh 做原生菜单栏应用，配 Solid Glass 质感；
//!   界面上提供配置的可视化修改。
//!
//! 当前状态（诚实记录）：**占位实现，尚未还原设计稿**。
//!   · 设计稿已固定：/tmp/shots/{popover,settings}-{light,dark}.png（2x，明暗成对）
//!   · shadcn-nsh 的 registry 在本网络不可达（SSL access denied），组件装不了
//!   · 纯 Rust 复刻需要逐项核对 nsh 的布局 API（`View` 上并没有 `row()`/`font_weight`
//!     等方法；`WindowKind` 未公开），属于一次独立的重写，不能在猜 API 的状态下堆代码
//!
//! 本文件职责（2 个正交意图，其余待实现）：
//!   1. 提供一个可编译、可随 bundle 分发的菜单栏应用入口
//!   2. 明确记录设计规格与未完成项，避免后续接手者重新调研
//! 不承载：同步逻辑（core/daemon）；配置编辑（待实现）。

use nsh_core::prelude::*;

/// 应用根组件。当前只渲染占位，等布局 API 核对完成后替换为设计稿。
#[component]
pub fn App() -> impl IntoElement {
    view()
        .flex_1()
        .p(px(16.0))
        .gap(px(8.0))
        .child(text("agents-sync").text_size(px(15.0)))
        .child(
            text("菜单栏应用已就位；面板与配置窗口待还原设计稿。")
                .text_size(px(12.0)),
        )
}

/// 启动菜单栏应用。nsh 的 `run_app` 会进入 AppKit 事件循环并阻塞。
///
/// `ActivationPolicy::Accessory` 与 Info.plist 的 `LSUIElement` 一致：
/// 没有 Dock 图标、不进 Cmd-Tab，只活在菜单栏。
pub fn run() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        nsh_platform_macos::run_app(
            nsh_platform_macos::AppSource::Rust(nsh_platform_macos::Root::of::<App>()),
            nsh_platform_macos::AppOptions {
                title: "agents-sync".to_owned(),
                activation_policy: nsh_platform_macos::ActivationPolicy::Accessory,
                size: (380, 460),
                ..Default::default()
            },
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("当前只实现 macOS 菜单栏形态");
    }
    Ok(())
}
