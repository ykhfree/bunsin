# Fixtures

- `a2a-v1.0.0.schema.json`: the JSON schema bundle of A2A v1.0.0, unchanged.
  Source: https://a2a-protocol.org/v1.0.0/spec/a2a.json, downloaded on 2026-11-20.
  SHA-256 `6b6560c726289734799b7d5883be84e4cc0452600736db0f811341bac43b8d62`.
  The A2A repository calls it a non-normative build artifact of `specification/a2a.proto`
  and does not commit it (`specification/json/README.md` at tag `v1.0.0`).
  Its `$ref` values name files such as `lf.a2a.v1.Task.jsonschema.json`.
  The bundle keys its definitions by title instead, for example `Task Status`.
  The tests map one to the other. The schema has no `required` lists, so the tests check the required fields by hand.
- `a2a-spec-8.5-agent-card.json`: the sample Agent Card of A2A v1.0.0, section 8.5, unchanged.
  Source: the specification page of a2a-protocol.org ("Latest Released Version 1.0.0"), copied on 2026-11-20.
