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
| `bunsin-core` | 정책, 제안 검증, 승인 상태 머신, AI 표시, 도구 pin 비교. IO가 없다. | 없음 (serde만) |
| `bunsin-store` | SQLite 저장소, 산출물 파일, 감사 로그 | core |
| `bunsin-llm` | LLM 어댑터 | core |
| `bunsin-connectors` | MCP 커넥터, API 커넥터 | core |
| `bunsin-coding` | 코딩 위임 실행기 | core |
| `bunsin-engine` | 하네스 파이프라인 | core, store, llm, connectors, coding |
| `bunsin-api` | `EngineApi` trait과 DTO | core |
| `bunsin-daemon` | headless 실행 파일과 CLI | engine, api |
| `bunsin-app` | GPUI 창 | api (+ engine) |
| `bunsin-testkit` | 테스트용 가짜 LLM, 가짜 커넥터 | core |

- `bunsin-core`는 tokio, reqwest, rusqlite, gpui에 의존하지 않는다. 시계와 ID는 호출자가 넣는다.
- `bunsin-daemon`의 의존성 트리에 gpui가 없어야 한다. CI가 이를 검사한다.

## 3. GPUI

- 의존성은 `gpui-kit = "=0.9.1"`이다. 이 crate는 `gpui-pre =0.3.12`와 `gpui-component`를 묶는다 (검증: Cargo.toml).
- 공식 `gpui` crate는 0.2.2에서 멈춰 있어 쓰지 않는다 (검증: crates.io).
- rustc 1.96에서는 `gpui-pre-macos` 0.3.12가 빌드에 실패한다. rustc 1.98.1에서는 빌드된다 (검증: /tmp 빌드).
  그래서 `rust-toolchain.toml`이 1.98.1을 고정한다.
- 위험: `gpui-pre`와 `gpui-kit`의 crates.io 소유자는 개인 1명이다 (검증: crates.io).
  대응: 버전을 `=`로 고정하고 `Cargo.lock`을 커밋한다. UI 코드는 `bunsin-app`에만 둔다.
- UI와 엔진은 `tokio::sync` 채널로만 통신한다. GPUI 실행기에서 tokio 타이머를 poll하면 panic이 난다 (검증: 테스트).
- 실제 창의 표시와 Linux 빌드는 확인하지 않았다.

## 4. 저장소

- SQLite는 `rusqlite`의 `bundled` 기능으로 쓴다 (검증: 빌드).
- 프로필마다 DB 파일을 따로 둔다: `profiles/<id>/agent.db`. 공용 `app.db`는 프로필 목록만 가진다.
  기각한 안: DB 하나에 `profile_id` 컬럼. `WHERE` 조건 하나를 빠뜨리면 다른 프로필의 데이터가 샌다.
- 산출물은 `profiles/<id>/artifacts/sha256/<해시>` 파일로 저장하고, DB에 메타 행을 둔다.
- 감사 로그 테이블은 트리거로 UPDATE와 DELETE를 거부한다.
- 검색은 FTS5 `trigram`을 쓴다. trigram은 2음절 한국어 검색어를 찾지 못한다 (검증: 빌드). 2음절은 `LIKE`로 찾는다.
- 비밀 값은 `SecretStore` 뒤에 둔다. DB에는 참조만 저장한다.
  macOS는 Keychain을 쓴다. headless Linux는 파일 기반 저장을 쓴다 (가정).

## 5. LLM 어댑터

- `bunsin-core`가 `CompletionRequest`와 `CompletionResponse`를 정의한다. 하네스는 wire 형식을 모른다.
- `bunsin-llm`의 `LlmClient` trait이 프로필마다 `Arc<dyn LlmClient>`로 쓰인다.
- wire 형식은 프로필 설정값이다: `openai-chat`, `openai-responses`, `anthropic-messages`.
- 행동 제안은 구조화 출력으로 받는다. 도구 호출을 강제할 수 없는 모델이 있기 때문이다.
- 검증 순서: serde(`deny_unknown_fields`) → 도메인 검사 → 정책. 실패하면 LLM에 수리를 한 번 요청한다.

## 6. 코딩 위임

- `CodingRunner`가 git worktree, 브랜치 `agent/<job>`, diff, 테스트 실행을 맡는다.
- 백엔드는 두 가지만 구현한다: 실행할 명령(`CommandSpec`)과 출력 해석.
- `CommandSpec`은 실행 파일의 절대 경로를 가진다. macOS GUI app의 PATH에는 사용자의 CLI 경로가 없기 때문이다.
- 결과(`CodingResult`)는 브랜치, base/head, diff 요약, 테스트 명령과 출력, 종료 상태, 기록 파일을 가진다.

## 7. 인바운드 호출 인터페이스

- v1은 MCP 서버 카드를 쓴다. A2A는 두 번째 프로토콜 스택이 되므로 미룬다.
- 도구는 둘이다: `ask`는 즉시 `{request_id, pending_approval}`을 돌려주고, `get_request(id)`가 결과를 돌려준다.
- 인바운드 요청은 신뢰하지 않는 데이터로 수신 단계에 들어간다. 승인함을 우회하지 않는다.
- 인증은 프로필별 호출자 bearer token이다. token은 해시로 저장한다. OAuth는 다음 단계이다.

## 8. 미해결

- LLM endpoint의 wire 형식.
- MCP 서버의 인증 방식. OAuth이면 headless 환경의 흐름이 필요하다.
- 임베딩 검색의 제공 여부.
- 라이선스.
