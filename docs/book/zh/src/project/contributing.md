# 参与贡献

欢迎各种形式的贡献：问题报告、文档修正、测试和代码。本章介绍项目的工作方式，帮助你的改动顺利融入。

## 动手之前

除了小修小补，请先开一个 issue，描述你想解决的问题。很多设计问题已经在[设计决策](../architecture/decisions.md)里讨论过，先简单交流一下，可能会省掉一次重写。

安全问题请走[安全](security.md)中所述的私密渠道，不要发到 issue 里。

## 构建与测试

```bash
cargo build --workspace
cargo test --workspace --locked
```

集成测试会启动真实的网关。集群测试需要一个本地 Redis：

```bash
docker run -d --name wsp-redis -p 6379:6379 redis:7.2 redis-server --save "" --appendonly no
```

SDK 和示例应用各有自己的端到端测试：

```bash
sdk/js/run-tests.sh
examples/chat-and-state/run-tests.sh
```

CI 会运行以上全部内容，外加格式检查、把警告视为错误的 Clippy、在最低支持 Rust 版本上的构建、依赖审计、混沌测试和一轮简短的模糊测试。提交 pull request 之前，至少请运行：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## 我们如何做改动

- **修复从一个失败的测试开始。** 先写出能复现缺陷的测试，确认它失败，再修复。然后把修复临时去掉一次，确认测试确实能抓住这个缺陷。
- **一个提交只做一件事。** 重构不要和行为变更混在一起。提交信息使用约定式前缀（`feat:`、`fix:`、`docs:`、`perf:`、`test:`、`refactor:`）。
- **先有边界，再有功能。** 新能力放在 trait、注册表或配置段之后。要在核心路径里加特判（`if svc == "x"`），需要非常充分的理由，并附上一条说明，写清楚将来如何去掉它。
- **性能改动要测量。** 声称更快的改动，必须附上基准场景下的前后数据。得不偿失的改动会被撤回，但结果依然记录下来。
- **把决策写下来。** 客户端可见的变化写进协议文档，设计决策及被否决的方案写进 ADR，缺陷修复写进加固日志。
- **契约只增不改。** 对线路协议、服务端 API 和配置格式的改动必须是增量式的。新增的配置字段一律要有默认值。

## 不稳定的测试

时好时坏的测试被视为缺陷，可能出在测试本身，也可能出在产品里。集成测试会反复运行以确认稳定，对不稳定的测试要查明原因，而不是靠重试把问题盖过去。

## 许可证

提交贡献即表示你同意这些贡献与项目其余部分一样，以 Apache License 2.0 授权。
