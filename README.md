# ✦ ASTRA

**Свободный аналог Soundpad для Linux.** Rust + egui + PipeWire. Работает на Wayland (niri, GNOME, KDE, wlroots-композиторы) и играет звуки в **колонки**, в **виртуальный микрофон** (чтобы их слышал Discord и другие голосовые приложения) или **в оба места сразу**.

A free, open-source Soundpad alternative for Linux: a soundboard written in Rust that plays sounds to your speakers, to a virtual microphone (so Discord etc. hear them), or both. English summary at the bottom.

---

## Возможности

- 🎵 **Форматы**: MP3, WAV, OGG/Vorbis, FLAC, AAC/M4A (через symphonia)
- 🎚 **Куда играть**: динамики / виртуальный микрофон / оба — глобально и **персонально для каждого звука**
- 🎙 **Виртуальный микрофон**: ASTRA создаёт настоящий входной источник `ASTRA Virtual Microphone` (его слышат Discord/Telegram/OBS; внутренний микшер `ASTRA Sounds` при этом просто играет в колонках-«наушниках» в списке вывода — его выбирать не нужно)
- 🗣 **Сквозной микрофон**: ваш настоящий микрофон микшируется в тот же виртуальный (голос + звуки одновременно), с выбором устройства и локальным мониторингом
- ⌨️ **Горячие клавиши**:
  - в окне приложения;
  - глобальные через **портал GlobalShortcuts** (GNOME / KDE / niri ≥ 25.02);
  - генерация **биндов niri** одной кнопкой — сниппет вставляется в `config.kdl`, проверяется `niri validate` и работает даже при закрытом окне;
  - «стоп всё» по своей клавише
- 📁 **Категории**, поиск, сортировки (по имени/дате/частоте), недавние, drag & drop файлов и папок
- 🔊 **Настройка звука**: общая громкость, громкость каждого звука, нормализация громкости по пику, скорость 0.5–2.0×, зацикливание, «останавливать другие звуки» (глобально и для каждого звука)
- 🛠 **CLI / IPC**: `astra play 3`, `astra stop-all`, `astra mic toggle` — управляйте из скриптов, niri-биндов, OBS
- 🌐 **Двуязычный интерфейс**: русский / английский
- 💾 Конфиг в одном JSON: `~/.config/astra/config.json`

## Скриншот

Окно — это таблица звуков с кнопками Play/Stop, прогресс-баром, хоткеем и панелью категорий; сверху — поиск, маршрут, громкость и переключатель «Голос → микрофон»; в настройках — устройства, микрофон, хоткеи, хранилище.

## Установка

### Готовый бинарник (Arch, x86_64)

```bash
tar -xzf astra-v*-x86_64-linux.tar.gz
cd astra-*/ && ./install.sh          # в ~/.local (без root)
```

### cargo

```bash
cargo install --git https://github.com/SEVENinthem/astra
```

### Из исходников

Зависимости для сборки: Rust 1.78+, `gtk3` (файловый диалог), `pkg-config`. Системных аудио-зависимостей нет: libpulse-binding — чистые Rust-биндинги.

```bash
git clone https://github.com/SEVENinthem/astra && cd astra
cargo build --release
./install.sh   # или просто: ./target/release/astra
```

**Runtime-зависимости**: PipeWire + `pipewire-pulse` (для звука через pactl; есть в любом стандартном окружении). `pactl` из пакета `libpulse`. Опционально: `xdg-desktop-portal-gnome`/`-gtk` (диалог выбора файлов и глобальные хоткеи через портал).

## Быстрый старт

1. Запустите `astra`.
2. Перетащите аудиофайлы в окно (или «Добавить файлы»).
3. Чтобы Discord слышал звуки: (пере)запустите ASTRA, затем **перезапустите Discord** (он строит список устройств при старте) и выберите **устройство ввода → «ASTRA Virtual Microphone»**. Включите переключатель **«Голос → микрофон»**, чтобы ваш голос шёл туда же. Это настоящий источник, а не monitor — Chromium/Discord его не фильтрует.
4. Назначьте хоткеи: двойной клик по строке → правый клик → *Изменить* → *Задать клавишу*.

## Горячие клавиши под Wayland

На Wayland приложение не может перехватывать клавиши само — есть три пути:

1. **Портал GlobalShortcuts** (включён по умолчанию). Работает в GNOME, KDE и niri ≥ 25.02 со свежим `xdg-desktop-portal-gnome`. Статус виден в *Настройках → Горячие клавиши*.
2. **Бинды niri** (самый надёжный путь на niri): *Настройки → Клавиши niri → Вставить в конфиг niri*. ASTRA вставит блок вида

   ```kdl
   binds {
       // === ASTRA soundboard: begin (managed by the app) ===
       Mod+F1 { spawn "astra" "play" "1"; }
       // === ASTRA soundboard: end ===
   }
   ```

   внутрь существующего узла `binds` (второго узла niri не допускает), прогонит `niri validate` и откатит изменения при ошибке. niri перечитывает конфиг на лету.
3. **Любой другой композитор**: сгенерируйте сниппет кнопкой «Копировать» и адаптируйте под свой формат (`i3`, `hyprland.conf` и т.д.) — команды те же: `astra play <id>`.

## CLI

Команды работают, пока запущено окно ASTRA (общение по unix-сокету):

```text
astra play <id|имя> [--volume 80]   играть
astra toggle <id|имя>               играть / остановить
astra stop [<id|имя>]               остановить звук или всё
astra stop-all                      остановить всё
astra list                          список: id, хоткей, имя
astra add <пути...>                 добавить файлы/папки
astra mic on|off|toggle|status      сквозной микрофон
astra vol <0..150>                  общая громкость, %
astra reload                        перечитать конфиг
astra quit                          закрыть ASTRA
```

## Конфиг

`~/.config/astra/config.json` — звуки (id, имя, путь, громкость, хоткей, маршрут, категория, loop, normalize, скорость…), категории, настройки. Правится и руками, и через UI.

## Известные ограничения / roadmap

- Нет истории позиций воспроизведения, drag-reorder строк и иконки в трее (Soundpad-фичи, планируются).
- Глобальные хоткеи через портал зависят от бэкенда портала вашего окружения; на niri проще бинды.
- Папка `~/.local/bin` должна быть в `PATH` для CLI и биндов.

## Лицензия

MIT. Soundpad — товарный знак Leppsoft, ASTRA никак с ним не связана.

---

## English

**ASTRA** is a free, open-source Soundpad alternative for Linux (Rust + egui + PipeWire, Wayland-first: niri / GNOME / KDE / wlroots).

- Plays MP3/WAV/OGG/FLAC/AAC to **speakers**, a **virtual microphone** (PipeWire null-sink, pick it as the input device in Discord), or both at once.
- Microphone passthrough mixes your real mic into the same virtual mic; optional local monitoring.
- Global hotkeys: XDG GlobalShortcuts portal (GNOME/KDE/niri ≥ 25.02) or generated niri `binds` (auto-inserted into `config.kdl`, validated with `niri validate`, atomic rollback on failure).
- Categories, search, sorting, drag & drop, per-sound volume / speed (0.5–2×) / loop / normalize / route, stop-others behaviour.
- IPC CLI for scripts and compositor binds: `astra play 3`, `astra stop-all`, `astra mic toggle`, `astra vol 80`…
- Config: `~/.config/astra/config.json`. Install: `./install.sh` (user-local) or `cargo install --git https://github.com/SEVENinthem/astra`.

Build: `cargo build --release` (needs Rust 1.78+ and gtk3 for the file dialog; no native audio libs required). Runtime needs PipeWire with `pipewire-pulse` and `pactl`. MIT licensed.
