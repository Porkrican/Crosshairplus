# 🏹 Crosshair+

A portable Windows 10/11 gaming app for **custom crosshairs**, **display calibration**, and **system-wide sound EQ** in one OLED-black interface.

| 🎯 Aim | 🖥️ Display |
| :---: | :---: |
| ![Aim](screenshots/crosshair.png) | ![Display](screenshots/display.png) |

| 🔊 Audio | ⚡ Slots |
| :---: | :---: |
| ![Audio](screenshots/audio.png) | ![Slots](screenshots/slots.png) |

## Features
* **🎯 Aim:** 7 customizable shapes + PNG/JPG/WebP/SVG/animated GIF import, rotation, outlines, and smart hiding (`Hide on Aim`, `Only on Aim`, `Hide in menus`).
* **🖥️ Display:** Live Day/Night preview, Brightness/Contrast/Gamma/Warmth/RGB, **🌙 Blue Light Filter**, **Hold to Compare**, panel-aware curves (`Auto` / OLED / IPS / VA / MiniLED / TN), and auto-restore on exit.
* **🔊 Audio:** 10-band EQ + Bass/Treble + Equalizer APO/AutoEq import powered by [FxSound](https://www.fxsound.com/) (`v1.2.11+`). Uses a dedicated **Crosshair+** preset so your existing FxSound presets stay untouched.
* **⚡ Slots & Quality of Life:** Bundle Aim + Display + Sound into up to 6 hotkey/game-triggered slots (auto-scans Steam, Epic & Xbox), themed system tray menu, optional OSD hotkey pill, and `.zip` backup/restore.

## Quick Start
1. Download **`CrosshairPlus.exe`** from **[Releases](../../releases)** and run it anywhere *(click **More info → Run anyway** if SmartScreen prompts)*.
2. Sound EQ prompts for 1-click FxSound setup if missing *(requires FxSound `1.2.11+`)*.
3. **In-game tip:** Use **Borderless Windowed** mode (external overlays don't appear in legacy Exclusive Fullscreen). If a game runs as Administrator, run Crosshair+ as Administrator too.
4. **Audio/Headset fix:** If sound cuts out after headset sleep/unplug, click **Settings → Maintenance → Refresh Audio**.

## Default Hotkeys
| Key | Action |
| :--- | :--- |
| **`F8`** | Toggle crosshair overlay *(customizable)* |
| **`F9` – `F11`** | Toggle Slots 1–3 *(Cycle Slots & Panic Reset assignable in Settings)* |
| **`Ctrl + Shift + End`** | Reset crosshair rotation & offsets |

## Build From Source
Requires Windows 10/11 x64, [Rust (MSVC)](https://www.rust-lang.org/tools/install), and Visual Studio Build Tools (**Desktop development with C++**):
```bat
BUILD.BAT build    :: Build standalone CrosshairPlus.exe
BUILD.BAT test     :: Run tests
BUILD.BAT run      :: Build and launch
```

[MIT License](LICENSE)