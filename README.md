# womic-mic

Телефон на Android как системный микрофон — на Linux и на Windows, по USB (adb),
с шумодавом RNNoise. Одна программа вместо скрипта: сама держит всю цепочку и
перезапускает её при сбоях. Один и тот же исходник собирается под обе системы.

Linux:

```
rustmic (USB/adb, Opus -> PCM s16le 48k mono)
   -> RNNoise (nnnoiseless, in-process)
   -> pacat -> womic_sink (null sink)
   -> module-remap-source -> источник womic_mic
```

Windows:

```
rustmic.exe (USB/adb, Opus -> PCM s16le 48k mono)
   -> RNNoise (nnnoiseless, in-process)
   -> WASAPI (cpal) -> "CABLE Input" (VB-CABLE)
   -> приложения видят "CABLE Output" как микрофон
```

## Linux: сборка и установка

```fish
cd ~/Projects/womic-mic
cargo build --release --features gui
install -m 0755 target/release/womic-mic ~/.local/bin/womic-mic
install -m 0755 target/release/womic-mic-gui ~/.local/bin/womic-mic-gui
womic-mic install          # пишет systemd-юнит, включает и запускает
```

Зависимости: `rustmic` в `PATH` (клиент WO Mic), `pacat`/`parec`/`pactl` (PipeWire),
`adb` (по умолчанию `~/Android/Sdk/platform-tools/adb`), телефон с включённой
отладкой по USB и приложением WO Mic.

## Windows: сборка и установка

Нужны Rust (MSVC toolchain: rustup + Visual Studio Build Tools с «Desktop
development with C++»), [VB-CABLE](https://vb-audio.com/Cable/) и `rustmic.exe`
в `PATH` (клиент WO Mic, собранный под Windows). `adb.exe` берётся из
`%LOCALAPPDATA%\Android\Sdk\platform-tools\adb.exe`, либо укажите свой путь
флагом `--adb` / в конфиге.

```powershell
cargo build --release --features gui
.\target\release\womic-mic.exe run --stats   # проверить в консоли
.\target\release\womic-mic.exe install       # автозапуск (ключ Run) + старт
.\target\release\womic-mic-gui.exe           # окно-дашборд
```

GUI под Windows — на egui (без GTK): статус телефона и службы, включение
микрофона, шумодав, гейт, громкость, выбор выходного устройства (по умолчанию
`CABLE Input`), диагностика, журнал. Закрытие окна завершает GUI; фоновый
супервизор продолжает работать, пока его не остановят.

После запуска выберите в приложениях (Discord, Zoom, OBS…) микрофон
**«CABLE Output»** — Windows сам на виртуальный кабель не переключает.

Готовые exe также собираются в GitHub Actions (артефакты `womic-mic-windows`).
`set-default` / `unset-default` — только для Linux.

## Команды

| команда | что делает |
|---|---|
| `womic-mic run` | запустить супервизор (на Linux это же вызывает systemd) |
| `womic-mic status` | состояние цепочки, узлов/устройств, adb и источника |
| `womic-mic settings` | показать настройки и пути конфига/журнала |
| `womic-mic filter` | stdin → stdout, только шумодав (для тестов/других пайплайнов) |
| `womic-mic install` | Linux: systemd-юнит `enable --now`; Windows: ключ автозапуска + старт |
| `womic-mic install-no-enable` | создать без автозапуска |
| `womic-mic uninstall` | остановить и удалить автозапуск |
| `womic-mic start` / `stop` / `restart` | управление фоновым процессом |
| `womic-mic set-default` / `unset-default` | переключить микрофон по умолчанию (Linux) |

Флаги: `--adb PATH`, `--device NAME` (устройство вывода, Windows),
`--gate 0.0..1.0` (дополнительный гейт по VAD от RNNoise), `--no-denoise`,
`--stats` (печатать in/out RMS и VAD раз в 0.5 с).

## GUI и трей

`womic-mic-gui` — на Linux окно-дашборд GTK4 плюс иконка в системном трее
(StatusNotifierItem, хост — noctalia), на Windows — окно egui.

- статус: телефон (USB), сервис, индикатор уровня с затуханием;
- микрофон включён (старт/стоп), микрофон по умолчанию и шумодав (Linux);
- гейт; громкость виртуального микрофона 0–150 %;
- выбор устройства вывода (Windows);
- диагностика проверяет зависимости, аудиосервер/устройства, узлы и последние
  строки журнала;
- на Linux закрытие окна не завершает программу — она остаётся в трее;
- автозапуск на Linux: `spawn-at-startup "womic-mic-gui"` в конфиге niri
  (`~/.config/niri-noctalia/cfg/autostart.kdl` — именно это дерево читает niri,
  путь задан в `NIRI_CONFIG`).

Настройки:

- Linux: `~/.config/womic-mic/config.toml`
- Windows: `%APPDATA%\womic-mic\config.toml`

```toml
denoise = true
gate = 0.00
volume = 100
device = "CABLE Input"   # только Windows
```

Демон перечитывает шумодав, гейт и громкость при изменении файла; на Linux
громкость применяется при создании узла и периодически восстанавливается после
перезапуска PipeWire. Поэтому служба запускается без аргументов.

## Что делает супервизор

- перезапускает `rustmic` и аудиовыход при падении (backoff 1→30 с, сброс после
  60 с стабильной работы);
- на Linux раз в 3 с проверяет узлы PipeWire и пересоздаёт `womic_sink`/`womic_mic`,
  если их снесли (например, рестарт PipeWire);
- держит `parec` на мониторе sink, чтобы виртуальный sink не уходил в suspend
  и `pacat` не блокировался;
- проверяет `adb devices` и ждёт телефон, если он отключён;
- корректно завершается по SIGTERM/SIGINT (Linux) или `taskkill` (Windows),
  убирает pid-файл.

Журнал: Linux — `journalctl --user -u womic-mic`, Windows —
`%LOCALAPPDATA%\womic-mic\womic-mic.log` (кнопка «Журнал» в GUI).

## Шумодав

RNNoise (порт `nnnoiseless`, чистый Rust). Работает кадрами по 480 сэмплов (10 мс).
Замеры на синтетике: розовый шум — подавление −21…−30 dB; сигнал с голосом —
VAD 0.98–1.00, речь проходит, шум убирается. Тихая комната на выходе даёт почти ноль.

`--gate N` добавляет мягкий гейт: при VAD ниже N сигнал плавно уводится в тишину
(по умолчанию выключен, RNNoise сам справляется).

## Проверка

Linux:

```fish
womic-mic status
systemctl --user status womic-mic
journalctl --user -u womic-mic -f
womic-mic run --stats
```

Windows:

```powershell
.\target\release\womic-mic.exe status
.\target\release\womic-mic.exe restart
Get-Content "$env:LOCALAPPDATA\womic-mic\womic-mic.log" -Tail 40 -Wait
```
