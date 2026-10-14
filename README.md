# Roam

一套键鼠，控制多台电脑。在面前的直接切屏（A 模式），不在身边的远程看画面（B 模式）。
跨网络 P2P 直连、低延迟、开源。优先支持 Windows ↔ macOS。

设计见 [docs/design.md](docs/design.md)。

## 当前进度：M0 连通性测试

`roam-probe` 用来实测两台机器之间能不能直连、走哪条路径、延迟和丢包是多少。

```bash
cargo build --release
```

被测机器：

```bash
roam-probe listen
```

控制机器（粘贴 listen 打印的连接码）：

```bash
roam-probe connect roam1xxxx... --secs 60
```

选项：`--no-relay` 禁用中继，只测直连；`--rate 250` 数据报发包频率（Hz），模拟鼠标移动。

## 构建依赖

- Rust stable（≥ 1.85）
- Windows：VS Build Tools（C++ 工作负载）
- macOS：Xcode Command Line Tools

## 许可

AGPL-3.0-or-later
