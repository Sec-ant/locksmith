# locksmith

[English](README.md) · **简体中文**

Locksmith 用于合并和修复 pnpm 锁文件，也能在安装依赖后收敛 `pnpm-lock.yaml`，减少与本次修改无关的变更。

## 安装

在 macOS 或 Linux 上，可以用 Homebrew 安装预编译的命令行工具。

```sh
brew tap sec-ant/locksmith https://github.com/Sec-ant/locksmith.git
brew install sec-ant/locksmith/locksmith
```

也可以在仓库根目录用 Rust 1.89 或更高版本从源码安装。

```sh
cargo install --locked --path crates/locksmith-cli
```

Locksmith 支持 pnpm 9–12 默认使用的锁文件格式 `lockfileVersion: '9.0'`。

## 合并两个分支的锁文件

依次传入共同祖先、当前分支和另一分支的锁文件。

```sh
locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

要让 `git merge` 和 `git rebase` 自动调用 Locksmith，可以[配置 Git 合并驱动](#配置-git-合并驱动)。

## 修复已经合并的锁文件

即使 Git 合并 YAML 时没有报告冲突，锁文件中也可能有重复记录，或者某条依赖指向了不存在的包实例。在工作区根目录运行以下命令，可以修复指定提交中的锁文件并查看差异。

```sh
locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

`repair` 从同一个提交读取锁文件、各项目的依赖声明和工作区配置。它保留有效的锁定版本，将修复结果写到标准输出。输入正确时，输出与原文件逐字节相同。

例如，`packageExtensions` 给某个包补上了必需依赖，而 `overrides` 又为这条依赖指定了确切版本。如果锁文件中只有一个实例既符合该版本，又匹配使用方提供的 peer 依赖，`repair` 就能补上缺失的引用，或替换过期的引用。缺失的记录也可以从父提交恢复。加上 `--allow-network` 后，Locksmith 还可以查询缺失的包版本元数据。证据不足时，命令会在标准错误中指出相关依赖，并将标准输出留空。

## 收敛安装后的变更

修改 `package.json`、catalog、override 或包扩展规则后，`pnpm install` 有时也会更新其他依赖。在工作区根目录运行 `converge`，传入保存了原始依赖声明、工作区配置和锁文件的 Git 引用。

```sh
locksmith converge origin/main > converged.yaml
```

基准锁文件和安装后的锁文件应分别与各自的依赖声明和工作区配置一致。`converge` 保留仍满足新声明的基准版本，把 override 应用到匹配的依赖，并为每个使用方保留合适的 peer 绑定。例如，包扩展为插件新增了对 React 的 peer 依赖，两个应用仍然可以让同一个插件分别使用各自的 React 版本。检查输出后，可以直接用它替换 `pnpm-lock.yaml`。

如果安装时升级了某个包，可以加上 `--allow-network`，查询新版本的依赖声明，判断它是否仍接受基准锁文件中的子依赖版本。

## 配置 Git 合并驱动

在仓库的 `.gitattributes` 中加入以下规则。

```gitattributes
/pnpm-lock.yaml merge=locksmith
```

使用 Git 2.38 或更高版本，在每个本地克隆中注册一次合并驱动。

```sh
git config merge.locksmith.driver 'locksmith merge --git-driver %O %A %B %P'
```

驱动根据合并后的依赖声明和工作区配置逐条检查依赖。两个分支的版本同样符合要求时，优先保留主干版本。在驱动命令末尾加上 `--allow-network`，可从项目配置的 registry 查询缺失的包版本元数据。

如果 `git status` 仍将锁文件列为未合并，先解决 Locksmith 提示的项目文件，再打开锁文件。两边修改重叠处会出现熟悉的 Git 冲突标记。如果文本已自动合并，则检查终端指出的依赖或配置。确认后用 `git add` 暂存文件，再运行 `git merge --continue` 或 `git rebase --continue`。

## 性能

以下数据来自 Locksmith 0.1.2 在 x86-64 Linux 上的离线历史回放。样本取自一个包含 359–398 个工作区项目的 monorepo，锁文件约为 8–11 MiB。测试使用已预热的文件系统缓存。每份输入在相同的四个 CPU 核心上用新旧版本各运行一次，交替运行顺序。统计包含成功和拒绝处理的样本。P95 表示第 95 百分位数。

| 操作 | 样本数 | 平均耗时 | 耗时中位数 | 耗时 P95 | 平均峰值内存 | 峰值内存中位数 | 峰值内存 P95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `merge`（三个文件） | 461 | 1.07 s | 1.00 s | 1.98 s | 244 MiB | 244 MiB | 264 MiB |
| `merge --context` | 461 | 1.85 s | 1.36 s | 4.12 s | 262 MiB | 261 MiB | 302 MiB |
| 安装后运行 `converge` | 68 | 1.04 s | 1.00 s | 1.41 s | 312 MiB | 316 MiB | 335 MiB |
| 工作区规则变更后运行 `converge` | 186 | 1.30 s | 1.38 s | 1.63 s | 309 MiB | 310 MiB | 359 MiB |
| `repair` | 796 | 0.90 s | 0.84 s | 1.77 s | 269 MiB | 281 MiB | 331 MiB |

两组 `merge` 都读取三个锁文件，`--context` 组还读取预先准备好的工作区上下文。`converge` 和 `repair` 的耗时包含从 Git 读取项目数据。186 组规则变更覆盖了 90 天内对 catalog、override 和包扩展的修改。峰值内存按常驻内存（RSS）计算，每 10 毫秒将 CLI 及其运行中子进程的 RSS 相加，最后取采样峰值与操作系统记录的单进程峰值中的较大值。

另外，我们构造了两个存在锁文件冲突的工作区，对比 `locksmith merge --context` 和 pnpm 10.33.2 的 `install --lockfile-only`。每个命令在缓存预热后离线运行 11 次，交替运行顺序。两者的输出都与 pnpm 在无冲突工作区生成的锁文件逐字节相同。

| 工作区项目数 | 锁文件大小 | Locksmith 耗时 / 峰值内存中位数 | pnpm 耗时 / 峰值内存中位数 |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.016 s / 8 MiB | 0.59 s / 153 MiB |
| 384 | 248 KiB | 0.052 s / 17 MiB | 1.27 s / 196 MiB |

## Rust API

Rust 应用可以直接调用 `merge`、`repair` 和 `converge`。运行 `cargo doc -p locksmith --no-deps --open` 可查看完整 API。各接口的输入和调用示例见[架构设计（英文）](docs/design.md#rust-library-api)。
