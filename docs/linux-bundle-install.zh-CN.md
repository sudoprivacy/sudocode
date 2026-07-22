# Linux bundle 安装使用说明

本文档说明如何使用 `scode-linux-x64-bundle.tar.gz` 在 Linux 服务器上安装
`scode`。该方式不依赖 deb、rpm、apt 或 yum，适合客户服务器发行版和版本不确定的场景。

## 适用场景

推荐在以下场景使用 bundle：

- 客户服务器不确定是 Ubuntu、Debian、CentOS、RHEL、Rocky 还是其他发行版。
- 服务器没有可用的 apt/dpkg。
- 客户希望离线传包安装。
- deb 安装包在目标环境不可用。

如果客户明确是 Ubuntu/Debian，仍然可以优先使用 deb 安装包。

## 下载产物

从 GitHub Actions 或 Release 下载：

```text
scode-linux-x64-bundle.tar.gz
```

解压后目录结构：

```text
scode-linux-x64-bundle/
  scode
  scode-setup
  README-install.zh-CN.md
  VERSION
  SHA256SUMS.txt
```

## 上传到服务器

```bash
scp scode-linux-x64-bundle.tar.gz ubuntu@your-server:/tmp/
ssh ubuntu@your-server
```

## 安装

在服务器上执行：

```bash
cd /tmp
tar -xzf scode-linux-x64-bundle.tar.gz
cd scode-linux-x64-bundle
sudo ./scode-setup install
```

默认安装到：

```text
/usr/local/bin/scode
/usr/local/bin/scode-setup
/usr/local/lib/scode/install-manifest
```

说明：

- `/usr/local/bin` 是手工安装软件的推荐路径。
- 安装后任意目录都可以执行 `scode` 和 `scode-setup`。
- `install-manifest` 用于记录脚本安装过哪些文件，方便后续卸载。

如果客户明确要求安装到 `/usr/bin`：

```bash
sudo ./scode-setup install --bin-dir /usr/bin
```

## 首次配置模型

`sudo ./scode-setup install` 安装完成后，会进入终端配置向导。

向导会询问：

```text
Base URL
API Key
默认模型
是否启用 web_search
```

默认 Base URL：

```text
https://hk.sudorouter.ai/v1
```

配置文件写入真实登录用户的 home 目录，而不是 root：

```text
~/.nexus/sudocode/sudocode.json
~/.nexus/sudocode/settings.json
```

`sudocode.json` 包含 API Key，不要直接发送给他人。

## 验证安装

```bash
which scode
which scode-setup
scode --version
```

预期：

```text
/usr/local/bin/scode
/usr/local/bin/scode-setup
```

如果安装到了 `/usr/bin`，预期为：

```text
/usr/bin/scode
/usr/bin/scode-setup
```

检查安装记录：

```bash
cat /usr/local/lib/scode/install-manifest
```

运行安装检查：

```bash
scode-setup doctor
```

运行配置检查：

```bash
ls -la ~/.nexus/sudocode
cat ~/.nexus/sudocode/settings.json
```

`settings.json` 应类似：

```json
{ "model": "gpt-5" }
```

安全查看 `sudocode.json`：

```bash
sed -E 's/("apiKey"[[:space:]]*:[[:space:]]*)"[^"]*"/\1"***"/g' \
  ~/.nexus/sudocode/sudocode.json
```

试跑：

```bash
scode doctor
scode -p "你好"
```

## 后续更新模型配置

安装完成后，直接运行：

```bash
scode-setup
```

等价于：

```bash
scode-setup configure
```

已有配置时会显示：

```text
1) 只修改默认模型
2) 重新拉取模型列表并选择默认模型
3) 修改 Base URL / API Key 并重建配置
4) 开启/关闭 web_search
5) 退出
```

建议：

- 只切换默认模型：选择 `1`。
- 服务端模型列表变化：选择 `2`。
- Base URL 或 API Key 变化：选择 `3`。
- 调整联网搜索：选择 `4`。

## 非交互配置

自动化部署可以使用：

```bash
SCODE_BASE_URL=https://hk.sudorouter.ai/v1 \
SCODE_API_KEY=your-api-key \
SCODE_MODEL=deepseek-v4-pro \
SCODE_ENABLE_SEARCH=1 \
scode-setup configure --non-interactive
```

如果不希望访问 `/models`，可以提供模型列表：

```bash
SCODE_API_KEY=your-api-key \
SCODE_MODEL=gpt-5 \
SCODE_MODELS=gpt-5,gpt-5-mini,gpt-4.1 \
scode-setup configure --non-interactive
```

## 只安装，不配置

自动化场景中，如果只想安装二进制，暂时不生成模型配置：

```bash
sudo SCODE_SKIP_CONFIG=1 ./scode-setup install
```

后续由目标用户运行：

```bash
scode-setup configure
```

## 升级

下载新的 bundle 后重新执行安装：

```bash
tar -xzf scode-linux-x64-bundle.tar.gz
cd scode-linux-x64-bundle
sudo ./scode-setup install
```

升级会替换：

```text
/usr/local/bin/scode
/usr/local/bin/scode-setup
```

不会覆盖用户已有模型配置：

```text
~/.nexus/sudocode/sudocode.json
~/.nexus/sudocode/settings.json
```

如需刷新模型列表：

```bash
scode-setup configure
```

然后选择：

```text
2) 重新拉取模型列表并选择默认模型
```

## 卸载

如果使用默认安装路径：

```bash
sudo scode-setup uninstall
```

自动化场景：

```bash
sudo scode-setup uninstall --yes
```

如果安装时指定了 `/usr/bin`：

```bash
sudo scode-setup uninstall --bin-dir /usr/bin
```

卸载会删除脚本安装的全局文件：

```text
scode
scode-setup
install-manifest
```

用户配置默认保留：

```text
~/.nexus/sudocode
```

如确认不再需要，可手动删除：

```bash
rm -rf ~/.nexus/sudocode
```

## 常见问题

### 为什么默认安装到 /usr/local/bin

`/usr/local/bin` 是 Linux 上手工安装软件的推荐位置，不容易和系统包管理器管理的
`/usr/bin` 文件冲突。

如果客户环境只允许 `/usr/bin`，可以显式指定：

```bash
sudo ./scode-setup install --bin-dir /usr/bin
```

### sudo 安装后配置是否会写到 root

不会。安装脚本会尽量识别真实登录用户，并把配置写入该用户的：

```text
~/.nexus/sudocode
```

如果无法识别真实用户，会提示用户安装后手动运行：

```bash
scode-setup configure
```

### 安装时没有进入配置向导

如果当前不是交互式终端，安装脚本不会阻塞等待输入。安装后运行：

```bash
scode-setup configure
```

### settings.json 内容应该是什么

应只包含默认模型：

```json
{ "model": "gpt-5" }
```

如果内容异常，重新运行：

```bash
scode-setup configure
```
