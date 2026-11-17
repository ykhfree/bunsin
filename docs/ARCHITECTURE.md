# Bunsin 구조

이 문서는 `PLAN.md`의 요구사항을 구현하는 구조를 적습니다.
각 판단에는 `(검증)` 또는 `(가정)`을 붙였습니다. `(검증)`은 2026-11-14에 macOS에서 빌드나 원문으로 확인한 것입니다.

## 1. 프로세스

```
[서버]   bunsind serve ─┬─ engine ── store, ard, core
                        └─ RealBackends ── llm, connectors, coding   (엔진은 Backends trait으로 받는다)
            ▲                                    ▲
            │ 제어 리스너 --listen                │ 내부·공개 리스너 --internal-listen, --public-listen
            │ HTTPS + 프로필별 접속 token          │ 인바운드 MCP(호출자 token), server card,
            │ (bunsin-api::http)                 │ ARD catalog, webhook(서명)
            ▼                                    ▼
[내 컴퓨터]  GPUI app, bunsind의 원격 명령        [다른 사람의 agent, ARD client, Slack, Jira Cloud]
            (bunsin-client)
```

- 엔진은 서버의 `bunsind serve` 프로세스 안에서만 돈다. 화면이 없는 서버에서 동작한다.
- GPUI app은 관리 화면이다. app은 엔진을 품지 않고, `EngineApi`의 원격 구현(`RemoteEngine`)으로 서버에 접속한다.
- 같은 컴퓨터에서 시험할 때는 `bunsind serve`를 `127.0.0.1`에 띄우고 app이 그 주소로 접속한다.
- data dir 하나에 엔진 하나만 뜬다. `engine.lock` 파일의 배타 잠금이 이중 폴링과 이중 실행을 막는다.
- 서버 운영자의 명령(`profile-add`, `token-add`, `token-list`, `token-revoke`)은 저장소 파일을 직접 다룬다.
  엔진 잠금이 필요 없으므로 서버가 도는 동안에도 실행된다. 두 프로세스가 같은 SQLite 파일을 쓰므로 busy timeout을 둔다.
- 서버는 SIGTERM이나 Ctrl-C를 받으면 새 요청을 받지 않고, 실행 중인 요청을 최대 300초 기다린 뒤에 종료한다.
  그 시간 안에 끝나지 않은 승인 실행은 끊기고, 항목은 `executing` 상태로 남는다. 다음 시작이 그 항목을 `unknown`으로 옮긴다(아래 "재개").
  서비스 관리자의 종료 대기 시간(systemd의 기본값은 90초)이 더 짧으면 그 시간에 끊긴다.
  그래서 `packaging/linux/bunsind.service`는 `TimeoutStopSec=330`을 둔다.
- **배포 패키지**(PLAN 6절 4단계): 저장소의 스크립트가 만든다. 스크립트는 비밀 값을 읽지도 담지도 않는다.
  - `packaging/linux/build-tarball.sh`: `cargo build --release -p bunsin-daemon --locked` 뒤에 `bunsind`, unit, sysusers 파일,
    환경 파일 견본(`bunsind.env.example`, 비밀은 이름만), `INSTALL.md`, LICENSE, README를 tarball과 sha256 파일로 묶는다.
  - unit: 전용 사용자 `bunsin`, `EnvironmentFile=/etc/bunsin/bunsind.env`, `ExecStart=... serve $BUNSIND_SERVE_ARGS`,
    `StateDirectory`(home, data, key 폴더, 0700), `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, 빈 capability 집합 등.
    stdio 커넥터와 코딩 CLI가 깨지지 않게 `MemoryDenyWriteExecute`(Node.js JIT), `RestrictNamespaces`(CLI의 sandbox), `SystemCallFilter`는 두지 않았다.
    `--allow-coding`은 `ReadWritePaths=<repo root>`가 필요하다(git worktree가 `.git`에 쓴다). `INSTALL.md`가 바꿀 옵션을 적는다.
  - `packaging/macos/build-app.sh`: `cargo build --release -p bunsin-app --locked` 뒤에 `Bunsin.app`(Info.plist, PkgInfo)을 만든다.
    Developer ID 서명과 공증은 손으로 한다(`packaging/macos/README.md`). 저장소에 아이콘이 없어 아이콘을 넣지 않았다.
    bundle ID는 `BUNSIN_BUNDLE_ID`이고, 기본값 `local.bunsin.app`은 로컬 빌드용이다.
  - CI의 `package-linux` job이 ubuntu에서 shellcheck, tarball 빌드, `systemd-analyze verify`(결과를 막지 않음)를 하고 tarball을 artifact로 올린다. 배포는 하지 않는다.
  - 확인한 것(macOS): 두 스크립트 실행, bundle 구조와 `plutil -lint`, tarball 목록과 sha256, 견본 환경 값으로 `bunsind serve` 시작과 SIGTERM 종료.
    이 Mac에는 `systemd-analyze`와 `shellcheck`가 없어 실행하지 않았다. unit을 systemd로 시작해 보지 않았다.
- **재개**: `server::start`가 아래 순서로 시작한다. `main.rs`와 daemon 테스트가 같은 함수를 부른다.
  1. `mark_interrupted()`: 모든 프로필의 `executing` 항목을 `unknown`으로 옮기고 감사 `outcome_unknown`을 남긴다.
     같은 단계에서 `approved` 도구 호출의 ID를 적어 둔다.
  2. 서버가 요청을 받기 시작한다.
  3. `resume_approved()`: 1단계에서 적은 항목만 본다. 시작 뒤에 승인된 항목은 그 요청이 실행한다.
     정책을 다시 묻는다(`decide`: 정지 스위치, pin, 규칙, 보류). 소유자가 승인한 항목은 `Execute`나 `NeedsApproval`일 때,
     자율 항목은 `Execute`일 때만 실행한다(감사 `resumed`, 실행은 `Approved→Executing` CAS를 지난다).
     그 밖이면 항목을 `failed`로 옮기고 같은 내용의 `pending` 사본(`retry_of`)을 만든다(감사 `resume_abandoned`).
     옮기기와 사본 만들기는 한 transaction이다. 사본의 승인은 정책을 다시 본다.
  4. 폴링을 시작한다.
  - 프로필 하나의 실패는 stderr에 쓰고, 다른 프로필과 서버의 시작을 막지 않는다.
  - `unknown`에서 `approved`나 `executing`으로 가는 전이는 없다. 이미 보냈을 수 있으므로 자동 재실행은 기각했다.
  - **답이 없는 호출.** `execute`가 커넥터에서 `ConnectorError::Unanswered`를 받으면 항목을 `executing`에서 `unknown`으로 옮긴다.
    감사는 `outcome_unknown`(`inbox_id`, `tool`, `error`)이고, 결과는 `Outcome::ExecutionUnknown`이다. 이벤트는 다시 처리하지 않는다.
    `Unanswered`는 요청이 서버에 닿았을 수 있다는 뜻이다. MCP 커넥터는 도구 호출 단계의 시간 초과에 쓴다.
    호출 앞의 도구 목록 단계의 시간 초과는 호출을 보내지 않았으므로 `Transport`이다.
    REST API 커넥터는 reqwest가 연결 오류(`is_connect`)나 요청 생성 오류(`is_builder`)로 분류하지 않은 전송 오류와, 응답 본문 읽기 오류에 쓴다.
    쓰기 도구(`WRITE_TOOLS`: `post_message`, `add_comment`, `create_page`)가 HTTP 2xx와 JSON이 아닌 본문을 받을 때도 쓴다.
    서버가 요청을 받았으므로 쓰기가 끝났을 수 있다. 읽기 도구가 같은 답을 받으면 전처럼 `Transport`("the response is not JSON")이다.
    문구로 분류하지 않는다. `Failed`(서버의 거절, HTTP 4xx·5xx), `Transport`, `UnknownTool`은 전처럼 `failed`이다.
    읽기 도구의 오류는 모든 종류가 전처럼 자료로 LLM에 간다. 창은 승인 결과가 `ExecutionUnknown`이면 오류 줄에 "the outcome of the call is unknown"을 보인다.
  - inbox 열 `autonomous`는 정책이 소유자 없이 승인한 항목이다(처음부터 `approved`로 생긴 항목).
    이 열이 없던 DB의 `approved` 항목은 자율 항목으로 본다. 그래서 정책이 소유자 없이 허용할 때만 다시 실행되고, 아니면 소유자에게 다시 묻는다.
  - `execute`는 도구를 부르기 전에 감사 `execution_started`를 남긴다. 실행의 ID는 inbox ID이다.
  - **`resolve_unknown(id, done|retry|drop)`**(`POST .../resolve-unknown`, `bunsind resolve-unknown`, 창의 버튼): `unknown` 항목만 받는다.
    done은 `executed`, drop은 `failed`로 옮긴다. retry는 항목을 `failed`로 옮기고 `pending` 사본(`retry_of`)을 한 transaction으로 만든 뒤,
    그 사본을 `approve_item`으로 실행한다. 그래서 새 inbox ID로 한 번 실행되고, 정책을 다시 본다.
    정책이 거부하면 사본은 `pending`으로 남고, 오류가 사본의 번호를 알려 준다. 감사는 `unknown_resolved`이다.
  - 처음 설계는 `updated_at < started_at`으로 시작 전 항목을 골랐다. 시계가 초 단위라서 같은 초의 항목을 놓치므로, 1단계에서 ID를 적어 두는 방식으로 바꿨다.
- **리스너 셋.** 리스너마다 router가 따로 있다.
  - 제어(`--listen`): `/v1/...`만 있다. 접속 token을 받는다.
  - 내부(`--internal-listen`, `--internal-url`)와 공개(`--public-listen`, `--public-url`): `/v1`이 없다(404).
    인바운드 MCP와 server card, ARD catalog(7절), webhook(11절)이 여기에 있다.
  - 리스너 L은 `InboundConfig::served_on(L)`인 프로필만 제공한다: endpoint가 켜져 있고, L이 내부이거나 범위가 `public`이다.
    MCP 경로, card, ARD catalog가 이 함수 하나를 쓴다(불변식: `ard.json`은 L이 endpoint를 제공하는 프로필만 싣는다). daemon의 `served_on(config, Reach)`는 이 함수를 부른다.
  - `--*-url`은 호출자가 쓰는 주소이다. server card의 URL과 Host 검사가 이 주소에서 나온다.
    `https://`만 받고, `http://`는 host가 loopback일 때만 받는다. 사용자 이름, query, fragment는 거부한다.
  - 리스너마다 제어 API의 TLS 규칙(loopback이 아니면 TLS)을 적용한다. 같은 주소를 두 리스너가 쓸 수 없다(포트 0은 예외).
    이 검사(`check_listeners`)는 데이터 폴더의 잠금과 키 읽기 전에 한다.
  - 기각: 리스너 하나를 Host로 나누기. 프록시 설정 하나로 경계가 사라진다.
  - SIGTERM이면 모든 리스너가 함께 새 요청을 멈추고, 실행 중인 요청을 기다린다(`Started::stop`).
- 기각한 안: GPUI 바이너리 하나에 엔진을 넣는 구조. 창을 닫으면 agent가 멈추고, Linux 서버에 디스플레이가 필요하다.

## 2. Crate

| Crate | 역할 | 의존 |
|---|---|---|
| `bunsin-core` | 정책, 제안 검증, 승인 상태 머신, AI 표시, 도구 pin 비교, 설정 형식, `LlmClient`·`Connector`·`Coder` trait. IO가 없다. | 없음 |
| `bunsin-store` | SQLite 저장소, 산출물 파일, 감사 로그, 봉인된 비밀 값의 행 | core |
| `bunsin-llm` | LLM 어댑터 | core |
| `bunsin-connectors` | MCP 커넥터, REST API 커넥터, webhook 서명 검사와 해석 | core |
| `bunsin-ard` | ARD 항목 검사, catalog, registry 검색 client | 없음 |
| `bunsin-coding` | 코딩 위임 실행기(git worktree, 코딩 CLI 백엔드) | 없음 |
| `bunsin-api` | `EngineApi` trait과 DTO, HTTP 경로와 요청 본문(`http` 모듈) | core |
| `bunsin-engine` | 하네스 파이프라인, `EngineApi`의 구현, 비밀 값 봉인(`secrets.rs`). LLM, 커넥터, 코딩 실행기는 `Backends` trait으로 받는다. | api, ard, core, store |
| `bunsin-client` | `RemoteEngine`: HTTPS로 서버에 접속하는 `EngineApi` 구현 | api, core |
| `bunsin-daemon` | HTTP 서버와 리스너, 프로필 설정으로 실제 LLM client, 커넥터, ARD 검색, 코딩 실행기를 만드는 `RealBackends`, 실행 파일 `bunsind` | api, ard, client, coding, connectors, core, engine, llm, store |
| `bunsin-app` | GPUI 창 | api, client, core |
| `bunsin-testkit` | 테스트용 가짜 LLM, 가짜 커넥터 | core |

- 의존 열은 workspace crate만 적는다. 일반 의존이고, dev 의존은 뺐다 (검증: crate마다 `cargo tree -p <crate> --depth 1 -e normal`, 2026-11-17).

- `bunsin-core`는 tokio, reqwest, rusqlite, gpui에 의존하지 않는다. 시각은 호출자가 넣는다.
- `bunsin-daemon`의 의존성 트리에 gpui가 없어야 한다. CI가 이를 검사한다.
- `bunsin-app`은 엔진, 저장소, rmcp에 의존하지 않는다 (검증: `cargo tree`).

## 3. GPUI

- 의존성은 `gpui-kit = "=0.9.1"`이다. 이 crate는 `gpui-pre =0.3.12`와 `gpui-component`를 묶는다 (검증: Cargo.toml).
- 공식 `gpui` crate는 0.2.2에서 멈춰 있어 쓰지 않는다 (검증: crates.io).
- rustc 1.96에서는 `gpui-pre-macos` 0.3.12가 빌드에 실패한다. rustc 1.98.1에서는 빌드된다 (검증: /tmp 빌드).
  그래서 `rust-toolchain.toml`이 1.98.1을 고정한다.
- 위험: `gpui-pre`와 `gpui-kit`의 crates.io 소유자는 개인 1명이다 (검증: crates.io).
  대응: 버전을 `=`로 고정하고 `Cargo.lock`을 커밋한다. UI 코드는 `bunsin-app`에만 둔다.
- UI와 엔진은 `tokio::sync` 채널로만 통신한다. GPUI 실행기에서 tokio 타이머를 poll하면 panic이 난다 (검증: 테스트).
- 창은 엔진 상태의 사본을 스스로 바꾸지 않는다. 창은 명령을 보내고, 엔진이 돌려준 상태만 표시한다.
- 승인 화면은 도구 호출의 모든 인자를 보여 준다. 사용자가 실행될 내용 전체를 보고 승인하기 위해서이다.
- headless UI 테스트는 `cx.executor().allow_parking()`을 먼저 호출한다. 엔진이 tokio 스레드에서 창을 깨우기 때문이다.
- app은 처음 실행하면 접속 화면을 보여 준다. 사용자가 서버 주소, 접속 token, 선택 사항인 CA 인증서 경로를 넣는다.
  app은 `list_profiles`를 한 번 호출해 주소와 token을 확인한 뒤에 저장한다.
- 서버 주소와 CA 경로는 `connection.json`에 저장한다(macOS: `~/Library/Application Support/Bunsin/`).
  접속 token은 macOS Keychain에 저장한다. token은 JSON 파일, 오류 문구, 접근성 label에 들어가지 않는다.
- token 입력칸은 가려져 있고, 값을 보여 주는 토글이 없다.
- **기존 Keychain 항목에서 token 복사(LLM, 주소 커넥터).** 창이 Mac의 로그인 Keychain에서 서비스와 계정으로 항목을 찾아
  가려진 token 칸에 넣는다. Save를 눌러야 서버로 간다. 복사이지 연결이 아니다: 이후 Keychain의 변경은 서버에 가지 않는다.
  - Keychain 접근은 `KeychainLookup` trait 뒤에 있다. 실제 구현(`MacKeychain`)은 `main.rs`만 설치한다.
    그 외의 창과 모든 테스트는 아무것도 읽지 않는 `NoKeychain`을 쓴다.
  - 오류 문구는 직접 쓴다. `keyring`의 일부 오류는 암호의 바이트를 담고 있어서, 오류를 그대로 출력하지 않는다.
  - 읽기는 창의 주 스레드에서 실행된다. macOS가 접근 허용을 묻는 동안 창은 반응하지 않는다.
  - 읽기가 실패하면 token 칸을 비운다. 앞서 읽은 token이 "읽지 못했다"는 문구 옆에 남아 저장되지 않게 하기 위해서이다.
  - 로그인 Keychain의 generic password(응용 프로그램 암호)만 찾는다. 인터넷 암호와 iCloud Keychain 항목은 찾지 못한다.
  - 대상은 LLM token과 주소 커넥터의 token이다. Keychain 입력란은 한 벌이고, 한 번에 한 token 칸에만 열린다.
    읽은 값은 그때 열린 칸에만 들어간다. 실패하면 그 칸을 비운다.
  - 그 칸의 저장이 성공하거나, 커넥터 입력란이 다른 커넥터로 바뀌거나, 프로필이 바뀌면 입력란을 닫고 비운다.
  - 명령 커넥터의 비밀 변수 값에는 이 기능이 없다. 실제 Keychain으로는 확인하지 않았다.
- **모양.** 색과 공통 요소는 `theme.rs`에 있다. 라이트와 다크 두 팔레트가 있고, 창은 시스템 설정을 따른다.
  강조색은 서명 잉크의 파랑이다(승인은 서명이다). 왼쪽 열에 화면 메뉴, 프로필, 접속 상태가 있고,
  모든 화면 위에 agent 상태와 정지 스위치가 있다. Inbox는 승인 카드 목록과 활동 기록(감사 로그)을 나란히 둔다.
  - 승인 카드는 호출의 인자를 한 줄에 하나씩 보여 준다. 긴 값은 줄바꿈하고, 자르거나 높이를 제한하지 않는다.
- **승인 전 초안 편집**(`draft_editor.rs`): 대기 중인 도구 호출의 카드에 "Edit the text" 버튼이 있다.
  - 편집 대상은 그 시점 권한 규칙의 `outgoing_text`가 가리키는 자리뿐이다. `*`는 목록의 항목마다 입력란 하나가 된다
    (`bunsin_core::marker::text_places`). 규칙이 글을 보내지 않거나 자리가 호출에 없으면 버튼이 없다.
  - 입력란은 AI 표시를 뺀 글로 시작한다(`strip_marker`). 표시는 입력란 밖의 고정된 글로 보인다.
    표시 문구는 `permissions` 응답의 `ai_marker`에서 온다. 엔진은 수정 뒤에 표시를 다시 붙인다(8절).
  - 편집 중에도 카드는 모든 인자를 그대로 보여 준다. 승인은 불러온 인자에서 글의 자리만 바꾼 `Edit`를 보낸다.
  - 글을 바꾸지 않으면 `Edit` 없이 승인한다. 교정 기록이 생기지 않는다. 빈 글은 보내지 않는다.
  - 다른 프로필을 고르거나, 항목이 대기 상태가 아니거나, 서버의 인자가 바뀌면 편집을 닫는다.
  - 테스트는 접근성 label을 읽고, 그림은 읽지 않는다. 그래서 label과 그림을 같은 값에서 만든다(`labelled_as`).
  - 실제 앱은 아이콘 자원을 등록한다(`with_assets`). 등록하지 않으면 체크 표시 같은 아이콘이 그려지지 않는다.
  - 그림 확인: `cargo test -p bunsin-app --test screens -- <폴더>`가 창을 띄우지 않고 각 화면을 두 모드의 PNG로 그린다.
    이 방법으로는 hover, focus 이동, 제목 표시줄, 시스템 모드 변경을 따르는 동작을 확인할 수 없다.
- 창은 Inbox, Notes, Permissions, Agent, Learning, Settings 여섯 화면과 Setup 중 하나를 보여 준다. 다른 화면이 승인함의 자리를 좁히지 않게 하기 위해서이다.
  승인함 테스트는 1280x800 창에서 실행한다.
  - Inbox: 상태, 정지 스위치, 승인함, 감사 로그. `unknown` 항목은 done, drop, retry 버튼과 "두 번 보낼 수 있다"는 안내를 보인다.
    retry는 같은 항목에서 두 번 눌러야 실행한다. 첫 클릭은 버튼을 "Yes, run it again"으로 바꾼다. 다른 항목의 retry를 누르면 그 항목이 대신 묻는 상태가 된다.
    정책이 소유자 없이 실행한 항목(`executed`, `failed`)에는 "Report a problem" 버튼이 있다(`report_autonomous`). 도구가 보류 중이면 버튼을 감춘다.
  - Notes: 페이지 목록과 한 페이지의 열람, 편집, 생성, 삭제, 오른쪽 열의 미리 읽기와 점검 결과(15절).
  - Permissions: 승인된 도구마다 권한 규칙 하나. 아래 "Permissions 화면" 항목.
  - Agent: 소유자의 원칙과 poll, 호출자 token(9절), 인바운드 endpoint와 ARD 등록, webhook, ARD registry. 아래 "Agent 화면" 항목과 "부분 저장 카드" 항목.
  - Learning: 교정 기록, 원칙 후보, 재현 평가, 자율 제안 기준. 아래 "Learning 화면" 항목.
  - Settings: 연결 설정(LLM endpoint, 커넥터, token, 연결 확인, 도구 승인), 개인정보 설정, notes 설정, 평가 설정.
  - Setup: 새 agent의 구성 마법사. 아래 "Setup 화면" 항목.
- 설정 화면은 전용 API만 쓴다(`connections`, `set_llm`, `set_connector`, `set_command_connector`, `set_api_connector`, `remove_connector`,
  `privacy`, `set_privacy`, 그리고 아래 "부분 저장 카드"의 `notes_settings`, `eval_settings`와 그 저장).
  app은 설정 전체를 읽거나 쓰지 않는다.
- 설정 화면의 입력 내용은 사용자의 초안이다. 5초마다의 새로고침은 초안을 덮어쓰지 않는다. 프로필을 바꾸면 초안을 버린다.
- token 입력칸은 가려져 있다. token은 입력칸에서 명령으로만 이동하고, 화면 상태나 label에 들어가지 않는다. 저장에 성공하면 입력칸을 비운다.
- **Agent 화면의 원칙**은 전용 API `principles`와 `set_principles`(목록 전체를 바꾼다)를 쓴다.
  - 소유자는 원칙을 더하고, 고치고, 위아래로 옮기고, 지운다. Save를 눌러야 서버로 간다. 새로고침은 초안을 덮어쓰지 않는다.
  - 저장은 읽은 목록을 `Expected`로 보낸다. 그 사이에 목록이 바뀌었으면 충돌(412)이고, 화면은 "서버 것 불러오기"와 "그래도 저장"을 보여 준다.
  - 한도는 서버가 검사한다: 최대 50개, 하나는 앞뒤 공백을 뺀 1자에서 1,000자. 오류는 원칙을 번호로 가리킨다. 서버는 앞뒤 공백을 지우고 저장한다.
    한도는 이 호출에만 있다. `config-set`으로 넣은 설정은 검사하지 않는다.
  - 원칙은 소유자의 지시이므로 개인정보 필터를 거치지 않는다(12절). 프롬프트에도 거르지 않고 들어간다.
  - 감사 로그 `principles_changed`는 개수만 남긴다(이전, 이후, 더한 것, 지운 것). 글은 남기지 않는다.
  - 원칙 하나를 고치는 동안에는 목록의 순서와 삭제 버튼을 감춘다. 고치는 원칙의 번호가 바뀌지 않게 하기 위해서이다.
- **Agent 화면의 poll**은 전용 API `polls`와 `set_polls`(목록 전체를 바꾼다), 기존 `test_poll`을 쓴다.
  - 입력란: 커넥터, 도구, 간격(초), 인자(JSON 객체), mapping의 dot 경로(items, id, text, author, thread), 건너뛸 작성자(한 줄에 하나).
    창은 숫자와 JSON만 검사한다. 나머지는 서버가 검사하고, 창은 서버의 거부 이유를 그대로 보여 준다.
  - 저장은 `check_config`(간격 30초 이상, mapping 경로, `read` 규칙)를 지나고, 이어서 도구가 승인된 목록에 있는지 검사한다.
    승인 검사는 이 호출에만 있다. `config-set`과 `set_permissions`는 그대로이고, 승인되지 않은 도구는 실행할 때 실패한다.
  - 충돌 검사(`Expected`)와 "그래도 저장"은 원칙과 같다. 감사 로그 `polls_changed`는 개수만 남긴다. 검색 인자에 사람 이름이 있을 수 있기 때문이다.
  - 각 poll은 snapshot의 마지막 실행 결과를 보여 준다. 초안이 서버 목록과 다르면 보여 주지 않는다. 결과는 서버 목록의 순서를 따르기 때문이다.
  - Test는 저장된 poll만 실행한다. 바뀐 초안에서는 "먼저 저장하라"고 알린다. 결과는 개인정보 필터를 거친 이벤트이다(11절의 `poll-test`와 같다).
- **부분 저장 카드**(`publishing_form.rs`, `checks_form.rs`, 공통 `part_form.rs`): 설정 파일이나 명령줄로만 바꾸던 설정을 창에서 바꾼다.
  - Agent 화면: "Inbound endpoint and ARD listing"(`inbound` 전체), "Webhooks"(`webhooks`와 서명 secret), "ARD registries"(`ard_registries`).
    Settings 화면: "Notes"(`notes`: `auto_apply`, `stale_days`, `lint_every_hours`, `lint_conflicts`), "Evaluation"(`eval`: `pass`, `judge`).
  - 전용 API 쌍: `inbound_settings`/`set_inbound_settings`, `ard_registries`/`set_ard_registries`, `webhooks`/`set_webhooks`,
    `notes_settings`/`set_notes_settings`, `eval_settings`/`set_eval_settings`(`POST .../<이름의 kebab case>`).
    저장은 엔진의 `replace_part`(`crates/engine/src/settings_parts.rs`) 하나를 지난다.
  - `replace_part`는 그 부분만 `Expected`와 비교하고, 바꾼 설정 전체에 `set_profile_config`와 같은 단계를 적용한다:
    답 도구 규칙 추가(`with_reply_rule`), 서버 규칙(`Backends::validate`: ARD publisher 도메인, registry 주소), `check_config`,
    읽은 그대로일 때만 저장, 지운 webhook의 secret 삭제(`prune_secrets`). registry 저장은 runtime을 버린다. 검색 client가 registry 목록으로 만들어지기 때문이다.
  - 감사: `inbound_settings_changed`(on/off, scope, 등록 여부만. 제목과 설명 글은 남기지 않는다), `ard_registries_changed`와 `webhooks_changed`(이름),
    `notes_settings_changed`와 `eval_settings_changed`(새 값).
  - bridge는 새로고침마다 `inbound_status`와 다섯 getter를 더 읽는다. 저장 결과는 `UiState::part_result`(`Part`, 결과) 하나와 `part_saves` 하나로 센다.
  - 카드는 원칙 카드처럼 초안을 지킨다. 바꾸지 않은 카드만 서버 값을 다시 읽는다. 충돌이면 "서버 것 불러오기"와 "그래도 저장"을 보여 준다.
  - 인바운드 카드는 `inbound_status`의 listener별 주소, 등록 identifier, 등록한 catalog, catalog가 빼는 이유(`refused`), 경고(`warnings`)를 양식 위에 보인다.
    입력란은 글을 다듬지 않는다. 그래서 창이 쓴 설정은 다시 읽어도 "Not saved"가 되지 않는다. 질문은 한 줄에 하나이고 빈 줄은 버린다.
  - webhook 카드는 서버에 있는 webhook에만 "Store the signing secret"을 보인다. secret은 가린 입력란에서 `UiCommand::SetWebhookSecret`으로만 간다.
    그 값은 `Typed`에 담겨 `Debug`에 나오지 않고, `UiState`와 label에 들어가지 않는다. 저장되면 입력란을 닫고 비운다.
  - Notes 카드는 자동 적용이 하지 않는 일을 적는다: 미리 읽기, 인바운드 질문, webhook 이벤트의 변경은 적용하지 않고, 사용자가 쓴 페이지를 덮어쓰지 않는다(15절, `notes.rs`의 `may_apply`).
  - 배치: 새 카드는 각 화면의 기존 카드 뒤에 온다. 그래서 기존 카드의 자리와 1280x800 승인함 테스트는 그대로이다.
    화면 그림 도구는 스크롤을 고정 픽셀 대신 요소 위치로 맞춘다(`bring`). 페이지가 길어지면 고정 픽셀 스크롤이 끝에서 멈추지 않기 때문이다.
  - 명령은 더하지 않았다. 명령줄 사용자는 `config-show`/`config-set`과 `webhook-secret`, `inbound`를 쓴다.
  - 테스트: `crates/app/tests/publishing.rs`, `checks.rs`(창), daemon `tests/server.rs`(HTTP, 권한, 충돌), `ard_search.rs`(다음 검색이 새 registry로 감), `webhooks.rs`(지운 webhook의 secret 삭제).
    그림: `docs/screens/agent-publishing-light.png`, `agent-webhooks-light.png`, `settings-checks-light.png`.
- **Permissions 화면**은 전용 API `permissions`(승인된 도구와 권한 표를 읽는다. 어디에도 접속하지 않는다)와
  `set_permissions`(표 전체를 바꾼다)를 쓴다.
  - 저장은 설정 파일과 같은 검증을 지난다. 그래서 poll이 쓰는 read 규칙을 지우면 거부된다.
    읽은 뒤에 표가 바뀌었으면 충돌(412)이고, 화면은 초안을 지키고 "서버 것 불러오기"와 "그래도 저장"을 보여 준다.
  - 감사 로그 `permissions_changed`가 바뀐 도구마다 이전 규칙과 새 규칙을 남긴다.
  - 화면은 도구의 이름이나 설명으로 class를 미리 고르지 않는다. 설명은 MCP 서버가 준 데이터이다.
  - write/delete 규칙은 소유자가 글 인자를 고르거나 "글을 보내지 않음"을 고르기 전에는 저장되지 않는다.
    AI 표시 없는 쓰기가 기본값으로 생기지 않게 하기 위해서이다.
  - class를 바꾸면 단계는 `approve`로 돌아간다. delete에는 `autonomous` 버튼이 없다(저장된 규칙에 있으면 보여 준다).
  - 버튼은 입력 schema의 최상위 string 인자만 보여 준다. 인자 버튼을 누르면 그 인자가 더해지거나 빠진다(8절).
    마지막 인자를 빼면 선택이 없는 상태가 된다. "글을 보내지 않음"이 되지 않는다.
  - schema에 없는 인자나 다른 값 안의 자리를 가리키는 저장된 규칙은 그대로 보여 주고, 저장할 때 유지하고, 선택지에 남긴다.
  - 승인된 도구가 없는 규칙은 목록 맨 위에 보여 주고, 지울 수 있게 한다.
  - Save와 바뀐 규칙 목록은 스크롤되는 목록 밖에 있다. 목록의 첫 줄은 바뀐 규칙의 개수이다. 도구가 30개여도 1280x800 창에서 Save를 누를 수 있다.
  - **기다리는 항목**: 승인 시점에 엔진이 그때의 규칙으로 AI 표시를 다시 붙인다.
    규칙이 가리키는 자리가 호출에 없으면 승인은 거부된다.
  - **지표, 제안, 보류**: 화면은 `autonomy` API도 읽는다. write·delete 도구마다 개수 한 줄을 보여 준다(4절의 "결정 기록과 보류").
    제안된 도구에는 "Suggested: Autonomous" 표시가 붙는다. 표시는 단계를 바꾸지 않는다. 소유자가 단계를 고르고 Save를 눌러야 한다.
    보류된 도구에는 안내와 "Clear the hold" 버튼이 있다. 버튼은 `clear_autonomy_hold`만 부르고 규칙을 저장하지 않는다.
    목록 위의 한 줄이 기준을 알려 준다. 기준이 없으면 제안이 꺼져 있다고 알린다. 이 줄을 한 줄로 둔 이유: 1280x800 창에서 기존 클릭 테스트의 버튼이 화면 밖으로 밀렸다.
- **Learning 화면**(`learning_form.rs`)은 카드 네 개를 감싸서 함께 스크롤한다. bridge는 새로고침마다 `corrections`, `principle_candidates`,
  `replay_cases`, `eval_report`를 더 읽는다.
  - 교정: 최신순 목록과 "Retire"(`retire_correction`). 폐기한 것은 표시만 남는다.
  - 원칙 후보: "Find in my corrections"와 "Find in my replay cases"가 `extract_principles`를 부른다. 결과 개수나 거부 이유는 카드 안에 보인다.
    `read` 출처는 도구와 인자 입력이 필요하므로 Learning 화면에는 없다. 명령(`bunsind extract-principles --from read`)과 Setup 화면의 5단계가 쓴다.
    `new` 후보마다 "Adopt", "Edit, then adopt"(입력란에 글을 넣고 "Adopt the edit"), "Dismiss". 채택은 창이 가진 원칙 목록을 `expected`로 보낸다.
    고치는 후보가 서버에서 `new`가 아니게 되면 입력란을 닫는다.
  - 평가: 사례 목록과 Delete, "Run the evaluation"(`start_eval`), run 상태, 비율, 통과 기준, 사례마다 한 줄(심판 판정 포함). 사례 추가는 명령(`add-replay-cases`)으로 한다.
    Learning 화면에는 사례 입력란을 두지 않았다. 지난 메시지를 여러 개 한 번에 넣는 일이 대부분이기 때문이다.
    Setup 화면의 6단계에는 JSON 배열 입력란이 있다(`add_replay_cases`, 명령과 같은 형식).
  - 자율 제안 기준: 전용 API `autonomy_thresholds`와 `set_autonomy_thresholds`(`POST .../autonomy-thresholds`, `.../set-autonomy-thresholds`).
    저장은 `check_config`(같은 `AutonomyThresholds::check`)를 지나고, 읽은 기준을 `Expected`로 보낸다. 다르면 412이고 화면은 "서버 것 불러오기"와 "그래도 저장"을 보여 준다.
    입력란의 기준값은 이미 읽는 `autonomy` 응답의 `thresholds`에서 온다(같은 값이므로 호출을 하나 줄였다). 세 칸이 비면 `None`, 모두 정수면 기준이다. 일부만 채우면 창이 보내지 않는다.
    감사 `autonomy_thresholds_changed`는 새 기준을 남긴다. 명령은 따로 두지 않았다. `config-set`이 같은 일을 한다.
- **Setup 화면**(`setup_form.rs`, PLAN 5절과 6절의 4단계): 일곱 단계(LLM endpoint, 연결, 도구와 규칙, 업무지식, 원칙 후보, 재현 평가, 마무리).
  - 새 API는 없다. 각 단계는 다른 화면의 카드를 그대로 그린다(`llm_card`, `connectors_card`, `permissions_section`, `preload_card`,
    `candidates_card`, `eval_card`). 더한 명령은 bridge의 `AddReplayCases` 하나이고, 기존 `add_replay_cases`를 부른다.
  - 서버에 자기 상태를 저장하지 않는다. 창이 열려 있는 동안 프로필마다 단계를 기억한다.
    새 창은 서버가 결과를 보여 주지 않는 첫 단계에서 시작한다(`first_open_step`: endpoint, 커넥터, 모든 커넥터의 승인 목록과 규칙, notes, 원칙이나 새 후보, 평가 run).
  - 창은 프로필마다 한 번, LLM endpoint나 커넥터가 없고 승인한 도구도 없는 프로필(`needs_setup`)이면 setup을 스스로 연다.
    승인한 도구가 있는 프로필은 설정 파일 같은 다른 길로 구성된 것으로 보고 열지 않는다. Agent 화면 머리줄의 "Set up the agent"가 다시 연다.
  - 1단계의 Next는 이 창에서 시험(`test_llm`)이 성공한 endpoint가 서버의 endpoint와 같을 때만 넘어간다. 모든 단계에 Skip이 있다.
  - 3단계는 커넥터마다 `connector_tools`의 도구 이름과 설명(데이터)을 보여 주고 `approve_tools`로 승인한다. 규칙은 Permissions 화면의 초안과 같다.
    Setup 화면에서 단계 버튼은 Observe와 Approve만 보인다(저장된 규칙의 단계는 보인다). 그래서 setup은 Autonomous를 정하지 않는다.
  - 7단계는 요약을 보여 준다. read가 아닌 규칙이 모두 Observe나 Approve일 때만 Finish가 끝낸다.
    "Set them to Approve"는 나머지를 Approve로 내리는 `set_permissions`이다(읽은 표를 `Expected`로 보낸다).
  - 원칙 후보는 교정 기록, 재현 사례, 읽기에서 온다. 새 프로필에는 앞의 둘이 없으므로 5단계는 읽기 출처를 먼저 둔다.
    PLAN 순서에서 평가(6단계)가 원칙(5단계)보다 뒤라서, 6단계에서 더한 사례로 후보를 찾으려면 Back으로 돌아간다.
  - 테스트: `crates/app/tests/setup.rs`(가짜 LLM과 커넥터로 일곱 단계 전체, 떠났다가 이어 하기, Autonomous 규칙이 있으면 끝나지 않음).
    그림: `docs/screens/setup-*-light.png`.
  - 명령줄 마법사(`bunsind setup`)는 만들지 않았다. 단계마다 입력을 묻는 대화형 명령이 되어 작은 명령 순서가 아니기 때문이다. README에 명령 순서를 적었다.
- **명령줄의 권한 규칙**: `bunsind rules`, `rule-set`, `rule-remove`는 같은 API(`permissions`, `set_permissions`)를 쓴다.
  - 명령은 표를 읽고 규칙 하나만 바꾼 뒤, 읽은 표를 `Expected`로 함께 보낸다. 그 사이에 표가 바뀌었으면 충돌로 거부된다.
    "그래도 저장"은 없다. 명령을 다시 실행하면 새 표에서 시작한다.
  - write/delete 규칙은 `--text`나 `--no-text`가 없으면 서버에 묻기 전에 거부된다. 창과 같은 규칙이다.
  - `--text`가 최상위 인자 하나이면 `arg` 형식, 그 밖에는 `paths` 형식으로 저장한다(8절). 창이 다른 값 안의 자리를 만들 수 없으므로 이 명령이 그 경로이다.
  - delete에 `autonomous`는 거부한다. 창도 그 버튼을 주지 않는다. 승인되지 않은 도구의 규칙도 저장된다(`rules`가 표시한다).
- 한계: 창의 모든 설정 저장은 읽은 값을 `Expected`로 보낸다(14절). 비교하는 값에 token은 없다.
  그래서 다른 client가 token만 바꾼 뒤의 저장은 충돌로 알리지 않는다.
- Disconnect는 Keychain의 token을 지우고 접속 화면으로 돌아간다.
- 개발할 때는 환경 변수 `BUNSIN_SERVER`와 `BUNSIN_TOKEN`이 저장된 접속보다 우선한다.
- 저장된 접속으로 시작했는데 서버가 응답하지 않으면 app은 접속 화면에 오류를 보여 준다. 저장된 token은 그대로 둔다.
  token 칸을 비우고 Connect를 누르면 app은 저장된 token을 쓴다. token은 발급할 때 한 번만 보이기 때문이다.
- 창은 5초마다 서버에서 상태를 다시 읽는다. 그래서 창을 열어 둔 동안 생긴 승인 항목이 클릭 없이 보인다.
  서버가 응답하지 않으면 창은 이전 상태를 지우고 "Not connected to the server"를 표시한다.
- Keychain 구현(`KeychainTokenStore`)은 테스트하지 않았다. 테스트는 메모리 저장소를 쓴다.
- 실제 창의 표시와 Linux 빌드는 확인하지 않았다.

## 4. 저장소

- SQLite는 `rusqlite`의 `bundled` 기능으로 쓴다 (검증: 빌드).
- 프로필마다 DB 파일을 따로 둔다: `profiles/<id>/agent.db`. 공용 `app.db`는 프로필 목록과 접속 token의 해시만 가진다.
  기각한 안: DB 하나에 `profile_id` 컬럼. `WHERE` 조건 하나를 빠뜨리면 다른 프로필의 데이터가 샌다.
- 산출물은 `profiles/<id>/artifacts/sha256/<해시>` 파일로 저장하고, DB에 메타 행을 둔다.
- 감사 로그 테이블은 트리거로 UPDATE와 DELETE를 거부한다.
- 업무지식은 notes(15절)에 쌓인다.
- **유사 작업 검색(임베딩 없음)**: 교정 기록과 notes를 FTS5 `trigram` 색인으로 찾는다. 번들 SQLite 3.54.1은 FTS5를 켠다.
  - `corrections_fts(task, original, edited, reason)`는 외부 콘텐츠 표이다. `corrections`의 `id`로 글을 읽는다.
    교정은 고치거나 지우지 않으므로 INSERT 트리거 하나로 맞춘다. 폐기는 색인하지 않는 열 `retired_at`만 바꾼다.
  - `wiki_fts(slug UNINDEXED, title, summary, body)`는 글을 직접 담는다. 외부 콘텐츠 표는 정수 키가 필요한데 `wiki_pages`의 키는 slug이기 때문이다.
    INSERT, UPDATE(제목, 요약, 본문), DELETE 트리거가 slug로 맞춘다. UPSERT는 UPDATE 트리거를 탄다.
  - 색인이 없는 DB(옛 DB 포함)를 열면 색인을 만들고 있는 행으로 채운다. 확인과 채우기는 한 transaction이다.
  - 검색어(core `search::query_terms`): `[REDACTED:…]`를 지우고, 영숫자가 아닌 글자로 나누고, 소문자로 바꾼다.
    한글만인 3음절 이상 낱말은 조사 하나를 뗀다(먼저 `에서 으로 에게 까지 부터 처럼`, 다음 `은 는 이 가 을 를 에 의 도 로 와 과 만`).
    3자 이상과 한글 2음절만 남기고, 중복을 빼고 앞의 24개를 쓴다. 영문 2자는 버린다.
  - trigram은 2자 검색어를 찾지 못한다(검증: probe). 그래서 3자 이상은 `MATCH "<t>"`, 한글 2음절은 `LIKE '%t%'`로 찾는다.
    `배포는`은 `배포를`을 찾지 못하지만, 조사를 떼면 찾는다.
  - 점수(core `search::rank`): 일치한 검색어마다 `ln(1+N/df)`를 더한다. N은 검색한 행 수, df는 그 검색어의 행 수이다.
    `df ≤ max(2, N/4)`인 드문 검색어가 일치한 행만 후보이다. 그래서 `the` 같은 흔한 낱말 하나로는 아무것도 찾지 않는다.
  - 같은 점수의 순서: 교정은 같은 source, 다음 최신 id. notes는 최근 수정, 다음 slug.
  - 기각: bm25만 쓰기. `LIKE` 결과와 한 순서로 섞지 못한다. 임베딩. 모델과 저장소가 더 필요하다.
  - 조사 목록, 드문 검색어 기준, 상수 24는 첫 값이다. 실제 데이터로 확인하지 않았다.
- **교정 기록**(PLAN 4.4): 소유자가 대기 중인 호출을 Edit로 고쳐 승인하면 엔진이 한 줄을 저장한다.
  - 열: 도구, source와 task(그 항목의 이벤트, 저장된 `events.payload`에서 읽음), 초안, 수정본, 이유, `retired_at`.
  - 초안과 수정본의 글은 규칙의 글 자리(`text_places`)에 있는 글을 빈 줄로 잇는다. 자리가 없는 규칙이면 인자 JSON이다.
    AI 표시는 떼고 저장한다. 표시가 있으면 표시의 낱말이 모든 교정과 일치하기 때문이다.
  - 초안, 수정본, 이유는 저장하기 전에 개인정보 필터를 거친다. 감사 `edited`와 산출물 `edited_draft`도 걸러진 글을 쓴다.
    task는 수신할 때 이미 걸러졌다. 보내는 호출은 소유자가 쓴 글 그대로이다(12절).
  - 실시간 이벤트의 프롬프트에만 유사 교정을 최대 5개 넣는다. 검색어는 이벤트 본문에서 만든다. 교정은 `<data>` 블록으로 인용한다.
  - **재현에는 교정을 넣지 않는다.** 교정의 수정본이 그 사례의 실제 답이면 점수가 무의미해진다.
    기각: 시각으로 거른 교정을 재현에 넣기. 믿을 수 있는 사례 시각이 없다.
  - `corrections`(`POST .../corrections`, `bunsind corrections`)는 폐기한 것까지 최신순으로 200개를 보여 준다.
    `retire_correction(id)`(`POST .../retire-correction`, `bunsind retire-correction`)는 `retired_at`을 적고 감사 `correction_retired`를 남긴다.
    폐기한 교정은 검색에서 빠지고 N에도 들지 않는다. 다시 폐기하면 거부한다.
  - LLM은 교정을 저장하거나 폐기하지 않는다. 저장은 엔진이, 폐기는 소유자가 한다.
- **결정 기록과 보류(좁은 자율, PLAN 4.7과 6절의 3단계)**: 표 `decisions(id, at, inbox_id, tool, kind)`는 추가 전용이다(트리거가 UPDATE와 DELETE를 거부한다).
  - kind: `approved`(소유자가 고치지 않고 승인), `edited`(고쳐서 승인), `rejected`(대기 중인 호출을 거부), `autonomous`(정책이 소유자 없이 실행),
    `reported`, `demoted`(보류 시작), `hold_cleared`(보류 해제).
  - 적지 않는 것: 초안의 거부(실행되지 않는다), notes 변경안, 넘김 항목. 사본(`retry_of`)의 승인, 수정, 거부도 적지 않는다.
    사본은 앞 항목을 되풀이하고, 앞 항목의 결정이 이미 적혀 있기 때문이다.
  - 보류: 그 도구의 `demoted`와 `hold_cleared` 중 마지막이 `demoted`이다. `demoted`는 보류가 아닐 때만 쓴다. 확인과 쓰기는 한 transaction이다.
  - 보류는 설정 밖에 있다. 그래서 `config-set`이나 낡은 "Save anyway"가 보류를 조용히 풀지 못한다.
  - 내리기: 소유자가 write·`autonomous` 규칙인 도구의 대기 중인 호출을 거부하면 엔진이 `demoted`와 감사 `autonomy_demoted`를 쓴다.
    이미 보류 중이면 `demoted`를 다시 쓰지 않는다. 기각: config의 단계를 고쳐 내리기. 낡은 저장이 조용히 되올린다.
  - 개수(도구별): 마지막 `demoted` 뒤의 `approved`, `edited`, `rejected`, `autonomous`를 센다. 그래서 보류가 개수를 처음부터 다시 센다.
    자율 실행의 실패는 항목이 `failed`이고 사본이 없는 것이다. 사본이 있으면 시작이나 소유자가 호출을 실행하지 않은 것이기 때문이다.
  - 보류의 효과: `Policy::decide(tool, pin, kill, held)`는 보류된 도구의 `Execute`를 `NeedsApproval`로 바꾼다. 낮은 단계, 정지 스위치, pin은 그대로이다.
    엔진의 `decide`가 보류를 읽으므로 새 제안, 승인, 재개가 모두 따른다. 재개에서 자율 항목은 실행되지 않고 `pending` 사본이 된다.
  - 신고: `report_autonomous(inbox_id)`(`POST .../report-autonomous`, `bunsind report-autonomous`)는 정책이 소유자 없이 실행한 도구 호출 중
    `executed`나 `failed`인 항목만 받고, 항목마다 한 번만 받는다. `reported`, 감사 `autonomous_reported`를 쓰고 도구를 보류한다(`cause: reported`).
    기각: 실패만으로 내리기. 아무것도 보내지 않았다.
  - 해제: `clear_autonomy_hold(tool)`(`POST .../clear-autonomy-hold`, `bunsind clear-autonomy-hold`)만 `hold_cleared`를 쓴다. 감사는 `autonomy_hold_cleared`이다.
  - 제안(core `autonomy::suggest`, 순수 함수): 설정 `autonomy: {min_reviewed, min_unchanged_percent, window_days}`가 있고, write 도구이고,
    단계가 `approve`이고, `bunsin.coding/` 도구가 아니고, 보류가 아니고, 최근 `window_days`일의 reviewed가 `min_reviewed` 이상이고,
    unchanged%(내림)가 `min_unchanged_percent` 이상일 때만 참이다. 설정이 없으면(`None`) 제안이 없고 개수는 모든 날을 센다.
    `min_unchanged_percent`가 100을 넘거나 `window_days`가 0이면 저장을 거부한다.
  - 엔진은 단계를 올리지 않는다. 소유자만 `set_permissions`(창, `bunsind rule-set`)로 올린다. delete는 제안하지 않고 자율이 되지 않는다.
  - `autonomy`(`POST .../autonomy`, `bunsind autonomy`): write·delete 규칙과 보류된 도구마다 개수, 보류, 제안을 돌려준다.
- **재현 평가(PLAN 4.7)**: 표 `replay_cases(id, event, actual_reply, expect, created_at)`, `eval_runs(id, state, started_at, finished_at)`,
  `eval_results(run_id, case_id, expect, kind, kind_ok, overlap_permille, draft, error, judge)`.
  - 사례의 이벤트와 실제 답은 저장하기 전에 개인정보 필터를 거친다. `expect`는 `reply`(기본값), `escalate`, `ignore`이다.
  - 재현은 도구를 실행하지 않고, notes와 교정 기록을 받지 않는다(13절). LLM 호출 로그의 purpose는 `replay`이다.
  - 채점은 LLM 없이 한다(core `eval`). 종류: 쓰기 규칙의 도구 호출은 `reply`, 읽기 규칙이나 notes 도구의 호출은 `read`,
    delete 규칙이나 규칙 없는 도구의 호출은 `other_tool`이다. `read`는 불일치로 세고 비율을 따로 낸다.
    겹침: 소문자로 바꾸고 공백을 하나로 줄인 글의 문자 3-gram Dice 계수(천분율). 초안 글은 교정 기록과 같은 `draft_text`(AI 표시 제외)이다.
    `reply` 사례에만 있고, 답이 아닌 초안은 0이다. 경계: `escalate` 사례 중 넘긴 비율. 과잉 넘김: `reply` 사례 중 넘긴 비율.
    비율은 내림한 백분율이고, 셀 사례가 없으면 `None`이다. 실패한 재현(LLM 오류)은 어느 비율에도 들지 않는다.
  - 결과 행은 `expect`를 함께 저장한다. 사례를 지워도 보고서가 바뀌지 않게 하기 위해서이다. 그래서 설계의 `eval_runs.summary` 열은 두지 않고 결과에서 계산한다.
  - 초안 글과 오류 글은 저장하기 전에 필터를 거친다. 오류는 upstream의 응답 본문을 담을 수 있기 때문이다.
  - `start_eval`은 실행 행만 만든다. 실행은 한 번에 하나이고, 사례가 없으면 거부한다. 서버의 poll 작업이 `poll_all()` 뒤에
    `background_tick()`을 부르고, 이것이 프로필마다 실행 중인 run의 사례를 3개씩 재현한다. 남은 사례가 없으면 run을 `done`으로 바꾸고 감사 `eval_finished`를 남긴다.
    진행은 저장된 행에서만 정한다(결과가 없는 사례). 그래서 재시작 뒤에도 이어진다. 실행 중에 더한 사례도 그 run에 든다.
  - 재현은 밖에 아무것도 보내지 않으므로 정지 스위치와 상관없이 진행한다. 기각: 요청 하나에서 전체 실행. 종료 유예 300초를 넘는다.
  - 통과 기준은 소유자가 정한다: 설정 `eval.pass: {kind_percent, overlap_percent, boundary_percent}`(serde 기본값 없음). 100을 넘으면 저장을 거부한다.
    끝난 run은 실패한 사례가 없고 세 비율이 모두 기준 이상일 때만 통과한다. `None`인 비율은 기준을 넘지 못한다(경계 사례가 없는 세트는 통과하지 않는다).
  - API: `add_replay_cases`, `replay_cases`(200개), `delete_replay_case`, `start_eval`, `eval_report`(최신 run). 경로는 `POST .../<kebab-case 이름>`, 명령은 같은 이름의 `bunsind` 명령이다.
    `replay`(사례 하나를 바로 재현)는 결과에 `score`를 더한다.
  - **LLM 심판**(설정 `eval.judge`, 기본 false): `reply` 사례이고 초안의 종류가 `reply`일 때만 사례마다 한 번 묻는다.
    초안과 실제 답(둘 다 필터를 거친 글)을 `<data>`로 인용하고 `{verdict: same|partial|different, reason}`을 받는다. LLM 호출 로그의 purpose는 `judge`이다.
    판정은 `eval_results.judge`(이유는 필터를 거친다)에만 저장하고 보고서에만 보인다. 호출이 실패하거나 답이 형식에 맞지 않으면 판정이 없고, 사례는 실패가 아니다.
    비율과 통과 판정(`EvalSummary::of`)의 입력은 `(Expect, Option<CaseScore>)`이고, 정책과 `suggest`의 입력에도 판정이 없다. 타입으로 막는다.
    테스트: 심판을 켜고 판정이 `same`, `different`, 형식 오류여도 비율, 통과, `autonomy()`, 결정 개수가 같다.
- **원칙 후보(PLAN 5절의 5단계)**: 표 `principle_candidates(id, text, evidence, source, state, created_at, decided_at)`. state는 `new`, `adopted`, `dismissed`이다.
  - `extract_principles(source)`의 출처: `corrections`(폐기하지 않은 교정의 수정본과 이유), `replay_cases`(재현 사례의 실제 답),
    `read{tool, args}`(read 규칙이고 `decide`가 Execute인 도구를 한 번 부른 결과, poll의 `fetch`와 같은 검사). 글은 다시 필터를 거친다. 저장 뒤에 소유자가 단어를 더했을 수 있기 때문이다.
  - 글은 `<data source="owner-text">`로 인용하고 합계 20,000자까지 보낸다. LLM은 `{candidates: [{text, evidence}]}`로 답한다. LLM 호출 로그의 purpose는 `principles`이다.
  - 엔진의 거부 규칙(core `candidates::accept`): 글과 근거를 필터한 뒤, 빈 글, 300자 초과, `[REDACTED:` 포함, 입력 글에 정확히 없는 근거를 버린다.
    이미 있는 후보(모든 state)나 원칙과 같은 글도 버린다. 그래서 기각한 후보는 다시 나오지 않는다. 최대 10개를 `new`로 저장한다.
    근거는 `quote_untrusted`가 바꾼 `&lt;`와 `&amp;`를 되돌린 뒤 비교한다. 감사 `principles_extracted`는 출처 이름과 개수만 남긴다(read의 인자는 사람 이름을 담을 수 있다).
  - LLM 호출이 실패하면 거부(409)이고, 오류 글은 필터를 거친다. 출처에 글이 없으면 LLM을 부르지 않고 거부한다.
  - `adopt_principle{id, text, expected}`: 후보가 `new`이고 `expected`가 지금 원칙과 같아야 한다(다르면 412). 후보를 `adopted`로 먼저 옮긴 뒤(CAS)
    `replace_principles`로 글 하나를 끝에 더한다. 그래서 공백 정리, 50개·1,000자 한도, 감사 `principles_changed`가 같다. 저장이 실패하면 후보를 `new`로 되돌린다.
    `text`는 소유자가 고친 글이다. 원칙은 필터를 거치지 않는다(12절). 감사 `principle_adopted`는 id와 고쳤는지만 남긴다.
  - `dismiss_principle(id)`: `new`만 `dismissed`로 옮긴다. LLM은 채택하지 않는다. 기각: 일괄 채택.
  - API와 명령: `extract-principles`, `principle-candidates`, `adopt-principle`(명령은 원칙을 읽어 `expected`로 보낸다), `dismiss-principle`.
- 비밀 값은 설정에 저장하지 않는다. 9절을 본다.

## 5. LLM 어댑터

- `bunsin-core`가 `CompletionRequest`와 `CompletionResponse`를 정의한다. 하네스는 wire 형식을 모른다.
- `bunsin-llm`의 `LlmClient` trait이 프로필마다 `Arc<dyn LlmClient>`로 쓰인다.
- wire 형식은 프로필 설정값이다: `openai-chat`, `openai-responses`, `anthropic-messages`.
  - `openai-responses`는 `POST {base_url}/responses`에 `instructions`, `input`, `max_output_tokens`, `text.format`을 보낸다.
    필드 이름은 API의 OpenAPI 명세(openai/openai-openapi)에서 가져왔다(2026-11-15에 읽음).
    `store: false`를 보내 endpoint가 응답을 보관하지 않게 한다. HTTP 200으로 오는 `status: failed`는 오류로 처리한다.
  - 세 형식 모두 실제 endpoint로 실행해 보지 않았다. 테스트는 요청을 기록하는 로컬 서버를 쓴다.
- **연결 테스트**(`test_llm`, `llm-test`, 창의 Test): 저장된 endpoint로 요청 1건을 보낸다.
  실제 요청과 같은 응답 schema와 출력 한도를 쓴다. 단순한 요청은 통과하고 실제 요청은 실패하는 endpoint가 있기 때문이다.
  요청에는 프로필의 데이터가 없다. 정지 스위치가 켜져 있어도 실행된다(외부에 아무것도 쓰지 않는다).
  감사 로그에 `llm_tested`를 남긴다. 기다리는 시간은 LLM 설정의 timeout까지이고, 그동안 창의 다른 명령은 대기한다.
  창은 저장되지 않은 입력을 테스트하지 않는다.
- **호출 로그**: 모든 LLM 호출은 `Engine::complete`(`engine/src/llm_log.rs`)를 지난다.
  이 함수가 호출 1건마다 표 `llm_calls`에 1행을 쓴다: 시각, job, purpose, 입력·출력 token, 지연(ms, `Instant`로 잰다), 끝난 방식.
  - job은 이벤트 처리와 재현에서 이벤트의 dedupe key이고, 연결 테스트에서는 없다. 수리 요청도 같은 job의 행이 된다.
  - purpose는 `event`, `replay`, `llm_test`이다.
  - 끝난 방식은 고정된 종류만 담는다: `end`, `max_tokens`, `refusal`, `rate_limited`, `http_error`, `transport_error`, `malformed`.
    오류 문구는 저장하지 않는다. HTTP 오류의 본문은 필터를 지나지 않은 상대의 글이기 때문이다. 요청과 응답의 글도 담지 않는다.
  - `llm_usage`(`POST .../llm-usage`, `bunsind llm-usage`)는 지난 90일의 행을 UTC 날짜와 purpose별로 합친다(SQLite `date(at,'unixepoch')`).
    실패는 `end`가 아닌 호출이다. 프로필은 DB 파일이 나타내므로 표에 프로필 열은 없다.
  - LLM client를 만들지 못해 보내지 않은 요청은 기록하지 않는다.
- `llm-set` 명령과 창의 LLM 화면은 Claude API를 기본값으로 채운다: `https://api.anthropic.com`, `claude-opus-5-5`.
  출처는 Claude 문서의 API overview와 models overview이다(2026-11-15에 읽음). 기본값은 프로필에 미리 저장하지 않는다.
  운영자가 허용 목록을 쓰는 서버에서는 저장된 기본 주소가 다른 설정의 저장까지 막기 때문이다.
- Anthropic wire는 `x-api-key`와 `anthropic-version: 2026-10-21`을 보낸다. 문서는 `Authorization: Bearer`를 기본으로 적고 `x-api-key`를 계속 지원한다고 적는다.
- workspace에 묶이지 않은 key를 위해 설정의 `llm.workspace_id`를 `anthropic-workspace-id` header로 보낸다.
- 새 프로필의 출력 한도(`max_output_tokens`)는 16,000이다. 기본 모델은 thinking을 끌 수 없고, thinking도 출력 토큰을 쓰기 때문이다.
- 행동 제안은 구조화 출력으로 받는다. 도구 호출을 강제할 수 없는 모델이 있기 때문이다.
- LLM은 허용된 도구마다 ID, 사용자가 승인한 설명, 인자 스키마를 인용 데이터로 받는다.
- 제안의 도구 인자는 `args_json` 필드에 JSON 문자열로 담는다. 자유 형식 object는 엄격한 응답 스키마에 넣을 수 없기 때문이다.
- LLM client는 redirect를 따르지 않는다. redirect된 요청에 인증 header가 실릴 수 있기 때문이다.
- 실제 LLM endpoint로는 아직 호출해 보지 않았다. 테스트는 로컬 stub 서버를 쓴다.
- 검증 순서: serde(`deny_unknown_fields`) → 도메인 검사 → 정책. 실패하면 LLM에 수리를 한 번 요청한다.

## 6. 코딩 위임

- `CodingRunner`가 git worktree, 브랜치 `agent/<job>`, diff, 테스트 실행을 맡는다.
- 백엔드는 두 가지만 구현한다: 실행할 명령(`CommandSpec`)과 출력 해석.
- `CommandSpec`은 실행 파일의 절대 경로를 가진다. macOS GUI app의 PATH에는 사용자의 CLI 경로가 없기 때문이다.
- 결과(`CodingResult`)는 브랜치, base/head, diff 요약, 테스트 명령과 출력, 종료 상태, 기록 파일을 가진다.
- 백엔드는 Claude Code(`-p --output-format stream-json --verbose`), Codex(`exec --json -C <작업 폴더> -`), 사용자 지정 명령이다. 지시문은 stdin으로 넘긴다.
- 백엔드 명령에는 권한 확인이나 sandbox를 끄는 플래그가 없다. 두 CLI의 기본 모드는 파일 수정을 막을 수 있다.
  편집을 허용하는 opt-in 설정은 두지 않았고, 실제 CLI로 실행해 보지 않았다. 운영자는 서버에서 CLI 자체의 설정을 바꿀 수 있다.
- **엔진 연결**(core `coding`, engine `coding.rs`):
  - 설정 `coding: Option<{backend, command, timeout_secs, repos: {이름: {path, test_command}}}>`(`serde(default)`, `deny_unknown_fields`).
    프로필은 `claude_code`, `codex`의 프로그램을 정하지 못한다(`program` 필드는 거부된다). `command`는 백엔드 `command`에만 있고 절대 경로로 시작한다.
    저장소 경로는 절대 경로, 시간 한도는 1초에서 24시간, 저장소 이름은 a-z, 0-9, `-`, `_` 40자까지이다.
  - 내장 도구 `bunsin.coding/start {repo, base_ref?, instructions}`. 규칙은 `write`와 `"none"`만 받고, 다른 `bunsin.coding/` 도구의 규칙은 거부한다.
    설정과 규칙이 모두 있을 때만 LLM이 도구를 본다. LLM은 고정 설명과 저장소 이름(스키마의 `enum`)만 보고, 경로, 프로그램, 테스트 명령은 설정에서 온다.
  - pin은 언제나 참이다(명세가 코드에 있다). `Policy::decide`는 delete처럼 coding 도구의 `Autonomous`도 `NeedsApproval`로 둔다. `autonomy::suggest`도 제안하지 않는다.
  - 승인 → `Approved→Executing` CAS → `Backends::coder()`의 `Coder::start(CodingJobSpec)` → `Outcome::Started`. 기본 `Backends::coder()`는 Err이다.
    job ID는 `b<inbox id>-<profile>`(최대 61자)이다. 시작이 실패하면 항목은 `failed`이다.
  - 끝난 작업은 `Coder::finished()`가 돌려주고, `background_tick`이 모은다. 엔진은 필터를 거친 `coding_result`(브랜치, 커밋, diff 요약, 테스트, 종료)와
    `coding_transcript`를 산출물로 저장하고, 항목을 `executed`(코딩 agent가 종료 코드 0, 시간 안)나 `failed`로 옮기고, "브랜치를 검토하고 직접 병합하라"는 Draft를 넣는다.
    `executing`이 아닌 항목의 보고는 무시한다(감사 `coding_report_ignored`).
  - 설계는 coder가 `engine.finish_coding_job`을 부르는 방식이었다. engine에 tokio와 역참조가 들어오므로, coder가 보고를 쌓고 `background_tick`이 가져가는 방식으로 바꿨다. 끝난 뒤 최대 5초 늦게 기록된다.
  - 작업 중에 서버가 멈추면 항목은 `executing`으로 남고, 다음 시작이 `unknown`으로 옮긴다(1절의 재개). 자식 프로세스는 자기 process group이라 남을 수 있다(가정).
  - **서버 쪽**(daemon `coding.rs`): 운영자 플래그 `--allow-coding`과 `--coding-repo-root <dir>`(둘은 서로를 요구한다)가 있어야
    `RealBackends::with_coding`이 `RealCoder`를 만든다. 없으면 `validate`가 `coding` 설정의 저장을 거부하고, `coder()`는 Err이다.
  - 프로그램은 운영자가 정한다: `--coding-program claude-code=<절대 경로>`, `--coding-program codex=<절대 경로>`(반복). 운영자 프로그램이 없는 백엔드는 거부한다.
  - 프로필이 정한 프로그램(백엔드 `command`, `test_command`)은 `--allow-coding-commands`가 있을 때만 받는다. 이 옵션은 `--allow-stdio-connectors`와
    같은 권한이다(9절): 프로필 소유자가 서버 사용자로 아무 프로그램이나 실행하고, 다른 프로필의 DB와 `master.key`를 읽을 수 있다.
  - 저장소는 `canonicalize`(링크를 따라감)한 경로가 root 아래이고 `.git`이 있어야 한다. 위의 모든 검사를 저장할 때(`validate`)와 시작할 때(`start`) 한다.
    그래서 플래그를 빼고 다시 시작한 서버는 전에 저장된 설정의 작업도 시작하지 않는다(항목은 `failed`).
  - 한계: 작업은 서버 사용자로 실행되고 sandbox가 없다. 코딩 CLI 자체의 권한 설정만 적용된다.
  - `RealCoder::start`는 tokio task에서 `CodingRunner`를 돌린다. worktree는 `<data>/coding/<profile>/worktrees`, transcript는 `<data>/coding/<profile>/logs`이다.
    끝나면 transcript 파일을 읽어 `CodingReport`를 쌓고, 엔진의 `background_tick`(5초마다)이 가져간다. 백엔드는 설정의 `backend`로 고른다(`command`는 `Generic`, 인자 그대로).
  - 테스트는 임시 git 저장소와 `/bin/sh` 명령 백엔드로 끝까지 돈다. 실제 Claude Code와 Codex는 실행하지 않았다.
  - 한계: 두 CLI의 기본 모드는 사람이 없으면 편집을 막을 수 있다. 그때 작업은 변경 없이 끝나고, Draft가 "no change"를 보인다. 우회 플래그는 넣지 않는다.
  - 창: 대기 중인 coding 호출의 카드는 "Approve and start the job"과 "Bunsin은 병합하지 않는다"는 안내를, 실행 중인 호출은 "결과는 새 항목으로 온다"는 안내를 보인다.

## 7. 인바운드 호출 인터페이스

- v1은 MCP 서버 카드를 쓴다. A2A는 두 번째 프로토콜 스택이 되므로 미룬다.
- 도구는 둘이다: `ask`는 즉시 `{request_id, pending_approval}`을 돌려주고, `get_request(id)`가 결과를 돌려준다.
- 인바운드 요청은 신뢰하지 않는 데이터로 수신 단계에 들어간다. 승인함을 우회하지 않는다.
  - 질문을 처리하다 나온 쓰기와 삭제는 규칙이 `autonomous`여도 승인을 기다린다. 엔진은 이벤트의 글이 아니라 `intake` 행으로 질문인지 판단한다.
  - 읽기는 규칙대로 실행된다. 답은 답 도구를 거치므로, 읽은 내용은 소유자가 승인한 뒤에만 호출자에게 간다.
- 인증은 프로필별 호출자 bearer token이다(9절). token은 해시로 저장한다. OAuth는 다음 단계이다.
- **설정** `inbound: {enabled(기본 false), scope(internal | public, 기본 internal), title?, description}`(`bunsin_core::inbound`).
  켜져 있으면 description은 1~100자, title은 1~100자이다. server card의 스키마가 100자까지만 받기 때문이다(설계의 500자에서 바꿈).
  `set_profile_config`가 endpoint를 꺼짐에서 켜짐으로 바꾸고 답 규칙이 없으면 `{class: write, tier: approve, outgoing_text: {arg: "text"}}`를 넣는다.
- **intake**(agent.db): `intake(id, channel, delivery_id, caller_id, event, event_key, received_at, attempts, next_try_at, outcome, UNIQUE(channel, delivery_id))`.
  - `ask`는 질문을 필터를 거친 `InboundEvent`(source `bunsin.inbound`, external_id = request ID, author `caller:<label>`, channel outside)로 저장하고 바로 답한다.
    `ingest`는 LLM을 기다리기 때문이다. request ID는 `req_`와 OS 난수 16바이트의 hex이다.
  - 한도: 질문 1~4,000자, 호출자당 시간당 20개, 프로필당 처리하지 않은 질문 50개. 한도는 `intake` 행으로 세므로 재시작 뒤에도 유지된다.
  - `drain_intake`는 서버의 intake task 하나가 부른다(시작할 때, 5초마다, `ask` 직후의 신호). 정지 스위치가 켜진 프로필은 건너뛴다.
    행마다 `ingest_event`를 부른다. `Failed`나 `Err`이면 `60초 × 시도 수` 뒤에 다시 한다(이벤트는 `events`에서 열린 채이다).
    `MAX_EVENT_ATTEMPTS`+1번째 호출은 포기 항목을 넣고 `Failed`를 돌려주므로, 그 뒤 `outcome`을 `failed`로 쓴다. 그 밖의 결과는 `Outcome`의 이름이 `outcome`이 된다.
  - SIGTERM이면 intake task도 폴링 task처럼 진행 중인 주기를 끝내고 멈춘다(`Poller::stop`).
- **요청 상태**(`request_status`, 처음 맞는 규칙): `outcome`이 없으면 `working`. 항목이 failed이면 `failed`.
  답 항목이 executed이면 `answered`(답은 그 항목의 `text`, AI 표시와 소유자의 수정 포함). 항목이 rejected이면 `declined`. 그 밖은 `pending_approval`.
  - 호출자는 소유자의 결정만 본다. agent의 선택(무시, 거부된 호출, 기록만, 포기)은 `pending_approval`로 보인다.
    질문이 그 선택을 agent가 읽은 내용에 따르게 만들면, 호출자가 승인 없이 1비트씩 알아내기 때문이다.
  - 그래서 agent가 답하지 않은 질문(ignored, denied, recorded)에는 넘김 항목이 생긴다. 소유자가 기각하면 `declined`가 된다.
  - 한계: 처리에 걸린 시간은 여전히 약한 신호이다. 상태가 바뀌는 시각은 읽기 횟수에 따라 다르다.
  `get_request`는 `(channel, request ID, caller_id)`로 찾는다. 다른 호출자의 ID와 없는 ID는 같은 오류이다.
- **답 도구 `bunsin.inbound/reply {text}`**
  - source가 `bunsin.inbound`인 이벤트에만 보인다. 다른 이벤트의 제안은 수리 요청으로 돌린다.
  - 언제나 승인: `Policy::decide`가 이 도구의 `autonomous`를 `NeedsApproval`로 낮추고(coding 도구와 같다), `check_config`가 `autonomous` 규칙의 저장을 거부한다.
    이유: 자율 답은 호출자 token을 읽기 도구 전체에 대한 질의 창구로 만든다. 7절 "승인함을 우회하지 않는다"를 지킨다.
  - 규칙은 class `write`, outgoing text `{arg: "text"}`여야 한다. 다른 `bunsin.inbound/*` 도구는 없다.
  - `decide`는 이 도구에 pin 검사를 하지 않는다(명세가 코드에 있다). `execute`는 connector를 부르지 않는다. intake 행이 있는지 보고 항목을 `executed`로 옮긴다(감사 `inbound_answered`).
  - 그래서 AI 표시, 정지 스위치, 수정의 교정 기록, 1회 실행이 다른 쓰기와 같다. 기각: 새 `InboxBody` variant(CLI와 app이 전부 match한다).
- 인바운드 질문에서 나온 notes 변경은 자동 반영하지 않는다(channel outside, source 검사 둘 다).
- **MCP endpoint** `POST /agents/{p}/mcp`(daemon `inbound.rs`): rmcp `StreamableHttpService`, 세션 없음(`NeverSessionManager`, legacy session mode off, JSON 응답).
  - 무상태인 이유: 열린 stream은 폐기된 token보다 오래 살고 종료 대기 300초를 붙잡는다. POST 하나가 응답 하나이고, GET은 405이다.
  - `route_layer`의 미들웨어가 먼저 확인한다: `Bearer` 값이 `caller_tokens`의 유효한 token이고, 그 프로필이 경로의 `{p}`이고, `served_on`이다.
    하나라도 아니면 같은 본문의 401이다. 통과하면 `Caller`를 request extension에 넣고, 도구 handler가 `http::request::Parts`의 extension에서 읽는다.
  - rmcp 설정: `allowed_hosts`는 `--*-url`의 host 하나(다른 Host는 403, DNS rebinding 방어), 본문 64 KiB(넘으면 413).
  - 리스너당 동시 요청 64(semaphore, 넘으면 503). `get_request`는 호출자당 1분에 120번(메모리, 재시작하면 초기화). `ask`의 한도는 위의 intake 행으로 센다.
  - 도구 오류(검사 실패, 한도, 남의 ID)는 MCP tool 오류 결과이다. 내부 오류의 문구는 서버 내용을 담지 않는다.
  - `ask`가 성공하면 `Notify`로 intake task를 깨운다. 그래서 질문은 5초 tick을 기다리지 않는다.
  - 기각: token만으로 프로필 고르기. card와 ARD 항목은 프로필마다 URL이 필요하다.
- **Server card** `GET /agents/{p}/mcp/server-card`: MCP server card 확장(snapshot `526201bb`)이 권하는 `<streamable-http-url>/server-card`이다.
  인증이 없고, `served_on`이 아니면 404이다.
  - 헤더: `Content-Type: application/mcp-server-card+json`, `Access-Control-Allow-Origin: *`, `Cache-Control: public, max-age=300`, `ETag`(본문의 SHA-256).
  - 본문: `$schema`(`https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json`, 스키마의 pattern이 이 값만 받는다), `name`(리스너 host의 label을 뒤집고 `/<프로필>`, 예 `com.example.agents/kim`),
    `version`(bunsind 버전), `description`, `title`(있으면), `remotes: [{type: "streamable-http", url, headers: [{name: "Authorization", isRequired, isSecret}]}]`.
    token과 소유자 이름은 넣지 않는다.
- **ARD 게시**(engine `ard.rs`, crate `bunsin-ard`): `inbound.ard: {publisher, namespace?, agent_name, representative_queries}`.
  - 운영자의 `--ard-publisher-domain`(반복) 목록 안의 publisher만 게시한다. 목록이 없으면 아무도 게시하지 않는다.
    `RealBackends::validate`가 저장할 때, `Backends::ard_publishers`로 catalog가 쓸 때 검사한다. 기각: 소유자가 고르기(운영자 host에서 남의 도메인을 주장한다).
  - `check_config`의 `check_ard`가 자리표시 URL로 `build_entry`를 돌려 MUST 위반을 거부한다. `ard`는 endpoint가 켜져 있어야 한다.
    대표 질문 수가 2~5 밖이면 SHOULD 위반이라 경고와 함께 싣는다(설계 결정).
  - `GET /.well-known/ard.json`(외부 리스너마다, 인증 없음, `Access-Control-Allow-Origin: *`): 요청마다 `bunsin_ard::catalog`로 만든다.
    endpoint가 켜지고 publisher가 허용된 모든 프로필을 넘겨 identifier 중복을 전체에서 검사하고, catalog의 scope 규칙(공개 리스너는 `public`만)이 `served_on`과 같은 것을 고른다.
    항목의 `url`은 같은 리스너의 card URL이다. 중복 identifier는 두 프로필 모두 빠진다.
  - 엔진은 `serve_outer`가 등록한 리스너 주소(`add_outer_listener`)로 URL을 만든다.
  - 스키마 적합성: `crates/ard/tests/schema.rs`가 `catalog(..)`의 본문과 모든 항목을 스펙의 JSON 스키마 사본(`tests/fixtures/ard-entry.schema.json`,
    ard-spec commit `9d33ea1`)으로 검사한다. daemon `tests/server.rs`는 두 리스너가 실제로 내는 본문을 같은 스키마로 검사한다.
    검사기는 dev 의존성 `jsonschema` 0.60.2(기본 기능 없음, `format` 검사 켬)이다. 위반은 찾지 못했다.
    MUST 위반 항목(url과 data 둘 다나 둘 다 없음, type이나 displayName 없음, 잘못된 URN, 상대 URL)은 스키마와 crate의 `validate`가 모두 거부한다.
  - 남은 차이: 스펙 본문은 `trustManifest`, 스키마는 `TrustManifest`라고 쓴다. crate는 본문을 따른다. catalog는 trust envelope를 쓰지 않으므로 게시물에는 영향이 없다.
  - 공식 conformance 도구는 ard-spec 저장소의 `conformance/bin/conformance-test`(Python 3, 표준 라이브러리만)이다.
    `manifest <파일>` 모드는 네트워크 없이 돈다. Python `jsonschema`가 있으면 같은 스키마 검사도 한다. 이 도구는 CI에서 돌리지 않는다.
  - 제어 경로 `GET /v1/profiles/{p}/inbound`(`InboundStatus`, CLI `bunsind inbound`): 리스너별 MCP, card, catalog URL과 ARD identifier,
    싣는 리스너, 빠진 이유(MUST 위반, 허용 밖 publisher, 제공하는 리스너 없음, 중복), 경고.
- 아직 없는 것: 호출자용 OAuth.

## 8. AI 표시

- 권한 표의 모든 규칙은 `outgoing_text`를 반드시 가진다. 값은 셋 중 하나이다.
  - `"none"`: 다른 사람에게 글을 보내지 않는다.
  - `{"arg": "<인자 이름>"}`: 최상위 인자 하나. 이름에 `.`이 있어도 인자 이름 하나로 본다.
  - `{"paths": ["text", "blocks.*.text.text"]}`: 여러 자리. 경로는 인자 이름을 `.`로 잇는다. `*`는 목록의 모든 항목이다.
  이 필드가 없거나 이름이 틀린 규칙은 설정을 읽을 때 거부된다. 빈 `paths`, 빈 단계, `*`로 시작하는 경로는 저장할 때 거부된다.
- 엔진은 모든 자리에 AI 표시를 붙인다. 제안을 받을 때 붙이고, 승인할 때 그 시점의 규칙으로 다시 붙인다.
- 자리 하나라도 없거나 문자열이 아니면 호출을 만들지 않는다. `*` 아래에서는 모든 항목에 나머지 경로가 있어야 한다.
  항목 하나에 경로가 없으면 그 항목의 글은 규칙이 모르는 자리에 있을 수 있기 때문이다. 빈 목록은 글이 없으므로 통과한다.
  - 이 규칙 때문에 divider처럼 글이 없는 Slack block이 섞인 메시지는 `blocks.*.text.text` 규칙에서 거부된다.
- 표시는 실패하면 인자를 바꾸지 않는다. 앞 자리에만 표시가 붙은 호출은 생기지 않는다.
- 창은 최상위 문자열 인자를 여러 개 고를 수 있다. 하나를 고르면 `arg` 형식으로, 둘 이상이면 `paths` 형식으로 저장한다.
  다른 값 안의 자리를 가리키는 저장된 규칙은 있는 그대로 보여 주고, 저장할 때 유지한다.
  다른 값 안의 자리는 `bunsind rule-set --text 'blocks.*.text'`나 설정 파일로 정한다.
- 한계: 규칙에 `"none"`을 잘못 적으면 표시 없이 나간다. 이 선택은 사용자의 책임이다.

## 9. 서버 API와 프로필 격리

서버는 여러 사람의 프로필을 담고, 사외에 노출될 수 있다. 그래서 아래 규칙을 코드로 강제한다.

**접속 token**

- 접속 token 하나는 프로필 하나에만 통한다. 서버는 token을 먼저 확인하고, 그다음에 프로필을 본다.
  다른 프로필에 대한 요청과 없는 프로필에 대한 요청은 같은 오류(401)를 받는다.
- token은 OS 난수 32바이트로 만든다. 저장소에는 SHA-256 해시만 저장한다. token은 발급할 때 한 번만 출력된다.
- 서버는 요청마다 token 표를 읽는다. 운영자가 token을 폐기하면 다음 요청부터 거부된다.
- 프로필 생성과 접속 token 발급은 네트워크 API에 없다. 서버 운영자가 서버에서 명령으로 한다.
- **호출자 token**은 다르다. 프로필 소유자가 제어 API로 발급하고 폐기한다
  (`GET`/`POST /v1/profiles/{p}/callers`, `DELETE .../callers/{id}`, `bunsind callers`, `caller-add`, `caller-revoke`, 창의 Agent 화면).
  - 호출자 token은 그 프로필의 인바운드 endpoint(`ask`, `get_request`)만 연다. 제어 API는 열지 못한다.
  - 표가 다르다: 접속 token은 `tokens`, 호출자 token은 `caller_tokens`(app.db)에 있다. 그래서 한쪽 token은 다른 쪽 확인에서 언제나 "없는 token"이다.
    접두어도 다르다(`bsn_`, `bsc_`).
  - `caller_tokens`의 key는 정수 `id`이다. 해시는 unique 열이고, 어떤 응답에도 나오지 않는다. 목록과 폐기는 `id`로 한다.
  - 발급한 token은 한 번만 보인다. 폐기는 행을 지우지 않고 `revoked_at`을 쓴다. 지난 요청이 호출자의 이름을 잃지 않게 하기 위해서이다.
  - 다른 프로필의 `id`로 폐기하면 없는 `id`와 같은 오류(404)이다. label은 한 줄, 1~60자이다(agent에게 질문의 작성자로 보인다).
  - 감사 로그는 `caller_token_added`(id, label)와 `caller_token_revoked`(id)이다. token 값은 남기지 않는다.
- 관리자용 token은 없다. 네트워크로는 누구도 다른 사람의 프로필을 읽을 수 없다.

**전송**

- 서버는 loopback이 아닌 주소에 TLS 없이 뜨지 않는다. TLS 프록시 뒤에 둘 때는 `127.0.0.1`에 띄운다.
- client는 `https://`만 받는다. `http://`는 host가 정확히 loopback일 때만 받는다.
- client는 redirect를 따르지 않는다. 접속 token은 header로만 보내고, 오류 문구와 `Debug` 출력에 넣지 않는다.
- 사설 CA를 쓰는 서버에는 client에 CA 인증서 파일을 지정한다.
- TLS 구현은 rustls이고 암호 provider는 aws-lc-rs 하나이다 (검증: `cargo tree`).

**프로필 설정이 서버에서 할 수 있는 일**

프로필 소유자는 API로 자기 프로필의 설정을 바꾼다. 설정은 서버가 접속할 주소와 읽을 비밀 값을 정하므로 제한한다.
검사는 설정을 저장할 때와 설정을 쓸 때 두 번 한다.

- **비밀 값**: 설정은 비밀 값의 key만 가진다(`token_secret`). 값은 두 곳 중 하나에서 온다.
  1. 프로필 소유자가 app이나 명령으로 저장한 token. 서버가 암호화해 그 프로필의 DB에 둔다(아래 "저장된 token").
  2. 저장된 token이 없을 때만, 서버 운영자가 넣은 환경 변수 `BUNSIN_SECRET_<프로필>__<KEY>`.
  key에는 `__`를 쓸 수 없다. 그래서 한 프로필은 다른 프로필의 비밀 값이나 서버의 다른 환경 변수를 가리킬 수 없다.
- **stdio 커넥터**: 서버에서 명령을 실행하므로 기본으로 거부한다. 운영자가 `--allow-stdio-connectors`로 켠다.
  이 옵션은 프로필 격리를 넘는 권한이다: 프로필 소유자가 서버 사용자의 권한으로 아무 프로그램이나 실행할 수 있고,
  그 프로그램은 다른 프로필의 DB와 `master.key`를 읽을 수 있다. 모든 소유자를 신뢰하는 서버에서만 켠다.
  `--allow-coding-commands`(6절)도 같은 권한을 준다. `--allow-coding`과 `--coding-program`만으로는 운영자가 정한 코딩 CLI만 실행된다.
  그 CLI도 서버 사용자로, sandbox 없이 돈다. 지시문은 프로필 소유자가 승인한 글이다.
  설정이 바뀌면 그 프로필의 client를 버리고, `rmcp`의 자식 프로세스 transport가 버려질 때 프로그램을 종료한다
  (`rmcp-3.10.0`의 `ChildWithCleanup::drop`). 그래서 설정을 저장해도 프로그램이 남지 않는다.
- **LLM과 MCP 주소**: 기본 규칙은 `https://` 주소만 받고, `localhost`와 내부 IP 주소(loopback, 사설, link-local 등)로 적힌 host를 거부한다.
  도메인 이름은 모두 통과한다. 사내 DNS 이름도 통과하므로, 기본 규칙은 내부망 접근을 완전히 막지 못한다.
  운영자가 `--allow-upstream-host <host>`를 주면 그 목록의 host만 허용한다. 사내 LLM gateway는 이 옵션으로 허용한다.
  - 여러 사람이 쓰는 서버나 사외에 노출된 서버의 운영자는 이 목록을 주어야 한다.
- LLM client와 MCP HTTP client는 redirect를 따르지 않는다. redirect로 위 규칙을 우회할 수 없다.

**저장된 token**

프로필 소유자는 LLM endpoint와 커넥터(MCP 주소)를 token과 함께 저장한다. GPUI app의 연결 설정 화면과 `bunsind llm-set`, `connector-set` 명령이 이 경로를 쓴다.

- **주소와 token은 한 번의 호출로 저장한다.** token을 비워 두면 기존 token을 유지한다. 비밀 값의 key 이름은 서버가 정한다(`LLM`, `MCP_<커넥터>`).
- **token은 돌려주지 않는다.** 조회는 token의 출처만 알려 준다: 서버에 저장됨, 서버 환경 변수, 없음.
  token은 설정 출력, 감사 로그, 오류 문구에 들어가지 않는다. 감사 로그에는 대상과 "set / kept / cleared"만 남는다.
- **token은 입력한 주소에 묶인다.** 서버는 token을 저장할 때 그 주소의 origin(scheme, host, port)을 함께 저장한다.
  - 주소를 다른 origin으로 바꾸면서 token을 유지하려는 저장은 거부한다. 사용자는 token을 다시 입력해야 한다.
  - 설정 전체를 바꾸는 경로(`config-set`)로 주소만 바꾼 경우에는 사용할 때 거부한다.
  - 이유: 접속 token을 가진 사람이 주소를 자기 서버로 바꾸면, 서버가 저장된 token을 그곳으로 보내게 된다. token을 읽는 API가 없어도 token이 새는 경로이다.
- **암호화**: XChaCha20-Poly1305. 쓸 때마다 새 난수 nonce를 쓴다. 암호문은 프로필 ID, key 이름, origin에 묶여 있어 다른 자리로 옮기면 풀리지 않는다.
- **마스터 키**: `--key-store`가 위치를 정한다. 기본값은 macOS에서 `keychain`, 그 외에서 `file`이다. 저장 코드와 DB의 암호문은 두 경우에 같다.
  - `file`: 데이터 폴더의 `master.key`(권한 0600) 또는 `--master-key-file`의 파일. 없으면 서버가 만든다.
  - `keychain`: macOS 로그인 Keychain의 항목 1개(서비스 `Bunsin server`). 계정은 데이터 폴더의 `keychain-id` 파일에 있는 무작위 ID이다.
    경로를 계정으로 쓰지 않는 이유: 한 폴더의 경로 표기가 여러 가지이고, 폴더는 옮겨질 수 있다.
  - **자동 이전**: Keychain 항목이 없고 `master.key`가 있으면 키를 항목에 쓰고, 다시 읽어 같은지 확인한 뒤에만 파일을 지운다.
    둘 다 있고 같으면(중단된 이전) 파일을 지운다. 둘이 다르거나, 항목이 키가 아니거나, Keychain이 잠겼거나 쓰기를 거부하면 아무것도 바꾸지 않고 멈춘다.
  - **새 키를 말없이 만들지 않는다**: 데이터 폴더에 암호화된 값이 있는데 고른 저장소에 키가 없으면 서버가 시작하지 않는다(두 저장소 모두).
    새 키는 그 값을 하나도 읽지 못하기 때문이다. 오류가 키가 있을 만한 곳을 알려 준다. Keychain 오류에서 파일로 넘어가지 않는다.
    - 키를 잃은 경우의 출구: 운영자가 `--accept-new-key`를 주면 새 키로 시작한다. notes와 설정은 남고, 저장된 token은 읽을 수 없다고 보고되어 소유자가 다시 입력한다.
      이 옵션은 키가 없을 때만 새 키를 만든다. 있는 키는 그대로 쓰고, 항목과 파일이 다른 경우처럼 거부되는 상태는 그대로 거부된다.
    - 읽을 수 없는 프로필은 "암호화된 값이 있다"고 본다. 그 프로필 때문에 다른 프로필의 시작이 멈추지 않는다.
    - `keychain-id` 파일은 키의 일부이다. 지워지면 항목을 찾지 못한다.
  - **순서**: 주소 검사(TLS 필요 여부)와 데이터 폴더의 잠금 뒤에 키를 읽는다. 거부된 시작과 두 번째 인스턴스는 키를 건드리지 않는다.
  - Keychain 라이브러리는 macOS에서만 의존성이다. 저장소를 고르는 함수는 OS 이름을 인자로 받아 두 경우를 모두 테스트한다.
  - 오류 문구는 직접 쓴다. `keyring`의 일부 오류는 저장된 값(여기서는 마스터 키)의 바이트를 담고 있기 때문이다.
  - 실제 Keychain으로는 실행하지 않았다. 테스트는 메모리의 가짜 Keychain을 쓰고, 서버 프로그램을 띄우는 테스트는 파일 저장소로 고정한다.
  - 이 암호화가 막는 것: 프로필 DB만 복사되거나 백업이 유출된 경우. 키 파일을 데이터 폴더의 백업에 넣지 말아야 한다.
  - 막지 못하는 것: 서버의 키 파일을 읽을 수 있는 사람.
  - 키 파일을 잃으면 저장된 token을 풀 수 없다. 소유자가 token을 다시 입력해야 한다. 풀리지 않는 token은 오류이고, 환경 변수로 대신하지 않는다.
- 주소나 token이 바뀌면 서버는 그 프로필의 client를 다시 만든다.
- **저장 후 확인**: token을 다시 읽을 수 없으므로, 커넥터의 도구 목록 조회(`bunsind tools`, app의 Check)가 주소와 token이 동작하는지 알려 준다.
- **명령 커넥터(stdio)도 창에서 저장한다**(`set_command_connector`). 이전에는 설정 파일로만 가능했다.
  - 새 권한은 없다. 설정 파일로 저장할 때와 같은 검증을 지난다: 운영자가 `--allow-stdio-connectors`로 띄운 서버에서만 저장되고,
    명령은 절대 경로이고, 환경 변수 이름은 문자, 숫자, `_`이다. 이 검사는 저장과 실행 양쪽이 지나는 자리에 있다.
  - 이름은 종류를 유지한다. 주소 커넥터의 이름에 명령을 저장하거나 그 반대는 거부한다. 먼저 지워야 한다.
  - **비밀 환경 변수**: 커넥터 설정의 `secret_env`는 변수 이름만 담는다. 값은 token과 같은 암호화 저장소에 있다
    (key는 `ENV_<커넥터>_<변수 이름의 hex>`). 값은 어떤 응답에도 없고, 프로그램을 시작할 때 그 프로세스의 환경에만 들어간다.
    - 이 기능이 주는 것은 저장 시 암호화와 "다시 보여 주지 않음"이다. 그 이상은 아니다: `--allow-stdio-connectors`가 켜진 서버에서는
      프로필의 접속 token을 가진 사람이 아무 프로그램이나 실행할 수 있다. 값은 프로그램 경로나 인자에 묶지 않는다.
    - 저장된 값의 origin은 주소가 아닌 문구(`environment of connector <이름>`)이다. 그래서 주소 커넥터가 이 key를 token으로 지정해도 값이 전송되지 않는다.
    - 저장 전에 모든 것을 검증한다. 거부된 저장은 값을 저장하거나 지우지 않는다.
    - 설정이 저장될 때마다 설정에 이름이 없는 값을 지운다(창, 커넥터 삭제, 설정 파일 모두). 값이 남지 않는다.
      지운 값은 되돌릴 수 없다. 예전 설정 파일을 적용하면 그 파일에 이름이 없는 값이 지워진다. 감사 로그에 지운 개수를 남긴다(`secret_values_removed`).
    - 설정 파일은 값 없이 이름만 적을 수 있다. 조회 응답의 `secret_env_missing`이 값이 없는 이름을 알려 주고,
      창은 그런 변수를 "stored"로 표시하지 않고 값이 없다고 알린다.
    - 새 값을 덮어쓴 뒤 설정 저장이 동시 변경으로 거부되면 새 값이 남는다. token도 같은 순서로 저장된다.
    - `secret_env` 없이 보낸 갱신은 모든 비밀 변수를 지운다. 지금은 창만 이 호출을 쓴다.
    - 창은 이름 칸과 가려진 값 칸으로 받는다. 저장 후에는 이름과 "stored" 표시만 보인다. 비밀 값은 창에서만 입력한다.
  - 인자와 그 밖의 환경 변수는 평문 설정이다. 비밀 값이 들어갈 수 있으므로 목록, 감사 로그, 오류 문구에는 넣지 않는다.
    목록은 프로그램 경로와 개수만 보여 주고, 값은 Edit 후의 입력란에만 나온다. `connections` 응답에는 값이 들어 있다.
  - 창의 입력란은 줄 단위이다: 인자는 한 줄에 하나, 환경 변수는 한 줄에 `이름=값`(첫 `=`에서 나눔). 줄 안의 공백은 그대로 둔다.
    빈 인자나 줄바꿈이 든 값은 입력란이 담을 수 없으므로, 그런 커넥터는 Edit를 거부하고 설정 파일을 안내한다.
- **client 하나의 실패는 그 client만 멈춘다.** Jira 주소가 틀려도 Slack의 폴링과 승인은 계속된다. LLM 설정이 없어도 커넥터의 도구 목록은 볼 수 있다.
- 한계: 운영자가 환경 변수로 넣은 token은 주소에 묶이지 않는다.

**아직 없는 것**

- 서버 운영자로부터의 보호. 서버 파일에 접근할 수 있는 사람은 프로필 데이터를 읽을 수 있다.
- 접속 실패 횟수 제한. token이 256비트 난수이므로 두지 않았다.

## 10. MCP 커넥터

- `McpConnector`는 공식 Rust SDK `rmcp =3.10.0`의 client이다. transport는 streamable HTTP와 stdio 자식 프로세스이다.
- stdio 명령은 절대 경로만 받는다.
- rmcp의 도구 목록 cache를 끈다. cache된 목록은 서버의 변경을 숨겨 pin 검사를 무력하게 만들기 때문이다.
- 연결은 30초 안에 끝나야 한다. 시간이 지나면 `RealBackends`가 오류를 돌려준다.
- 도구 호출 하나는 앞의 도구 목록 조회까지 120초(`CALL_TIMEOUT`) 안에 끝나야 한다. 도구가 느릴 수 있어 넉넉히 잡았다.
  엔진이 호출 전 pin 검사로 따로 부르는 `list_tools`도 같은 한도이다. 그렇지 않으면 목록에 답하지 않는 서버가 이벤트를 막는다.
  시간이 지나면 요청을 버리고, rmcp가 서버에 `notifications/cancelled`를 보낸다.
  목록 단계에서 시간이 지나면 오류는 `Transport`이다. 호출 단계에서 지나면 `Unanswered`이고, 승인된 쓰기는 `unknown`이 된다(1절).
  쓰기 도구라면 서버가 이미 실행했을 수 있다. 엔진은 다른 호출 오류처럼 `failed`로 둔다. OAuth 커넥터도 같다.
- 커넥터는 권한을 판단하지 않는다. 엔진이 호출 전에 정책과 pin을 검사한다.
- HTTP transport는 token을 `Authorization: Bearer <token>`으로 보내거나, OAuth로 받은 access token을 보낸다(아래 **OAuth**).
  커넥터 설정의 `token_header`가 있으면 그 헤더가 token을 그대로 담는다(예: `X-API-Key`).
  연결이 스스로 정하는 헤더(`Host`, `Content-Type`, `Mcp-Session-Id` 등)는 거부한다.
  이 검사는 주소 규칙과 같은 자리에 있어서, 설정 파일로 저장할 때와 접속할 때 모두 지난다.
  `Host`를 바꿀 수 있으면 허용된 주소로 보낸 요청이 다른 가상 호스트로 갈 수 있기 때문이다. token이 없는 커넥터는 두 헤더 모두 보내지 않는다.
  LLM token의 헤더는 wire 형식이 정하고, 바꿀 수 없다.
- HTTP transport는 redirect를 따르지 않는다.
- 실제 Slack, Jira, Confluence MCP 서버로는 아직 연결해 보지 않았다. 테스트는 로컬 rmcp 서버를 쓴다.

**OAuth**

- 설정: 주소 커넥터의 `oauth: {client_id?, scopes?}`(`OAuthConfig`). 명령 커넥터, REST API 커넥터, token이나 `token_header`와 함께 쓰면 거부한다
  (`ConnectorSection::check_oauth`, 저장할 때와 접속할 때). client ID와 scope는 공백과 `"`, `\` 없는 출력 가능 ASCII만 받는다.
  지금은 설정 파일(`config-set`)로만 켠다. 창과 `connector-set`의 저장은 기존 `oauth`를 유지한다.
- 흐름은 rmcp `auth` 기능(`AuthorizationManager`, `AuthorizationSession`)을 쓴다. 보호 자원 metadata와 인가 서버 metadata를 찾고,
  `client_id`가 없으면 dynamic client registration(client 이름 `Bunsin`)을 하고, PKCE S256과 `state`로 인가 주소를 만든다.
- 모든 OAuth 요청(발견, 등록, token, 갱신)은 `PolicyHttp`를 지난다: 보내기 전에 `UpstreamPolicy::check`, redirect 없음, 요청당 30초, 답은 1 MiB까지.
  rmcp가 같은 origin 안에서 따라가는 발견의 redirect도 이 client로 다시 들어온다. 정책이 거부하면 그 이유가 오류 문구가 된다.
- 제어 API `POST /v1/profiles/{p}/connectors/{c}/oauth/start`(`redirect_uri`) → 인가 주소, `.../oauth/finish`(`callback_url`).
  redirect 주소는 http의 loopback(127.0.0.1, ::1, localhost)만 받는다(RFC 8252 7.3).
  - 인가 주소는 MCP 서버의 metadata가 정하고, 창이 그 주소를 연다. 그래서 서버와 창 모두 `https`와 loopback `http`만 받는다.
    `smb:`, `file:`, 앱 전용 scheme을 열면 소유자의 Mac에서 프로그램이 실행되기 때문이다.
  시작한 로그인은 서버 메모리(`SignIns`)에서 10분 기다리고, 프로필당 3개까지이다. 서버를 다시 시작하면 잊는다.
  finish는 프로필, 커넥터, `state`가 맞는 로그인을 한 번만 꺼낸다. 거부, 틀린 code, 다른 redirect 위치도 그 로그인을 끝낸다.
  `OAuthFinishBody`의 Debug는 code를 담은 주소를 출력하지 않는다.
- 창: **Sign in**이 127.0.0.1의 빈 port에 한 번 쓰는 listener(`sign_in.rs`)를 열고 start를 보낸 뒤 브라우저로 인가 주소를 연다.
  `/callback` 요청 하나를 받고 닫는다. 다른 경로는 404, 10분이 지나면 오류이다.
  CLI `bunsind connector-oauth`는 인가 주소를 출력하고, 사용자가 붙여 넣은 주소로 finish를 보낸다.
- credentials(rmcp `StoredCredentials`의 JSON)는 key `OAUTH_<NAME>`으로 봉인하고, origin 자리에 `oauth of <커넥터> for <MCP origin>`을 둔다.
  주소가 아니므로 token으로 어느 곳에도 보내지지 않는다. finish는 로그인하는 동안 주소의 origin이 바뀌었으면 거부한다.
  저장한 뒤 그 프로필의 runtime cache를 버린다. 감사 로그는 `oauth_signed_in`, `oauth_sign_in_needed`와 대상만 남긴다.
- 접속(`RealBackends::oauth_connector`): 현재 주소의 origin에 묶인 credentials만 쓴다. 없거나 다른 origin이면 접속하지 않고 다시 로그인하라는 오류를 돌려준다.
  접속 전에 access token을 한 번 받는다. 만료되었고 refresh token이 없거나 갱신이 거부되면 같은 오류이다.
- 갱신: rmcp가 갱신한 credentials는 엔진의 `SecretWriter`(`ProfileWriter`)로 다시 봉인한다. 인가 서버가 refresh token을 거부하면
  저장된 credentials가 그 값 그대로일 때만 지우고 `oauth_sign_in_needed`를 남긴다. 그 사이 끝난 새 로그인은 지우지 않는다(엔진 전체의 잠금 하나).
- 상태(`OAuthState`): `signed_in`은 현재 origin의 credentials가 있고, 그 안에 token 응답이 있고, access token이 만료되지 않았거나 refresh token이 있을 때이다.
  만료는 `token_received_at + expires_in`을 엔진 시계와 비교한다. 둘 중 하나가 없으면 만료되지 않은 것으로 본다(rmcp도 그대로 쓴다).
  그 외(없음, 다른 origin, 열 수 없음, 만료되고 refresh token 없음)는 `sign_in_needed`이다.
- `oauth`나 커넥터를 지우면 다음 저장에서 `OAUTH_` credentials를 지운다(`prune_secrets`).
- 테스트는 로컬 stub 인가 서버(daemon `tests/support/oauth_stub.rs`: metadata, 등록, PKCE 검사, 갱신, 거부)로 엔진, 서버, CLI, 창을 지난다.
  실제 OAuth 제공자로는 실행하지 않았다.

**REST API 커넥터**

- 설정은 `mcp` 대신 `api` 키를 쓴다: `{"api": {service: slack|jira|confluence, base_url, auth: bearer|basic(기본 bearer), email?}, token_secret}`.
  `ConnectorSection`은 두 키 중 하나만 받는다(`ConnectorKind`). 저장 형식은 `deny_unknown_fields`이므로 설정 안의 token은 거부된다.
  이 키가 없던 설정은 그대로 읽히고 그대로 저장된다.
- `ApiConnector`(`bunsin-connectors::api`)가 서비스마다 고정된 도구 셋을 둔다. 지우는 도구는 없다. 도구 목록은 요청 없이 코드에서 나온다.
  그래서 도구 승인(pin), 규칙, 읽기 5회 한도, AI 표시, 승인 실행이 MCP 커넥터와 같은 경로를 지난다.
- 검사(`ApiConfig::check`, `RealBackends::connector_section`): Slack은 bearer만, basic은 email이 필요하고 bearer는 email이 없다.
  token이 반드시 있어야 하고 `token_header`는 받지 않는다. `base_url`은 `UpstreamPolicy::check`를 지난다. 저장할 때와 접속할 때 모두 지난다.
- token은 MCP token처럼 key `MCP_<NAME>`으로 봉인되고 `base_url`의 origin에 묶인다. 다른 origin으로 옮기면 token을 다시 넣어야 한다.
- 이름은 종류를 지킨다: 주소, 명령, REST API 사이를 바꾸려면 먼저 지운다(`same_kind`).
- 제어 API `PUT /v1/profiles/{p}/connections/connectors/{c}/api`(`ApiConnectorUpdate`), CLI `connector-set --api <service> [--auth basic --email ...]`,
  창의 종류 **REST API**(서비스, base URL, 인증 방식, email, token). 창은 Slack을 고르면 `https://slack.com/api`를 채운다.
  REST API 커넥터의 token은 창에서 지울 수 없다. token이 없으면 쓸 수 없기 때문이다.
- HTTP client는 redirect를 따르지 않는다. 오류 문구에서 token과 basic 인코딩 값을 지운다.
- 요청 하나는 30초 안에 끝나야 한다(MCP 커넥터의 연결 한도와 같은 `CONNECT_TIMEOUT`). 답하지 않는 서버는 이벤트 처리를 막지 못한다. 연결 뒤의 시간 초과는 `Unanswered`이다(1절).
- 요청 모양은 공식 API 문서를 따랐다. 테스트는 로컬 stub(daemon `tests/api_connectors.rs`)으로 도구 목록, 승인, 규칙, 읽기, 승인 뒤 쓰기와 AI 표시를 끝까지 돈다.
  실제 서비스로는 실행하지 않았다.

## 11. 이벤트 수신 (폴링)

서버는 프로필 설정의 `polls`에 적힌 검색을 반복해서 새 항목을 이벤트로 만든다. MCP만 쓰고, 서버에 공개 주소가 필요 없다.

- 서버의 폴링 작업은 5초마다 모든 프로필을 살피고, 간격(`every_secs`, 최소 30초)이 지난 poll을 실행한다.
- **읽기 도구만 쓴다.** poll의 도구는 권한 표에 `read` 규칙이 있어야 하고, 승인된 도구 목록과 일치해야 한다.
  이 검사는 설정을 저장할 때와 실행할 때마다 한다. poll은 사용자가 승인하지 않은 호출 경로가 되지 않는다.
- **결과의 모양은 설정이 정한다.** MCP 서버마다 결과가 다르므로 `mapping`이 항목 목록과 ID, 본문, 작성자의 위치를 dot 경로로 적는다.
  코드에는 특정 서비스의 모양이 들어 있지 않다. JSON을 text content block에 담아 주는 서버의 결과는 풀어서 읽는다.
- **첫 실행은 과거 이력을 처리하지 않는다.** 검색(커넥터, 도구, 인자)이 처음 실행되면 결과를 "이미 본 것"으로만 기록한다.
  검색 조건을 고치면 새 검색으로 취급해 다시 기준선을 잡는다.
- **자기 글은 이벤트가 아니다.** 프로필의 AI 표시가 든 글과 `skip_authors`의 작성자가 쓴 글은 건너뛴다. 그렇지 않으면 agent가 자기 답글에 다시 답한다.
- **실패한 이벤트는 잃지 않는다.** 이벤트는 처리가 끝나야 닫힌다. LLM 오류 같은 실패로 끝나면 열린 채로 남고, 다음 실행에서 다시 시도한다.
  3번 실패하면 서버는 승인함에 "처리하지 못했다"는 항목을 한 번 넣고 그 이벤트를 닫는다. 직접 넣는 이벤트(`ingest`)도 같은 규칙을 따른다.
- 정지 스위치가 켜져 있으면 폴링을 쉰다. 켜진 동안 이벤트를 가져오면 제안이 거부된 채로 이벤트가 닫히기 때문이다.
- 한 번의 실행은 새 이벤트를 최대 10개 처리한다. 나머지는 다음 실행에서 처리한다. 한 프로필은 동시에 한 작업만 폴링한다.
- 각 poll의 마지막 결과(항목 수, 쓸 수 있는 항목 수, 새 이벤트 수, 오류)는 `status` 명령과 snapshot에 보인다.
  항목은 있는데 쓸 수 있는 항목이 0이면 `mapping`이 결과와 맞지 않는 것이다. 감사 로그에는 상태가 바뀔 때만 기록한다.
- `bunsind poll-test`와 창의 Test는 poll을 한 번 실행해 추출된 이벤트를 보여 준다. 아무것도 저장하지 않고 처리하지 않는다.
- 창은 poll을 `polls`/`set_polls`로 읽고 바꾼다(3절). 이 저장은 도구가 승인된 목록에 있는지도 검사한다.
- 서버는 SIGTERM을 받으면 진행 중인 폴링 주기가 끝난 뒤에 폴링 작업을 끝낸다.
- 한계: 검색 결과에 더 이상 나오지 않는 항목은 가져오지 못한다. 검색이 최근 항목을 충분히 돌려주도록 인자를 정해야 한다.
- 한계: 한 ID는 한 번만 이벤트가 된다. 같은 ID로 내용이 바뀌는 Jira 이슈나 Confluence 문서의 변경은 다시 보지 못한다.
  변경마다 다른 ID를 주는 검색(예: 댓글이나 변경 이력)을 써야 한다. `mapping`의 한 경로는 필드 두 개를 합칠 수 없다.
- 자율 단계의 쓰기는 호출을 시작하기 전에 이벤트를 닫는다. 호출 뒤의 저장 단계가 실패해도 재시도가 같은 글을 다시 보내지 않는다.
- 한계: 실제 Slack, Jira, Confluence MCP 서버의 검색 도구와 결과 모양은 확인하지 않았다.

**Webhook**(core `webhook`, engine `webhooks.rs`, daemon `webhooks.rs`, 해석과 서명은 `bunsin-connectors::webhook`)

- 설정 `webhooks: {name: {connector, scope(기본 internal), skip_authors}}`. 설계의 `service`, `source`, `event_types`, `mapping`은 두지 않았다.
  병합된 고정 해석기(`parse_slack`, `parse_jira`)가 이벤트 ID와 종류를 정하고, 서비스는 connector(같은 서비스의 REST API connector)가 정한다.
  `check_webhooks`: 이름 규칙, connector가 Slack이나 Jira의 REST API connector, 같은 connector에 poll이 없을 것(이벤트 ID가 달라 두 번 처리된다).
- secret: `PUT /v1/profiles/{p}/webhooks/{name}/secret`(CLI `webhook-secret`, 표준 입력). key `HOOK_<NAME>`, origin 문구 `webhook of <name>`.
  주소가 아니므로 `RealBackends::token`이 보내지 않는다. `prune_secrets`(이름이 바뀜)가 `ENV_`와 `HOOK_` 중 설정이 이름을 대지 않는 값을 지운다.
- `POST /hooks/{p}/{name}`(외부 리스너, `scope`가 고른다. `served_on`은 인바운드와 같은 `reaches`): 1 MiB 넘으면 413(`DefaultBodyLimit`).
  `webhook_target`이 없으면(프로필, webhook, 리스너, secret, origin) 같은 401이다. 서명 확인이 본문 해석보다 먼저이다.
  Slack은 `verify_slack`(timestamp 300초), Jira는 `verify_atlassian`과 서명된 본문 `timestamp`(ms)가 2시간 안. 시각은 엔진의 clock이다.
  `url_verification`은 서명이 맞을 때만 `200 text/plain <challenge>`이다.
- 전달 ID(Slack `event_id`, Jira `X-Atlassian-Webhook-Identifier`, 없으면 이벤트 ID)로 `intake(channel = webhook 이름)`에 `INSERT OR IGNORE`한다.
  저장 전에 자기 글(`own_post`: poll과 같은 함수)을 버리고 필터를 적용한다. 처리가 끝난 행은 7일 뒤 지운다(`prune_intake`).
  저장하면 intake task를 깨우고 바로 200을 준다. 처리는 `drain_intake`가 한다(재시작 뒤에도 남는다). 같은 메시지의 다른 전달은 이벤트 key로 한 번만 처리된다.
- 이벤트는 `outside`이다. notes 변경은 자동 반영하지 않는다. `ask`의 미처리 한도 50은 이제 `bunsin.inbound` 채널만 센다.
- 한계: 실제 Slack과 Jira가 보내는 요청으로 시험하지 않았다. Jira의 재시도가 원래 본문을 보낸다는 것은 가정이다.

**도구 목록과 승인**

- `bunsind tools <프로필> <커넥터>`는 MCP 서버가 지금 알려 주는 도구를 보여 주고, 승인된 목록과의 차이(new, changed, gone)를 표시한다.
- `--approve`는 화면에 보인 목록을 승인한다. client가 보인 도구의 fingerprint를 보내고, 서버는 그 사이 목록이 바뀌었으면 거부한다.
- 승인은 권한 규칙을 만들지 않는다. agent가 도구를 쓰려면 `policy.rules`에 규칙이 따로 있어야 한다.

## 12. 개인정보 필터

- 필터는 다음을 `[REDACTED:<종류>]`로 바꾼다: 이메일, 전화번호, 주민등록번호, 카드번호(검증 숫자가 맞는 것만), 사용자가 정한 정규식 패턴,
  사용자가 정한 단어(`terms`, `[REDACTED:TERM]`).
  새 프로필은 네 가지가 모두 켜져 있다.
- **필터를 거치지 않은 글은 LLM에 가지 않는다.** 필터는 두 곳에서 적용한다.
  1. 이벤트를 저장하기 전: 본문, 작성자, thread. 그래서 지워진 값은 DB와 산출물 파일에도 없다.
  2. 프롬프트에 인용하는 모든 글: 이벤트, 교정 기록, 업무지식, 도구 설명. 나중에 추가되는 출처도 이 지점을 지난다.
- 사용자의 원칙(`principles`)은 사용자가 직접 쓴 지시이므로 거르지 않는다. 사용자가 승인하면서 직접 고친 글도 그대로 전송한다.
- **설정이 깨져 있으면 처리하지 않는다.** 저장된 패턴이 컴파일되지 않으면 서버는 이벤트를 저장하지 않고 LLM을 호출하지 않는다.
- 서버는 패턴을 저장하기 전에 검사한다: 컴파일되는지, 빈 문자열과 일치하지 않는지, 길이와 개수가 한도 안인지.
  정규식은 Rust `regex` crate의 문법이다. 이 crate는 역추적을 하지 않으므로 입력 길이에 선형인 시간에 끝난다.
- 감사 로그에는 지운 개수만 종류별로 기록한다. 지운 값은 로그, 오류, `poll-test` 출력 어디에도 쓰지 않는다.
- LLM에는 자리표시자가 지워진 개인정보라는 점을 알린다. 도구 인자에 자리표시자가 들어 있는 제안은 거부하고 한 번 다시 요청한다.
- GPUI app의 개인정보 설정은 `privacy` 전용 API만 쓴다. app이 설정 전체를 되돌려 보내면, app이 모르는 항목이 지워질 수 있기 때문이다.
- **단어 목록(`terms`)**: 사용자가 적은 이름, 주소 같은 글자 그대로의 단어이다.
  - 대소문자를 가리지 않고, 단어 경계 없이 글 어디에서나 일치한다. 한국어 이름 뒤에 조사나 `님`이 붙어도 지우기 위해서이다.
    대신 짧은 단어는 다른 단어의 일부도 지운다(`Kim`은 `Kimball`의 앞부분을 지운다).
  - 모든 단어를 정규식 하나로 컴파일한다(`regex::escape`). 긴 단어를 앞에 두어, 한 단어가 다른 단어의 앞부분일 때 긴 쪽이 이긴다.
  - 한도: 200개, 앞뒤 공백을 뺀 2자에서 100자. 한 글자는 너무 많은 글을 지우므로 거부한다.
  - 단어는 개인정보이다. 오류 문구는 단어를 순서로만 가리키고, 감사 로그 `privacy_changed`에는 단어의 개수만 남는다.
    단어 자체는 프로필 설정에 저장된다.
  - 이름을 찾으려고 거르지 않은 글을 다른 곳(LLM, 외부 서비스)에 보내지 않는다. 목록에 없는 이름은 지워지지 않는다.
- **한계: 패턴과 목록 기반이다.** 목록에 없는 사람 이름과 주소, 계좌번호는 찾지 못한다. 필요한 것은 단어나 사용자 정의 패턴으로 추가한다.
- 한계: 필터는 들어오는 글에만 적용된다. LLM이 스스로 만들어 낸 글은 거르지 않는다.

## 13. 여러 단계 처리 (읽고 나서 제안)

agent는 이벤트만 보고 답하지 않는다. 필요한 사실을 `read` 도구로 찾아 읽은 뒤에 제안한다.

1. LLM이 `read` 규칙이 있는 도구를 제안하고, 정책이 실행을 허용하면 엔진이 그 도구를 호출한다.
2. 엔진은 결과를 개인정보 필터에 통과시키고, 인용 데이터(`<data>`)로 LLM에 돌려준 뒤 다시 묻는다.
3. LLM이 `read`가 아닌 것(무시, 사용자에게 넘김, 쓰기)을 제안하면 반복이 끝나고, 기존 흐름(정책 검사, 승인함)으로 간다.

- **읽기는 이벤트 하나에 커넥터 도구 최대 5번, notes 페이지 최대 5번이다.** 결과 하나는 20,000자까지만 쓰고 나머지는 자른다.
- **읽은 결과는 지시가 아니다.** 결과 안의 문장이 규칙을 바꾸거나 다른 도구를 쓰라고 해도, 권한은 정책 표가 결정한다.
- **결과는 저장할 때도 필터를 거친다.** 프롬프트용 글과 디스크에 저장하는 글을 같은 함수가 만든다. 승인된 쓰기의 결과도 같다.
- **중간 읽기는 승인함에 넣지 않는다.** 감사 로그(`read`)와 산출물(`read_result`)로 남는다.
  초안과 넘김 항목에는 그 제안의 근거가 된 읽기 목록(`basis`)이 붙고, 창이 승인 전에 보여 준다.
- **실패한 읽기는 데이터로 돌려준다.** LLM은 다른 방법을 고르거나 사용자에게 넘길 수 있다.
- **정책이 거부한 읽기는 실행하지 않는다.** 표에 없는 도구, 승인 후 바뀐 도구가 해당한다.
- **다시 해도 같은 결과가 나오는 실패는 재시도하지 않는다.** 읽기 한도 초과, 같은 읽기의 반복, 토큰 예산 소진, 출력 한도에서 잘린 답이 해당한다.
  엔진은 이벤트를 닫고 승인함에 이유를 적은 항목을 넣는다. 재시도하면 같은 비용을 다시 쓰기 때문이다.
  토큰 예산은 LLM을 호출하기 전마다 확인한다.
- **재현 평가는 도구를 실행하지 않는다.** 오늘 검색하면 사용자의 실제 답이 나와 비교가 무의미해지기 때문이다.
  같은 이유로 재현에는 notes와 교정 기록도 넣지 않는다(4절).
- **`read` 등급의 뜻**: 아무것도 바꾸지 않고, 자기 서비스 밖으로 아무것도 보내지 않는 도구이다.
  뒤의 읽기는 앞에서 읽은 글을 인자에 담을 수 있다. 그래서 통제할 수 없는 URL을 가져오는 도구는 `read`로 두면 안 된다.
- 한계: 무엇을 읽을지는 LLM이 정한다. notes에는 관련도 순위와 검색이 있다(15절). 커넥터의 문서에는 색인이 없다.

**ARD 검색 `bunsin.ard/search {text, registry?}`**(core `ard`, daemon `ard_search.rs`)

- 설정 `ard_registries: [{name, url}]`(`ProfileConfig`, `#[serde(default)]`). 설계의 `federation` 필드는 두지 않았다. referral을 따르지 않으므로 고를 값이 `none` 하나뿐이다.
- 엔진에는 HTTP client가 없으므로 `RealBackends`가 runtime에 내장 connector `bunsin.ard`를 넣는다. 이름의 `.` 때문에 소유자 connector와 겹치지 않는다.
- 요청: `POST <url>/search`, `query.text`, `federation: "none"`, `pageSize: 5`. `SearchClient`의 hop 한도는 0이라 referral 요청이 없다(테스트가 목록 안의 registry를 가리키는 referral로 확인한다). redirect도 따르지 않는다.
- registry 주소는 `UpstreamPolicy::check`를 저장할 때와 만들 때 지난다. 이름은 `check_registries`가 검사한다(connector 이름 규칙, 중복 없음).
- 엔진은 이 도구를 notes 읽기처럼 규칙 없이 허용한다(registry가 있을 때만, 읽기 허용 단계에서만). 실행과 결과 처리는 connector 읽기와 같아서
  필터, 인용, 감사 `read`, 산출물 `read_result`를 지나고 커넥터 읽기 5회 한도에 든다. 정지 스위치는 다른 읽기처럼 막지 않는다.
- 결과는 identifier, displayName, type, url, description, capabilities, score, source만 넘긴다. `score`는 글일 뿐이고 `Policy::decide`에는 score 입력이 없다.
- 찾은 자원은 부를 수 없다. 소유자가 connector로 더하고 도구를 승인해야 한다.

## 14. 설정 저장의 충돌 검사

- 저장 요청은 "화면이 읽었던 값"을 함께 보낼 수 있다(`Expected::Value`). 서버의 현재 값이 다르면 서버는 `Conflict`로 거부하고 아무것도 저장하지 않는다.
  개인정보 설정, LLM endpoint, 커넥터 각각이 자기 값만 비교한다. 다른 항목의 변경은 충돌이 아니다.
- 창은 충돌을 경고하고 두 가지를 제안한다: 서버 값을 다시 읽기, 그래도 저장하기(검사 없이 저장).
- 좁은 범위의 저장(개인정보, 연결, 도구 승인, 3절의 부분 저장 카드)은 설정을 "읽은 그대로일 때만 교체"한다. 같은 설정에서 출발한 두 변경이 둘 다 저장되어 하나가 조용히 사라지는 일이 없다.
- 한계: token만 바뀐 경우는 비교에 보이지 않는다. 명령줄과 `config-set`은 검사 없이 저장한다.
  예외는 권한 규칙 명령(`rule-set`, `rule-remove`)이다. 이 명령은 읽은 표를 함께 보낸다(3절).

## 15. agent의 notes (LLM wiki)

agent는 이벤트를 처리하며 읽은 사실 중 오래 쓸 것을 페이지로 정리해 둔다. 다음 이벤트에서는 그 페이지를 먼저 본다.
발상은 Karpathy의 "LLM wiki"에서 가져왔다. 원문은 한 사람이 고른 자료를 LLM과 함께 정리하는 상황을 전제한다.
Bunsin은 다른 사람이 쓴 글을 혼자 읽으므로 아래 규칙을 더했다.

**저장**

- 페이지는 프로필의 DB에 있다: 슬러그, 제목, 한 줄 요약, 본문(마크다운), 출처 목록, revision, 작성 주체(agent 또는 owner).
  마크다운 폴더가 아니라 DB에 둔 이유는 승인과 페이지 쓰기를 한 트랜잭션으로 묶고, 변경 이력을 남기기 위해서이다.
- 색인은 따로 관리하지 않는다. 제목과 요약에서 만든다. `bunsind wiki-export`가 `index.md`와 `pages/*.md`로 내보낸다.
- 모든 변경은 이력 표에 남는다. 삭제도 한 줄로 남는다. `bunsind wiki-history`와 API가 이력을 보여 주고, 옛 본문은 `wiki-set`으로 되돌린다.

**쓰기: agent가 제안하고 사용자가 승인한다**

- LLM의 최종 답에 선택 항목 `wiki_updates`가 있다. 한 번에 최대 3쪽이고, 각 항목은 페이지 전체이다.
- **출처가 있어야 한다.** 그 이벤트에서 커넥터 도구로 읽은 것이 하나도 없으면 변경안을 버린다.
  notes를 읽은 것은 출처가 아니다. 그렇지 않으면 notes가 자기 자신을 근거로 삼는다.
  출처에는 처리한 메시지와 읽은 도구의 인자가 기록된다.
- **있는 페이지는 읽은 뒤에만 고친다.** 변경안은 페이지 전체를 바꾸므로, 그 이벤트에서 내장 도구로 그 페이지를 읽지 않았으면 버린다.
- 한 페이지에 대기 중인 변경안은 하나뿐이다. agent는 페이지를 지울 수 없다.
- 변경안은 승인함에 들어간다. 항목은 새 본문, 출처, 바꾸려는 이전 본문을 함께 보여 준다.
- **승인은 한 번만 적용된다.** 승인함 상태의 이동과 페이지 쓰기가 한 트랜잭션이다.
  변경안을 쓴 뒤에 페이지가 바뀌었으면 승인을 거부한다(revision 비교).
- 변경안의 승인은 정지 스위치가 켜져 있어도 된다. 다른 시스템에는 아무것도 쓰지 않기 때문이다.
- 제목, 요약, 본문은 저장하기 전에 개인정보 필터를 거친다. 버린 변경안은 이유와 함께 감사 로그에 남는다.

**읽기**

- 실시간 이벤트의 프롬프트에는 notes의 색인(한 줄에 한 쪽, 수정 후 지난 날수 포함)이 인용 데이터로 들어간다.
  - 순서: 이벤트 본문의 검색어와 일치한 후보 페이지를 점수순으로(4절의 검색), 나머지를 최근 수정순으로 둔다.
  - 최대 60줄이다. 넘치면 "N more page(s) not listed" 한 줄을 붙인다.
  - 기각: 상위 페이지의 본문을 프롬프트에 미리 넣기. 매번 token이 들고, "읽음"이 아니어서 그 페이지의 변경안이 버려진다.
- agent는 내장 도구 `bunsin.wiki/read`로 페이지를 읽는다. 이 ID는 커넥터 도구의 ID가 될 수 없다(커넥터 이름에는 `.`을 쓸 수 없다).
  이 도구는 권한 표와 pin 검사를 거치지 않고 엔진이 직접 처리한다. 프로필 자신의 페이지만 읽는다.
  읽으면 열 `wiki_pages.last_read_at`에 시각을 적는다. 점검(고아 페이지 찾기)이 쓸 값이다.
- 내장 도구 `bunsin.wiki/search {query}`는 상위 10쪽의 slug, 제목, 요약을 인용 데이터로 돌려준다. LLM은 검색어만 고른다.
  검색은 notes 읽기 한도 5번에 든다. 페이지를 읽은 것이 아니므로 그 페이지를 고칠 근거가 되지 않는다.
- `bunsin.wiki/`로 시작하는 도구는 모두 출처가 아니다. 그렇지 않으면 notes가 자기 자신을 근거로 삼는다.
- **페이지는 데이터이다.** 승인된 페이지의 문장도 인용 블록 안에만 들어간다. 시스템 프롬프트는 바뀌지 않고, 권한은 정책 표가 결정한다.
- 승인되지 않았거나 거부된 변경안은 어떤 프롬프트에도 들어가지 않는다.
- LLM에는 notes가 오래됐을 수 있으니, 중요한 사실은 도구로 다시 확인하라고 알린다.
- **재현 평가에는 notes를 넣지 않는다.** notes에는 그 이벤트 이후에 알게 된 사실이 있기 때문이다.
  시스템 프롬프트는 재현에서도 같으므로 notes 도구를 언급하지만, 재현은 어떤 도구도 실행하지 않는다.

**사용자의 직접 편집**

- 사용자는 API, 명령(`wiki-list`, `wiki-show`, `wiki-set`, `wiki-delete`), 창의 Notes 화면으로 페이지를 쓰고 지운다.
- 사용자의 검색은 `wiki_search`(API `POST .../wiki-search`, 명령 `wiki-search`)이다.
  agent의 `bunsin.wiki/search`와 같은 엔진 함수(`notes.rs`의 `find_pages`)와 같은 저장소 질의(`search_wiki`)를 쓴다. 그래서 순위가 같다.
  다른 점은 두 가지다. 사용자는 맞는 페이지를 모두 받는다(agent는 10쪽). 쓸 낱말이 없는 검색어는 `Invalid`로 거부한다(agent는 "없음"을 받는다).
  사용자의 검색은 agent의 읽기가 아니므로 감사 로그에 `read`로 남지 않고 `last_read_at`도 바꾸지 않는다.
  경로를 `wiki/search`로 두지 않았다. `search`는 올바른 slug라서 `GET wiki/{slug}`와 겹친다.
- Notes 화면의 규칙:
  - 입력한 글은 Save를 눌러야 서버로 간다. 새로고침은 입력한 글을 덮어쓰지 않는다.
    입력하지 않은 화면은 서버의 새 revision을 따라간다.
  - Save는 화면이 읽은 revision을 보낸다. 서버의 페이지가 바뀌었으면 경고하고, "Save anyway"로 덮어쓸 수 있다.
  - 새 페이지는 "그 이름의 페이지가 없어야 한다"는 조건으로 저장한다. 이름이 이미 있으면 덮어쓰는 방법을 주지 않는다.
  - 저장하지 않은 글이 있으면 다른 페이지나 새 페이지로 가기 전에 묻는다. 프로필을 바꾸면 묻지 않고 버린다.
  - 삭제는 같은 페이지에서 두 번 눌러야 한다. 삭제는 revision을 확인하지 않는다. 본문은 이력에 남는다.
  - 열린 페이지에 대기 중인 agent 변경안이 있으면 알린다. 여기서 저장하면 그 변경안은 승인할 수 없게 된다.
  - 바꾼 것이 없으면 Save는 아무것도 저장하지 않는다. 저장하면 새 revision이 사용자 이름으로 생기기 때문이다.
  - agent가 쓴 페이지를 사용자가 고쳐 저장하면 작성 주체가 사용자로 바뀐다. 출처 목록은 남는다. 화면이 이를 미리 알린다.
  - 글자 수 같은 한도는 서버만 검사한다. 창은 서버의 거부 사유를 그대로 보여 준다.
  - 목록 위의 검색란과 Search 버튼은 `UiCommand::SearchNotes`를 보낸다. bridge는 검색어를 `UiState::note_search`에 두고, 새로고침마다 다시 검색한다.
    목록은 그동안 검색 결과만 순위대로 보인다. "Show all"(`ClearNoteSearch`)이나 빈 검색어는 검색을 끝낸다. 프로필을 바꾸어도 끝난다.
    거부된 검색어는 연결 실패가 아니다. 창은 서버의 거부 사유를 검색 결과 자리에 보인다.
  - History 버튼은 열린 페이지의 revision 목록을 보여 준다(`wiki_history`). 보이는 동안 새로고침마다 다시 읽는다.
    고른 revision의 본문은 읽기 전용이다. "Put this text into the fields"는 옛 본문을 입력란에 넣을 뿐이고,
    저장은 평소의 Save가 화면이 읽은 revision으로 한다. 저장하지 않은 입력 내용이 있으면 옛 본문을 넣지 않는다.
- 사용자가 페이지를 고쳐도 그 페이지의 출처 목록은 남는다. 사용자가 새로 만든 페이지에는 출처가 없다.
  사용자가 고친 문장은 남아 있는 출처와 맞지 않을 수 있다. 작성 주체가 owner이면 본문은 사용자가 쓴 것이다.
- 사용자가 쓴 글은 저장할 때 거르지 않는다. 프롬프트에 인용할 때 필터를 거친다.
- 편집은 출발한 revision을 보낼 수 있고, 다르면 `Conflict`가 된다.

**점검**

- 설정 `notes: {stale_days, lint_every_hours, lint_conflicts}`(모두 `serde(default)`, 기본은 꺼짐). `stale_days`나 `lint_every_hours`가 0이면 저장을 거부한다.
  `lint_every_hours`가 있는데 `stale_days`도 `lint_conflicts`도 없으면 거부한다. 볼 것이 없는 점검은 `lint_notes`가 거부한다.
- stale: `stale_days`일보다 오래(경계 초과) 고치지 않은 페이지. orphan: 다른 페이지의 제목, 요약, 본문에 slug가 낱말로 없고
  (앞뒤에 영숫자나 `-`가 없음), `last_read_at`(없으면 마지막 변경)부터 `stale_days`일보다 오래 지난 페이지. 계산은 core `lint`(IO 없음).
- 모순(`lint_conflicts`): 페이지마다 제목, 요약, 본문의 검색어(4절의 `query_terms`)를 만들고, `df ≤ max(2, N/4)`인 드문 검색어를
  2개 이상 함께 쓰는 쌍을 공유 수 순서로 10개까지 고른다. 기각한 쌍은 고르지 않는다. 쌍의 페이지를 필터를 거쳐 `<data>`로 인용하고
  LLM에 한 번 묻는다(purpose `notes_check`). 답 `{conflicts: [{a, b, detail}]}`에서 보낸 쌍만 받고, 이유는 필터를 거쳐 300자로 자른다.
  기각: 모든 쌍을 묻기. 300쪽이면 44,850쌍이다.
- 결과는 표 `lint_findings(id, run_at, kind, slugs, detail, state)`에 둔다. 점검은 `new` 결과를 한 transaction으로 새 결과로 바꾼다.
  `dismissed` 결과는 남고, 같은 종류와 같은 slug의 결과는 다시 넣지 않는다. 모순 LLM 호출이 실패하면 아무것도 바꾸지 않고 거부한다(오류는 필터를 거친다).
- 점검 시각은 설정 표의 `notes_checked_at`에 시작할 때 적는다. `background_tick`은 `lint_every_hours`가 지났을 때만 점검한다.
  그래서 실패한 점검도 다음 간격까지 다시 돌지 않는다. 점검은 밖에 아무것도 보내지 않으므로 정지 스위치와 상관없이 돈다.
- 점검은 페이지를 고치지 않는다. 감사 `notes_checked`는 종류별 개수와 보낸 쌍의 수만 남긴다.
- API: `lint_notes`, `lint_findings`(`new`만, 찾은 순서), `dismiss_finding`(`POST .../lint-notes` 등, 같은 이름의 `bunsind` 명령).
  창의 Notes 화면은 오른쪽 열의 **Checks** 카드에 결과, "Check now", 결과마다 "Dismiss"를 보인다. 열은 스크롤한다.

**자동 반영**

- 설정 `notes.auto_apply`(기본 false)가 켜지면 `file_wiki_changes`가 변경안을 `pending`으로 넣은 뒤 같은 요청에서
  `approve_wiki_change`를 부른다. 그래서 출처 규칙, 읽은 뒤에만 고치기, revision 비교, 한 번만 적용이 승인과 같다.
- 조건: 정지 스위치가 꺼져 있다. 새 페이지이거나 마지막 작성 주체가 agent이다(owner 페이지는 덮어쓰지 않는다).
  이벤트의 `channel`이 `owner`이다.
- `InboundEvent.channel`은 이벤트가 온 길이다. 기본값은 `outside`이고(fail closed), `serde(skip_deserializing)`이라서
  요청 본문이나 저장된 payload의 값은 읽지 않는다. `owner`는 access token을 확인한 `POST .../events` route만 붙인다.
  poll의 추출(core `poll::extract`)은 `outside`를 붙인다. 검색은 소유자가 정했지만, 찾은 글은 다른 사람이 썼기 때문이다.
  webhook 해석기(`bunsin-connectors::webhook`)도 `outside`를 붙인다.
  인바운드 호출자는 아무것도 붙이지 않아도 `outside`이다. 다른 사람이 직접 넣는 이벤트는 agent를 조종하려고 쓸 수 있으므로,
  그 변경안은 언제나 소유자를 기다린다. dedupe key에는 들지 않는다.
- 저장된 이벤트를 다시 읽으면 `outside`이다. 다시 읽는 곳은 교정 기록의 task(`corrections.rs`)뿐이고, 자동 반영은 처리 중인 이벤트의 값만 보므로 동작이 바뀌지 않는다.
  엔진 안에서 `EngineApi::ingest`를 직접 부르는 코드는 `owner`를 스스로 붙여야 한다.
- 감사 `wiki_change_applied`에 `auto`가 붙는다. 저장하지 못하면(예: 그 사이 소유자의 편집) 항목은 `pending`으로 남고 감사 `wiki_change_auto_apply_failed`가 남는다.
- 결정 기록(4절)에는 들지 않는다. 기각: owner 페이지 덮어쓰기.

**미리 읽기**

- `preload_notes{tool, args}`(`POST .../preload-notes`, `bunsind preload-notes`, 창의 "Read into the notes"): 소유자가 시작한다.
  도구는 `read` 규칙이 있고 `decide`가 `Execute`여야 한다(poll의 `fetch`, 원칙 후보의 read 출처와 같은 `read_once`). 한 번 부른다.
- 고정된 요청만 담은 이벤트(source `bunsin.preload`, 작성자는 소유자, `channel` `outside`)를 만들고, 그 읽기를 첫 읽기로 넘겨
  `propose`를 돌린다(LLM 호출 로그의 purpose `preload`). 결과(필터를 거치고 20,000자로 자른 글)는 이벤트 뒤에 `<data source="tool-result">`로만 들어간다.
  요청 글과 섞지 않는다. 변경안의 출처 규칙이 그대로 지켜진다: 출처는 이벤트 줄과 그 도구의 인자이다. LLM은 평소의 한도 안에서 더 읽을 수 있다.
- 변경안은 `file_wiki_changes`로 승인함에 가고, 자동 반영은 하지 않는다. 소유자가 시작했지만 읽은 글은 다른 사람이 썼기 때문이다.
  두 겹으로 막는다: 이벤트의 `channel`이 `outside`이고, `file_wiki_changes`가 source `bunsin.preload`의 변경안을 자동 반영하지 않는다. `ignore`가 아닌 제안은 실행하지 않고 감사 `preload_proposal_skipped`(종류만)를 남긴다.
  제안이 실패하면(수리 뒤에도 형식 오류, LLM 오류) 거부하고 아무것도 넣지 않는다. 감사 `notes_preloaded`는 도구와 변경안 수만 남긴다.
- 실제 Confluence나 Jira 검색 도구로는 실행해 보지 않았다.

**아직 없는 것**
- 사용자가 출처 문서의 열람 권한을 잃었을 때 페이지를 다시 확인하는 기능.
- 한계: 쪽수는 300으로 제한한다. 색인은 60줄까지만 프롬프트에 들어가고, 나머지는 검색으로 찾는다.

## 16. 구현 상태 (PLAN 6절의 단계 기준)

| 항목 | 상태 |
|---|---|
| 정책, 제안 검증, 승인 상태 머신, AI 표시(여러 자리, 다른 값 안의 자리), pin 검사 | 구현, 테스트 있음 |
| 프로필별 저장소, 산출물 저장, 추가 전용 감사 로그 | 구현, 테스트 있음 |
| 엔진 파이프라인(중복 제거, 수리 1회, 정책, 승인, 1회 실행, 정지 스위치, 재현 평가의 제안 단계) | 구현, 테스트 있음 |
| LLM 어댑터, MCP 커넥터, 코딩 위임 실행기 | 구현, 로컬 stub으로만 테스트 |
| 코딩 위임(설정, 내장 도구, 언제나 승인, 운영자 플래그와 root 검사, 백그라운드 실행, 결과 산출물과 Draft) | 구현, 테스트 있음(core, 엔진, 서버, 창). 서버 테스트는 임시 git 저장소와 `/bin/sh`를 쓴다. 실제 Claude Code와 Codex는 실행하지 않았다. |
| GPUI 창(접속 화면, 프로필, 정지 스위치, Inbox와 감사 로그, Notes, Permissions, Agent, Learning, Settings, Setup) | 구현, headless 테스트와 offscreen 그림만. 실제 창은 띄워 보지 않았다. |
| `bunsind serve`(HTTPS 서버, 제어·내부·공개 리스너), 접속 token, 원격 client, `bunsind`의 원격 명령 | 구현, 테스트 있음. CI가 Linux(ubuntu)에서 빌드와 테스트를 돌린다. Linux에서 서비스로 실행해 보지는 않았다. |
| 이벤트 폴링, 실패한 이벤트의 재시도, 도구 목록 조회와 승인 | 구현, 가짜 커넥터로만 테스트 |
| 개인정보 필터(기본 4종, 사용자 정의 패턴, 단어 목록) | 구현, 테스트 있음 |
| 연결 설정과 봉인 저장소(`secrets.rs`: 소유자가 넣은 token, 비밀 환경 변수, OAuth credentials, webhook secret. XChaCha20-Poly1305, 키 파일이나 Keychain, origin 결합) | 구현, 테스트 있음. 키의 Keychain 저장은 가짜 Keychain으로만 테스트했다. |
| webhook 방식의 이벤트 수신(Slack, Jira Cloud: 서명, url_verification, 전달 ID 중복 제거, intake, 자기 글, 설정 검사, 봉인된 secret) | 구현, 테스트 있음(core, store, 서버, CLI 명령, 창). 실제 Slack과 Jira로는 보내 보지 않았다. |
| 읽기 도구의 결과를 LLM에 되돌리는 여러 단계 처리 | 구현, 테스트 있음. 커넥터 문서의 색인과 순위는 없음. |
| 설정 저장의 충돌 검사 | 구현, 테스트 있음 |
| LLM 호출 로그와 사용량 합계(`llm-usage`) | 구현, 테스트 있음. 창에는 없음. |
| 권한 규칙의 명령(`rules`, `rule-set`, `rule-remove`) | 구현, 테스트 있음 |
| agent의 notes(제안, 승인, 관련도 순 색인, 내장 읽기와 검색 도구, 사용자 편집과 검색, 내보내기, 점검, opt-in 자동 반영, 미리 읽기) | 구현, 테스트 있음. 모순 점검은 실제 LLM으로 실행해 보지 않았다. |
| 재현 평가(사례 저장, 백그라운드 실행, LLM 없는 채점, 소유자의 통과 기준, opt-in LLM 심판) | 구현, 테스트 있음(core, store, 엔진, 서버, CLI, 창). 창은 Setup 화면에서만 사례를 추가한다. 통과 기준과 심판은 Settings 화면에서 정한다. |
| 원칙 후보(추출, 거부 규칙, 채택, 기각) | 구현, 테스트 있음(core, store, 엔진, 서버, CLI, 창). `read` 출처는 명령과 Setup 화면에 있다. |
| 교정 기록(필터, AI 표시 없음, 유사 작업 검색, 목록, 폐기) | 구현, 테스트 있음(엔진, 서버, CLI, 창) |
| 승인 화면의 초안 편집 | 구현, 테스트 있음(엔진, CLI, 창) |
| REST API 커넥터(Slack, Jira, Confluence의 고정 도구, 설정, 저장, CLI, 창) | 구현, 테스트 있음(core, connectors, 엔진, 서버, CLI, 창). 로컬 stub으로만 테스트 |
| MCP 커넥터의 OAuth(설정, 발견, 등록, PKCE, 봉인, 갱신, 다시 로그인 상태, CLI, 창의 loopback 로그인) | 구현, 테스트 있음(core, 엔진, 서버, CLI, 창). 로컬 stub 인가 서버로만 테스트. 설정 파일로만 켠다. |
| 구성 마법사(창의 Setup 화면, 일곱 단계, 기존 호출만 사용) | 구현, headless 창 테스트만. 명령줄 마법사는 없고 README에 명령 순서가 있다. |
| 인바운드 요청(설정, intake, 한도, 답 도구, 요청 상태, 호출자 token, 내부·공개 리스너, MCP endpoint, server card) | 구현, 테스트 있음(core, store, 엔진, 서버, CLI, 창). MCP client는 `McpConnector`(rmcp)로 실제 TCP에서 테스트했다. 다른 MCP client로는 해 보지 않았다. |
| ARD 게시(설정, 운영자 도메인 목록, 리스너별 catalog, 소유자의 상태 조회) | 구현, 테스트 있음(core, 서버, CLI, 창). 게시 본문은 스펙 JSON 스키마 검사를 통과한다. 공식 conformance 도구는 실행해 보지 않았다. |
| ARD 검색(내장 읽기 도구, referral 없음, 주소 규칙, 읽기 한도) | 구현, 테스트 있음(core, 서버, 창의 목록 편집). 로컬 stub registry로만 테스트 |
| 좁은 자율(결정 기록, 도구별 개수, 기준에 따른 제안, 거부·신고로 보류, 소유자만 해제) | 구현, 테스트 있음(core, store, 엔진, 서버, CLI, 창) |
| 서버가 멈춘 뒤의 재개(`executing`→`unknown`, 시작하지 않은 `approved`의 정책 재확인과 실행 또는 사본), 답이 없는 승인 호출(시간 초과)의 `unknown`, `unknown`의 처리(done, retry, drop) | 구현, 테스트 있음(connectors, 엔진, 서버, CLI, 창) |
| 배포 패키지(Linux 서버 tarball과 systemd unit, macOS `Bunsin.app`, CI artifact) | 구현. macOS에서 두 스크립트를 실행해 확인. unit은 systemd로 시작해 보지 않았고, 앱은 서명하지 않았다. |

## 17. 미해결

- 코딩 백엔드의 편집 허용 설정(예: Claude Code의 permission mode, Codex의 sandbox 설정)을 opt-in으로 둘지.
- 평문 `env`에 이미 들어 있는 비밀 값을 비밀 변수로 옮기는 기능. 지금은 사용자가 다시 입력하고 평문 줄을 지운다.
- 설정 파일이나 CLI로 비밀 변수의 값을 넣는 방법. 지금은 창에서만 입력한다.
- OAuth를 창이나 `connector-set`에서 켜는 방법. 지금은 설정 파일로만 켠다.
- 임베딩 검색의 제공 여부.
- macOS 앱의 bundle ID와 Developer ID 서명의 주체. 지금은 서명하지 않고, bundle ID는 로컬용 기본값이다. 앱 아이콘도 없다.
- 인바운드 호출자의 OAuth. 지금은 호출자 token만 있다.
