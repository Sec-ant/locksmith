# Locksmith 架构

[English](design.md) · **简体中文**

pnpm 的锁文件记录包的版本，也记录每个包实例实际绑定了哪些依赖。Locksmith 利用这些绑定合并分支上的修改、收敛安装带来的额外变更，或修复已提交的锁文件。安装方式和常用命令见[中文 README](../README.zh-CN.md)。

## 一份锁文件怎样同时采用两个分支的版本

假设一个应用使用 `lib@1.0.0`。它向 `lib` 提供两个 peer 依赖 `a` 和 `b`。合并后的 `package.json` 允许任意版本的 `a`，要求 `b@2.0.0`。两边的 `a` 都满足要求，本次调用偏好 theirs，因此选 theirs 的 `a`。只有 ours 的 `b` 满足要求，所以选 ours 的 `b`。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/peer-selection-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="diagrams/peer-selection.svg">
  <img src="diagrams/peer-selection.svg" alt="应用选用 theirs 的 a@3.0.0 和 ours 的 b@2.0.0，这两个 peer 共同决定 lib@1.0.0 的实例标识。">
</picture>

最后得到一条 `lib@1.0.0(a@3.0.0)(b@2.0.0)` 记录。这就是一个 **snapshot（包实例记录）**，它描述该实例实际连接的依赖。另一个应用可以使用同一版本的 `lib`，却因为提供了不同的 peer 而拥有另一条 snapshot。选择哪个分支的版本，要逐条依赖判断。[对应的合并测试](../crates/locksmith/tests/contextual_merge/workspace.rs)

## 输入从哪里来

CLI 读取锁文件、依赖声明和工作区配置。图中的虚线表示按需读取的数据。命令引用提交时会读取 Git，需要查询包元数据时可以访问 registry。Rust 应用可以直接把这些数据从内存传给同一个库。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="diagrams/architecture.svg">
  <img src="diagrams/architecture.svg" alt="CLI 读取项目文件，按命令需要读取 Git 或查询 registry。CLI 和 Rust 应用调用同一个内存中的 Rust 库。">
</picture>

| CLI 调用方式 | CLI 读取的数据 | Rust 库函数 |
| --- | --- | --- |
| `merge BASE OURS THEIRS` | 三个锁文件。加上 `--context FILE` 后，还会读取最终的依赖声明和工作区配置。 | `merge` |
| `merge --git-driver %O %A %B %P` | Git 提供的三个锁文件，以及合并后的工作区声明。 | `merge` |
| `converge REF` | `REF` 中的基准锁文件和声明，以及当前工作区安装后的锁文件和声明。 | `converge` |
| `repair [REF]` | 同一个提交中的锁文件和声明，需要时再读取父提交的锁文件。默认使用 `HEAD`。 | `repair` |

rebase 时，驱动把正在重放的提交的原始父提交当作 base，把当前已完成重放的 `HEAD` 当作 ours。驱动先核对 Git 提供的三个锁文件与对应提交是否一致。两侧版本同样合适时，它根据 `origin/HEAD` 识别主干。缺少这个符号引用时，检查本地的 `main` 和 `master`。优先选择正好位于主干顶端的一侧，其次选择主干历史中的一侧，其余平局保留 ours。

## Rust 库 API

三个函数都返回 `Result<Lockfile, LockfileError>`。成功后调用 `Lockfile::to_yaml()` 输出 YAML。Locksmith 复用原文中没有变化的记录，并保留 override 的排列顺序，因为顺序会影响规则的优先级。

### 合并分支上的修改

```rust
let merged = merge(&base, &ours, &theirs, Some(&merge_context))?;
```

三个输入可以是解析好的锁文件，也可以是 YAML 文本。第四个参数传 `None` 时，函数做三方比较，遇到可达依赖中的分歧会报告冲突。提供上下文后，最终的工作区声明可以确定哪些版本可用。Locksmith 从选中的版本继续追踪依赖，检查每个使用方提供的 peer。某个选择无法组成有效依赖图时，可以只对涉及的依赖改用另一侧的偏好，再尝试合并。

启用 `autoDedupe` 后，如果所有受影响的父包都接受同一版本，两个分支引入的传递依赖可以共用这个版本。已有的锁定关系或父包发布时的依赖声明能提供判断依据。传入原始 YAML 和上下文时，如果重复 snapshot 的所有解释都得到同一结果，也可以自动合并。

### 收敛安装后的变更

```rust
let reduced = converge(&baseline, &installed, &manifest_changes, options)?;
```

每个有变更的项目都要提供修改前后的声明。例如，依赖范围从 `^1.0.0` 改成 `>=1 <2`，安装选了 `1.1.0`，原来的 `1.0.0` 仍然符合要求，可以保留。同一次编辑新增的依赖会带入它所需的依赖图。

工作区规则有变更时，通过 `ConvergeOptions::baseline_configuration` 和 `configuration` 分别传入基准配置和本次安装的配置。其他解析设置、工作区成员匹配规则、钩子和补丁在两侧需要保持一致。

- **Catalog** 的变更只影响引用该条目的项目。只要都满足声明范围，catalog 缓存中的版本和项目实际使用的版本可以不同。
- **Override** 按依赖名、父包和声明范围匹配。删除规则时，替代的依赖关系来自安装后的锁文件。如果包扩展补充了一条必需依赖，对它生效的精确版本 override 也会参与校验，即使这条规则本次没有修改。
- **包扩展** 可以修改普通依赖、可选依赖或 peer 依赖。一个应用升级了 peer 的提供方，另一个应用仍可保留原有绑定。如果循环或共享实例需要互不兼容的绑定，收敛会在这些路径上保留安装后的 peer 选择，其他路径上的子依赖仍可恢复。

父包版本、补丁和 peer 绑定一致时，已有实例能说明它原来使用哪些子依赖。如果父包升级了，Locksmith 可以核对新版本的完整性信息、发布时的依赖范围和生效的 override，再决定能否恢复旧的子依赖版本。

### 修复已提交的锁文件

```rust
let repaired = repair(yaml, &repair_context)?;
```

修复从当前锁文件出发，清除内容相同的重复包记录或 snapshot，校正锁文件中过期的工作区依赖声明，恢复仍被引用的缺失记录。例如，包扩展补上一条必需依赖，override 又指定了它的确切版本。如果锁里有唯一一个实例匹配使用方的 peer，修复就能补回这条引用。同一键下的重复记录如果内容不同，需要调用方决定。

健康的输入会逐字节返回。如果调用方要补充父提交或 registry 的信息，可以先调用一次 `RepairInput::parse(yaml)`，再用 `.apply(&context)` 尝试不同的上下文。每次都从原始输入开始。

## 合并上下文

`merge --context FILE` 接受如下格式的 JSON。键 `"."` 表示工作区根项目。对于 monorepo，还需按项目路径提供各个工作区最终的 manifest。

```json
{
  "target": "theirs",
  "manifests": { ".": { "dependencies": { "a": "*", "b": "2.0.0", "lib": "1.0.0" } } },
  "configurations": [{}, {}, {}],
  "configuration": {},
  "patches": {}
}
```

`configurations` 按 base、ours、theirs 的顺序给出三份配置，`configuration` 是最终配置。`patches` 的键是配置中的补丁路径，值是文件内容。Git 合并驱动会自动收集这些数据。需要的项目文件仍有冲突时，驱动会点名该文件，并让锁文件保持未合并状态。

## 信息不足时会发生什么

假设最终 manifest 要求 `a@4.0.0`，三份锁文件里只有 1 到 3 的版本。库可以返回 `ResolutionRequired`，并附上 `RegistryRequest::Version` 请求缺失版本的信息。`RegistryRequest::Manifest` 则请求某个已知版本在发布时的依赖声明。如果 snapshot 只留下一个 peer 哈希，还需要找到能生成同一哈希的提供方组合。

加上 `--allow-network` 后，CLI 使用项目的默认 registry 或包作用域对应的 registry，按相应凭据查询元数据，遵循版本发布时间和版本选择策略，在本次命令中缓存结果。如果项目钩子或信任策略需要更多信息，合并会报告尚待确定的依赖。收敛过程中，可选的查询如果失败，CLI 会报告原因并保留安装后的依赖关系。Rust 调用方可以通过相应的上下文或选项传入 `RegistryMetadata`，用原输入重试。下载和安装依赖包由 pnpm 负责。

有工作区上下文时，CLI 还会应用项目的 peer 校验规则。启用 `strictPeerDependencies` 时，未解决的 peer 问题会使操作失败。关闭时，CLI 报告问题数量。Rust 调用方可以用 `inspect_peers` 获取这些问题。

独立运行的命令在成功时把 YAML 写到标准输出，诊断写到标准错误。退出码 `1` 表示有冲突、依赖选择待定或 peer 要求未满足，其他错误使用 `2`。Git 合并驱动失败时返回 `1`，并把三份原文交给 Git 做文本合并。即使文本已经合并干净，Git 也会保留未合并状态，直到开发者确认诊断中指出的依赖。
