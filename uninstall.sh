#!/usr/bin/env bash
#
# sift 卸载脚本
#
# 用法：
#   ./uninstall.sh                 从 ~/.local/bin 卸载（默认）
#   ./uninstall.sh --system        从 /usr/local/bin 卸载（需要 sudo）
#   ./uninstall.sh --prefix DIR    从指定目录卸载
#   ./uninstall.sh -h | --help     显示帮助
#
set -euo pipefail

# ---------------------------------------------------------------- 输出样式

if [ -t 1 ]; then
    C_BLUE=$'\033[34m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'
    C_RED=$'\033[31m';  C_BOLD=$'\033[1m';  C_RESET=$'\033[0m'
else
    C_BLUE=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_BOLD=''; C_RESET=''
fi

info()  { printf '%s==>%s %s\n' "$C_BLUE"  "$C_RESET" "$*"; }
ok()    { printf '%s ✓ %s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()  { printf '%s ! %s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die()   { printf '%s ✗ %s %s\n' "$C_RED"   "$C_RESET" "$*" >&2; exit 1; }

# ---------------------------------------------------------------- 参数解析

PREFIX="$HOME/.local/bin"

usage() {
    sed -n '3,9p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --system)    PREFIX="/usr/local/bin" ;;
        --prefix)    [ $# -ge 2 ] || die "--prefix 需要一个目录参数"; PREFIX="$2"; shift ;;
        --prefix=*)  PREFIX="${1#*=}" ;;
        -h|--help)   usage 0 ;;
        *)           die "未知参数: $1（--help 查看用法）" ;;
    esac
    shift
done

# ---------------------------------------------------------------- 权限判断

SUDO=""
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
fi

# 目录当前用户不可写时才需要 sudo（向上找第一个已存在的祖先目录判断）
prefix_needs_sudo() {
    [ "$(id -u)" -eq 0 ] && return 1
    local d="$PREFIX"
    while [ ! -d "$d" ]; do d="$(dirname "$d")"; done
    [ ! -w "$d" ]
}

# ---------------------------------------------------------------- 卸载

printf '%s%s sift 卸载程序%s\n' "$C_BOLD" "$C_BLUE" "$C_RESET"

TARGET="$PREFIX/sift"

if [ ! -f "$TARGET" ]; then
    ok "未找到 $TARGET，sift 未安装或已卸载"
else
    SUDO_PREFIX=""
    if prefix_needs_sudo; then
        [ -n "$SUDO" ] || die "删除 $TARGET 需要 root 权限，请使用 sudo 运行"
        SUDO_PREFIX="$SUDO"
    fi

    $SUDO_PREFIX rm -f "$TARGET"
    ok "已删除: $TARGET"
fi

# bat 主题是全局资源（bat 自身也在用），仅提示不删除
CONFIG_DIR="$(bat --config-dir 2>/dev/null || true)"
if [ -n "$CONFIG_DIR" ] && [ -f "$CONFIG_DIR/themes/Catppuccin Macchiato.tmTheme" ]; then
    info "保留 bat 主题: $CONFIG_DIR/themes/Catppuccin Macchiato.tmTheme（bat 可独立使用，不需要可手动删除）"
fi

echo
ok "卸载完成（运行时依赖 fd / rg / bat 为系统软件包，未做移除）"
