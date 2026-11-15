# womic-mic

Телефон на Android как системный микрофон в Linux, по USB (adb), с шумодавом.
Одна программа вместо скрипта: сама держит всю цепочку и перезапускает её при сбоях.

```
rustmic (USB/adb, Opus -> PCM s16le 48k mono)
   -> RNNoise (nnnoiseless, in-process)
   -> pacat -> womic_sink (null sink)
   -> module-remap-source -> источник womic_mic
```

## Сборка и установка

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

## Команды

| команда | что делает |
|---|---|
| `womic-mic run` | запустить супервизор (это же вызывает systemd) |
| `womic-mic status` | состояние цепочки, узлов, adb и источника по умолчанию |
| `womic-mic filter` | stdin → stdout, только шумодав (для тестов/других пайплайнов) |
| `womic-mic install` | создать юнит, `enable --now` |
| `womic-mic install-no-enable` | создать юнит без автозапуска |
| `womic-mic uninstall` | остановить и удалить юнит |
| `womic-mic set-default` / `unset-default` | переключить микрофон по умолчанию |

Флаги: `--adb PATH`, `--gate 0.0..1.0` (дополнительный гейт по VAD от RNNoise),
`--no-denoise`, `--stats` (печатать in/out RMS и VAD раз в 0.5 с).

## GUI и трей

`womic-mic-gui` — окно-дашборд GTK4 плюс иконка в системном трее
(StatusNotifierItem, хост — noctalia).

- статус: телефон (USB), сервис, индикатор уровня с затуханием;
- микрофон включён (старт/стоп юнита), микрофон по умолчанию, шумодав, гейт;
- громкость виртуального микрофона: 0–150%;
- диагностика проверяет зависимости, PipeWire/PulseAudio, узлы и последние строки журнала;
- закрытие окна не завершает программу — она остаётся в трее;
- автозапуск: `spawn-at-startup "womic-mic-gui"` в конфиге niri
  (`~/.config/niri-noctalia/cfg/autostart.kdl` — именно это дерево читает niri,
  путь задан в `NIRI_CONFIG`).

Настройки хранятся в `~/.config/womic-mic/config.toml`:

```toml
denoise = true
gate = 0.00
volume = 100
```

Демон перечитывает шумодав и гейт при изменении файла; громкость применяется
при создании узла и периодически восстанавливается после перезапуска PipeWire.
Поэтому юнит запускается без аргументов.

## Что делает супервизор

- перезапускает `rustmic` и `pacat` при падении (backoff 1→30 с, сброс после 60 с стабильной работы);
- раз в 3 с проверяет узлы PipeWire и пересоздаёт `womic_sink`/`womic_mic`, если их снесли (например, рестарт PipeWire);
- держит `parec` на мониторе sink, чтобы виртуальный sink не уходил в suspend и `pacat` не блокировался;
- проверяет `adb devices` и ждёт телефон, если он отключён;
- корректно завершается по SIGTERM/SIGINT, убирает pid-файл.

## Шумодав

RNNoise (порт `nnnoiseless`, чистый Rust). Работает кадрами по 480 сэмплов (10 мс).
Замеры на синтетике: розовый шум — подавление −21…−30 dB; сигнал с голосом —
VAD 0.98–1.00, речь проходит, шум убирается. Тихая комната на выходе даёт почти ноль.

`--gate N` добавляет мягкий гейт: при VAD ниже N сигнал плавно уводится в тишину
(по умолчанию выключен, RNNoise сам справляется).

## Проверка

```fish
womic-mic status
systemctl --user status womic-mic
journalctl --user -u womic-mic -f          # логи rustmic и статистика
womic-mic run --stats                      # посмотреть in/out RMS и VAD вживую
```
