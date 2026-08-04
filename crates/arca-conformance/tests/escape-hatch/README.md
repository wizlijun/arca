# 逃生舱恢复演示（I1，进 CI）

每晚用**纯 shell + coreutils（不含任何 arca 代码）**从测试库里完整取回
`files/` 并校验哈希（spec §12.1）——逃生舱是被持续验证的承诺，
不是 README 里的一句话。

TODO(M0)：`recover.sh` + 测试库夹具 + CI 工作流。
