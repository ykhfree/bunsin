# Bunsin 구조

이 문서는 `PLAN.md`의 요구사항을 구현하는 구조를 적습니다.
각 판단에는 `(검증)` 또는 `(가정)`을 붙였습니다. `(검증)`은 2026-11-14에 macOS에서 빌드나 원문으로 확인한 것입니다.

## 1. 프로세스

```
 GPUI app ──(EngineApi)──> engine <──(EngineApi)── daemon CLI
                             │
        ┌────────┬───────────┼───────────┬─────────┐
       llm   connectors    store       coding     core (sans-IO)
```

- `engine`은 라이브러리이다. headless `daemon`과 GPUI `app`이 같은 `engine`을 쓴다.
- 운영에서는 `daemon`이 엔진을 실행한다. app의 창을 닫아도 agent는 계속 동작한다.
- app은 `EngineApi` trait만 본다. 구현은 in-process와 원격 두 가지이다. 원격 구현은 아직 없다.
- data dir 하나에 엔진 하나만 뜬다. `engine.lock` 파일의 배타 잠금이 이중 폴링과 이중 실행을 막는다.
- 제어 API는 Unix domain socket으로 제공할 계획이다. 공개 리스너(ARD, 인바운드 MCP)는 별도로 둔다.
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
| `bunsin-api` | `EngineApi` trait과 DTO | core |
| `bunsin-daemon` | 프로필 설정으로 실제 LLM client와 커넥터를 만드는 `RealBackends`, headless CLI `bunsind` | engine, api, llm, connectors |
| `bunsin-app` | GPUI 창. 지금은 엔진을 같은 프로세스에서 실행한다. | api, engine, daemon |
| `bunsin-testkit` | 테스트용 가짜 LLM, 가짜 커넥터 | core |

- `bunsin-core`는 tokio, reqwest, rusqlite, gpui에 의존하지 않는다. 시각은 호출자가 넣는다.
- `bunsin-daemon`의 의존성 트리에 gpui가 없어야 한다. CI가 이를 검사한다.

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
- 비밀 값은 설정에 저장하지 않는다. 설정은 환경 변수의 이름(`token_env`)만 가진다. `bunsind`와 app이 실행될 때 그 환경 변수에서 값을 읽는다.
  Keychain 같은 비밀 저장소 연동은 아직 없다.

## 5. LLM 어댑터

- `bunsin-core`가 `CompletionRequest`와 `CompletionResponse`를 정의한다. 하네스는 wire 형식을 모른다.
- `bunsin-llm`의 `LlmClient` trait이 프로필마다 `Arc<dyn LlmClient>`로 쓰인다.
- wire 형식은 프로필 설정값이다: `openai-chat`, `anthropic-messages`. `openai-responses`는 아직 없다.
- 행동 제안은 구조화 출력으로 받는다. 도구 호출을 강제할 수 없는 모델이 있기 때문이다.
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

## 8. MCP 커넥터

- `McpConnector`는 공식 Rust SDK `rmcp =3.10.0`의 client이다. transport는 streamable HTTP와 stdio 자식 프로세스이다.
- stdio 명령은 절대 경로만 받는다.
- rmcp의 도구 목록 cache를 끈다. cache된 목록은 서버의 변경을 숨겨 pin 검사를 무력하게 만들기 때문이다.
- 연결은 30초 안에 끝나야 한다. 시간이 지나면 `RealBackends`가 오류를 돌려준다.
- 커넥터는 권한을 판단하지 않는다. 엔진이 호출 전에 정책과 pin을 검사한다.
- OAuth는 아직 없다. HTTP transport는 bearer token만 보낸다.
- 실제 Slack, Jira, Confluence MCP 서버로는 아직 연결해 보지 않았다. 테스트는 로컬 rmcp 서버를 쓴다.

## 9. 구현 상태 (PLAN 6절의 단계 기준)

| 항목 | 상태 |
|---|---|
| 정책, 제안 검증, 승인 상태 머신, AI 표시, pin 검사 | 구현, 테스트 있음 |
| 프로필별 저장소, 산출물 저장, 추가 전용 감사 로그 | 구현, 테스트 있음 |
| 엔진 파이프라인(중복 제거, 수리 1회, 정책, 승인, 1회 실행, 정지 스위치, 재현 평가의 제안 단계) | 구현, 테스트 있음 |
| LLM 어댑터, MCP 커넥터, 코딩 위임 실행기 | 구현, 로컬 stub으로만 테스트 |
| GPUI 창(프로필, 승인함, 감사 로그, 정지 스위치) | 구현, headless 테스트만. 실제 창은 띄워 보지 않았다. |
| `bunsind` CLI | 구현. 명령 하나가 엔진을 열고 닫는다. |
| 이벤트 폴링, 상주 데몬, 제어 소켓 | 없음 |
| 읽기 도구의 결과를 LLM에 되돌리는 여러 단계 처리 | 없음. 지금은 이벤트 하나에 제안 하나이다. |
| 업무지식 검색, 원칙 후보 추출, 재현 평가의 채점 | 없음 |
| 승인 화면의 초안 편집 | 엔진과 CLI에만 있음. 창에는 없음. |
| 구성 마법사, API 커넥터, 비밀 저장소 | 없음 |
| ARD 게시와 검색, 인바운드 MCP 서버 | 없음 |
| 승인 후 실행 전에 중단된 항목의 재개 | 없음. 항목은 `approved` 또는 `executing` 상태로 남고 다시 실행되지 않는다. |

## 10. 미해결

- 사용자의 LLM endpoint가 쓰는 wire 형식.
- 코딩 백엔드의 편집 허용 설정(예: Claude Code의 permission mode, Codex의 sandbox 설정)을 opt-in으로 둘지.
- stdio MCP 서버가 env로 받는 비밀 값. 지금 구조에서는 `McpConfig.env`에 들어가 설정과 함께 저장된다.
- MCP 서버의 인증 방식. OAuth이면 headless 환경의 흐름이 필요하다.
- 임베딩 검색의 제공 여부.
- 라이선스.
