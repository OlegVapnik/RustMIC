# RustMIC

Телефон на Android как системный микрофон — на Linux и Windows. Без кабеля
(Wi‑Fi / Wi‑Fi Direct / Bluetooth) или по USB (adb), с шумодавом RNNoise.
Одна программа вместо набора скриптов: сама держит цепочку, перезапускает её при
сбоях и умеет **пускать музыку сразу в микрофон** — без скачивания файлов.

```
телефон (WO Mic) --rustmic--> Opus → PCM s16le 48k mono
                              -> RNNoise (nnnoiseless, в процессе)
                              -> виртуальный микрофон системы
                              -> Discord / Zoom / OBS / игра
```

Дополнительно: `RustMIC play "название или ссылка"` запускает mpv с
YouTube / YouTube Music / SoundCloud и смешивает звук в тот же виртуальный
микрофон. На Linux вы его одновременно слышите в наушниках.

## Возможности

- **Транспорт без кабеля**: `wifi` (IP телефона), `wifi-direct`, `bluetooth`
  (MAC-адрес) — `adb` не нужен вообще.
- **USB** через `adb`: классический режим WO Mic.
- **Шумодав** RNNoise + мягкий VAD-гейт, громкость 0–150 %.
- **Музыка в микрофон**: name → `ytsearch1:<name>`, ссылки Passing-through.
- **Linux**: PipeWire null-sink → `module-remap-source` → `rustmic_mic`.
- **Windows**: WASAPI → VB-CABLE, приложения выбирают **CABLE Output**.
- **Один файл на Windows**: `RustMIC-portable.exe` распаковывается,
  один раз запрашивает UAC для VB-CABLE и дальше работает без установки.

## Windows: один файл

Скачайте `RustMIC-portable.exe`, запустите двойным щелчком:

1. Программа распакует всё в `%LOCALAPPDATA%\RustMIC`.
2. Если VB-CABLE не найден — один раз появится UAC-запрос на
   `VBCABLE_Setup_x64.exe`. Повторных запросов не будет.
3. Откроется окно RustMIC. Выберите **Подключение → Wi-Fi** и введите IP
   телефона из приложения WO Mic (или USB / Bluetooth).
4. В приложениях (Discord, Zoom, OBS…) выберите микрофон **CABLE Output**.

В комплекте: `RustMIC.exe`, `RustMIC-gui.exe`, `RustMIC-client.exe`
(клиент протокола WO Mic), `mpv.exe`, `yt-dlp.exe`, `VBCABLE_Setup_x64.exe`.
Ничего докачивать и копировать вручную не нужно.

> VB-CABLE — сторонний драйвер. Это единственный способ получить виртуальный
> микрофон в Windows без подписи kernel-драйвера; установка происходит один раз
> и требуется только при первом запуске.

## Настройка Wi-Fi (без кабеля)

1. В WO Mic на телефоне: **Settings → Transport → Wi-Fi** (или Wi-Fi Direct).
2. Запомните IP, показанный на экране телефона (например `192.168.1.50`).
3. В RustMIC откройте настройки: **Подключение → wifi**, поле **Хост** = IP.
   Для Bluetooth выберите `bluetooth` и введите MAC.
4. Перезапустите службу кнопкой «Перезапустить» (или `RustMIC restart`).

То же из консоли:

```fish
RustMIC run --transport wifi --host 192.168.1.50
RustMIC run --transport wifi-direct --host 192.168.49.1
RustMIC run --transport bluetooth --bt-address AA:BB:CC:DD:EE:FF
RustMIC run --transport usb          # старый режим, нужен adb
```

Телефон и компьютер должны быть в одной сети. WO Mic слушает TCP-порт 7800;
мешать могут файрволл и мобильный режим — проверьте `RustMIC status`.

## Музыка в микрофон

```fish
RustMIC play "Harvest Moon"                       # поиск YouTube
RustMIC play "https://youtu.be/dQw4w9WgXcQ"       # прямая ссылка
RustMIC play "https://soundcloud.com/foo/bar"
RustMIC now-playing
RustMIC stop-music
```

То же есть в GUI: поле со ссылкой/названием, кнопки «Играть» и «Стоп».
Ссылки YouTube Music / SoundCloud передаются mpv как есть; обычные имена
превращаются в `ytsearch1:<name>`, то есть поиск и стриминг делает mpv + yt-dlp
на лету. Файл скачивать вручную не надо (mpv буферизует поток).

На Linux звук идёт в отдельный `rustmic_music` null-sink и двумя лупбеками
смешивается в `rustmic_sink` (микрофон) и в обычный выход (наушники).
Лупбеки снимаются командой `stop-music`, чтобы не было эха.

Ограничения: DRM-защищённые треки из браузера не перехватываются — здесь
воспроизводится поток, а не экран. Задержка ≈20–40 мс. Наушники обязательны,
иначе микрофон подхватит звук из колонок. В Discord/OBS выключите Krisp/шумодав,
иначе он может вырезать музыку как «фоновый шум».

## Linux: сборка и установка

```fish
cd ~/Projects/RustMIC
cargo build --release --features gui
install -m 0755 target/release/RustMIC ~/.local/bin/RustMIC
install -m 0755 target/release/RustMIC-gui ~/.local/bin/RustMIC-gui
RustMIC install          # systemd-юнит, enable --now
```

Зависимости: `rustmic` в `PATH` (клиент WO Mic), `pacat`/`parec`/`pactl`
(PipeWire), `mpv`, `yt-dlp`, `adb` (только для USB). GUI — GTK4 + ksni.

## Windows: сборка одного файла

Кросс-сборка с Linux (нужны цели `x86_64-pc-windows-gnu` и `rust-lld` +
библиотеки MinGW; подробности в `tools/windows-portable/`):

```fish
cd ~/Projects/RustMIC
./tools/windows-portable/build-portable.sh
# результат: tools/windows-portable/RustMIC-portable.exe
```

`fetch-payloads.sh` тянет бинарники, `pack.py` добавляет контейнер к
launcher'у. Сборка на самой Windows: `cargo build --release --features gui`,
затем те же шаги `fetch-payloads.sh` + `pack.py` (или Tasks в GUI).

## Команды

| команда | что делает |
|---|---|
| `RustMIC run` | супервизор (на Linux им управляет systemd) |
| `RustMIC status` | состояние цепочки, устройства, телефона, клиента |
| `RustMIC settings` | текущие настройки и пути |
| `RustMIC play <query\|URL>` | включить трек/ссылку в микрофон |
| `RustMIC stop-music` | остановить музыку и снять лупбеки |
| `RustMIC now-playing` | что играет сейчас |
| `RustMIC filter` | stdin → stdout, только шумодав |
| `RustMIC install` / `uninstall` | автозапуск (systemd / Run key) |
| `RustMIC start` / `stop` / `restart` | управление фоновым процессом |
| `RustMIC set-default` / `unset-default` | микрофон по умолчанию (Linux) |

Флаги `run`: `--transport usb|wifi|wifi-direct|bluetooth`, `--host IP`,
`--bt-address MAC`, `--adb PATH`, `--device NAME` (Windows), `--gate 0.0..1.0`,
`--no-denoise`, `--stats`.

## Настройки

- Linux: `~/.config/RustMIC/config.toml`
- Windows: `%APPDATA%\RustMIC\config.toml`

```toml
transport = "wifi"          # usb | wifi | wifi-direct | bluetooth
host = "192.168.1.50"       # только для Wi-Fi
bt_address = ""             # только для Bluetooth
denoise = true
gate = 0.00
volume = 100
device = "CABLE Input"      # только Windows
```

Демон перечитывает шумодав, гейт и громкость «на горячую»; смену транспорта
GUI применяет перезапуском службы.

## Шумодав

RNNoise (порт `nnnoiseless`, чистый Rust), кадры по 480 сэмплов (10 мс).
Розовый шум подавляется на −21…−30 dB, речь проходит. `--gate N` добавляет
мягкий гейт по VAD (по умолчанию выключен).

## Диагностика

```fish
RustMIC status
RustMIC settings
journalctl --user -u RustMIC -f        # Linux
```

Windows: `%LOCALAPPDATA%\RustMIC\RustMIC.log`, кнопка «Диагностика» в GUI.
Клиент протокола — открытый `rustmic` (github.com/sambukuper/rustmic); бренд
и код RustMIC не имеют отношения к WO Mic / Wolicheng Tech.
