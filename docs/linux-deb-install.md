# Linux deb install guide

This guide covers installing and validating the Ubuntu/Debian `scode`
package produced by the `Release binaries` workflow.

## Download the package

For a manual GitHub Actions run, download the `scode-linux-x64-deb`
artifact. After extracting it, you should have a file named like:

```text
scode_0.0.0.12345_amd64.deb
```

For a tagged release, download the versioned package from the GitHub
Release assets:

```text
scode_0.1.13_amd64.deb
```

## Upload to the server

Copy the package to the Ubuntu server:

```bash
scp scode_*.deb ubuntu@your-server:/tmp/
```

Then connect to the server:

```bash
ssh ubuntu@your-server
```

## Install

Install the package with apt:

```bash
sudo apt install /tmp/scode_*.deb
```

The package installs:

```text
/usr/bin/scode
/usr/bin/scode-setup
/usr/lib/scode/scode-setup
```

If the install is running in an interactive terminal, the package runs
`scode-setup` automatically. The setup wizard is terminal-only and does
not require a graphical desktop.

## First-time setup

The installer prompts for:

```text
Base URL
API Key
Default model
web_search on/off
```

By default, Base URL is:

```text
https://hk.sudorouter.ai/v1
```

The wizard calls the selected Base URL with `/models`, shows the returned
model IDs, and writes user-level configuration files:

```text
~/.nexus/sudocode/sudocode.json
~/.nexus/sudocode/settings.json
```

The API key is stored in `sudocode.json`, so the file is written with
user-only permissions.

## Validate the install

Check the global commands:

```bash
which scode
which scode-setup
scode --version
```

Expected command paths:

```text
/usr/bin/scode
/usr/bin/scode-setup
```

Check generated config:

```bash
ls -la ~/.nexus/sudocode
cat ~/.nexus/sudocode/settings.json
```

Do not print or share the raw `sudocode.json` because it contains the API
key. To inspect it safely:

```bash
sed -E 's/("apiKey"[[:space:]]*:[[:space:]]*)"[^"]*"/\1"***"/g' \
  ~/.nexus/sudocode/sudocode.json
```

Run a health check:

```bash
scode doctor
```

Run a one-shot prompt:

```bash
scode -p "你好"
```

## Update model configuration

Run the setup command again:

```bash
scode-setup
```

When an existing config is present, the wizard shows:

```text
1) 只修改默认模型
2) 重新拉取模型列表并选择默认模型
3) 修改 Base URL / API Key 并重建配置
4) 开启/关闭 web_search
5) 退出
```

Use option `1` to change only `settings.json`. Use option `2` when the
remote model list has changed. Use option `3` when the Base URL or API key
has changed.

## Non-interactive setup

For automated server provisioning, run:

```bash
SCODE_BASE_URL=https://hk.sudorouter.ai/v1 \
SCODE_API_KEY=your-api-key \
SCODE_MODEL=deepseek-v4-pro \
SCODE_ENABLE_SEARCH=1 \
scode-setup --non-interactive
```

If `SCODE_MODEL` is omitted, setup chooses `deepseek-v4-pro` when present
in the fetched model list, otherwise the first returned model.

You can also avoid the network model fetch by providing a model list:

```bash
SCODE_API_KEY=your-api-key \
SCODE_MODEL=gpt-5 \
SCODE_MODELS=gpt-5,gpt-5-mini,gpt-4.1 \
scode-setup --non-interactive
```

## Upgrade

Install the newer package over the existing one:

```bash
sudo apt install /tmp/scode_0.1.14_amd64.deb
```

Package upgrades replace `/usr/bin/scode` and `scode-setup`, but do not
overwrite existing user configuration under `~/.nexus/sudocode`.

## Uninstall

Remove the package:

```bash
sudo apt remove scode
```

Confirm the global commands are gone:

```bash
which scode || echo "scode removed"
which scode-setup || echo "scode-setup removed"
```

User configuration is intentionally preserved:

```bash
ls -la ~/.nexus/sudocode
```

## Troubleshooting

If `settings.json` contains anything other than a single model ID, rerun
setup with the latest package:

```bash
scode-setup
cat ~/.nexus/sudocode/settings.json
```

Expected format:

```json
{ "model": "gpt-5" }
```

If installation happens in a non-interactive environment, the package does
not block waiting for input. Run setup manually as the target user:

```bash
scode-setup
```
