#!/usr/bin/env bash
#
# sift 一键安装脚本（仅 Linux）
#
# 用法：
#   ./install.sh                 安装到 ~/.local/bin（默认，无需 root）
#   ./install.sh --system        安装到 /usr/local/bin（需要 sudo）
#   ./install.sh --prefix DIR    安装到指定目录
#   ./install.sh --skip-deps     跳过系统依赖安装（fd/rg/bat 已装好时）
#   ./install.sh --skip-theme    跳过 bat 的 Catppuccin Macchiato 主题安装
#   ./install.sh -h | --help     显示帮助
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
SKIP_DEPS=0
SKIP_THEME=0

usage() {
    sed -n '3,11p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --system)    PREFIX="/usr/local/bin" ;;
        --prefix)    [ $# -ge 2 ] || die "--prefix 需要一个目录参数"; PREFIX="$2"; shift ;;
        --prefix=*)  PREFIX="${1#*=}" ;;
        --skip-deps) SKIP_DEPS=1 ;;
        --skip-theme) SKIP_THEME=1 ;;
        -h|--help)   usage 0 ;;
        *)           die "未知参数: $1（--help 查看用法）" ;;
    esac
    shift
done

# ---------------------------------------------------------------- 环境检查

[ "$(uname -s)" = "Linux" ] || die "sift 仅支持 Linux（当前系统: $(uname -s)）"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ -f "$SCRIPT_DIR/Cargo.toml" ] || die "未找到 Cargo.toml，请在 sift 源码目录内运行本脚本"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    fi
fi

# 安装目录当前用户不可写时才需要 sudo（向上找第一个已存在的祖先目录判断）
prefix_needs_sudo() {
    [ "$(id -u)" -eq 0 ] && return 1
    local d="$PREFIX"
    while [ ! -d "$d" ]; do d="$(dirname "$d")"; done
    [ ! -w "$d" ]
}

# ---------------------------------------------------------------- 依赖安装

install_deps() {
    local pm=""
    for c in apt-get dnf pacman zypper apk; do
        if command -v "$c" >/dev/null 2>&1; then pm="$c"; break; fi
    done

    if [ -z "$pm" ]; then
        warn "未识别的包管理器，请手动安装运行时依赖：fd、ripgrep、bat"
        return
    fi

    if [ -z "$SUDO" ] && [ "$(id -u)" -ne 0 ]; then
        warn "需要 root 权限安装依赖但无 sudo，请手动安装：fd、ripgrep、bat"
        return
    fi

    info "使用 $pm 安装运行时依赖（fd / ripgrep / bat）..."
    case "$pm" in
        apt-get)
            $SUDO apt-get update
            $SUDO apt-get install -y fd-find ripgrep bat curl
            ;;
        dnf)
            $SUDO dnf install -y fd-find ripgrep bat curl
            ;;
        pacman)
            $SUDO pacman -Sy --needed --noconfirm fd ripgrep bat curl
            ;;
        zypper)
            $SUDO zypper --non-interactive install fd ripgrep bat curl
            ;;
        apk)
            $SUDO apk add fd ripgrep bat curl
            ;;
    esac
}

# Debian/Ubuntu 上二进制名为 fdfind / batcat，补 fd / bat 软链
fix_binary_names() {
    local link_dir="$HOME/.local/bin"
    mkdir -p "$link_dir"
    if ! command -v fd >/dev/null 2>&1 && command -v fdfind >/dev/null 2>&1; then
        ln -sf "$(command -v fdfind)" "$link_dir/fd"
        info "已创建软链: $link_dir/fd -> $(command -v fdfind)"
    fi
    if ! command -v bat >/dev/null 2>&1 && command -v batcat >/dev/null 2>&1; then
        ln -sf "$(command -v batcat)" "$link_dir/bat"
        info "已创建软链: $link_dir/bat -> $(command -v batcat)"
    fi
}

check_deps() {
    local missing=()
    command -v fd  >/dev/null 2>&1 || missing+=("fd")
    command -v rg  >/dev/null 2>&1 || missing+=("ripgrep")
    command -v bat >/dev/null 2>&1 || missing+=("bat")
    if [ ${#missing[@]} -gt 0 ]; then
        die "缺少运行时依赖: ${missing[*]}，请先安装后重试（或去掉 --skip-deps）"
    fi
    ok "运行时依赖就绪: fd $(fd --version | awk '{print $2}'), rg $(rg --version | head -1 | awk '{print $2}'), bat $(bat --version | awk '{print $2}')"
}

# ---------------------------------------------------------------- Rust 工具链

ensure_rust() {
    # 已 source 过 cargo env 的情况也覆盖到
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

    if command -v cargo >/dev/null 2>&1; then
        ok "Rust 工具链已安装: $(rustc --version)"
        return
    fi

    command -v curl >/dev/null 2>&1 || die "安装 Rust 需要 curl，请先安装 curl"
    info "未检测到 Rust，通过 rustup 安装（minimal profile）..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
    ok "Rust 安装完成: $(rustc --version)"
}

# ---------------------------------------------------------------- 构建与安装

build_and_install() {
    info "编译 sift（release）..."
    cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

    local sudo_prefix=""
    if prefix_needs_sudo; then
        [ -n "$SUDO" ] || die "安装到 $PREFIX 需要 root 权限，请使用 sudo 运行或改用默认用户目录安装"
        sudo_prefix="$SUDO"
    fi

    $sudo_prefix mkdir -p "$PREFIX"
    $sudo_prefix install -m 0755 "$SCRIPT_DIR/target/release/sift" "$PREFIX/sift"
    ok "已安装: $PREFIX/sift"
}

# ---------------------------------------------------------------- bat 主题

install_bat_theme() {
    # sift 检测到 Catppuccin Macchiato 主题时才启用，装一下让预览配色统一
    local config_dir themes_dir
    config_dir="$(bat --config-dir 2>/dev/null)" || return 0
    themes_dir="$config_dir/themes"

    if bat --list-themes 2>/dev/null | grep -q "Catppuccin Macchiato"; then
        ok "bat 已包含 Catppuccin Macchiato 主题"
        return 0
    fi

    command -v curl >/dev/null 2>&1 || { warn "无 curl，跳过 bat 主题安装（可选）"; return 0; }

    info "为 bat 安装 Catppuccin Macchiato 主题..."
    mkdir -p "$themes_dir"
    local base="https://raw.githubusercontent.com/catppuccin/bat/main/themes"
    local name="Catppuccin Macchiato.tmTheme"
    if curl -fsSL "$base/Catppuccin%20Macchiato.tmTheme" -o "$themes_dir/$name"; then
        bat cache --build >/dev/null 2>&1 || true
        ok "bat 主题已安装: $themes_dir/$name"
    else
        warn "主题下载失败（可选步骤，不影响使用），可稍后手动安装: https://github.com/catppuccin/bat"
    fi
}

# ---------------------------------------------------------------- PATH 检查

check_path() {
    case ":$PATH:" in
        *":$PREFIX:"*) ;;
        *)
            warn "$PREFIX 不在 PATH 中，请将下面这行加入你的 shell 配置（~/.bashrc / ~/.zshrc）："
            printf '    export PATH="%s:$PATH"\n' "$PREFIX" >&2
            ;;
    esac
}

# ---------------------------------------------------------------- 主流程

printf '%s%s sift 安装程序%s\n' "$C_BOLD" "$C_BLUE" "$C_RESET"

if [ "$SKIP_DEPS" -eq 0 ]; then
    install_deps
fi
fix_binary_names
check_deps

ensure_rust
build_and_install

if [ "$SKIP_THEME" -eq 0 ]; then
    install_bat_theme
fi

check_path

echo
ok "安装完成！运行 ${C_BOLD}sift${C_RESET}${C_GREEN} 开始使用${C_RESET}"
