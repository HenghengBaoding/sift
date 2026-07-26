#!/usr/bin/env bash
#
# sift 更新脚本：拉取最新代码并重新编译安装
#
# 用法：
#   ./update.sh                 拉取最新代码并更新安装（默认 ~/.local/bin）
#   ./update.sh --system        更新 /usr/local/bin 中的安装（需要 sudo）
#   ./update.sh --prefix DIR    更新指定目录中的安装
#   ./update.sh --with-deps     同时安装/更新系统依赖（fd/rg/bat）
#   ./update.sh -h | --help     显示帮助
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

WITH_DEPS=0
PREFIX_ARGS=()

usage() {
    sed -n '3,10p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --system)    PREFIX_ARGS=(--system) ;;
        --prefix)    [ $# -ge 2 ] || die "--prefix 需要一个目录参数"; PREFIX_ARGS=(--prefix "$2"); shift ;;
        --prefix=*)  PREFIX_ARGS=(--prefix "${1#*=}") ;;
        --with-deps) WITH_DEPS=1 ;;
        -h|--help)   usage 0 ;;
        *)           die "未知参数: $1（--help 查看用法）" ;;
    esac
    shift
done

# ---------------------------------------------------------------- 环境检查

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

command -v git >/dev/null 2>&1 || die "未找到 git，请手动拉取代码后运行 ./install.sh"
[ -d .git ] || die "当前目录不是 git 仓库，无法自动更新；请重新 clone 后运行 ./install.sh"
[ -f Cargo.toml ] || die "未找到 Cargo.toml，请在 sift 源码目录内运行本脚本"

# 本地有未提交改动时 pull 可能冲突，提前提示
if [ -n "$(git status --porcelain)" ]; then
    die "检测到本地未提交的修改，请先提交或 stash 后再更新（git stash）"
fi

pkg_version() { sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1; }

# ---------------------------------------------------------------- 拉取与安装

OLD_VERSION="$(pkg_version)"
OLD_HEAD="$(git rev-parse HEAD)"

info "拉取最新代码..."
git pull --ff-only || die "git pull 失败，请手动处理冲突后重试"

NEW_HEAD="$(git rev-parse HEAD)"
NEW_VERSION="$(pkg_version)"

if [ "$OLD_HEAD" = "$NEW_HEAD" ]; then
    ok "已是最新版本（v$NEW_VERSION），无需更新"
    exit 0
fi

info "版本更新: v$OLD_VERSION -> v$NEW_VERSION"

# install.sh 幂等，会覆盖旧二进制；依赖默认跳过，--with-deps 时一并处理
INSTALL_ARGS=()
[ "$WITH_DEPS" -eq 0 ] && INSTALL_ARGS+=(--skip-deps)

info "重新编译安装..."
if [ ${#INSTALL_ARGS[@]} -eq 0 ] && [ ${#PREFIX_ARGS[@]} -eq 0 ]; then
    "$SCRIPT_DIR/install.sh"
else
    "$SCRIPT_DIR/install.sh" ${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"} ${PREFIX_ARGS[@]+"${PREFIX_ARGS[@]}"}
fi

echo
ok "更新完成！当前版本: ${C_BOLD}v$NEW_VERSION${C_RESET}"
