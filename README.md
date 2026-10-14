# Roam

一套键鼠，控制多台电脑。在面前的直接切屏（A 模式），不在身边的远程看画面（B 模式）。
跨网络 P2P 直连、低延迟、开源。优先支持 Windows ↔ macOS。

设计见 [docs/design.md](docs/design.md)。

## 当前进度：M1 键鼠核心（Mac 主机 → Windows 被控）

```bash
cargo build --release
```

**Windows（被控端）**

```bash
roam client
```

启动后会打印连接码（同时保存在 `%APPDATA%oam	icket.txt`）。主机第一次连接时，终端里会询问是否允许，输入 `y` 后就会记住这台主机。

**Mac（主机）**

```bash
roam host
```

浏览器会自动打开布局页面 `http://127.0.0.1:7878`：

1. 把 Windows 的连接码粘贴到「添加被控设备」
2. 对方上线后，它的屏幕会出现在画布上，拖动屏幕可以调整位置，松手后会自动贴到相邻屏幕的边上
3. 绿色线段就是光标可以穿过的边，鼠标从这条边移出去就切到了对方

首次运行需要在 **系统设置 → 隐私与安全性** 中给运行 roam 的程序（例如终端）授予「辅助功能」和「输入监控」权限。

- 逃生热键：`Ctrl + Option + Cmd + Esc` 强制回到 Mac
- 控制 Windows 时默认 Cmd ↔ Ctrl 对调，可以在页面里关掉
- 拖拽（按住鼠标键）时不会切屏

### 连通性测试

`roam-probe` 用来实测两台机器之间能不能直连、走哪条路径、延迟和丢包是多少：

```bash
roam-probe listen                 # 被测机器
roam-probe connect roam1xxxx...   # 另一台机器
```

## 构建依赖

- Rust stable（≥ 1.85）
- Windows：VS Build Tools（C++ 工作负载）
- macOS：Xcode Command Line Tools

## 许可

AGPL-3.0-or-later
