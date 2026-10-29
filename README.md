# SessTui

一个终端界面，用来查看、筛选和删除本机 AI 编码客户端留下的会话：

- **DeepSeek Harness**（`~/.dsh`）
- **Codex**（`~/.codex`，含 `CODEX_HOME` 形式的独立实例）
- **Opencode**（`~/.local/share/opencode/opencode.db`）

所有会话汇总到一个列表里，显示标题、最后更新、占用空间、消息数和是否正在使用；
支持多选删除、按客户端切换、按关键字过滤，删除前要确认，也可以只演练。

## 安装

Arch Linux 用 AUR：

```sh
paru -S sesstui
```

从仓库里的 `PKGBUILD` 本地构建并安装：

```sh
git clone https://github.com/Ricarsz/sesstui.git SessTui
cd SessTui
paru -Bi .
# 或者：makepkg -si
```

只想要二进制的话，直接 `cargo build --release`，产物在 `target/release/sesstui`。
需要 Rust 1.88 或更新（使用了 let-chains）。运行时优先用内置的纯 Rust zstd 解码；
系统里有 `zstd` 命令时会作为兜底。

## 使用

```sh
sesstui                 # 打开 TUI
sesstui list            # 列出会话
sesstui delete <id>     # 删除指定会话（可加 --dry-run）
sesstui doctor          # 检查扫描位置、数据库与依赖
sesstui paths           # 只打印扫描路径
sesstui config --init   # 写入默认配置
```

`delete` 的 ID 形如 `dsh:session-<uuid>`、`codex:<uuid>`、`opencode:ses_<id>`；
不带前缀时会在所有客户端里按 ID 匹配，匹配到多个会拒绝执行。

### 键位

| 按键 | 作用 |
| --- | --- |
| `↑` `↓` / `k` `j` | 选择会话 |
| `PgUp` `PgDn` | 翻 10 行 |
| `Home` `End` | 跳到首尾 |
| `空格` | 勾选 / 取消勾选并下移 |
| `a` | 全选当前视图 / 取消全选 |
| `d` | 删除（有勾选删勾选，否则删当前） |
| `Tab` / `Shift+Tab` | 切换客户端视图 |
| `/` | 按标题、ID、目录过滤 |
| `J` / `K` | 滚动右侧详情 |
| `r` | 立即重新扫描 |
| `t` | 切换主题 |
| `?` | 显示 / 隐藏帮助 |
| `q` / `Esc` | 退出（有过滤时先清除） |

## 会话存储

| 客户端 | 位置 | 删除方式 |
| --- | --- | --- |
| DeepSeek Harness | `~/.dsh*/sessions/<workspace>/session-<uuid>/` | 删除整个会话目录；`session.lock` 被占用时跳过 |
| Codex | `~/.codex*/sessions/YYYY/MM/DD/rollout-*.jsonl` | 删除 rollout 文件，并同步移除 `session_index.jsonl` 里的记录 |
| Opencode | `~/.local/share/opencode/opencode.db` | 在事务里删除 `session_v2` 行，消息等子表由外键级联删除 |

扫描位置可以用环境变量覆盖，多个路径用 `:` 分隔：

```sh
SESSTUI_DSH_ROOT=~/.dsh-alt sesstui list
SESSTUI_CODEX_HOME=~/.codex-alt sesstui doctor
SESSTUI_OPENCODE_DIR=/path/to/opencode sesstui list
```

若多个根目录的 `sessions/` 指向同一份存储（例如用符号链接共享），只会列一次，
并在 `doctor` 里提示已合并。

## 配置

路径：`$XDG_CONFIG_HOME/sesstui/config.toml`，默认 `~/.config/sesstui/config.toml`。

```toml
theme = "ocean"        # ocean | forest | sunset
mouse = true           # 滚轮滚动、点击选择
rescan_secs = 30       # 自动重扫间隔，最小 5 秒
confirm_delete = true  # 删除前弹确认框
```

## 删除安全

- 删除前会把目标路径规范化，并确认它仍在对应的存储根目录内，避免误删。
- DeepSeek Harness 会话被其他进程加锁时直接跳过，不会强删。
- Opencode 的删除在单个事务里完成，失败会整体回滚。
- `sesstui delete --dry-run` 只打印目标，不落盘。

## 限制

- 扫描器按格式解析各客户端的会话文件；客户端改格式后需要同步更新。
- 正在写入的会话可能读到不完整的数据，按 `r` 重扫即可刷新。
- Opencode 数据库被长时间占用时删除会报锁错误；关闭对应进程后重试。
- 目前只在 Linux 上验证过。

## 许可

MIT，见 [LICENSE](LICENSE)。
