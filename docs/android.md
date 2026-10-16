# kanban4ai на телефоне: Arch Linux в proot (aarch64)

Телефон — полноценный узел `mobile` с локальным store, а не браузерный клиент
сервера. Доска доступна без сервера; сетевые ограничения не должны мешать чтению
и локальным правкам. Контракт синхронизации — [sync.md](sync.md), разделы 2.1,
6, 8, 10.6 и 11.5.

**Граница готовности.** Этот документ описывает установку и целевой сценарий
эпика TASK-428. Поставлены: CLI `net`, роль `mobile` с outbound-only воркерами
(TASK-432), managed Tailscale (TASK-433), system-режим и сопряжение через
tailnet (TASK-435), выдача ID сервером с резервом для телефона (TASK-430) и
git-relay (TASK-440). Ещё нет: вкладка Sync в Settings (TASK-415/416/434) и
кнопка «Подготовить к поездке» — вся настройка узлов идёт правкой
`<store>/config.yaml` и командами `kanban4ai net` / `kanban4ai serve`,
описанными ниже. Проверяйте поддержку в установленной версии через
`kanban4ai --help` и `kanban4ai net --help`. Не считайте старый релиз полной
реализацией sync.md. Managed Tailscale с вендорским VPN **не проверен на
телефоне**: подтверждено только открытие login.tailscale.com в браузере.
Прогон отложен человеком до конца работ; [матрица TASK-431](android-spike.md)
остаётся обязательной перед эксплуатацией.

## 1. Установка релизного бинаря

Нужны уже установленный Arch Linux в proot, доступ к его приватной файловой
системе и терминал (например, Termux). Команды этого раздела выполняются
**внутри Arch**, не в оболочке Android/Termux: релиз использует Linux/glibc,
а не Android/Bionic. `uname -m` должен показывать `aarch64`; x86_64-архив
не подходит. Systemd для этой инструкции не нужен.

Как root внутри proot (это не root Android):

```sh
pacman -Syu --needed ca-certificates curl tar coreutils git tmux
uname -m
```

Выберите опубликованный тег `v…` на
[странице релизов](https://github.com/sougstron/kanban4ai/releases).
Скачивайте архив и checksum **одного тега**; следующий блок требует сначала
задать `VERSION` равным выбранному тегу:

```sh
: "${VERSION:?Задайте VERSION опубликованным тегом v…}"
(
    set -eu
    archive="kanban4ai-${VERSION}-aarch64-unknown-linux-gnu"
    base="https://github.com/sougstron/kanban4ai/releases/download/${VERSION}"
    mkdir -p "$HOME/downloads/kanban4ai/$VERSION"
    cd "$HOME/downloads/kanban4ai/$VERSION"
    curl -fLO "$base/$archive.tar.gz"
    curl -fLO "$base/$archive.tar.gz.sha256"
    sha256sum -c "$archive.tar.gz.sha256"
    tar -xzf "$archive.tar.gz"
    install -m 0755 "$archive/kanban4ai" /usr/local/bin/kanban4ai
    # Только для новой установки: не заменять чужие kanban/kb.
    ln -s kanban4ai /usr/local/bin/kanban
    ln -s kanban4ai /usr/local/bin/kb
    /usr/local/bin/kanban4ai --version
)
```

Ожидается checksum `OK`, затем версия бинаря. При ошибке checksum не
устанавливать. SHA-256 проверяет целостность загрузки, не является независимой
подписью издателя. При существующих aliases сначала выясните, кому они
принадлежат; не перезаписывайте их вслепую. Для последующих обновлений ручной
установки: `kanban4ai update --check`, затем `kanban4ai update` и перезапуск
TUI. Pacman-owned установки обновляются через pacman, не self-update.

## 2. Store и первый запуск

Store по умолчанию — `$HOME/.local/share/kanban4ai`; если задан
`XDG_DATA_HOME`, это `$XDG_DATA_HOME/kanban4ai`. Абсолютный `KANBAN_HOME`
имеет приоритет. Выберите один `$HOME`/store для всех оболочек proot:
смена пользователя или переменных может выглядеть как пропажа доски.
Держите store и checkout в приватном Linux-каталоге, не в `/sdcard`:
нужны обычные права, symlinks, блокировки и атомарные rename.

Для **новой самостоятельной доски**, не для bootstrap чужой базы:

```sh
mkdir -p "$HOME/projects/phone-demo"
cd "$HOME/projects/phone-demo"
kanban4ai init
kanban4ai project list
kanban4ai
```

`init` регистрирует рабочую папку; данные лежат в
`<store>/projects/<id>/.kanban/`, а не в checkout. Голый `kanban4ai` открывает
TUI; без выбранного проекта — список проектов. CLI доступен и без TUI.
Формат store и разрешение пути — [data-model.md](data-model.md#projects--store).

Для телефона-реплики существующей базы **не создавайте заранее независимые
проекты**. На сервере выполните `kanban4ai serve --phone-qr` (адрес
определяется сам; можно указать `<адрес>:7420`). Он печатает команду вида
`kanban4ai join 123456@192.168.0.10:7420` и её QR. Скопируйте текст QR
сканером телефона и выполните команду с пустым store — `join` сам сделает
узел `mobile` с именем `phone`, при tailnet-адресе включит managed Tailscale,
сопряжётся и выполнит первую синхронизацию. Приглашение одноразовое и живёт
5 минут ([sync.md](sync.md), 11.2).
Bootstrap переносит общую базу,
`store_id` и метаданные; `node_id`, токены, сессии и пути остаются локальными.
`SYNC_STORE_MISMATCH` означает две разные непустые базы: сохраните резервную
копию, затем явно выберите нужную базу; автоматического слияния баз нет.
Не копируйте store другого узла целиком как способ сопряжения — получите
чужую машинную идентичность и секреты.

После `join` роль и имя уже заданы; другое имя — `join --name`. При ручном
сопряжении задайте уникальное имя (например, `phone`) и роль `mobile` в
локальных настройках узла. В конфиге `<store>/config.yaml` это поля `node.name` и
`node.role`; UUID генерируется приложением, не задавайте его вручную. При
ручной правке закройте процессы kanban4ai, сохраните копию config и **объедините**
ключи с существующим YAML, не заменяйте весь файл примером.
Телефон инициирует pull/push/SSE сам; входящий API должен слушать только
loopback. Не открывайте порт телефона в сеть. До поставки TASK-432 не
предполагайте, что смена роли уже обеспечивает этот режим.

## 3. Выживание процессов Android

В оболочке **Termux вне proot** перед длительной работой:

```sh
termux-wake-lock
# После завершения длительной работы:
termux-wake-unlock
```

В Android Settings → Apps → Termux → Battery выберите «Без ограничений» /
отключите оптимизацию батареи. То же для приложения вендорского VPN; на
прошивках с автозапуском разрешите его и фоновую работу. Названия пунктов
зависят от производителя. Wake-lock расходует батарею и не защищает от
нехватки памяти, принудительной остановки или phantom process killer.

### Phantom process killer: Android 12+

`Process completed (signal 9)` может означать убийство proot/его дочерних
процессов Android; это не доказательство ошибки kanban4ai. Диагностируйте
через `adb logcat` с компьютера, ищите `PhantomProcessRecord` /
`Trimming phantom processes`. Не создавайте нагрузку/fork bomb для проверки.

После включения Developer options / USB debugging подключите доверенный
компьютер, подтвердите его ключ и проверьте `adb devices`. **Следующие команды
выполняются на компьютере через adb, не в Arch proot.** Они меняют системные
ограничения для всех приложений, не только Termux.

На Android 12L/13+ с поддержкой AOSP-флага отключение мониторинга:

```sh
adb shell settings put global settings_enable_monitor_phantom_procs false
adb shell settings get global settings_enable_monitor_phantom_procs
# Вернуть мониторинг:
# adb shell settings put global settings_enable_monitor_phantom_procs true
```

Ожидаемый вывод чтения — `false`, но наличие значения не доказывает, что
прошивка использует флаг. На Android 12 без этого флага обход **лимита числа**
phantom-процессов (не полное отключение CPU-killer):

```sh
adb shell /system/bin/device_config get activity_manager max_phantom_processes
# Сохраните исходное значение для восстановления.
adb shell /system/bin/device_config put activity_manager max_phantom_processes 2147483647
adb shell /system/bin/device_config get activity_manager max_phantom_processes
# Если исходное значение было null, восстановить отсутствие override:
# adb shell /system/bin/device_config delete activity_manager max_phantom_processes
# Иначе вернуть исходное число через device_config put.
```

Google/OEM может сбросить DeviceConfig после обновления/перезапуска; проверьте
значение снова. Не отключайте глобальную синхронизацию DeviceConfig только
ради proot: это затрагивает несвязанные системные настройки. Флаг, права adb
и меню «Disable child process restrictions» зависят от версии/прошивки.
Источник и ограничения обходов:
[Termux: Android 12 phantom processes](https://github.com/termux/termux-app/issues/2366#issuecomment-1237468220).
Отключите USB debugging/отзовите разрешение компьютера после настройки.
Ни один обход не обещает бессмертие процесса.

### Долгие задачи: detach, не просто `&`

Внутри Arch можно держать TUI либо foreground `kanban4ai daemon` в отдельной
сессии tmux; tmux сохраняет терминал при выходе из оболочки, но не при убийстве
всего приложения Android. Агент использует текущий callback-бинарь:

```sh
"$KANBAN_CMD" detach "$KANBAN_TASK_ID" --session "$KANBAN_SESSION" \
    --eta 600 --note 'Ожидаю результат длительной команды' -- sh ./long-job.sh
```

`long-job.sh` — реальная команда проекта; `600` замените ожидаемым временем.
Detach создаёт отдельную `setsid`-сессию, пишет вывод и exit code в
`.kanban/detached/<task>-<stamp>.log` / `.status` и объявляет ожидание.
Это защищает от завершения **агентской сессии**, не от Android SIGKILL,
перезагрузки или потери питания. Для проверки результата после срока нужен
работающий TUI/daemon; если Android убил весь proot, откройте его заново и
запустите kanban4ai. Отсутствие `.status` не означает успех. Детали —
[CLI](cli.md) и [оркестрация](orchestration.md).

## 4. Managed Tailscale при вендорском VPN

Не устанавливайте второй Android Tailscale VPN: Android VpnService уже занят
вендорским VPN. Здесь `tailscaled` — userspace-процесс **внутри proot**, без
TUN и systemd; kanban4ai отправляет tailnet HTTP/SSE через SOCKS5 на loopback.
LAN остаётся прямым. DNS MagicDNS разрешается через SOCKS5h, не DNS Android.
На сервере/ноуте нужен обычный Tailscale (`system`), тот же tailnet и API
kanban4ai на разрешённом ACL/firewall порту (обычно 7420).

Установите пару бинарей внутри Arch: `pacman -S tailscale`, **без** запуска
systemd unit. Альтернатива — `kanban4ai net install`: официальный static
arm64 tarball с проверкой SHA-256 устанавливается в `<store>/net/bin/`.
Не запускайте одновременно standalone spike daemon и managed sidecar на
одном SOCKS-порту; закончите спайк перед обычной работой.

В локальном `<store>/config.yaml` объедините с имеющимися секциями:

```yaml
net:
  tailscale:
    mode: managed
    hostname: kanban-phone
    socks_port: 1055
    auth_key_file: null
```

Режим по умолчанию `off`; `net up` **не** включает managed автоматически.
Блок «Сеть» появится в Settings → Sync позже (TASK-434), сейчас вход — CLI:

```sh
kanban4ai net up
# Открыть показанный URL в браузере телефона либо отсканировать QR.
# Войти в нужный tailnet, не выключая вендорский VPN.
kanban4ai net up
kanban4ai net status
```

Первый `up` с `NET_TAILSCALE_LOGIN_REQUIRED` и URL — запрос входа, не успешное
подключение. После подтверждения повторите команду. В Tailscale Admin Console
→ Machines выберите **именно этот телефон** → Disable key expiry; при
необходимости то же для постоянных server/laptop. Это убирает периодический
повторный вход ценой долгоживущего ключа устройства; при утрате телефона
отзовите машину в админке и сопряжение kanban4ai.

Альтернатива браузерному входу — одноразовый auth key из Tailscale Admin
Console → Settings → Keys. Перенесите его в приватный файл внутри proot с
правами 0600, не в `/sdcard`, не в тред/лог, argv или shell history:

```sh
kanban4ai net up --auth-key-file "$HOME/.config/kanban4ai/tailscale-auth-key"
```

Файл удаляется только после подтверждённого входа; при ошибке он остаётся
секретом, который нужно защитить/удалить после диагностики. Срок действия
**auth key** ограничивает регистрацию; key expiry **машины** — другое
ограничение. Не используйте ephemeral-узел для постоянной реплики.
State лежит в `<store>/net/tailscale/`, приватный socket — рядом, в
`<store>/net/tailscale.sock` (каталоги 0700, файлы и socket 0600), не
реплицируются. Не публикуйте state, auth key, полный login URL
или токены сопряжения.

`net status` показывает mode, аккаунт, IP и пути пиров; `Peer[].CurAddr` /
`Relay` позволяют отличить direct от DERP. DERP через HTTPS — допустимый
рабочий путь, не ошибка. Нужен HTTPS к controlplane/login/DERP Tailscale;
UDP 41641 желателен для direct. `net down` отключает sidecar и автоматический
рестарт до следующего `up`; `net logout` удаляет вход. TUI/daemon управляют
жизненным циклом sidecar; если жив владелец, следующий тик пытается поднять
убитый sidecar. Если убит весь proot, никакой тик не произойдёт до нового запуска.

| Код / симптом | Действие |
|---|---|
| `NET_TAILSCALE_MISSING` | Установить оба бинаря через pacman или `net install`, проверить PATH внутри того же proot. |
| `NET_TAILSCALE_LOGIN_REQUIRED` | Открыть свежий URL/QR, подтвердить вход; проверить expiry машины, повторить `net up` / `net status`. |
| `NET_TAILSCALE_SIDECAR_DOWN` | Сохранить обезличенную причину, проверить свободный SOCKS-порт и права state; повторить `net up` (при необходимости `net down`, затем `net up`). Если убит proot — сначала открыть его заново. |
| Нет пути при включённом VPN | Выполнить матрицу TASK-431: вход, SOCKS health, Wi-Fi/mobile, экран, смена сети; сравнить полный MagicDNS и tailnet IP. Netlink-warning сам по себе не доказывает отказ. |
| Tailscale недоступен | Работать через LAN, если он доступен; вне LAN запасной путь только git-relay, иначе офлайн. Не выключать вендорский VPN ради обхода. |

## 5. Без сервера: автономно, резерв ID, подготовка к поездке

После первоначального bootstrap телефон хранит полную базу. Нет связи —
правки/ответы/треды пишутся локально и ждут доставки; отсутствие LAN
(`NODE_OUT_OF_LAN`) — статус пути, не причина блокировать доску. Сервер не
нужен для чтения, редактирования и создания задач: номера `TASK-N` выдаются
локально (`docs/sync.md` раздел 6). Для запуска агента нужны локальный код,
backend и его логин.

**Перед поездкой, пока сервер доступен** (вручную, пока нет вкладки Sync):

1. Дождаться синка всех проектов и отсутствия ожидающих операций.
2. Подготовить код проектов, чей runner допускает mobile: заранее
   клонировать/обновить их в локальный `sync.clone_root` (по умолчанию
   `<store>/clones/`). Это подготовка **кода**, не замена синка доски. Кнопки «Подготовить к поездке» в сборках ещё нет; клон создаётся
   при первом запуске задачи на телефоне (git bundle через serve или из
   remote), поэтому до поездки сделайте пробный запуск и установите
   зависимости.
3. У каждого нужного проекта проверить git remote и доступ к нему с телефона.
   При `SYNC_NO_REMOTE` добавить remote; полностью офлайн отсутствующий клон
   скачать нельзя. Учётные данные git/SSH и backend на телефоне локальны.
4. Проверить runner `here`/`mobile` либо разрешённый fallback `auto`,
   установленный backend, логин и зависимости сборки. Сам бинарь kanban4ai
   не устанавливает агентские CLI. Запустить нужный рабочий сценарий на
   телефоне до отключения сети.
5. Убедиться, что wake-lock/батарея настроены, и выполнить телефонную матрицу
   TASK-431. Сохранить резервную копию store командой `kanban backup` (`docs/backup.md`); sync не заменяет backup.

Если телефон и другой узел офлайн создали задачу с одним номером, после
синка задача с более ранним созданием сохраняет его, а вторая на всех узлах
получает следующий свободный номер; старый номер остаётся алиасом.

### Git-relay, когда сервер выключен, но интернет есть

Запасной транспорт git-relay (TASK-440) использует git remote любого
синхронизируемого проекта: каждый узел пишет свой
`refs/kanban/sync/<node_id>`, читает чужие refs, доставляет операции через
обычное слияние. HTTP-пути остаются прямыми (LAN, затем tailnet); relay-воркер
работает независимо — без домашней сети и без Tailscale, в том числе на
outbound-only телефоне. Ноут может получить изменения телефона через remote,
даже когда сервер выключен.
Remote — `sync.relay_remote` в локальном конфиге либо уникальные origin
зарегистрированных проектов; опрос — `sync.relay_interval` (60 с),
`serve --sync` выполняет цикл принудительно. Каждый remote получает журнал
всего store: используйте отдельный приватный remote или age.

Relay переносит журнал, хвосты логов и WIP, **не** bootstrap snapshot,
удалённые launch/stop или live-лог. Первоначально сопрягите/синхронизируйте
узлы прямым путём до поездки. Remote должен разрешать чтение/запись служебных
refs, а не только checkout обычной ветки. Не force-push чужие refs.
Без шифрования содержимое операций доступно всем читателям remote:
для `sync.relay_encrypt: true` установите `age` и `age-keygen` на всех узлах
**до повторного сопряжения** — ключи создаются при нём. Секреты и машинные
state в relay не входят, но текст задач/тредов может содержать
чувствительные данные.

Обычный `git push` исходников доску не переносит: relay-воркеры запускаются
из serve/TUI/daemon сами.
Если нет ни одного пути, сохраняйте изменения локально. При восстановлении
связи узлы обмениваются журналами; телефон пополняет запас ID у сервера.
Конфликты одного поля офлайн разрешаются LWW, проигравшее значение сохраняется
для просмотра — проверьте конфликты после reconnect, не считайте отсутствие
сетевой ошибки доказательством отсутствия конфликтов.
