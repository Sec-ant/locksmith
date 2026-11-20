# Locksmith 的工作原理

[English](design.md) · **简体中文**

本文说明 Locksmith 解决的问题、pnpm v9 锁文件如何表示依赖图，以及 `merge`、`converge` 和 `repair` 如何修改这张图。读者应使用过 pnpm，但不需要读过 `pnpm-lock.yaml` 的内部结构。安装和命令用法见 [README](../README.zh-CN.md)。

## 1. 锁文件不能当文本合并

Git 把 `pnpm-lock.yaml` 当作 YAML 文本合并。三方文本合并可能产出一个能解析、但不再描述有效依赖图的文件：

- **重复 snapshot。** 两个分支添加了同一个 snapshot 键，内容不同。YAML 允许重复键，pnpm 不允许。
- **过期的 importer 声明。** manifest 已经改变，importer 仍指向旧版本。
- **记录缺失。** 文本合并丢掉了仍被其他记录引用的包或 snapshot。
- **peer 绑定错误。** snapshot 键编码了 peer 提供方。文本合并可能保留一条键和内容都不再匹配其使用方的 snapshot。

常见的做法是在冲突后运行 `pnpm install`，让 pnpm 重新生成锁文件。结果有效，但 pnpm 会从 manifest 重新解析整张图：

- 无关依赖的版本可能因为范围允许新版本而移动。
- 提供方被重新解析后，peer 绑定可能改变。
- 除非所有元数据都已缓存，否则需要访问 registry。
- 大型工作区要付出完整的解析成本。

锁文件的 diff 很少有人 review，这些变更会悄悄进入分支。

Locksmith 走第三条路。它把锁文件当作图来读，用最终 manifest 和工作区配置作为约束，保留所有仍满足约束的记录。只有锁文件里的证据不足时才查询 registry。本文其余部分描述这个模型和建立在它之上的三个操作。

## 2. 数据模型

v9 锁文件是一张分三层的图。importer 指向 snapshot，snapshot 通过依赖边指向其他 snapshot，`packages` 保存 snapshot 共享的元数据。pnpm 11 及以后还会为包管理器依赖写一份单独的 environment 文档；下面的项目文档是 Locksmith 的主要输入。

### Importers（项目条目）

`importers` 每个工作区项目一条记录。根项目使用键 `"."`。每条依赖有两个字段：

- `specifier`：`package.json` 里写的范围（如 `^1.0.0` 或 `catalog:default`）。
- `version`：pnpm 选定的锁定引用（如 `1.0.0` 或 `1.0.0(a@3.0.0)(b@2.0.0)`）。

### Packages（发布记录）

`packages` 保存发布元数据，键是 `name@version`（git、file、tarball 依赖则使用 locator）：

- `resolution` 和 `integrity`：产物标识。
- `engines`、`os`、`cpu`、`libc`、`bin` 等安装元数据。
- `peerDependencies` 和 `peerDependenciesMeta`：该版本要求使用方提供什么。

无论安装多少次，一个发布版本只有一条 `packages` 记录。

### Snapshots（实例记录）

`snapshots` 保存实例。键是 `name@version`，后面每个 peer 提供方跟一对括号：

```
lib@1.0.0(a@3.0.0)(b@2.0.0)
```

记录内容是实例的边：`dependencies`、`optionalDependencies`，有时还有 `transitivePeerDependencies`。同一个发布版本在不同使用方提供不同 peer 时会有多条 snapshot。这就是为什么 `lib@1.0.0` 在一个项目里是 `lib@1.0.0(a@2.0.0)(b@1.0.0)`，在另一个项目里是 `lib@1.0.0(a@3.0.0)(b@2.0.0)`。`packages` 记录共享，`snapshots` 记录不共享。

peer 后缀属于标识本身。同一个发布版本、不同提供方的两条 snapshot 有不同的依赖边，也可能解析出不同的子依赖版本。把它们当作同一条记录合并会丢失信息。

### 工作区规则

几个顶层字段决定哪些 pin（锁定的版本）有效：

- `settings`：`autoInstallPeers`、`dedupePeers` 等相关解析开关。
- `catalogs`：`catalog:` 说明符引用的命名版本范围。
- `overrides`：按依赖名、父包和范围匹配的替换规则。多条规则同时匹配时，顺序会影响结果。
- `patchedDependencies`：应用到特定版本的补丁文件。
- `packageExtensionsChecksum`、`pnpmfileChecksum`、`ignoredOptionalDependencies`：必须与工作区配置匹配的校验和与设置。

包扩展和工作区设置本身在 workspace 配置里，锁文件只保存它们的影响或校验和。

### 最小例子

```yaml
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      a:
        specifier: '*'
        version: 3.0.0
      b:
        specifier: 2.0.0
        version: 2.0.0
      lib:
        specifier: 1.0.0
        version: 1.0.0(a@3.0.0)(b@2.0.0)

packages:
  a@3.0.0:
    resolution: {integrity: sha512-a}
  b@2.0.0:
    resolution: {integrity: sha512-b}
  lib@1.0.0:
    resolution: {integrity: sha512-lib}
    peerDependencies:
      a: '*'
      b: '*'

snapshots:
  a@3.0.0: {}
  b@2.0.0: {}
  lib@1.0.0(a@3.0.0)(b@2.0.0):
    dependencies:
      a: 3.0.0
      b: 2.0.0
```

根项目选中 `a@3.0.0`、`b@2.0.0` 和 `lib@1.0.0` 的一个实例。`lib` 发布版本声明 `a` 和 `b` 为 peer；snapshot 记录这个实例收到的是 `a@3.0.0` 和 `b@2.0.0`。另一个项目可以使用同一个 `lib@1.0.0` 发布版本、提供 `a@2.0.0`，得到另一条 snapshot。

可达性很重要。只有 importer 是入口。任何 importer 都到不了的包或 snapshot 是无用记录；可达路径上缺少记录则会让锁文件失效。Locksmith 的操作保留可达记录，清理其余部分。

## 3. merge

`merge` 把共同祖先和两个分支的锁文件合在一起。它有两种模式：由最终工作区声明驱动的带上下文合并，以及没有声明时的三方比较。Git 驱动使用带上下文的模式。

### 输入

- 三个锁文件：`base`、`ours`、`theirs`。
- 每个工作区项目的最终 manifest，包括 `"."`。
- base、ours、theirs 和最终合并状态各自的有效工作区配置。
- 配置中补丁路径对应的补丁内容。
- 可选的 registry 元数据。

库在内存中接收这些数据。CLI 从文件、Git 和工作区收集。

### 按最终工作区选择

带上下文的合并把最终 manifest 当作判断依据：

1. 对每个 importer 依赖，从三份输入里收集满足最终 manifest 范围的候选。
2. 优先选择解析设置与最终配置一致的来源，按目标分支排序。首选来源缺少合适候选时，再从其他来源取单独兼容的记录。每条依赖单独选择，所以一个结果可以同时包含两个分支的 pin。
3. 沿着选中的记录追踪它的依赖。候选只有在使用方选中的依赖能满足它的 peer 上下文时才有效。
4. 组装可达图：复制选中的 snapshot 和 package，按选中的提供方重新绑定 peer，应用 override、补丁和包扩展。
5. 校验结果。缺少版本或发布声明时返回 `ResolutionRequired` 和 `RegistryRequest`；调用方可以补充元数据后重试。

多条锁定候选同样合适时，目标分支决定平局。Git 驱动从仓库主干选择目标：先看 `origin/HEAD`，再看本地的 `main` 或 `master`。正好位于主干顶端的提交优先，其次是主干历史中的提交；其余平局保留 `ours`。

如果某个选择在后续步骤失败——peer 无法满足，或缺少必需记录——Locksmith 不会从头重来。它把失败的记录追溯到选中它的 importer 边，只对这些边及其受影响的使用方翻转偏好，然后用同一份输入重试。重试范围由失败涉及的边数决定。

启用 `autoDedupe` 后，如果所有受影响的父包都接受同一版本，两个分支引入的兼容传递依赖可以共用一个发布版本。已有的锁定关系或父包发布时的依赖声明能提供判断依据。未启用时，这些实例保持分开。

### 不带上下文

没有最终声明时，`merge` 做逐路径三方比较。相同的修改和只有一侧的修改直接接受。`packages` 和 `snapshots` 记录是原子的；snapshot 的依赖按边合并；独立的 peer 上下文变更组合成经过检查的实例，使用的版本必须已在输入中出现。可达记录有分歧时报告冲突，不返回合并后的锁文件。这个模式可以在 Git 仓库外使用，但无法在两个都有效的 pin 之间做选择。

### Git 驱动

Git 用 `%O %A %B %P` 调用驱动：base 锁文件、ours、theirs 和锁文件路径。驱动读取合并后的工作区树，收集 manifest、配置和补丁，调用带上下文的合并。成功后用合并后的 YAML 替换 `OURS`。

rebase 时，驱动把正在重放的提交的原始父提交当作 base，把当前已完成重放的 `HEAD` 当作 ours。使用合并后的工作区之前，它会核对 Git 提供的锁文件与这些提交是否一致。

失败时，驱动把 Git 对三份输入做文本合并的结果写回锁文件并返回 `1`。Git 保留未合并状态，开发者可以解决冲突，或检查诊断中点名的依赖。文本合并干净但图无效时这一点很重要：诊断仍会阻止合并，直到有人确认结果。

### 原始 YAML 中的重复 snapshot

文本合并可能留下同一个 snapshot 键两次。输入是原始 YAML 时，Locksmith 可以不偏向第一次或最后一次出现，直接恢复。它把重复值解析成候选，逐个组合尝试，只有所有组合都产出相同的、通过校验的图（包括 override 优先级）时才接受结果。搜索上限是 16 种组合。超过上限时返回 `ResolutionRequired`，不会做无界的工作。

### 合并上下文

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

### 走一遍的例子

应用依赖 `lib@1.0.0`，它从应用获得 peer `a` 和 `b`。base 有 `a@1.0.0` 和 `b@1.0.0`。ours 把 `a` 升到 `2.0.0`。theirs 把 `a` 升到 `3.0.0`，把 `b` 升到 `2.0.0`。合并后的 manifest 允许任意版本的 `a`，要求 `b@2.0.0`。两边的 `a` 都有效，目标（`theirs`）打破平局。只有 theirs 有要求的 `b`。

结果是一条 `lib@1.0.0(a@3.0.0)(b@2.0.0)` snapshot。应用的 importer 指向这条 snapshot，snapshot 记录 `a: 3.0.0` 和 `b: 2.0.0`。分支偏好按依赖逐条生效；这就是为什么结果可以从同一个分支取 `a` 和 `b`，也可以在约束需要时从不同分支取。

### 失败模式

- **冲突**：可达记录有分歧，最终声明也无法裁决。不返回锁文件。
- **ResolutionRequired**：缺少版本、发布声明或 peer 组合。请求会说明调用方需要补充什么。
- **peer 要求未满足**：最终 peer 策略拒绝结果。CLI 报告问题；启用 `strictPeerDependencies` 时转为错误。Rust 调用方可以用 `inspect_peers` 获取相同的问题列表。

## 4. converge

`converge` 从安装后的候选锁文件出发，去掉本次修改不需要的变更。这就是“改一个依赖，锁文件其余部分保持不变”背后的操作。

### 输入

- 基准锁文件和安装后的候选锁文件。
- 每个有变更的 importer 在修改前后的 manifest。
- catalog、override 或包扩展有变更时，基准和最终的工作区配置。
- 可选的 registry 元数据，用于补充缺失的发布范围。

### 算法

1. 对每个 importer 依赖，新 manifest 范围仍接受基准 pin 时保留它；不再接受时采用安装后候选的 pin。
2. 新增依赖采用安装后候选的 pin，并带入它要求的闭包。
3. 删除依赖时清理不再可达的记录。
4. 按使用方重新绑定 peer。提供方改变的使用方得到新的 snapshot；无关使用方保留基准绑定。如果循环或共享实例需要互不兼容的绑定，收敛在这些路径上保留安装后的 peer 选择，其他路径上的子依赖仍可恢复。
5. 工作区规则有变更时，找出每条 catalog、override 或包扩展变更影响的项目。新规则要求的边采用安装后的结果，其余恢复。父包版本未变、补丁和 peer 匹配时，已有实例能作为子依赖 pin 的证据；父包升级后，可以核对发布时的依赖范围、完整性信息和生效的 override，再决定能否恢复旧的子依赖版本。
6. 清理不可达记录，校验结果。

### 和 pnpm 的对比

`pnpm install --lockfile-only` 从 manifest 解析整张图。范围会被重新评估，无关记录也可能移动。`converge` 只在本次修改要求的地方接受安装后的图。结果是一个有效的锁文件，变更集是本次修改能够证明的最小集合。

### 走一遍的例子

基准：`foo@1.0.0` 依赖 `leaf@1.0.0`，应用还依赖 `bar@1.0.0`。本次修改把 `foo` 升到 `2.0.0`。安装后的候选里有 `foo@2.0.0` → `leaf@2.0.0`，以及 `bar@1.1.0`。

`converge` 保留 `foo` 和 `leaf` 的 `2.0.0`，因为本次修改要求它们。`bar` 恢复为 `1.0.0`，因为 `^1.0.0` 仍然接受基准 pin；`bar@1.1.0` 被清理。应用的依赖图只在本次修改要求的地方变化。

### 失败模式

- **要求无法满足**：安装后的候选缺少本次修改要求的记录，也没有可用的 registry 元数据。
- **配置不一致**：基准锁文件和安装后的锁文件使用了不同的解析设置，pin 无法比较。
- **peer 要求未满足**：最终 peer 策略拒绝收敛结果。

## 5. repair

`repair` 修复 Git 已经合并过的锁文件。它从当前锁文件出发，只修改工作区证据要求的记录。

### 输入

- 锁文件文本。
- 同一个提交中的 manifest、工作区配置和补丁。
- 记录缺失时，父提交的锁文件及其配置。
- 可选的 registry 元数据。

### 算法

1. 解析锁文件。内容相同的重复 snapshot 记录可以接受；同一个键下内容不同时需要调用方决定。
2. 检查配置一致性：`settings`、`overrides`、`patchedDependencies`、`packageExtensionsChecksum`、`pnpmfileChecksum` 和 `ignoredOptionalDependencies`。锁文件由不同的规则生成时，修复会拒绝；这些规则的影响超出单条坏记录。
3. 用 manifest 校正 importer 声明：更新过期的版本引用，删除 manifest 不再声明的依赖，根据可用证据补入新声明。
4. 从 importer 计算可达闭包。每条缺失的 snapshot 或 package 记录按以下顺序寻找证据：
   - 内容一致的父提交锁文件。包含该记录的父提交必须全部一致，配置必须兼容。
   - registry 元数据。已发布的版本可以提供 package 记录或只含 peer 的 snapshot。
   - 用当前锁文件和父提交做带上下文的解析。这条回退路径最多使用两个父提交，用于处理 peer 或补丁标识只在多个输入组合后出现的记录。
5. 重新应用包扩展和 peer 绑定，重复直到图稳定。
6. 输入健康时，逐字节返回原文。

### 和 pnpm 的对比

`pnpm install` 从 manifest 重写锁文件。`repair` 保留所有有效的 pin，只增加或删除让图恢复一致所必需的记录。它还保留原文，所以健康的输入会原样返回。

### 失败模式

- **配置不一致**：锁文件与工作区在设置、override、补丁或扩展校验和上不一致。
- **记录无法补齐**：父提交、registry 和带上下文的解析都无法提供缺失记录。
- **父提交之间不一致**：可用的父提交对同一个缺失键给出不同内容。

## 6. 取舍与边界

### 三个命令

每个操作有不同的偏好模型。`merge` 偏好目标分支，`converge` 偏好基准，`repair` 偏好当前锁文件。输入也不同：三个锁文件、两个锁文件加 manifest 变更，或一个锁文件加工作区证据。合成一个命令需要模式开关，反而掩盖这些差异。

### 库不做 I/O

库不做文件系统、网络、Git 或包管理器操作。CLI 读取项目文件、Git 对象和 registry 元数据，再把数据以内存形式传入。这样库可以嵌入其他程序，操作也能脱离仓库测试。

### 默认离线

registry 访问需要显式加上 `--allow-network`。锁文件通常已经包含足够证据，可以在不访问 registry 的情况下完成 merge、converge 或 repair。缺少元数据时，库返回 `RegistryRequest` 而不是猜测，CLI 可以取得元数据后重试。

### 逐条选择，而非整图解析

Locksmith 逐条选择记录并沿边追踪。这能保留无关的 pin，但依赖证据：任何输入里都没有、registry 也查不到的记录无法凭空生成。整图解析是 pnpm 已经做过的事，也是本工具要避免的无关变更的来源。

### repair 使用父提交

合并提交可能丢掉两个父提交都还保留的记录。修复把提交的父提交当作证据，但只在它们内容一致、配置匹配时使用。直接搜索可以使用任意数量内容一致的父提交；带上下文的回退最多使用两个。父提交更多的合并需要缩小问题范围。

### 16 组合预算

重复 snapshot 恢复是对解释的搜索。预算同时限制工作量和内存。超过预算时返回 `ResolutionRequired`，由调用方选择一侧或补充证据。无界搜索会让极端输入挂起。

### 确定性与校验

没有变化的记录直接复用，不重新渲染。override 的顺序会保留，因为它影响优先级。写出之前，序列化后的 YAML 会重新反序列化并与内存中的值比较；不一致是错误，不会产生半成品文件。

### 支持的格式

Locksmith 支持 `lockfileVersion: '9.0'`，即 pnpm 9–12 的默认格式。它会读取 pnpm 11 和 12 为包管理器依赖写出的独立 environment 文档。实验性锁文件格式会按格式版本直接拒绝，与 pnpm 版本无关。

Locksmith 不安装依赖包，不支持旧版锁文件格式，也不替代 pnpm 的解析器。它修改锁文件；安装仍由 `pnpm install` 完成。

### 失败契约

独立运行的命令在成功时把 YAML 写到标准输出，诊断写到标准错误。退出码 `1` 表示冲突、依赖选择待定或 peer 要求未满足；其他错误使用 `2`。Git 驱动失败时返回 `1`，并把文本合并的结果写回锁文件，保留 Git 的冲突状态。任何操作都不会写出部分结果。

### 代码地图

| 区域 | 模块 | 职责 |
| --- | --- | --- |
| 解析与输出 | `source`、`verify` | YAML 文档、原文复用、输出校验 |
| 图 | `graph` | 可达性、闭包、引用校验 |
| 选择 | `scope`、`peer`、`reconcile` | importer 边、peer 绑定、候选选择 |
| 组装 | `assembly`、`reconcile::dedupe` | snapshot 与 package 组装、去重 |
| 修复 | `repair` | 证据查找、importer 校正 |
