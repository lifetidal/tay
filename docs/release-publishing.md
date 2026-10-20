# 发布与 AUR 流程

> 2026-10-20。CI release workflow **已落地**
> (`.github/workflows/release.yml`,tag `v*` 触发):自动构建
> tay/tayd release 二进制并附加到 GitHub Release(附 SHA256SUMS)。
> 它产出的是**裸二进制**;Arch 包(`pkg.tar.zst`)与 AUR 仍按本文
> 手动执行 —— AUR 从源码构建,需要真实源码 tar 校验和,与
> CI 二进制产物是两条独立链路。
> 未实际执行过的步骤明确标注「待首次执行」,不写成已完成。

## 1. 版本规则

- **单一版本源**:`Cargo.toml` 的 `[workspace.package] version`
  (当前 `0.1.0`)。各 crate 使用 `version.workspace = true`。
- `tools/package-arch.sh` 用 `sed` 从该字段读取版本,并校验
  `^[0-9]+\.[0-9]+\.[0-9]+$`;不合规则直接退出。
- Git tag 使用 `vX.Y.Z`(带 `v`);PKGBUILD 的 `pkgver` 使用
  `X.Y.Z`(不带 `v`)。
- `pkgrel` 是打包修订号,从 `1` 开始:
  - **上游版本变化**:`pkgver` 跟随 `Cargo.toml`, `pkgrel` 重置为 `1`;
  - **仅打包/依赖/安装文件变化**:`pkgver` 不变,`pkgrel` 加 1。
- 当前事实:`packaging/arch/PKGBUILD` 为 `pkgver=0.1.0`、
  `pkgrel=11`;`dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst` 是本机
  已构建产物。AUR 草案 `packaging/aur/PKGBUILD` 的 `pkgver` 仍是
  `r9.a6feccc`(VCS 包的示例值),实际构建时由 `pkgver()` 更新。
- 版本顺序固定为:`Cargo.toml` → 提交 → tag `vX.Y.Z` → GitHub Release
  → AUR。source URL 指向尚不存在的 tag 会导致 `makepkg` 下载失败。

## 2. 本地打包(`tools/package-arch.sh`)

以**普通用户**在仓库根目录运行;`makepkg` 拒绝 root。

```sh
sudo pacman -S --needed base-devel rust
./tools/package-arch.sh
```

### 2.1 选项

| 选项 | 行为 |
|---|---|
| 无参数 | 运行工作区测试,构建 release 包 |
| `--offline` | 设置 `CARGO_NET_OFFLINE=true`,仅用 Cargo 缓存 |
| `--syncdeps` | 给 `makepkg` 加 `--syncdeps`,允许用 sudo 安装缺失的构建依赖 |
| `--skip-tests` | 给 `makepkg` 加 `--nocheck`,跳过 PKGBUILD 的 `check()` |
| `-h` / `--help` | 打印用法 |

发布前不要使用 `--skip-tests`;`--offline` 只适用于缓存完整的环境。

### 2.2 脚本实际做了什么

1. 检查当前用户不是 root,且 `makepkg`、`tar`、`sha256sum`、`sed` 存在。
2. 在 `${TMPDIR:-/tmp}/tay-package.XXXXXXXX` 创建临时构建目录,结束时删除。
3. 只打包显式输入,避免把 `.git`、`target`、用户配置或构建产物带入:
   `Cargo.toml`、`Cargo.lock`、`LICENSE`、`README.md`、`crates`、
   `config`、`packaging`;归档顶层目录为 `tay/`。
4. 复制 `packaging/arch/PKGBUILD` 到临时目录。
5. 从 `Cargo.toml` 读取并校验版本,计算源码归档的 SHA256。
6. **只修改临时副本**:把 `pkgver=` 改成读取到的版本,把
   `sha256sums=('SKIP')` 改成实际摘要。仓库内的
   `packaging/arch/PKGBUILD` 不会被改写。
7. 设置 `PKGDEST=dist/arch`、`PKGEXT=.pkg.tar.zst`、
   `CARGO_TARGET_DIR=target/arch-package`,执行 `makepkg --force`
   (以及上面选择的 `--syncdeps` / `--nocheck`)。
8. 打印产物列表。

### 2.3 产物与安装

```text
dist/arch/tay-<pkgver>-<pkgrel>-<arch>.pkg.tar.zst
例:dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst
```

- 包内文件:`/usr/bin/tay`、`/usr/bin/tayd`、desktop 文件、图标、
  `tayd.service`、`/usr/share/doc/tay/examples/config.lua`、
  README 与 LICENSE。
- 安装:`sudo pacman -U ./dist/arch/tay-*.pkg.tar.zst`。
- 检查:`pacman -Qip <包>`、`pacman -Ql tay`;也可用 `bsdtar -tf`
  查看内容。
- 重复构建同一版本会覆盖 `dist/arch/` 中的同名包;重新打包而不改
  上游版本时,先提高 `packaging/arch/PKGBUILD` 的 `pkgrel`。
- 构建缓存保留在 `target/arch-package/`,不写入用户家目录。
- 源码来自**当前工作树**,包含未提交修改。正式发布必须在干净的
  tag 检出上重新打包,不能直接使用本地脏树产物。

## 3. GitHub Releases(当前手动,CI 未来)

### 3.1 预检

```sh
git status --short          # 应为空
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
./tools/package-arch.sh     # 不要加 --skip-tests
```

同时按 docs/compositor-matrix.md 记录本次实际验证过的合成器;
未验证项在发行说明中照实列出。

### 3.2 打 tag 与创建 Release

```sh
git tag -a v0.1.0 -m "tay 0.1.0"
git push origin v0.1.0

sha256sum dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst \
  > dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst.sha256

gh release create v0.1.0 \
  dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst \
  dist/arch/tay-0.1.0-11-x86_64.pkg.tar.zst.sha256 \
  --title "tay 0.1.0" \
  --notes-file <发行说明文件>
```

也可在 GitHub 网页创建 Release 后手动上传产物。要点:

- tag 名与 `Cargo.toml` 版本一致(`v` 前缀只在 tag 上)。
- 附加 `dist/arch/` 的 `pkg.tar.zst` 与对应 `.sha256`。
- 发行说明至少写明:升级后需退出旧 daemon 并重新启动才能加载
  新代码;哪些合成器/输入法/多显示器项目尚未验证。
- 该包按 Arch 当前环境构建,面向 Arch Linux;其它发行版建议源码构建。

### 3.3 CI release workflow(已落地)

`.github/workflows/release.yml` 由 tag `v*` 或手动 dispatch 触发:

- 自动跑 release 构建并附加 `tay-<target>` / `tayd-<target>` /
  `SHA256SUMS` 到对应 GitHub Release;
- **不构建 `pkg.tar.zst`**(Arch 打包需要 makepkg 环境),也不发
  AUR —— 这两步按 §2 / §4 手动执行;
- CI 产物是「裸二进制」,适合快速分发;发行说明中应写明其
  面向 x86_64 与未验证清单。

## 4. AUR 发布

### 4.1 两个包

| 包名 | 类型 | `source` | `sha256sums` | 版本 |
|---|---|---|---|---|
| `tay` | 发布快照 | 指向 `vX.Y.Z` 的远程源码归档 | 实际 SHA256 | `pkgver` = `Cargo.toml` 版本 |
| `tay-git` | VCS | `git+<仓库地址>` | `SKIP` | `pkgver()` 生成 `rN.<hash>` |

两个包不能同时安装,字段必须对称:

- `tay`:`conflicts=('tay-git')`(`packaging/arch/PKGBUILD` 已有)。
- `tay-git`:`provides=('tay')` 与 `conflicts=('tay')`
  (`packaging/aur/PKGBUILD` 已有,保留)。
- 依赖与 `optdepends` 也应尽量对齐:`glibc`、`gcc-libs`、
  `wayland`、`libxkbcommon`、`fontconfig`;Vulkan/OpenGL 驱动、
  portal 后端与 Nerd Font 作为 `optdepends`。当前 AUR 草案既没有
  `optdepends`,`depends` 也未含 `glibc`/`gcc-libs`,提交前补齐。
- 许可证标识统一用 SPDX `GPL-3.0-only`,与
  `packaging/arch/PKGBUILD` 一致;AUR 草案里的 `GPL3` 是旧写法。

当前仓库状态:

- `packaging/arch/PKGBUILD` 是**本地快照**配方:`source=('tay-source.tar.gz')`、
  `sha256sums=('SKIP')`,由 `package-arch.sh` 在临时目录替换。
  它不能原样提交 AUR。
- `packaging/aur/PKGBUILD` 是早期 `tay-git` 草案,仍有占位维护者
  邮箱和占位仓库地址;`Cargo.toml` 当前记录的仓库是
  `https://github.com/lifetidal/tay`。提交前统一为实际地址。
- 仓库内**还没有**可直接提交的 `tay`(快照)AUR 配方;需要按下文
  从 `packaging/arch/PKGBUILD` 派生。

#### 从本地配方派生 AUR `tay`

需要改动的字段:

| 字段 | 本地快照配方 | AUR `tay` |
|---|---|---|
| `source` | `tay-source.tar.gz` | 远程 tag/Release 归档 URL |
| `sha256sums` | `SKIP` 占位,脚本临时替换 | 实际 SHA256,不能用 `SKIP` |
| `pkgver` | 脚本临时替换 | 手动与 `Cargo.toml` / tag 同步 |
| `pkgrel` | 手动维护 | 手动维护,规则见 §1 |
| `conflicts` | `tay-git` | 保留 `tay-git` |
| `build()` / `package()` 的 `cd` | `$srcdir/tay` | 必须匹配归档实际解压目录 |

`source` 二选一:

1. GitHub Release 附件(推荐,便于固定内容):
   `https://github.com/<owner>/tay/releases/download/v$pkgver/<归档名>`。
2. GitHub tag 自动归档:
   `$url/archive/refs/tags/v$pkgver.tar.gz`。解压目录通常是
   `tay-$pkgver`,不是 `tay`,必须同步修改 `build()`/`package()`。
   自动归档的字节稳定性不如自己上传的 Release 附件,长期复现
   优先选方案 1。

`--locked` 构建依赖源码归档中包含 `Cargo.lock`;两种方案都满足。
如果使用自己生成的 `tay-source.tar.gz`,注意 `package-arch.sh`
生成的是**工作树快照**,且临时目录在脚本结束时删除,不能直接
当作已发布源码;正式 AUR 源应来自 tag 对应的干净归档。

### 4.2 首次提交步骤

前置:AUR 账号、已上传 SSH 公钥;包名已在 AUR 网页注册
(首次发布用网页的 Submit Package 建立仓库;已有包直接 clone)。
可用 `ssh aur@aur.archlinux.org help` 验证 SSH。

`tay`(快照包):

```sh
git clone ssh://aur@aur.archlinux.org/tay.git
cd tay
cp /path/to/packaging/aur/tay/PKGBUILD .   # 派生的快照配方
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: tay 0.1.0-11"
git push
```

`tay-git`(VCS 包):

```sh
git clone ssh://aur@aur.archlinux.org/tay-git.git
cd tay-git
cp /path/to/packaging/aur/PKGBUILD .       # 当前草案,先去掉占位地址
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: tay-git <pkgver>-<pkgrel>"
git push
```

规则:

- AUR 仓库只放 `PKGBUILD` 和 `.SRCINFO`(以及必要的本地文件);
  不要提交 `*.pkg.tar.zst`、`src/`、`pkg/`。
- 每次改 `PKGBUILD` 后都要重新生成 `.SRCINFO`;AUR 不会自动生成,
  缺少或过期会让 AUR 页面显示错误的版本/依赖信息。
- 提交前在本地验证:
  ```sh
  makepkg --printsrcinfo > .SRCINFO
  makepkg --cleanbuild --force
  namcap PKGBUILD
  ```
  `makepkg -si` 可顺带验证安装;`namcap` 需要安装 `namcap`。
- 首次提交的实际推送尚未在本仓库执行,以上为待执行流程。

### 4.3 版本更新流程

`tay`(快照包):

1. 确认 `Cargo.toml` 已改为新版本并已提交、已打 tag。
2. 改 `pkgver=X.Y.Z`;新上游版本把 `pkgrel` 重置为 `1`
   (仅打包修订则 `pkgrel` 加 1)。
3. 把 `source` 改为新 tag/Release 的归档。
4. 重新计算校验和:
   ```sh
   updpkgsums          # 来自 pacman-contrib,会下载 source 后写入
   # 或下载归档后:sha256sum <归档>
   ```
5. `makepkg --printsrcinfo > .SRCINFO`。
6. `git add PKGBUILD .SRCINFO && git commit && git push`。

`tay-git`(VCS 包):

- 不要手改 `pkgver`;`pkgver()` 用
  `r$(git rev-list --count HEAD).$(git rev-parse --short HEAD)` 生成。
- 仅打包字段变化时提高 `pkgrel`;源码本身更新不需要改 `pkgver`。
- 改了 `PKGBUILD` 就重新生成 `.SRCINFO` 并推送。
- 用户安装/升级时 `makepkg` 会重新拉取 git 源。

### 4.4 `sha256sums` 与源码包

- `tools/package-arch.sh` 计算的是它**临时生成**的
  `tay-source.tar.gz` 的摘要,并只替换临时副本中的
  `sha256sums=('SKIP')`;仓库文件始终保留 `SKIP`。
- 手工提交 AUR 时,**`tay` 不能使用 `SKIP`**。必须针对实际
  下载的源码归档重新计算:`updpkgsums`,或下载后
  `sha256sum <文件>`,把结果写入 `sha256sums`。
- `tay-git` 使用 `source=("git+...")` 时,`sha256sums=('SKIP')`
  是正确写法;VCS 包不钉固定摘要。
- GitHub 自动生成的 tag 归档不保证长期字节一致;若需要可复现,
  把发布用的源码归档作为 Release 附件上传,并固定其摘要。
- 更换归档、重新打包源码或改变归档生成方式后,摘要必须重算;
  不要沿用旧值。

## 5. 发布检查清单

- [ ] `Cargo.toml` 的版本已更新,`Cargo.lock` 与新依赖一致。
- [ ] `packaging/arch/PKGBUILD` 的 `pkgver`/`pkgrel` 正确
      (新版本 `pkgrel=1`;仅打包修订则加 1)。
- [ ] 测试、Clippy、构建、`git diff --check` 通过。
- [ ] 在干净工作树上运行 `./tools/package-arch.sh`,确认产物在
      `dist/arch/` 且未使用 `--skip-tests`。
- [ ] 在真实 Wayland 会话运行相关 smoke;按
      docs/compositor-matrix.md 记录验证范围,不把 KDE 结果外推。
- [ ] 提交版本变更,创建并推送 `vX.Y.Z` tag。
- [ ] GitHub Release 附加 `pkg.tar.zst`、`.sha256` 与发行说明。
- [ ] AUR `tay`:更新 `pkgver`/`pkgrel`/`source`,重算
      `sha256sums`,重新生成 `.SRCINFO`,推送。
- [ ] AUR `tay-git`:字段变化时提高 `pkgrel`,重新生成
      `.SRCINFO`,推送;确认 `provides=('tay')`、
      `conflicts=('tay')` 未丢失。
- [ ] 从 Release 下载产物,校验 SHA256,在干净环境
      `sudo pacman -U` 后运行 `tay doctor` 与一次呼出。

## 6. 待落地与未验证

- CI release workflow 尚未实现;本文的 GitHub Release 步骤是
  过渡期手动流程。
- AUR 推送尚未在本仓库执行;`packaging/aur/PKGBUILD` 仍是早期
  草案,占位地址/邮箱需要替换。
- 仓库内尚无 AUR `tay` 快照配方,需按 §4.1 从本地配方派生。
- 本机只验证过 `x86_64` 产物;PKGBUILD 声明了 `aarch64`,
  但未实测。
- AUR 首次提交(含本地生成 `.SRCINFO`)与后续升级流程
  待首次发布时执行并补充记录。
