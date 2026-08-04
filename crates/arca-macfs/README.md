# arca-macfs

macOS File Provider 扩展（`NSFileProviderReplicatedExtension`，macOS 12+）——
**Swift/ObjC 工程，不在 cargo workspace 内**（spec §3、§6.2）。

## 结构（待建）

- `ArcaFileProvider/` —— Swift 扩展工程（Xcode）：沙箱进程薄壳，
  `fetchContents(for:)` 等回调经 **XPC** 转发给 arca-agentd。
- `xpc-protocol/` —— XPC 协议定义（与 `arca-agentd/src/ipc.rs` 对应，
  协议格式属于 PROTOCOL.md 的约束范围）。

## 已知风险（spec §13，M4 首要技术风险）

- File Provider 域路径可能被限制在 `~/Library/CloudStorage/` 下，而笔记库通常在用户自选路径。
  **须在 M3 期间做原型验证**：能否对任意路径注册域；不可行则退回
  "整库位于 CloudStorage + 符号链接/书签访问"或 FSKit（macOS 15+）路线。
- `fileproviderd` 调度是黑盒；投影可重建（I9）是兜底。
