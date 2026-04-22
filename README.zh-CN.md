# WSL Memory Agent

[English](README.md)

WSL Memory Agent 是一个小型 WSL2 内存回收工具，用来缓解 Windows 上
`vmmem` 长时间占用大量内存的问题。

设计目标是够用、可靠、易部署：

- Windows 执行一行命令安装 host agent
- WSL 内执行一行命令安装 guest agent
- 不带 GUI，不引入数据库
- 使用 YAML 配置文件
- 默认优先使用 cgroup v2 `memory.reclaim`
- 默认不启用 `drop_caches`

## 快速安装

先在 Windows 管理员 PowerShell 中安装 host：

```powershell
irm https://github.com/AuroraMaster/wsl-memory/releases/latest/download/install-host.ps1 | iex
```

再进入每个需要管理的 WSL 发行版，安装 guest：

```bash
curl -fsSL https://github.com/AuroraMaster/wsl-memory/releases/latest/download/install-guest.sh | sudo sh
```

Windows 安装脚本会创建共享 token：

```text
C:\Users\Public\wsl_agent_token
```

WSL 侧默认从这里读取：

```text
/mnt/c/Users/Public/wsl_agent_token
```

如果你的 WSL 没有把 Windows C 盘挂载到 `/mnt/c`，可以手动指定 token：

```bash
curl -fsSL https://github.com/AuroraMaster/wsl-memory/releases/latest/download/install-guest.sh \
  | sudo WSL_MEMORY_TOKEN_PATH=/usr/local/etc/wsl-memory-agent/token sh
```

## 配置

Windows host 配置：

```yaml
# %APPDATA%\WSLMemoryAgent\config.yaml
listen_ip: "0.0.0.0"
listen_port: 15555
token_path: 'C:\Users\Public\wsl_agent_token'
```

如果配置文件不存在，host 会自动创建配置文件，并写入第一个可用的推荐端口。
你可以在这里自定义监听 IP 和端口。旧格式
`listen_addr: "0.0.0.0:15555"` 仍然兼容。

WSL guest 配置：

```yaml
# /usr/local/etc/wsl-memory-agent/config.yaml
host: auto:multi
token_path: /mnt/c/Users/Public/wsl_agent_token
interval: 4
allow_drop: false
multi_path: true
tcp: false
```

`host` 可以是 `auto:multi`，表示自动发现网关并探测推荐端口；也可以写成固定
地址，例如 `172.24.128.1:15555`。

两端会每 5 秒重新读取一次运行中可安全更新的配置：

- Windows host：`token_path` 和 token 文件内容
- WSL guest：`token_path`、`interval`、`allow_drop`

网络连接类字段需要重启服务或等待重连后生效，包括 `listen_ip`、
`listen_port`、`listen_addr`、`host`、`multi_path`、`tcp`。这些字段会影响监听
socket 或连接目标，不能在已有连接上无缝切换。

`allow_drop` 默认是 `false`。常规回收走 `/sys/fs/cgroup/memory.reclaim`。
只有你显式启用 `allow_drop`，并且系统处于临界压力且空闲时，guest 才会尝试
`drop_caches`。

## 构建

需要：

- Rust stable
- Zig 和 `cargo-zigbuild`，用于稳定的跨平台 release 构建

```bash
cargo test
cargo build --release --bin host
cargo build --release --bin guest
```

WSL/Linux release 构建：

```bash
cargo zigbuild --release --bin guest
```

发布包需要包含：

```text
host.exe
wsl-memory-guest
install-host.ps1
install-guest.sh
```

## 常用命令

Windows host：

```powershell
.\host.exe
.\host.exe --install
.\host.exe --uninstall
.\host.exe --check-port
.\host.exe --listen 0.0.0.0:15555
```

WSL guest：

```bash
sudo wsl-memory-guest --install
sudo wsl-memory-guest --status
sudo systemctl restart wsl-memory-guest
sudo journalctl -u wsl-memory-guest -f
sudo wsl-memory-guest --uninstall
```

## 原理

WSL2 的 Linux page cache 有时不会及时归还给 Windows，导致 Windows 侧
`vmmem` 占用远高于 WSL 进程实际需要的内存。

guest agent 采集 WSL 内的 resident memory、cache、CPU、IO。host agent 采集
Windows 内存压力和 `vmmem` RSS。两边数据合并后计算压力分数，只在必要时发出
保守回收请求。

核心回收路径是 cgroup v2 `memory.reclaim`，它让 Linux 内核主动回收内存，不
杀进程。

## 仓库结构

```text
src/                  Rust 源码
scripts/              一行安装脚本
examples/             YAML 配置示例
debian/               systemd unit 和 deb 打包脚本
.github/workflows/    CI 检查
```

## 排查

Windows 服务：

```powershell
Get-Service WSLMemoryHost
```

WSL 服务：

```bash
sudo systemctl status wsl-memory-guest --no-pager
sudo journalctl -u wsl-memory-guest -n 100 --no-pager
```

查看 WSL 到 Windows host 的网关：

```bash
ip route
cat /etc/resolv.conf
```

## 参考

- [microsoft/WSL issue #4166](https://github.com/microsoft/WSL/issues/4166)
- [cgroup v2 memory.reclaim](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html#memory)
- [Linux drop_caches](https://www.kernel.org/doc/Documentation/sysctl/vm.txt)

## 许可证

MIT
