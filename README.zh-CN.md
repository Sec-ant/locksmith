# locksmith

[English](README.md) · **简体中文**

Locksmith 用来合并 pnpm 锁文件，让无关的依赖变更留在 diff 之外。

merge 或 rebase 时 `pnpm-lock.yaml` 发生冲突，通常的做法是跑一遍 `pnpm install`，让 pnpm 重新生成锁文件。锁文件会恢复有效，但 pnpm 会重新解析整张依赖图。无关依赖的版本和 peer 绑定可能跟着变，而锁文件的 diff 很少有人 review。

Locksmith 直接处理锁文件里的依赖图。它保留本次修改不需要改动的每一条记录，默认离线运行；输入健康时，输出与原文逐字节相同。

| 遇到的情况 | 使用的命令 |
| --- | --- |
| 两个分支都改了锁文件 | [`merge`](#合并两个分支的锁文件) |
| 修改依赖后，`pnpm install` 连带更新了其他依赖 | [`converge`](#收敛安装后的变更) |
| Git 已经完成合并，锁文件里有重复、过期或缺失的记录 | [`repair`](#修复已经合并的锁文件) |

## 安装

在 macOS 或 Linux 上，可以用 Homebrew 安装预编译的 CLI。

```sh
brew tap sec-ant/locksmith https://github.com/Sec-ant/locksmith.git
brew install sec-ant/locksmith/locksmith
```

从源码安装需要 Rust 1.89 或更高版本。在本仓库根目录运行 `cargo install --locked --path crates/locksmith-cli`。

Locksmith 支持 pnpm 9–12 默认使用的 `lockfileVersion: '9.0'`。

## Agent 技能

[Locksmith 技能](skills/locksmith/SKILL.md)帮助 agent 合并、修复和收敛锁文件。按上文安装 CLI，再将 `skills/locksmith/` 添加到 agent 的技能目录。

例如，可以直接说「解决这次 rebase 的 `pnpm-lock.yaml` 冲突」。技能会使用可用的工作区上下文，默认按需查询 registry。需要跳过查询时，在请求里注明「离线」。

## 合并两个分支的锁文件

依次传入共同祖先、当前分支和另一分支的锁文件。在 Git 仓库外也可以运行。

```sh
locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

当两个分支修改了同一条依赖，最终的工作区 manifest 决定保留哪个版本。Git 合并驱动会自动读取这些声明；脚本可以用 `--context FILE` 提供相同的信息，格式见[合并上下文](docs/design.zh-CN.md#合并上下文)。不带上下文时，Locksmith 做三方比较，可达记录有分歧就报告冲突。

### 配置 Git 合并驱动

在仓库的 `.gitattributes` 中加入一行。

```gitattributes
/pnpm-lock.yaml merge=locksmith
```

使用 Git 2.38 或更高版本，在每个本地克隆中注册一次驱动。

```sh
git config merge.locksmith.driver 'locksmith merge --git-driver %O %A %B %P'
```

在 merge 或 rebase 过程中，Git 需要对锁文件做三方合并时就会调用 Locksmith。驱动读取合并后的工作区声明，据此选择记录；两侧版本同样合适时，优先保留主干版本。

驱动默认通过 `origin/HEAD` 识别主干。需要指定其他主干时，在本地仓库设置 `locksmith.mainline`。两者都没有设置时，平局保留 `ours`。

```sh
git config --local locksmith.mainline refs/heads/trunk
```

如果 Git 因锁文件暂停，先解决诊断中点名的项目文件，再打开 `pnpm-lock.yaml`。有冲突标记时直接编辑。如果文本已经合并干净，就检查诊断指出的依赖。确认后用 `git add` 暂存文件，继续 merge 或 rebase。

## 收敛安装后的变更

修改 manifest（如 `package.json`）、catalog、override 或包扩展规则后，照常运行 `pnpm install`。随后在工作区根目录运行以下命令，传入的 Git 引用应包含修改前的锁文件和工作区声明。

```sh
locksmith converge origin/main > converged.yaml
git diff --no-index pnpm-lock.yaml converged.yaml
```

基准锁文件和安装后的锁文件必须分别与生成它们时的 manifest 和配置匹配。Locksmith 保留这次修改所需的变更，并在新声明仍然允许时恢复原有版本和 peer 绑定。选择规则和示例见[收敛设计](docs/design.zh-CN.md#4-converge)。

检查差异，再用 `converged.yaml` 替换 `pnpm-lock.yaml`。

## 修复已经合并的锁文件

即使 Git 合并时没有报告冲突，锁文件也可能留下重复记录、过期的 importer 声明，或有依赖指向缺失的包实例。在工作区根目录运行以下命令，用同一个提交中的声明和配置检查锁文件。

```sh
locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

输入健康时，输出与原文逐字节相同。修复会保留有效的锁定版本，也可以从父提交恢复缺失的记录；加上 `--allow-network` 时还可以使用 registry 元数据。如果现有信息仍不足以确定某条依赖，命令会在标准错误中点名该依赖，标准输出为空。

## 脚本契约

- 成功时，独立运行的命令把 YAML 写到标准输出。
- 诊断写到标准错误。
- 退出码 `1` 表示冲突、依赖选择待定或 peer 要求未满足；其他错误使用 `2`。
- Git 驱动失败时返回 `1`，并把 Git 对三份输入做文本合并的结果写回锁文件，便于手动解决冲突。即使文本合并干净，Git 也会保留未合并状态，直到你确认诊断中指出的依赖。

## 按需查询 registry

Locksmith 默认离线运行。带上下文的 merge、`converge` 和 `repair` 可以加上 `--allow-network`，从项目配置的 registry 查询包元数据。CLI 会使用项目的 registry 设置和凭据，遵循版本发布时间和版本选择策略，并在本次命令中缓存结果。安装依赖仍由 pnpm 完成。

## Rust API

Rust 应用可以把内存中的数据直接传给 `merge`、`converge` 和 `repair`，也可以用 `inspect_peers` 应用 peer 策略。库本身不做文件系统、网络、Git 或包管理器操作；manifest、工作区配置和 registry 元数据都由调用方提供。

```rust
use locksmith::{Lockfile, LockfileError, merge};

fn main() -> Result<(), LockfileError> {
    let base = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}}\n")?;
    let ours = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}, lib: {}}\n")?;
    let merged = merge(&base, &ours, &base, None)?;
    assert!(merged.project().unwrap()["importers"].get("lib").is_some());
    Ok(())
}
```

在本仓库运行 `cargo doc -p locksmith --no-deps --open` 可查看完整 API。数据模型和算法见[《Locksmith 的工作原理》](docs/design.zh-CN.md)。

## 性能

在一个包含 359–398 个工作区项目、锁文件为 8–11 MiB 的 monorepo 上，各项操作的耗时中位数为 **0.84–1.38 秒**，峰值内存中位数为 **244–316 MiB**。其中，三文件合并的耗时中位数为 1.00 秒，P95 为 1.98 秒。

<details>
<summary>完整测量数据及与 pnpm 的对比</summary>

以下是 Locksmith 0.1.2 在 x86-64 Linux 上的离线测量结果，文件系统缓存已预热。每份输入在相同的四个 CPU 核心上用两个版本各运行一次，交替运行顺序。统计包含成功和拒绝处理的样本，P95 表示第 95 百分位数。

| 操作 | 样本数 | 平均耗时 | 耗时中位数 | 耗时 P95 | 平均峰值内存 | 峰值内存中位数 | 峰值内存 P95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `merge`（三个文件） | 461 | 1.07 s | 1.00 s | 1.98 s | 244 MiB | 244 MiB | 264 MiB |
| `merge --context` | 461 | 1.85 s | 1.36 s | 4.12 s | 262 MiB | 261 MiB | 302 MiB |
| 安装后运行 `converge` | 68 | 1.04 s | 1.00 s | 1.41 s | 312 MiB | 316 MiB | 335 MiB |
| 工作区规则变更后运行 `converge` | 186 | 1.30 s | 1.38 s | 1.63 s | 309 MiB | 310 MiB | 359 MiB |
| `repair` | 796 | 0.90 s | 0.84 s | 1.77 s | 269 MiB | 281 MiB | 331 MiB |

两组合并都读取三个锁文件，带上下文的一组还读取预先准备好的工作区数据。其余命令的耗时包含从 Git 读取数据。186 组规则变更覆盖 90 天内对 catalog、override 和包扩展的修改。峰值内存取两个数中的较大值，一个是每 10 毫秒采样的 CLI 及其子进程常驻内存（RSS）之和的峰值，另一个是操作系统记录的单进程峰值。

另一次对比使用两个有锁文件冲突的生成工作区，分别运行 `locksmith merge --context` 和 pnpm 10.33.2 的 `install --lockfile-only`。每个命令在缓存预热后离线运行 11 次，交替运行顺序。两者的输出都与 pnpm 在无冲突工作区生成的锁文件逐字节相同。

| 工作区项目数 | 锁文件大小 | Locksmith 耗时 / 峰值内存中位数 | pnpm 耗时 / 峰值内存中位数 |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.016 s / 8 MiB | 0.59 s / 153 MiB |
| 384 | 248 KiB | 0.052 s / 17 MiB | 1.27 s / 196 MiB |

</details>

## 兼容性与限制

- 支持 `lockfileVersion: '9.0'`，即 pnpm 9–12 的默认格式。实验性锁文件格式会按格式版本直接拒绝。
- Locksmith 不安装依赖包，安装仍由 `pnpm install` 完成。
- 库不做 I/O。CLI 负责读取项目文件、Git 和 registry。
- registry 访问需要显式加上 `--allow-network`。
- Git 合并驱动需要 Git 2.38 或更高版本。
- 输出在写出前会重新校验；操作失败时不会产生半成品锁文件。

## 准备发布

安装 Knope 后，运行 `just change`，将面向用户的变更说明和版本升级级别记录到 `.changeset/`，并随代码一起提交。变更记录合入 `main` 后，Knope Bot 会创建或更新发版 PR，其中包含下一版的版本号、`Cargo.lock` 和 `CHANGELOG.md`。

审核并合入发版 PR 后，GitHub Actions 会构建 macOS 和 Linux 二进制，上传产物并正式发布，再更新 Homebrew 公式。
