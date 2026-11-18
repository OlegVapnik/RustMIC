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
- **Шумодав** RNNoise + гейт по VAD или по уровню (RMS, дБ), громкость 0–100 %.
- **Музыка в микрофон**: name → `ytsearch1:<name>`, ссылки передаются как есть.
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

То же есть в GUI на вкладке «Саундпад»: поле со ссылкой/названием,
кнопки «Играть»/«Стоп», пауза, перемотка ±10 с, обложка, название и прогресс.
Метаданные (название, длительность, обложку) демон получает через `yt-dlp -J`,
позицию/паузу — через IPC самого mpv (`--input-ipc-server`). По плитке можно
нажать, чтобы запустить звук; плитки добавляются/удаляются прямо в GUI и
сохраняются в `~/.cache/RustMIC/tiles.json`.
Ссылки YouTube Music / SoundCloud и локальные файлы передаются mpv как есть;
обычные имена превращаются в `ytsearch1:<name>`, то есть поиск и стриминг
делает mpv + yt-dlp на лету. Файл скачивать вручную не надо.

На вкладке «Настройки» доступны громкость музыки, `loudnorm` (через
`lavfi`, с откатом на обычную громкость, если фильтр недоступен), ducking
громкости музыки при открытом голосовом гейте и мониторинг в наушники
(только Linux; на Windows мониторинг отключён — музыка и голос идут в одно
устройство CABLE Input, и для раздельного мониторинга нужен отдельный PCM-микшер
на стороне Windows).

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
(PipeWire), `mpv`, `yt-dlp`, `adb` (только для USB). GUI — общий egui с
тёмной темой, шрифтом Inter (Regular/SemiBold, лицензия OFL в `assets/fonts`),
трей на Linux (ksni) и Windows (tray-icon).

### Интерфейс

Окно делится на вкладки «Микрофон», «Саундпад» и «Настройки».
Вверху — статус службы и меню (журнал, диагностика, перезапуск, выход);
на вкладке «Микрофон» — состояние соединения, живой индикатор уровня,
переключатель шумодава, режим и чувствительность гейта (черту порога можно
тянуть мышью прямо по шкале), громкость микрофона и кнопка «Тест микрофона».

Если демон не запущен, GUI показывает «Служба не запущена» и кнопку
«Запустить» — он не притворяется работающим и не показывает устаревшие
значения. Закрытие окна сворачивает его в трей; «Выход» в меню/трее
завершает приложение. Вкладка «Саундпад» полностью рабочая: плитки,
обложка, пауза/перемотка и громкость музыки. Мониторинг музыки — только Linux.

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
| `RustMIC ctl state` | состояние демона по локальному сокету (JSON) |
| `RustMIC ctl mic 80` | громкость микрофона на лету |
| `RustMIC ctl music 70` | громкость музыки, которая идёт в микрофон |
| `RustMIC ctl monitor-volume 30` | громкость музыки только у вас в колонках (0 — тишина) |
| `RustMIC ctl gate-mode voice\|level` | режим гейта |
| `RustMIC ctl mute` / `unmute` | быстрое выключение микрофона |
| `RustMIC ctl loudnorm on\|off` | выравнивание громкости (с откатом) |
| `RustMIC ctl ducking on\|off`, `ctl ducking-db 12` | приглушение музыки при голосе |
| `RustMIC ctl tile-add "Имя" "запрос"`, `ctl tile-remove <id>` | плитки саундпада |
| `RustMIC play <query\|URL>` | включить трек/ссылку в микрофон |
| `RustMIC ctl play <query\|URL>` | то же через демона (состояние трека в `ctl state`) |
| `RustMIC ctl pause true\|false`, `ctl seek 10` | пауза/перемотка через mpv IPC |
| `RustMIC stop-music` | остановить музыку и снять лупбеки |
| `RustMIC now-playing` | что играет сейчас |
| `RustMIC filter` | stdin → stdout, только шумодав |
| `RustMIC install` / `uninstall` | автозапуск (systemd / Run key) |
| `RustMIC start` / `stop` / `restart` | управление фоновым процессом |
| `RustMIC set-default` / `unset-default` | микрофон по умолчанию (Linux) |

Флаги `run`: `--transport usb|wifi|wifi-direct|bluetooth`, `--host IP`,
`--bt-address MAC`, `--adb PATH`, `--device NAME` (Windows),
`--gate-mode voice|level`, `--gate-voice 0..100`, `--gate-level -60..0`,
`--mic-volume 0..100`, `--music-volume 0..100`, `--loudnorm`, `--no-ducking`,
`--no-monitor`, `--no-denoise`, `--stats`.

Демон и GUI общаются по локальному сокету (`$XDG_RUNTIME_DIR/RustMIC.sock`,
на Windows — именованный pipe). Формат — JSON Lines с версией протокола;
`RustMIC ctl` — маленький клиент для того же протокола.

## Настройки

- Linux: `~/.config/RustMIC/config.toml`
- Windows: `%APPDATA%\RustMIC\config.toml`

```toml
config_version = 5
transport = "wifi"          # usb | wifi | wifi-direct | bluetooth
host = "192.168.1.50"       # только для Wi-Fi
bt_address = ""             # только для Bluetooth
denoise = true
gate_mode = "voice"         # voice | level
gate_voice = 0              # 0..100 %, 0 = выключен
gate_level_db = -45.0       # -60..0 dBFS, 0 = выключен
mic_volume = 100
music_volume = 100
loudnorm = false
ducking = true
ducking_db = 12.0
monitor_music = true
device = "CABLE Input"      # только Windows
```

Файл версионирован (сейчас `config_version = 5`): старые `gate = 0.46` и
`volume = 78` автоматически превращаются в `gate_voice = 46` и
`mic_volume = 78`. Неизвестные ключи (в том числе от удалённых функций)
игнорируются и исчезают при следующем сохранении. Демон перечитывает
шумодав, гейт и громкости «на горячую»; смену транспорта GUI применяет
перезапуском службы.

## Шумодав

RNNoise (порт `nnnoiseless`, чистый Rust), кадры по 480 сэмплов (10 мс).
Розовый шум подавляется на −21…−30 dB, речь проходит. Гейт работает в двух
режимах: `voice` — вероятность голоса RNNoise (0–100 %, по умолчанию
выключен), `level` — RMS в dBFS с гистерезисом 3 дБ.

## Диагностика

```fish
RustMIC status
RustMIC settings
journalctl --user -u RustMIC -f        # Linux
```

Windows: `%LOCALAPPDATA%\RustMIC\RustMIC.log`, кнопка «Диагностика» в GUI.
Клиент протокола — открытый `rustmic` (github.com/sambukuper/rustmic); бренд
и код RustMIC не имеют отношения к WO Mic / Wolicheng Tech.
