#!/bin/bash
#
# Interactive first-run configuration wizard for the `scode` CLI on macOS.
#
# The user supplies a SudoRouter API key, we fetch the available model list,
# select a model, and write ready-to-use
# sudocode.json + settings.json into ~/.nexus/sudocode. Pure bash + curl,
# no python/jq dependency, so it runs on a stock macOS.
#
# Installed to /usr/local/bin/scode-setup by the .pkg. Safe to re-run; it
# asks before replacing existing files and retains private backups. Model
# capabilities and protocol selection remain owned by the scode runtime.

set -euo pipefail

umask 077
DEFAULT_BASE_URL="https://api.sudorouter.ai/v1"
DEFAULT_MODEL=""
SEARCH_API_URL="https://hk.sudorouter.ai/search/tavily/search"
CONFIG_DIR="${SUDO_CODE_CONFIG_HOME:-$HOME/.nexus/sudocode}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --model|--base-url|--config-dir)
      [ "$#" -ge 2 ] && [ -n "$2" ] || { echo "$1 requires a value" >&2; exit 2; }
      case "$1" in
        --model) DEFAULT_MODEL="$2" ;;
        --base-url) DEFAULT_BASE_URL="$2" ;;
        --config-dir) CONFIG_DIR="$2" ;;
      esac
      shift 2 ;;
    --help|-h)
      echo 'Usage: scode-setup [--model MODEL_ID] [--base-url URL] [--config-dir DIR]'
      echo 'Enter the API key at the hidden prompt; never pass it as an argument.'
      echo 'Existing files require confirmation and are backed up before replacement.'
      exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

# Minimal JSON string escaping (backslash + double quote).
json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

echo "================================================"
echo "     Sudo Code · scode 首次配置向导"
echo "================================================"
echo

if [ -L "$CONFIG_DIR/sudocode.json" ] || [ -L "$CONFIG_DIR/settings.json" ]; then
  echo '配置文件是符号链接，请在其原位置编辑；向导未修改配置。' >&2
  exit 1
fi
if [ -f "$CONFIG_DIR/sudocode.json" ] || [ -f "$CONFIG_DIR/settings.json" ]; then
  printf "检测到已有配置：%s\n向导将备份并替换 sudocode.json 和 settings.json（包括原有账号与偏好）。\n保留原有配置请选 N；也可用 --config-dir 指定独立目录。\n是否继续替换？[y/N] " "$CONFIG_DIR"
  read -r ans
  case "$ans" in
    [yY]*) ;;
    *) echo "已取消，保留现有配置。"; exit 0 ;;
  esac
fi

# --- Base URL (fixed default, editable) ---
printf "Base URL [%s]: " "$DEFAULT_BASE_URL"
read -r BASE_URL
BASE_URL="${BASE_URL:-$DEFAULT_BASE_URL}"
BASE_URL="${BASE_URL%/}"
case "$BASE_URL" in
  https://*|http://127.0.0.1:*|http://localhost:*) ;;
  *) echo "远程 Base URL 必须使用 https://；本机测试可用 localhost / 127.0.0.1。"; exit 1 ;;
esac
case "$BASE_URL" in
  */v1) ;;
  */chat/completions|*/messages|*/responses)
    echo '请填写 API 根地址到 /v1，不要包含具体请求路径。' >&2; exit 1 ;;
  *) BASE_URL="$BASE_URL/v1" ;;
esac

# --- API Key (hidden input) ---
printf "API Key (sk-...): "
read -r -s API_KEY
echo
if [ -z "$API_KEY" ]; then
  echo "API Key 不能为空。"
  exit 1
fi
if printf '%s' "$API_KEY" | LC_ALL=C grep -q '[[:cntrl:][:space:]]'; then
  echo 'API Key 不应包含空格或换行，请重新复制。' >&2
  exit 1
fi

# --- Fetch models ---
echo "正在拉取模型列表…"
if ! MODEL_RESPONSE="$(printf 'header = "Authorization: Bearer %s"\n' "$(json_escape "$API_KEY")" \
  | curl -fsS --connect-timeout 15 --max-time 45 --config - "$BASE_URL/models")"; then
  echo '模型列表请求失败，配置尚未修改。请检查 HTTP 状态、接入点、密钥和账号权限后重试。' >&2
  exit 1
fi
MODELS="$(printf '%s' "$MODEL_RESPONSE" \
  | grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | sed -E 's/.*"([^"]*)"$/\1/' \
  | awk 'NF && !seen[$0]++' || true)"

if [ -z "$MODELS" ]; then
  echo '模型列表为空或无法识别，配置尚未修改。请确认该账号和分组已开放模型。'
  exit 1
else
  COUNT="$(printf '%s\n' "$MODELS" | grep -c . || true)"
  echo "✓ 已拉取 $COUNT 个模型"
fi

# --- Select default model ---
echo
echo '请选择默认模型（输入编号；开发任务请选择支持工具调用的 Claude/GPT）：'
i=1
DEF_INDEX=""
while IFS= read -r m; do
  [ -z "$m" ] && continue
  printf "  %2d) %s\n" "$i" "$m"
  if [ "$m" = "$DEFAULT_MODEL" ]; then DEF_INDEX="$i"; fi
  i=$((i + 1))
done <<< "$MODELS"
TOTAL=$((i - 1))

if [ -n "$DEFAULT_MODEL" ] && [ -z "$DEF_INDEX" ]; then
  printf '指定模型 %s 不在当前账号列表中，请明确选择另一个模型。\n' "$DEFAULT_MODEL"
fi
if [ -n "$DEF_INDEX" ]; then printf '编号 [%s]: ' "$DEF_INDEX"; else printf '编号（必填）: '; fi
read -r sel
sel="${sel:-$DEF_INDEX}"
if ! printf '%s' "$sel" | grep -qE '^[0-9]+$' || [ "$sel" -lt 1 ] || [ "$sel" -gt "$TOTAL" ]; then
  echo '编号无效，配置尚未修改。请重新运行并明确选择模型。' >&2
  exit 1
fi
CHOSEN_MODEL="$(printf '%s\n' "$MODELS" | awk 'NF' | sed -n "${sel}p")"
echo "默认模型：$CHOSEN_MODEL"

# --- web_search ---
printf "启用联网搜索 web_search？（需要账号另有搜索权限和额度）[y/N] "
read -r ws
case "$ws" in
  [yY]*) ENABLE_SEARCH=1 ;;
  *) ENABLE_SEARCH=0 ;;
esac

# --- Build account configuration; scode owns model discovery and capabilities. ---
if [ "$ENABLE_SEARCH" -eq 1 ]; then
  WEB_SEARCH=",
  \"web_search\": {
    \"provider\": \"tavily\",
    \"apiUrl\": \"$SEARCH_API_URL\",
    \"apiKey\": \"\"
  }"
else
  WEB_SEARCH=""
fi

SUDOCODE_JSON="{
  \"auth_modes\": {
    \"proxy\": {
      \"sudorouter\": { \"baseUrl\": \"$(json_escape "$BASE_URL")\", \"apiKey\": \"$(json_escape "$API_KEY")\" }
    }
  }$WEB_SEARCH
}"

SETTINGS_JSON="{ \"auth_profile\": \"sudorouter\", \"model\": \"$(json_escape "$CHOSEN_MODEL")\" }"

# --- Write files ---
mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR"
BACKUP_SUFFIX="backup-$(date +%Y%m%d-%H%M%S)-$$"
for name in sudocode.json settings.json; do
  if [ -f "$CONFIG_DIR/$name" ]; then
    cp -p "$CONFIG_DIR/$name" "$CONFIG_DIR/$name.$BACKUP_SUFFIX"
    chmod 600 "$CONFIG_DIR/$name.$BACKUP_SUFFIX"
    printf '原配置已备份：%s/%s.%s\n' "$CONFIG_DIR" "$name" "$BACKUP_SUFFIX"
  fi
done
CONFIG_TMP="$(mktemp "$CONFIG_DIR/.scode-config.XXXXXXXX")"
SETTINGS_TMP="$(mktemp "$CONFIG_DIR/.scode-settings.XXXXXXXX")"
trap 'rm -f "$CONFIG_TMP" "$SETTINGS_TMP"' EXIT
printf '%s\n' "$SUDOCODE_JSON" > "$CONFIG_TMP"
printf '%s\n' "$SETTINGS_JSON" > "$SETTINGS_TMP"
mv "$CONFIG_TMP" "$CONFIG_DIR/sudocode.json"
mv "$SETTINGS_TMP" "$CONFIG_DIR/settings.json"

echo
echo "✓ 配置已写入："
echo "    $CONFIG_DIR/sudocode.json"
echo "    $CONFIG_DIR/settings.json"
echo
echo '运行 scode --auth proxy --account sudorouter doctor 检查配置。'
echo '再用 scode --auth proxy --account sudorouter --permission-mode workspace-write 完成一次实际文件读写任务。'
echo '模型列表可见不等于实际推理、工具调用或多 Agent 环境已经验收。'
