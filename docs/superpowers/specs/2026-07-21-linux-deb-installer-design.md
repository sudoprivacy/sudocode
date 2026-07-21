# scode Linux deb 安装包设计

日期：2026-07-21

## 背景

当前 Linux 交付物主要是 `scode` 单个可执行二进制和 `tar.gz` 压缩包。客户希望获得一个标准 Linux 软件安装包，安装到 Ubuntu 服务器后可以直接使用：

- 安装完成后在任意路径执行 `scode`。
- 安装过程中支持配置模型、Base URL、API Key 和联网搜索。
- 客户服务器可能没有图形界面，安装流程必须支持纯终端交互。
- 后续用户可以重新更新模型配置。
- 安装包应作为 GitHub Release 的正式产物发布。

## 目标

为 Ubuntu/Debian 系统新增按版本命名的安装包，例如 `scode_0.1.12_amd64.deb`。

安装后提供两个全局命令：

```bash
scode
scode-setup
```

命令职责：

- `scode`：主 CLI 程序。
- `scode-setup`：首次配置和后续配置更新向导。

## 非目标

- 不做 GUI 安装器。目标服务器可能无桌面环境。
- 不在本阶段实现 apt repository。先通过 GitHub Release 发布 `.deb` 文件，客户使用 `apt install ./scode_0.1.12_amd64.deb` 这类命令安装。
- 不覆盖用户已有配置。升级包时保留用户配置。
- 不把 API Key 写入系统级 `/etc` 配置。

## 推荐交付形态

Release 产物建议包含：

```text
scode-linux-x64.tar.gz
scode-linux-x64-musl.tar.gz
scode_0.1.12_amd64.deb
SHA256SUMS.txt
```

`.deb` 是 Ubuntu 客户的主交付物；`tar.gz` 和 musl 静态包继续保留，用于非 Debian 系、脚本安装或临时排障。

## deb 包文件布局

```text
/usr/bin/scode
/usr/bin/scode-setup
/usr/lib/scode/scode-setup
/usr/share/doc/scode/README.md
/DEBIAN/control
/DEBIAN/postinst
/DEBIAN/prerm
```

说明：

- `/usr/bin/scode` 放主二进制，保证任意路径可执行。
- `/usr/lib/scode/scode-setup` 放配置脚本真实文件。
- `/usr/bin/scode-setup` 可以是软链接，也可以是一个很薄的 wrapper，方便用户直接运行。
- `/usr/share/doc/scode/README.md` 放基础使用说明。
- `postinst` 在安装后触发首次配置向导。
- `prerm` 暂时不删除用户配置，只处理包管理需要的清理动作。

## 包元数据

`DEBIAN/control` 示例：

```text
Package: scode
Version: 0.1.12
Section: devel
Priority: optional
Architecture: amd64
Maintainer: sudocode
Depends: ca-certificates, curl
Description: Sudo Code command line coding agent
```

依赖保持最小：

- `curl`：安装期拉取模型列表。
- `ca-certificates`：HTTPS 请求需要证书链。

## 安装体验

客户执行：

```bash
sudo apt install ./scode_0.1.12_amd64.deb
```

安装后 `postinst` 自动触发终端向导：

```text
Sudo Code · scode 首次配置向导

Base URL [https://hk.sudorouter.ai/v1]:
API Key:
正在拉取模型列表...

请选择默认模型：
   1) deepseek-v4-pro
   2) claude-sonnet-4-6
   3) gpt-5

启用联网搜索 web_search？[Y/n]
```

配置写入：

```text
~/.nexus/sudocode/sudocode.json
~/.nexus/sudocode/settings.json
```

安装完成后：

```bash
scode --version
scode doctor
scode -p "你好"
```

## 真实用户目录解析

安装通常由 `sudo apt install` 执行。如果 `postinst` 直接写 `$HOME`，可能会写到 `/root/.nexus/sudocode`，这不符合用户预期。

`postinst` 和 `scode-setup --install` 应解析真实登录用户：

优先级：

```text
1. SUDO_USER
2. logname
3. 当前有效用户
```

当解析到真实用户后：

- 配置目录写入该用户的 home，例如 `/home/ubuntu/.nexus/sudocode`。
- 文件 owner 设置为真实用户。
- 目录权限为 `0700`。
- `sudocode.json` 权限为 `0600`，因为包含 API Key。

如果无法可靠解析真实用户，则提示用户安装完成后手动运行：

```bash
scode-setup
```

## 图形界面约束

安装包不依赖 GUI。所有配置通过 shell 终端完成。

配置脚本必须只依赖常见基础工具：

- POSIX shell 或 bash
- curl
- sed
- grep
- awk
- chmod/chown/mkdir

不依赖：

- Python
- jq
- whiptail/dialog
- X11/Wayland

这样可以在最小化 Ubuntu Server、SSH 会话和云服务器控制台中运行。

## 非交互安装行为

服务器自动化部署可能使用：

```bash
DEBIAN_FRONTEND=noninteractive apt install ./scode_0.1.12_amd64.deb
```

也可能由 CI、Ansible、cloud-init 或脚本执行。安装包不能在非交互环境中阻塞。

规则：

- 如果 stdin/stdout 是 TTY，则自动进入 `scode-setup --install`。
- 如果不是 TTY，则跳过交互配置，安装仍然成功。
- 跳过时打印明确提示：

```text
scode installed.
No interactive terminal detected, skipped setup.
Run `scode-setup` as the target user to configure models.
```

## scode-setup 设计

`scode-setup` 是可重复运行的配置管理器，不只是首次安装脚本。

### 首次配置

当未发现 `~/.nexus/sudocode/sudocode.json` 时，直接进入完整配置：

```text
1. 输入 Base URL，默认 https://hk.sudorouter.ai/v1
2. 隐藏输入 API Key
3. 请求用户配置的 Base URL 加 `/models`
4. 解析模型 id 列表
5. 选择默认模型
6. 选择是否启用 web_search
7. 写入 sudocode.json 和 settings.json
```

### 已有配置时

检测到已有配置后显示菜单：

```text
检测到已有配置：~/.nexus/sudocode/sudocode.json

请选择操作：
  1) 只修改默认模型
  2) 重新拉取模型列表并选择默认模型
  3) 修改 Base URL / API Key 并重建配置
  4) 开启/关闭 web_search
  5) 退出
```

默认推荐操作是 `1) 只修改默认模型`。

### 修改默认模型

读取现有 `sudocode.json` 的模型列表，让用户选择默认模型，只更新：

```text
~/.nexus/sudocode/settings.json
```

### 重新拉取模型列表

使用现有 Base URL 和 API Key 请求 `/models`，刷新 `sudocode.json` 中的 `models` 块，然后让用户重新选择默认模型。

### 修改 Base URL / API Key

完整重走首次配置流程，并重建：

```text
sudocode.json
settings.json
```

### web_search 更新

允许用户开启或关闭 `web_search` 配置。默认搜索地址保持：

```text
https://hk.sudorouter.ai/search/tavily/search
```

## 生成的配置格式

`sudocode.json` 复用现有 macOS/Windows 逻辑：

```json
{
  "models": {
    "deepseek-v4-pro": {
      "alias": "deepseek-v4-pro",
      "name": "deepseek-v4-pro",
      "input": ["text"],
      "providers": {
        "proxy": {
          "provider": "sudorouter",
          "model": "deepseek-v4-pro",
          "api": "openai-completions"
        }
      }
    }
  },
  "auth_modes": {
    "proxy": {
      "sudorouter": {
        "baseUrl": "https://hk.sudorouter.ai/v1",
        "apiKey": "example-api-key"
      }
    }
  },
  "web_search": {
    "provider": "tavily",
    "apiUrl": "https://hk.sudorouter.ai/search/tavily/search",
    "apiKey": ""
  }
}
```

`settings.json` 示例：

```json
{ "model": "deepseek-v4-pro" }
```

## 非交互配置能力

为了支持自动化部署，`scode-setup` 应支持：

```bash
SCODE_BASE_URL=https://hk.sudorouter.ai/v1 \
SCODE_API_KEY=sk-xxx \
SCODE_MODEL=deepseek-v4-pro \
SCODE_ENABLE_SEARCH=1 \
scode-setup --non-interactive
```

规则：

- `SCODE_API_KEY` 必填。
- `SCODE_MODEL` 可选；未提供时优先使用默认模型 `deepseek-v4-pro`，若模型列表不存在该模型，则使用拉取列表中的第一个模型。
- `SCODE_BASE_URL` 默认 `https://hk.sudorouter.ai/v1`。
- `SCODE_ENABLE_SEARCH` 默认 `1`。
- 非交互模式失败时返回非零退出码，并输出可诊断错误。

## 升级和卸载行为

### 升级

执行：

```bash
sudo apt install ./scode_0.1.13_amd64.deb
```

或后续 apt repository 支持后：

```bash
sudo apt upgrade scode
```

升级规则：

- 替换 `/usr/bin/scode` 和 `scode-setup`。
- 不覆盖 `~/.nexus/sudocode/sudocode.json`。
- 不覆盖 `~/.nexus/sudocode/settings.json`。
- 如果已有配置，`postinst` 只提示：

```text
Existing scode config detected. It was not modified.
Run `scode-setup` to update models or credentials.
```

### 卸载

执行：

```bash
sudo apt remove scode
```

卸载规则：

- 删除包安装的系统文件。
- 默认保留用户目录下的配置和会话数据。
- 不删除 API Key 配置，避免误删用户数据。

如果未来支持 purge：

```bash
sudo apt purge scode
```

可以只删除系统级残留，不建议自动遍历所有用户 home 删除个人配置。

## 构建脚本设计

新增目录：

```text
packaging/linux/deb/
  build-deb.sh
  control.template
  postinst
  prerm
  scode-setup
```

`build-deb.sh` 输入：

```bash
packaging/linux/deb/build-deb.sh \
  --version 0.1.12 \
  --binary rust/target/release/scode \
  --output rust/dist
```

输出：

```text
rust/dist/scode_0.1.12_amd64.deb
```

实现方式：

```text
1. 创建临时 package root
2. 写入 DEBIAN/control、postinst、prerm
3. 复制 scode 到 usr/bin/scode
4. 复制 scode-setup 到 usr/lib/scode/scode-setup
5. 创建 usr/bin/scode-setup wrapper 或软链接
6. 复制 README
7. 设置权限
8. dpkg-deb --build
```

## Release 集成

在 `.github/workflows/release.yml` 的 Linux x64 构建完成后新增 deb 打包步骤。

建议新增独立 job：

```text
build-linux-deb
```

依赖：

```text
build-linux-x64
```

产物：

```text
scode_0.1.12_amd64.deb
```

发布时 `publish-release` 下载所有 artifact，生成统一 `SHA256SUMS.txt`，并上传：

```text
scode-linux-x64.tar.gz
scode-linux-x64-musl.tar.gz
scode-linux-arm64.tar.gz
scode-macos-arm64.tar.gz
scode-macos-x64.tar.gz
scode-windows-x64.zip
scode-windows-arm64.zip
scode_0.1.12_amd64.deb
SHA256SUMS.txt
```

## 测试计划

### 本地包结构测试

```bash
dpkg-deb --info scode_0.1.12_amd64.deb
dpkg-deb --contents scode_0.1.12_amd64.deb
```

检查：

- `/usr/bin/scode` 存在且权限为可执行。
- `/usr/bin/scode-setup` 存在且权限为可执行。
- `postinst` 存在且权限为可执行。
- `control` 元数据合法。

### Ubuntu 容器安装测试

```bash
docker run --rm -it -v "$PWD/dist:/dist" ubuntu:22.04 bash
apt-get update
apt-get install -y /dist/scode_0.1.12_amd64.deb
scode --version
scode-setup
```

检查：

- 安装成功。
- `scode` 任意路径可执行。
- 无 GUI 依赖。
- 交互向导可运行。

### 非交互安装测试

```bash
DEBIAN_FRONTEND=noninteractive apt-get install -y ./scode_0.1.12_amd64.deb
```

检查：

- 安装不阻塞。
- 未配置时给出清晰提示。
- 返回码为 0。

### 非交互配置测试

```bash
SCODE_BASE_URL=https://hk.sudorouter.ai/v1 \
SCODE_API_KEY=sk-test \
SCODE_MODEL=deepseek-v4-pro \
SCODE_ENABLE_SEARCH=1 \
scode-setup --non-interactive
```

检查：

- 生成 `sudocode.json`。
- 生成 `settings.json`。
- 文件权限正确。
- owner 是目标用户。

### 升级测试

```bash
sudo apt install ./scode_0.1.12_amd64.deb
sudo apt install ./scode_0.1.13_amd64.deb
```

检查：

- 二进制版本更新。
- 用户已有配置没有被覆盖。
- `scode-setup` 更新为新版本。

### 卸载测试

```bash
sudo apt remove scode
```

检查：

- `/usr/bin/scode` 删除。
- `/usr/bin/scode-setup` 删除。
- 用户配置保留。

## 风险和处理

### sudo 安装写入 root 配置

风险：安装时写到 `/root/.nexus/sudocode`。

处理：`postinst` 解析真实用户；无法解析时跳过自动配置并提示手动运行。

### 非交互安装卡住

风险：自动化部署时安装过程等待输入。

处理：只有检测到 TTY 才运行交互向导。

### API Key 泄露

风险：配置文件包含 API Key。

处理：写入用户 home，目录 `0700`，配置文件 `0600`，不打印 API Key。

### 模型列表解析不完整

风险：shell 用 grep/sed 解析 JSON 不如 jq 稳健。

处理：保持与现有 macOS/Windows 逻辑一致，解析 OpenAI 风格 `/models` 中的 `id` 字段；后续如果 CLI 内置配置命令，可迁移到 Rust 实现。

### 安装时网络不可达

风险：服务器不能访问模型接口。

处理：配置向导失败不应破坏包安装；提示用户检查网络/API Key 后运行 `scode-setup` 重试。

## 成功标准

- GitHub Release 中出现 `scode_0.1.12_amd64.deb` 这类版本化 deb 产物。
- Ubuntu 20.04/22.04/24.04 上可以通过 `apt install ./scode_0.1.12_amd64.deb` 安装。
- 安装后任意路径可执行 `scode`。
- 安装后任意路径可执行 `scode-setup`。
- 有交互终端时安装后自动进入配置向导。
- 无交互终端时安装不阻塞，并提示后续运行 `scode-setup`。
- `scode-setup` 可重复运行，用于更新模型、Base URL、API Key 和 web_search。
- 升级不会覆盖用户已有配置。
- 卸载不会删除用户配置。
