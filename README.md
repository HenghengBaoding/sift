# sift

基于 `fd` / `rg` / `bat` 的终端文件搜索 TUI（仅 Linux）。

## 安装

一键安装（自动安装依赖、编译并装入 `~/.local/bin`）：

```bash
git clone <repo-url> && cd sift
./install.sh
```

常用选项：`--system`（装入 `/usr/local/bin`）、`--prefix DIR`、`--skip-deps`、`--help` 查看全部。

更新到最新版本（拉取最新代码并重新编译安装，已是最新则跳过）：

```bash
./update.sh            # 在 sift 源码目录内运行；--system / --prefix 需与安装时一致
```

卸载：

```bash
./uninstall.sh         # 默认从 ~/.local/bin 移除；--system / --prefix 同上
```

## 依赖

- 运行时：[fd](https://github.com/sharkdp/fd)、[ripgrep](https://github.com/BurntSushi/ripgrep)、[bat](https://github.com/sharkdp/bat)（安装脚本会自动处理）
- 构建：Rust 1.85+（未安装时脚本会通过 rustup 自动安装）

## 运行

```bash
cargo run --release
```

## 功能

- **双模式搜索**：文件名搜索（fd，模糊匹配）/ 文件内容搜索（rg），`Tab` 切换
- **手动触发**：输入后按 Enter 执行搜索（输入时不即时搜索）；搜索中按 Enter / Tab 会先 kill 旧进程再立即重搜
- **模糊匹配**：仅匹配**文件名**，输入 `test.sh` 可匹配 `xxxxtest.sh`、`test.shxxxx`、`xxxtest.shxxxx`、`tesh.sh`
  （子串包含 > typo 容错：编辑距离小、首尾字符一致且不引入陌生字符；不做拆词/子序列匹配，不看路径成分）
- **流式内容搜索**：rg 结果边搜边显示，上限 400 条；支持多行搜索
  （`Shift+Enter` 或 `\n` 换行，`\\` 匹配字面反斜杠）；精准匹配（`--fixed-strings` + 大小写敏感）
- **智能排除**：内置固定忽略目录（`/proc` `/sys` `/dev` `/run` `/tmp` `/var/tmp` `/var/cache` `/mnt` `/media` `/var/lib` `/snap` `.git`），
  避免扫描虚拟/庞大目录；用户可通过 `Ctrl+I` 额外添加忽略目录，`Ctrl+S` 调整文件大小上限（fd/rg 同样生效）
- **路径优先级排序**：命中次数相同时，常用目录（`/home` `/etc` `/usr/local`）结果优先展示
- **语法高亮预览**：bat 渲染（自动使用 Catppuccin Macchiato 主题，若可用），支持滚动；
  大文件仅渲染头部 512KB；二进制/高熵乱码文件（如 go sumdb tile）给出「不提供预览」提示
- **图片预览（kitty 图形协议）**：在 kitty / Alacritty / Ghostty / WezTerm / Konsole / Rio 等支持该协议的终端下
  直接显示图片（png/jpeg/gif/bmp/webp），先缩放到预览区尺寸再传输，大图也秒开；其它终端回退为文字提示
- **输入框展开/折叠**：`Ctrl+H` 切换；展开态按宽度自动折行、最高 1/3 屏可滚动；折叠态单行截断
- **一键打开**：`Ctrl+G` 使用 `$VISUAL` / `$EDITOR` / `nvim` / `vim` / `code` 等，权限不足时自动 `sudo`
- **复制路径**：`Ctrl+C` 把选中文件完整路径复制到系统剪贴板（wl-copy / xclip / xsel）
- **配置持久化**：忽略目录与大小上限写入 `~/.config/sift/config.toml`，下次启动自动加载
- **界面**：Catppuccin Macchiato 配色、圆角边框、整行选中高亮、toast 弹框提示

## 快捷键

| 按键            | 功能                                   |
| ------------- | ------------------------------------ |
| Tab           | 切换搜索模式（文件名/内容；搜索中则先取消旧搜索再切换）        |
| Ctrl + H      | 展开/折叠顶部搜索输入框                         |
| Ctrl + P      | 修改搜索路径（弹窗输入，支持 `~`）                  |
| Ctrl + I      | 编辑额外忽略目录（换行分隔，持久化；fd/rg 同样生效）        |
| Ctrl + S      | 修改文件大小上限（M，持久化；fd/rg 同样生效）           |
| Alt + J/K     | 选择文件（下一个 / 上一个）                      |
| ↑ / ↓         | 展开态搜索框中按折行上下移动光标（折叠态无效）              |
| Ctrl + J/K    | 预览向下/向上滚动                            |
| Enter         | 触发搜索（搜索中则先 kill 旧进程再搜新关键词）           |
| Shift + Enter | 输入框插入换行（搜索框 / 忽略目录多行输入）              |
| Ctrl + G      | 打开文件                                 |
| Ctrl + C      | 复制选中文件的完整路径到剪贴板                      |
| Esc           | 搜索中：取消当前搜索；非搜索：退出程序                  |

## 界面

```
╭──────────────────────────────────────────────╮
│ 搜索输入框（模式 / 当前路径 / 展开折叠状态）       │
├───────────────┬──────────────────────────────┤
│ 文件列表       │ 文件内容预览（bat）           │
│               ├──────────────────────────────┤
│               │ 文件完整路径（自动换行）       │
├───────────────┴──────────────────────────────┤
│ 快捷键提示（自动折行）                         │
╰──────────────────────────────────────────────╯
```

## 配置文件

路径：`~/.config/sift/config.toml`（缺失或损坏时回退默认值，不阻断启动）

```toml
# 单文件大小上限（MB，默认 10.0；fd/rg 同样生效）
max_file_size_mb = 10.0

# 额外忽略目录（绝对路径，不含内置必忽略目录；Ctrl+I 编辑）
ignore_dirs = ["/data/logs", "/backup"]
```
