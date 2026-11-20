# Microsoft Modern Webcam Control & Preview (`moderncam`)

跨平台原生 Rust 桌面控制与实时预览工具，用于 **Microsoft Modern Webcam**（`VID 0x045E / PID 0x0840`），支持 **macOS / Linux / Windows**，无 Tauri / WebView 依赖。

## Workspace 架构

- `crates/moderncam-protocol`：零 I/O 纯协议层（HID `0x24`/`0x27` 帧编解码、USB Configuration Descriptor 动态解析、麦克风/Reset 重枚举一致性状态机）。
- `crates/moderncam-device`：跨平台硬件控制层：
  - **USB 拓扑与热插拔**：`nusb`（全平台免驱查询接口列表及监听拔插事件）；
  - **私有 HID 控制**：macOS 使用定向 `VID/PID` `IOHIDManager`（避开 `hidapi` 触发 TCC Input Monitoring 权限阻断），Linux/Windows 使用 `hidapi`（优先匹配 `0xFF07:0x0212`）；
  - **标准 UVC 画质控制**：macOS 使用 `IOUSBDeviceInterface182` EP0 控制传输（免 root、与预览并发），Linux 使用 `v4l` V4L2 ioctl，Windows 使用 `cameras` (`controls` / DirectShow)。
- `crates/moderncam-cli`：命令行诊断与控制工具。
- `crates/moderncam-gui`：基于 `eframe 0.37` + `egui-elegance 0.21` + `cameras 0.3.2` 的原生 GUI 实时预览控制台。

## 快速开始

```bash
# 运行全部单元测试
cargo test --workspace

# CLI 查看完整状态（HID + USB 接口 + UVC 参数）
cargo run -p moderncam-cli -- dump

# 启动图形界面（带实时画面预览）
cargo run -p moderncam-gui
```

## 平台注意事项

### macOS
- 首次开启实时预览时，系统会弹出摄像头访问权限提示；如被拒绝，请在「系统设置 → 隐私与安全性 → 摄像头」中授权。
- 如需打包 `.app`，可参考 [`packaging/macos/Info.plist`](packaging/macos/Info.plist)。
- HID 控制通过精确 `VID=0x045E / PID=0x0840` 匹配 `IOHIDManager`，无需「输入监控」权限；UVC 参数通过 EP0 控制传输调节，可与实时预览并发工作。

### Linux
- 请安装 [`packaging/udev/99-microsoft-modern-webcam.rules`](packaging/udev/99-microsoft-modern-webcam.rules) 以允许普通用户访问 `hidraw` 和 `video4linux`：
  ```bash
  sudo cp packaging/udev/99-microsoft-modern-webcam.rules /etc/udev/rules.d/
  sudo udevadm control --reload-rules && sudo udevadm trigger
  ```

## 关于麦克风开关与 Reset 后的重插提示

Microsoft Modern Webcam 固件内部没有软件重启 USB 总线的命令，且麦克风开关（`0x09`）会改变 USB 描述符集合（启用时新增 `#3/#4 class=1 Audio` 接口）。因此本程序会在切换麦克风或执行 `HID 0xAE` 重置后，自动核对 `hid_mic_enabled == usb_audio_interface_present`；如不一致则通过热插拔监听器引导插拔一次 USB 线并自动恢复预览与状态同步。
