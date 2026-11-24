# План реализации Volume Smart Icon

## Цель
Создать легковесное минималистичное Windows-приложение на Rust (`x86_64-pc-windows-msvc`), которое:
- отображает иконку в системном трее в зависимости от типа активного устройства вывода звука (**наушники** или **динамик/колонки**) с контрастными цифрами текущей громкости (`0–100`) и индикацией Mute;
- по **двойному левому клику** переключает устройство вывода звука по кругу среди выбранных пользователем устройств (по принципу SoundSwitch);
- оставляет одиночный левый клик без действия;
- предоставляет контекстное меню по ПКМ для ручного переключения устройства, настройки списка цикла, переопределения типа значка, автозапуска с Windows и выхода.

## Архитектура и модули
1. **[Completed] Окружение и проект** (`Cargo.toml`, `.gitignore`):
   - Установка Microsoft C++ Build Tools + `rustup` (`x86_64-pc-windows-msvc`).
   - Минимальные зависимости: `windows` (Win32 API, Core Audio WASAPI/MMDevice, GDI), `windows-core`, `serde` + `serde_json` для конфига.
   - Профиль сборки `release` с оптимизацией размера (`opt-level = "s"`, `lto = true`, `strip = true`, `panic = "abort"`).
2. **[Completed] Конфигурация и автозапуск** (`src/config.rs`):
   - Хранение в `%APPDATA%\VolumeSmartIcon\config.json`: список ID устройств для цикла двойного клика (`cycle_device_ids`), флаг первичной инициализации, ручные переопределения типа иконки (`Auto` / `Headphones` / `Speakers`).
   - Управление автозапуском через реестр `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
3. **[Completed] Переключение устройства вывода** (`src/policy_config.rs`):
   - Безопасная COM-обёртка над `IPolicyConfig` (и резервный `IPolicyConfigVista`) для вызова `SetDefaultEndpoint` по ролям `eConsole`, `eMultimedia`, `eCommunications`.
4. **[Completed] Мониторинг аудио без фонового опроса** (`src/audio.rs`):
   - `IMMDeviceEnumerator` — перечисление активных устройств вывода (`eRender`, `DEVICE_STATE_ACTIVE`) и определение текущего устройства по умолчанию.
   - `PKEY_AudioEndpoint_FormFactor` + эвристика по имени для различения наушников и колонок.
   - `IMMNotificationClient` и `IAudioEndpointVolumeCallback` — мгновенная доставка событий изменения громкости, Mute и смены/подключения устройств через `PostMessageW` в окно трея.
5. **[Completed] Динамическая отрисовка иконок** (`src/icon.rs`):
   - Генерация 32-битных ARGB `HICON` через `CreateDIBSection` + `CreateIconIndirect` под текущий DPI трея (`16×16`, `20×20`, `24×24`, `32×32` и т.д.).
   - Адаптация цветов под светлую/тёмную тему панели задач Windows (`SystemUsesLightTheme`).
   - Силуэт наушников и динамика + компактный пиксельный шрифт (честные цифры `0–100` с узкой `1` для `100` и контрастной подложкой) + состояние Mute.
6. **[Completed] Трей и цикл сообщений Win32** (`src/tray.rs`, `src/main.rs`):
   - Защита от повторного запуска (`CreateMutexW`).
   - Скрытое Win32-окно + `Shell_NotifyIconW`, восстановление иконки при перезапуске Explorer (`TaskbarCreated`), обновление при смене темы/DPI (`WM_SETTINGCHANGE`, `WM_DPICHANGED`).
   - Двойной клик (`WM_LBUTTONDBLCLK`) переключает следующее устройство из отмеченного списка; ПКМ (`WM_RBUTTONUP`) открывает нативное меню.
7. **[Completed] Тестирование и документация** (`README.md`, `CHANGELOG.md`):
   - Модульные тесты (`cargo test`) логики выбора устройств, определения типа по имени/FormFactor, сериализации конфига и рендеринга иконок во всех диапазонах и размерах.
   - Проверка релизной сборки и прогон на локальной системе.

