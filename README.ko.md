# Bunsin (분신)

[English](README.md)

Bunsin은 페르소나 AI agent를 실행하는 서버와 관리 도구입니다. agent 하나는 한 사람의 업무 원칙을 가지고,
MCP 서버를 통해 Slack, Jira, Confluence에서 그 사람을 대신해 일합니다.
서버 하나가 서로 격리된 여러 agent 프로필을 담습니다.

LLM은 제안만 합니다. 무엇을 실행할지는 하네스가 결정합니다.

- 프로필마다 권한 표가 허용된 도구를 정합니다. 표에 없는 도구는 거부됩니다.
- 모든 쓰기는 "소유자가 승인한 뒤 실행"에서 시작합니다. 소유자가 도구별로 단계를 올립니다.
- agent가 다른 사람에게 보내는 모든 글에 하네스가 AI 표시를 붙입니다.
- 프로필마다 정지 스위치가 있고, 켜면 모든 쓰기가 멈춥니다.
- 모든 제안, 초안, 결과를 저장합니다. 수정할 수 없는 감사 로그가 각 단계를 기록합니다.

## 현재 한계

Bunsin은 개발 초기 단계입니다. 사용하기 전에 아래 목록을 읽으십시오.

- **이벤트 수신은 사용하는 MCP 서버에 달려 있습니다.** 서버는 설정에 적힌 검색 도구를 반복 호출합니다
  ([이벤트 찾기](#이벤트-찾기-폴링) 참고). 이 기능은 테스트 서버로만 실행해 보았습니다. webhook 수신은 없습니다.
- **이벤트 하나에 제안 하나입니다.** agent가 도구 결과를 읽고 이어서 작업하는 기능이 아직 없습니다.
- **개인정보 필터는 패턴으로 동작합니다.** 이메일, 전화번호 같은 정해진 형식은 찾지만, 사람 이름과 주소는 찾지 못합니다.
- **구성 마법사가 없습니다.** 설정을 JSON 파일로 편집합니다.
- **아직 만들지 않은 것:** 업무지식 검색, ARD 게시와 검색, REST API 커넥터, MCP 서버용 OAuth, 재현 평가의 채점.
- **로컬 테스트 서버로만 시험했습니다.** 실제 LLM endpoint나 실제 MCP 서버로 실행하는 테스트는 없습니다.
- **데스크톱 창은 헤드리스로만 시험했습니다.** Keychain 저장 코드는 테스트가 없습니다.
- **서버 운영자는 프로필 데이터를 읽을 수 있습니다.** 프로필은 서로에게서 격리되어 있지만,
  서버 파일에 접근할 수 있는 사람에게서는 격리되어 있지 않습니다.

구현된 항목의 전체 목록은 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)에, 계획은 [docs/PLAN.md](docs/PLAN.md)에 있습니다.

## 구성 요소

| 구성 요소 | 설명 | 실행 환경 |
|---|---|---|
| `bunsind serve` | 서버. agent를 실행하고 데이터를 저장합니다. | Linux, macOS |
| `bunsind <명령>` | 운영자와 프로필 소유자가 쓰는 명령줄 도구 | Linux, macOS |
| `bunsin-app` | 데스크톱 창: 승인함, 감사 로그, 정지 스위치 | macOS |

## 빌드와 테스트

```sh
cargo build --release -p bunsin-daemon     # 서버와 명령줄 도구: target/release/bunsind
cargo build --release -p bunsin-app        # 데스크톱 창 (macOS): target/release/bunsin-app
cargo test --workspace
```

`rust-toolchain.toml`이 Rust 버전을 고정합니다. rustup이 처음 사용할 때 그 버전을 설치합니다.
Linux에서는 창을 빼고 빌드와 테스트를 합니다: `cargo test --workspace --exclude bunsin-app`.
창은 `gpui-kit` crate를 통해 GPUI를 씁니다.

## 서버 설정 (운영자)

```sh
export BUNSIN_DATA_DIR=/var/lib/bunsin
bunsind profile-add kim --owner "Kim"
bunsind token-add kim --label "Kim's laptop"   # 접속 token을 한 번만 출력합니다
export BUNSIN_SECRET_KIM__LLM=...               # 프로필 kim의 LLM endpoint token
export BUNSIN_SECRET_KIM__SLACK=...             # 프로필 kim의 Slack MCP token
bunsind serve --listen 0.0.0.0:8787 --tls-cert cert.pem --tls-key key.pem
```

- **접속 token.** token 하나는 프로필 하나에만 통합니다. 서버에는 해시만 저장되므로 다시 볼 수 없습니다.
  `bunsind token-list`와 `bunsind token-revoke <id>`로 관리합니다.
  프로필 생성과 token 발급은 서버에서만 할 수 있고, 네트워크로는 할 수 없습니다.
- **TLS.** 서버는 외부 주소에 TLS 없이 뜨지 않습니다. TLS 프록시 뒤에 둘 때는 `127.0.0.1`에 띄웁니다.
  한 컴퓨터에서 시험할 때는 `bunsind serve --listen 127.0.0.1:8787`로 띄우고 주소로 `http://127.0.0.1:8787`을 씁니다.
- **비밀 값.** 프로필 설정은 `LLM` 같은 비밀 값의 key만 가집니다. 서버는 환경 변수
  `BUNSIN_SECRET_<프로필>__<KEY>`에서 값을 읽습니다. 한 프로필은 다른 프로필의 비밀 값을 가리킬 수 없습니다.
- **접속 대상 주소.** LLM과 MCP 주소는 프로필 소유자가 정하고, 요청은 서버가 보냅니다.
  기본 규칙은 `localhost`와 내부 IP 주소를 뺀 모든 `https://` 주소를 받습니다.
  내부 주소로 풀리는 host 이름은 통과합니다. 여러 사람이 쓰는 서버나 사외에 노출된 서버는
  `--allow-upstream-host <host>`(여러 번 지정 가능)로 띄우십시오. 그러면 목록의 host만 허용되고,
  `http://`를 쓰는 사내 gateway도 목록에 넣을 수 있습니다.
- **로컬 MCP 명령.** 서버에서 명령으로 실행하는 MCP 서버는 `--allow-stdio-connectors`가 있어야 합니다.
- **종료.** 서버는 SIGTERM을 받으면 새 요청을 받지 않고, 실행 중인 요청을 최대 300초 기다립니다.

## agent 설정 (프로필 소유자)

```sh
export BUNSIN_SERVER=https://bunsin.example.com:8787
export BUNSIN_TOKEN=bsn_...                    # 운영자에게 받은 token
bunsind config-show kim > kim.json
# kim.json을 편집한 뒤:
bunsind config-set kim kim.json
```

사설 CA를 쓰는 서버에는 `BUNSIN_CA_CERT`에 CA 인증서 파일의 경로를 넣습니다.

새 프로필은 아무것도 할 수 없습니다. 설정에 LLM endpoint, 커넥터, 승인한 도구, 권한 규칙을 넣어야 합니다.

### 원칙, LLM, 커넥터

```json
{
  "principles": [
    "Answer in two sentences.",
    "Never promise a date without checking Jira."
  ],
  "llm": {
    "wire": "anthropic-messages",
    "base_url": "https://llm.example.com",
    "model": "your-model-id",
    "token_secret": "LLM"
  },
  "connectors": {
    "slack": {
      "mcp": { "transport": "http", "url": "https://mcp.example.com/slack" },
      "token_secret": "SLACK"
    }
  }
}
```

- `principles`는 모든 프롬프트에 지시로 들어갑니다. agent가 읽는 그 밖의 글(메시지, 문서, 도구 설명)은
  인용된 데이터로 들어갑니다.
- `wire`는 `anthropic-messages` 또는 `openai-chat`입니다.
- `token_secret`은 비밀 값의 key입니다. 프로필이 `kim`이면 `"LLM"`은 서버의 환경 변수
  `BUNSIN_SECRET_KIM__LLM`을 뜻합니다.
- `"transport": "stdio"` 커넥터는 `url` 대신 `command`(절대 경로), `args`, `env`를 받습니다.

### 권한 규칙

```json
{
  "approved_tools": {
    "slack": [
      {
        "name": "search_messages",
        "description": "Search messages",
        "input_schema": { "type": "object", "properties": { "query": { "type": "string" } } }
      },
      {
        "name": "post_message",
        "description": "Post a message",
        "input_schema": {
          "type": "object",
          "properties": { "channel": { "type": "string" }, "text": { "type": "string" } }
        }
      }
    ]
  },
  "policy": {
    "rules": {
      "slack/search_messages": { "class": "read", "tier": "autonomous", "outgoing_text": "none" },
      "slack/post_message": { "class": "write", "tier": "approve", "outgoing_text": { "arg": "text" } }
    }
  }
}
```

- **`approved_tools`** 에는 각 도구가 MCP 서버가 알려 주는 그대로 들어갑니다: 이름, 설명, 입력 스키마.
  직접 입력하지 않습니다. `bunsind tools kim slack`은 커넥터 `slack`의 도구를 보여 주고 각각을
  `new`, `changed`, `approved`, `gone`으로 표시합니다. `bunsind tools kim slack --approve`는 화면에 보인 목록을 그대로 승인합니다.
  서버는 호출할 때마다 승인된 목록을 현재 목록과 비교합니다. 도구가 바뀌었거나 없어졌으면,
  목록을 다시 승인할 때까지 호출을 거부합니다. 도구 목록을 보려면 `llm`과 `connectors` 설정이 먼저 있어야 합니다.
- **`policy.rules`** 가 권한 표입니다. key는 `<커넥터>/<도구>`입니다. agent는 규칙과 승인 항목이 모두 있는 도구만 제안할 수 있습니다.
- **`class`** 는 `read`, `write`, `delete` 중 하나입니다. 읽기는 바로 실행되고, 정지 스위치가 켜져 있어도 실행됩니다.
- **`tier`** 는 쓰기를 어디까지 허용할지 정합니다.

  | 단계 | 제안된 쓰기의 처리 |
  |---|---|
  | `observe` | 제안을 저장합니다. 보여 주지 않고 실행하지 않습니다. |
  | `draft` | 초안이 승인함에 나타납니다. agent는 실행하지 않습니다. |
  | `approve` | 사용자가 승인하면 실행합니다. 여기서 시작하십시오. |
  | `autonomous` | 묻지 않고 실행합니다. `delete`는 이 단계에서도 승인이 필요합니다. |

- **`outgoing_text`** 는 모든 규칙에 반드시 있어야 합니다. `{ "arg": "text" }`는 다른 사람이 읽을 글이 든 인자를 가리키고,
  서버가 그 인자에 `ai_marker`를 붙입니다. `"none"`은 그 도구가 다른 사람에게 글을 보내지 않는다는 뜻입니다.
  AI 표시는 문자열 인자 하나에만 붙습니다.

### 이벤트 찾기 (폴링)

```json
{
  "polls": [
    {
      "connector": "slack",
      "tool": "search_messages",
      "args": { "query": "to:me" },
      "every_secs": 60,
      "mapping": { "items": "messages", "id": "ts", "text": "text", "author": "user", "thread": "thread_ts" },
      "skip_authors": ["U012AB3CD"]
    }
  ]
}
```

서버는 `every_secs`초(최소 30초)마다 도구를 호출하고, 새 항목을 agent에게 넘깁니다.

- 도구는 `read` 규칙이 있어야 하고, 승인된 목록에 있어야 합니다.
- `mapping`은 도구 결과에서 각 필드의 위치를 `user.id` 같은 dot 경로로 적습니다.
  `items`는 항목 목록의 경로입니다. 결과 자체가 목록이면 비워 둡니다.
  ID는 같은 메시지에 대해 항상 같아야 합니다. `thread`는 `null`로 둘 수 있습니다.
- `bunsind poll-test kim 0`은 0번 poll을 한 번 실행해, agent에게 넘길 이벤트를 출력합니다.
  mapping을 맞출 때 쓰십시오. 아무것도 저장하지 않습니다.
- 검색의 첫 실행은 찾은 항목을 "이미 본 것"으로만 기록합니다. 그 뒤에 생긴 항목만 이벤트가 됩니다.
  검색을 바꾸면(`connector`, `tool`, `args`) 같은 방식으로 다시 시작합니다.
- `skip_authors`에는 자신의 계정 이름이나 ID를 적습니다. 자신의 메시지와 AI 표시가 든 글은 이벤트가 아닙니다.
- 처리에 실패한 이벤트는 다음 실행에서 다시 시도합니다. 모두 3번 시도한 뒤에는 승인함의 항목으로 알려 줍니다.
- 정지 스위치가 켜져 있는 동안 폴링은 쉽니다.
- `bunsind status kim`은 각 poll의 마지막 결과를 보여 줍니다. "none usable"은 mapping이 결과와 맞지 않는다는 뜻입니다.

### 개인정보 필터

```json
{
  "privacy": {
    "email": true,
    "phone": true,
    "resident_number": true,
    "card_number": true,
    "custom": [{ "label": "employee id", "pattern": "\\bEMP-\\d{6}\\b" }]
  }
}
```

서버는 이벤트를 저장하기 전에, 그리고 인용하는 글을 LLM에 보내기 전에, 일치하는 부분을
`[REDACTED:EMAIL]` 같은 자리표시자로 바꿉니다. 지워진 글은 DB, 파일, 로그 어디에도 없습니다. 감사 로그에는 개수만 남습니다.

- 새 프로필은 기본 4종이 모두 켜져 있습니다. `resident_number`는 주민등록번호입니다.
  카드번호는 검증 숫자가 맞는 것만 지웁니다.
- `custom` 패턴은 Rust [`regex`](https://docs.rs/regex) crate의 문법을 씁니다.
  서버는 컴파일되지 않는 패턴과 빈 문자열에 일치하는 패턴을 거부합니다.
- 데스크톱 창에 이 설정을 편집하는 화면이 있습니다.
- 사용자가 직접 쓴 `principles`와 사용자가 직접 고친 글은 거르지 않습니다.

### agent와 일하기

```sh
bunsind ingest kim --source slack --event-id 1700000000.000100 \
    --author lee --text "When do we deploy?"
bunsind status kim                 # 정지 스위치, poll 상태, 승인함
bunsind approve kim 1              # 대기 중인 1번 항목을 실행
bunsind approve kim 1 --args '{"channel":"C1","text":"At 3 pm."}' --reason "shorter"
bunsind reject kim 1 --reason "not needed"
bunsind stop kim on                # 정지 스위치: 이 프로필의 모든 쓰기를 멈춤
```

이유를 붙인 수정은 교정 기록으로 저장됩니다. agent는 다음 프롬프트부터 최근 교정 기록을 봅니다.

## 데스크톱 창 (macOS)

```sh
cargo run --release -p bunsin-app
```

처음 실행하면 창이 서버 주소, 접속 token, 그리고 사설 CA를 쓰는 경우 CA 인증서 파일을 묻습니다.
창은 서버에 접속해 값을 확인한 뒤, 주소를 `~/Library/Application Support/Bunsin/connection.json`에,
token을 macOS Keychain에 저장합니다.

창은 프로필, 승인함(각 호출의 모든 인자 포함), 감사 로그, 정지 스위치, 개인정보 설정을 보여 줍니다.
5초마다 서버에서 상태를 다시 읽습니다. 승인 전에 초안을 수정하는 기능은 명령줄에만 있습니다.

개발할 때는 환경 변수 `BUNSIN_SERVER`와 `BUNSIN_TOKEN`이 저장된 접속을 대신합니다.

## 라이선스

Apache-2.0. [LICENSE](LICENSE)를 보십시오.
