# Bunsin 구조

이 문서는 `PLAN.md`의 요구사항을 구현하는 구조를 적습니다.
각 판단에는 `(검증)` 또는 `(가정)`을 붙였습니다. `(검증)`은 2026-11-14에 macOS에서 빌드나 원문으로 확인한 것입니다.

## 1. 프로세스

```
[서버]   bunsind serve ── engine ── llm, connectors, store, core
              ▲
              │  HTTPS + 프로필별 접속 token   (bunsin-api::http)
              ▼
[내 컴퓨터]  GPUI app, bunsind의 원격 명령   (bunsin-client)
```

- 엔진은 서버의 `bunsind serve` 프로세스 안에서만 돈다. 화면이 없는 서버에서 동작한다.
- GPUI app은 관리 화면이다. app은 엔진을 품지 않고, `EngineApi`의 원격 구현(`RemoteEngine`)으로 서버에 접속한다.
- 같은 컴퓨터에서 시험할 때는 `bunsind serve`를 `127.0.0.1`에 띄우고 app이 그 주소로 접속한다.
- data dir 하나에 엔진 하나만 뜬다. `engine.lock` 파일의 배타 잠금이 이중 폴링과 이중 실행을 막는다.
- 서버 운영자의 명령(`profile-add`, `token-add`, `token-list`, `token-revoke`)은 저장소 파일을 직접 다룬다.
  엔진 잠금이 필요 없으므로 서버가 도는 동안에도 실행된다. 두 프로세스가 같은 SQLite 파일을 쓰므로 busy timeout을 둔다.
- 서버는 SIGTERM이나 Ctrl-C를 받으면 새 요청을 받지 않고, 실행 중인 요청이 끝난 뒤에 종료한다.
- 공개 리스너(ARD, 인바운드 MCP)는 아직 없다. 만들 때 제어 API와 분리한다.
- 기각한 안: GPUI 바이너리 하나에 엔진을 넣는 구조. 창을 닫으면 agent가 멈추고, Linux 서버에 디스플레이가 필요하다.

## 2. Crate

| Crate | 역할 | 의존 |
|---|---|---|
| `bunsin-core` | 정책, 제안 검증, 승인 상태 머신, AI 표시, 도구 pin 비교, `LlmClient`와 `Connector` trait. IO가 없다. | 없음 |
| `bunsin-store` | SQLite 저장소, 산출물 파일, 감사 로그 | core |
| `bunsin-llm` | LLM 어댑터 | core |
| `bunsin-connectors` | MCP 커넥터, API 커넥터 | core |
| `bunsin-coding` | 코딩 위임 실행기 | core |
| `bunsin-engine` | 하네스 파이프라인 | core, store, llm, connectors, coding |
| `bunsin-api` | `EngineApi` trait과 DTO, HTTP 경로와 요청 본문(`http` 모듈) | core |
| `bunsin-client` | `RemoteEngine`: HTTPS로 서버에 접속하는 `EngineApi` 구현 | api |
| `bunsin-daemon` | HTTP 서버, 프로필 설정으로 실제 LLM client와 커넥터를 만드는 `RealBackends`, 실행 파일 `bunsind` | engine, api, client, llm, connectors, store |
| `bunsin-app` | GPUI 창 | api, client |
| `bunsin-testkit` | 테스트용 가짜 LLM, 가짜 커넥터 | core |

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
- 실제 창의 표시와 Linux 빌드는 확인하지 않았다.

## 4. 저장소

- SQLite는 `rusqlite`의 `bundled` 기능으로 쓴다 (검증: 빌드).
- 프로필마다 DB 파일을 따로 둔다: `profiles/<id>/agent.db`. 공용 `app.db`는 프로필 목록만 가진다.
  기각한 안: DB 하나에 `profile_id` 컬럼. `WHERE` 조건 하나를 빠뜨리면 다른 프로필의 데이터가 샌다.
- 산출물은 `profiles/<id>/artifacts/sha256/<해시>` 파일로 저장하고, DB에 메타 행을 둔다.
- 감사 로그 테이블은 트리거로 UPDATE와 DELETE를 거부한다.
- 업무지식의 로컬 검색은 아직 없다. 계획은 FTS5 `trigram`이다. trigram은 2음절 한국어 검색어를 찾지 못하므로 (검증: /tmp 빌드) 2음절은 `LIKE`로 찾는다.
- 비밀 값은 설정에 저장하지 않는다. 9절을 본다.

## 5. LLM 어댑터

- `bunsin-core`가 `CompletionRequest`와 `CompletionResponse`를 정의한다. 하네스는 wire 형식을 모른다.
- `bunsin-llm`의 `LlmClient` trait이 프로필마다 `Arc<dyn LlmClient>`로 쓰인다.
- wire 형식은 프로필 설정값이다: `openai-chat`, `anthropic-messages`. `openai-responses`는 아직 없다.
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
  편집을 허용하는 opt-in 설정은 아직 없고, 실제 CLI로 실행해 보지 않았다.
- 실행기는 아직 엔진에 연결되지 않았다.

## 7. 인바운드 호출 인터페이스

- v1은 MCP 서버 카드를 쓴다. A2A는 두 번째 프로토콜 스택이 되므로 미룬다.
- 도구는 둘이다: `ask`는 즉시 `{request_id, pending_approval}`을 돌려주고, `get_request(id)`가 결과를 돌려준다.
- 인바운드 요청은 신뢰하지 않는 데이터로 수신 단계에 들어간다. 승인함을 우회하지 않는다.
- 인증은 프로필별 호출자 bearer token이다. token은 해시로 저장한다. OAuth는 다음 단계이다.

## 8. AI 표시

- 권한 표의 모든 규칙은 `outgoing_text`를 반드시 가진다. 값은 `"none"`(다른 사람에게 글을 보내지 않음) 또는 `{"arg": "<인자 이름>"}`이다.
  이 필드가 없거나 이름이 틀린 규칙은 설정을 읽을 때 거부된다.
- 엔진은 그 인자에 AI 표시를 붙인다. 인자가 없거나 문자열이 아니면 호출을 만들지 않는다.
- 한계: 표시는 문자열 인자 하나만 다룬다. `text`와 `blocks`를 함께 받는 도구에서는 `blocks`의 글에 표시가 붙지 않는다.
- 한계: 규칙에 `"none"`을 잘못 적으면 표시 없이 나간다. 이 선택은 사용자의 책임이다.

## 9. 서버 API와 프로필 격리

서버는 여러 사람의 프로필을 담고, 사외에 노출될 수 있다. 그래서 아래 규칙을 코드로 강제한다.

**접속 token**

- 접속 token 하나는 프로필 하나에만 통한다. 서버는 token을 먼저 확인하고, 그다음에 프로필을 본다.
  다른 프로필에 대한 요청과 없는 프로필에 대한 요청은 같은 오류(401)를 받는다.
- token은 OS 난수 32바이트로 만든다. 저장소에는 SHA-256 해시만 저장한다. token은 발급할 때 한 번만 출력된다.
- 서버는 요청마다 token 표를 읽는다. 운영자가 token을 폐기하면 다음 요청부터 거부된다.
- 프로필 생성과 token 발급은 네트워크 API에 없다. 서버 운영자가 서버에서 명령으로 한다.
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

- **비밀 값**: 설정은 비밀 값의 key만 가진다(`token_secret`). 서버는 환경 변수 `BUNSIN_SECRET_<프로필>__<KEY>`에서 값을 읽는다.
  key에는 `__`를 쓸 수 없다. 그래서 한 프로필은 다른 프로필의 비밀 값이나 서버의 다른 환경 변수를 가리킬 수 없다.
- **stdio 커넥터**: 서버에서 명령을 실행하므로 기본으로 거부한다. 운영자가 `--allow-stdio-connectors`로 켠다.
- **LLM과 MCP 주소**: 기본 규칙은 공개 `https://` 주소만 허용한다. `localhost`와 loopback, 사설, link-local 같은 내부 IP 주소는 거부한다.
  운영자가 `--allow-upstream-host <host>`를 주면 그 목록의 host만 허용한다. 사내 LLM gateway는 이 옵션으로 허용한다.
  - 한계: 기본 규칙은 이름을 조회하지 않는다. 내부 주소로 풀리는 공개 도메인 이름은 통과한다. 이를 막아야 하는 운영자는 목록을 준다.
- LLM client와 MCP HTTP client는 redirect를 따르지 않는다. redirect로 위 규칙을 우회할 수 없다.

**아직 없는 것**

- 서버 운영자로부터의 보호. 서버 파일에 접근할 수 있는 사람은 프로필 데이터를 읽을 수 있다.
- 접속 실패 횟수 제한. token이 256비트 난수이므로 두지 않았다.
- 비밀 값 저장소. 지금은 서버 프로세스의 환경 변수만 읽는다.

## 10. MCP 커넥터

- `McpConnector`는 공식 Rust SDK `rmcp =3.10.0`의 client이다. transport는 streamable HTTP와 stdio 자식 프로세스이다.
- stdio 명령은 절대 경로만 받는다.
- rmcp의 도구 목록 cache를 끈다. cache된 목록은 서버의 변경을 숨겨 pin 검사를 무력하게 만들기 때문이다.
- 연결은 30초 안에 끝나야 한다. 시간이 지나면 `RealBackends`가 오류를 돌려준다.
- 커넥터는 권한을 판단하지 않는다. 엔진이 호출 전에 정책과 pin을 검사한다.
- OAuth는 아직 없다. HTTP transport는 bearer token만 보낸다.
- HTTP transport는 redirect를 따르지 않는다.
- 실제 Slack, Jira, Confluence MCP 서버로는 아직 연결해 보지 않았다. 테스트는 로컬 rmcp 서버를 쓴다.

## 11. 구현 상태 (PLAN 6절의 단계 기준)

| 항목 | 상태 |
|---|---|
| 정책, 제안 검증, 승인 상태 머신, AI 표시, pin 검사 | 구현, 테스트 있음 |
| 프로필별 저장소, 산출물 저장, 추가 전용 감사 로그 | 구현, 테스트 있음 |
| 엔진 파이프라인(중복 제거, 수리 1회, 정책, 승인, 1회 실행, 정지 스위치, 재현 평가의 제안 단계) | 구현, 테스트 있음 |
| LLM 어댑터, MCP 커넥터, 코딩 위임 실행기 | 구현, 로컬 stub으로만 테스트 |
| GPUI 창(프로필, 승인함, 감사 로그, 정지 스위치) | 구현, headless 테스트만. 실제 창은 띄워 보지 않았다. |
| `bunsind serve`(HTTPS 서버), 접속 token, 원격 client, `bunsind`의 원격 명령 | 구현, 테스트 있음. Linux에서는 빌드해 보지 않았다. |
| 이벤트 폴링 | 없음. 이벤트는 `ingest` 명령이나 API로 넣는다. |
| 읽기 도구의 결과를 LLM에 되돌리는 여러 단계 처리 | 없음. 지금은 이벤트 하나에 제안 하나이다. |
| 업무지식 검색, 원칙 후보 추출, 재현 평가의 채점 | 없음 |
| 승인 화면의 초안 편집 | 엔진과 CLI에만 있음. 창에는 없음. |
| 구성 마법사, API 커넥터, 비밀 저장소 | 없음 |
| ARD 게시와 검색, 인바운드 MCP 서버 | 없음 |
| 승인 후 실행 전에 중단된 항목의 재개 | 없음. 항목은 `approved` 또는 `executing` 상태로 남고 다시 실행되지 않는다. |

## 12. 미해결

- 사용자의 LLM endpoint가 쓰는 wire 형식.
- 코딩 백엔드의 편집 허용 설정(예: Claude Code의 permission mode, Codex의 sandbox 설정)을 opt-in으로 둘지.
- stdio MCP 서버가 env로 받는 비밀 값. 지금 구조에서는 `McpConfig.env`에 들어가 설정과 함께 저장된다.
- 서버의 Linux 빌드와 systemd 실행 설정.
- MCP 서버의 인증 방식. OAuth이면 headless 환경의 흐름이 필요하다.
- 임베딩 검색의 제공 여부.
- 라이선스.
