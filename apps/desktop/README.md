# CodeDock Desktop（阶段 4 第一切口）

Tauri 2 + React 的 DevTools 控制台。架构边界（§5）：**Desktop 不内嵌
Runtime、不直接访问文件系统**——Tauri Rust 侧只做协议桥（UDS + JSON-RPC），
前端经 Tauri commands/events 与 daemon 交互。

## 结构

```
src-tauri/          # Tauri Rust：bridge.rs（可单测的 DaemonClient）+ commands
src/                # React Console：会话创建、流式对话（message.delta 实时渲染）、
                    # 事件时间线（含策略裁决/审批/冲突/Checkpoint 可解释事件）
```

事件数据源：daemon 的 `session.subscribe`（§8.2.5）——补发 durable 事件 +
`session.event` notification 实时推送，落后时 `session-resync` 提示重新对齐。

## 运行

```bash
# 1. 启动 daemon（默认 socket /tmp/codedock.sock，无配置文件时用内置 Mock Provider）
cargo run -p codedock-daemon --release

# 2. 桌面应用（开发模式，另开终端）
cd apps/desktop
pnpm install
pnpm tauri dev

# 仅构建前端产物（CI 用）
pnpm build
```

## 测试

```bash
cargo test --manifest-path src-tauri/Cargo.toml   # 桥接层单测（mock daemon）
pnpm build                                        # tsc 类型检查 + vite 构建
```

## 备注

- `src-tauri` 是独立 Cargo workspace（根 workspace `exclude`）——Tauri 依赖
  webkit2gtk 等系统库，主 CI 暂不构建 desktop，后续加专用 job（§18.8 同理）。
- GUI 交互冒烟需桌面环境；当前验证到桥接层单测 + 前端构建。
