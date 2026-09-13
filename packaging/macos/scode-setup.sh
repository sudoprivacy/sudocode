#!/bin/bash
#
# Interactive first-run configuration wizard for the `scode` CLI on macOS.
#
# Mirrors the logic of the browser-based config-tool: the user supplies a
# sudorouter API key, we fetch the available model list over HTTPS,
# pick a default model, and write ready-to-use
# sudocode.json + settings.json into ~/.nexus/sudocode. Pure bash + curl,
# no python/jq dependency, so it runs on a stock macOS.
#
# Installed to /usr/local/bin/scode-setup by the .pkg. Safe to re-run; it
# asks before overwriting an existing config.

set -euo pipefail

DEFAULT_BASE_URL="https://hk.sudorouter.ai/v1"
DEFAULT_MODEL="deepseek-v4-pro"
SEARCH_API_URL="https://hk.sudorouter.ai/search/tavily/search"
CONFIG_DIR="${SUDO_CODE_CONFIG_HOME:-$HOME/.nexus/sudocode}"

# Minimal JSON string escaping (backslash + double quote).
json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

echo "================================================"
echo "     Sudo Code · scode 首次配置向导"
echo "================================================"
echo

if [ -f "$CONFIG_DIR/sudocode.json" ]; then
  printf "检测到已存在配置：%s/sudocode.json\n是否覆盖？[y/N] " "$CONFIG_DIR"
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
  http://*|https://*) ;;
  *) echo "Base URL 需以 http:// 或 https:// 开头"; exit 1 ;;
esac

# --- API Key (hidden input) ---
printf "API Key (sk-...): "
read -r -s API_KEY
echo
if [ -z "$API_KEY" ]; then
  echo "API Key 不能为空。"
  exit 1
fi

# --- Fetch models ---
echo "正在拉取模型列表…"
MODELS="$(curl -fsS -H "Authorization: Bearer $API_KEY" "$BASE_URL/models" 2>/dev/null \
  | grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | sed -E 's/.*"([^"]*)"$/\1/' \
  | awk 'NF && !seen[$0]++' || true)"

if [ -z "$MODELS" ]; then
  echo "⚠ 拉取失败或列表为空（可能是网络/密钥/CORS），请检查后重试。"
  exit 1
else
  COUNT="$(printf '%s\n' "$MODELS" | grep -c . || true)"
  echo "✓ 已拉取 $COUNT 个模型"
fi

# --- Select default model ---
echo
echo "请选择默认模型（输入编号，回车默认 ${DEFAULT_MODEL}）："
i=1
DEF_INDEX=""
while IFS= read -r m; do
  [ -z "$m" ] && continue
  printf "  %2d) %s\n" "$i" "$m"
  if [ "$m" = "$DEFAULT_MODEL" ]; then DEF_INDEX="$i"; fi
  i=$((i + 1))
done <<< "$MODELS"
TOTAL=$((i - 1))

printf "编号 [%s]: " "${DEF_INDEX:-1}"
read -r sel
sel="${sel:-${DEF_INDEX:-1}}"
if ! printf '%s' "$sel" | grep -qE '^[0-9]+$' || [ "$sel" -lt 1 ] || [ "$sel" -gt "$TOTAL" ]; then
  echo "编号无效，使用默认。"
  sel="${DEF_INDEX:-1}"
fi
CHOSEN_MODEL="$(printf '%s\n' "$MODELS" | awk 'NF' | sed -n "${sel}p")"
echo "默认模型：$CHOSEN_MODEL"

# --- web_search ---
printf "启用联网搜索 web_search？（密钥自动复用上面的 API Key）[Y/n] "
read -r ws
case "$ws" in
  [nN]*) ENABLE_SEARCH=0 ;;
  *) ENABLE_SEARCH=1 ;;
esac

# --- Build sudocode.json ---
is_vision() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    *gpt-5*|*gpt-4o*|*gpt-4.1*|*gemini*|*claude-3*|*claude-opus*|*claude-sonnet*|*claude-haiku*|*vision*|*-vl*|*llava*|*pixtral*|*-image*|*multimodal*|*omni*)
      return 0 ;;
    *) return 1 ;;
  esac
}

MODELS_BLOCK=""
first=1
while IFS= read -r id; do
  [ -z "$id" ] && continue
  if is_vision "$id"; then input='["text", "image"]'; else input='["text"]'; fi
  eid="$(json_escape "$id")"
  entry=$(printf '    "%s": {\n      "alias": "%s",\n      "name": "%s",\n      "input": %s,\n      "providers": {\n        "proxy": { "provider": "sudorouter", "model": "%s", "api": "openai-completions" }\n      }\n    }' \
    "$eid" "$eid" "$eid" "$input" "$eid")
  if [ $first -eq 1 ]; then
    MODELS_BLOCK="$entry"
    first=0
  else
    MODELS_BLOCK="$MODELS_BLOCK,
$entry"
  fi
done <<< "$MODELS"

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
  \"models\": {
$MODELS_BLOCK
  },
  \"auth_modes\": {
    \"proxy\": {
      \"sudorouter\": { \"baseUrl\": \"$(json_escape "$BASE_URL")\", \"apiKey\": \"$(json_escape "$API_KEY")\" }
    }
  }$WEB_SEARCH
}"

SETTINGS_JSON="{ \"model\": \"$(json_escape "$CHOSEN_MODEL")\" }"

# --- Write files ---
mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR"
printf '%s\n' "$SUDOCODE_JSON" > "$CONFIG_DIR/sudocode.json"
chmod 600 "$CONFIG_DIR/sudocode.json"
printf '%s\n' "$SETTINGS_JSON" > "$CONFIG_DIR/settings.json"

echo
echo "✓ 配置已写入："
echo "    $CONFIG_DIR/sudocode.json"
echo "    $CONFIG_DIR/settings.json"
echo
echo "运行 'scode doctor' 体检，或 'scode -p \"你好\"' 试跑。"
