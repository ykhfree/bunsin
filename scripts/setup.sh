#!/usr/bin/env bash
# Sets up a Bunsin server and one agent on this computer in one go, and stops it again.
#
# Quick start:
#   scripts/setup.sh          # build, start the server, print the address and the access token
#   scripts/setup.sh app      # open the desktop window, already signed in (macOS)
#   scripts/setup.sh stop
#
# Usage:
#   scripts/setup.sh [start] [options] [-- <more options of bunsind serve>]
#   scripts/setup.sh app | token | stop | status | env | logs
#
# `start` (the default) does these steps. Each one is skipped when it is already done, so the
# script can run again at any time:
#   1. builds bunsind (cargo build --release -p bunsin-daemon)
#   2. creates the profile in the data folder           (bunsind profile-add)
#   3. creates an access token and saves it              (bunsind token-add)
#   4. starts `bunsind serve --allow-stdio-connectors` in the background and waits for it
#   5. optional: saves the settings file                 (bunsind config-set)
#   6. optional: saves the LLM endpoint and tests it     (bunsind llm-set, llm-test)
#   7. optional: saves the connectors and lists tools    (bunsind connector-set, tools)
#
# Options of start:
#   --profile <id>         Profile ID: 1 to 40 of a-z, 0-9 and `-`. Default: your user name.
#   --owner <name>         The owner's display name. Default: the profile ID.
#   --listen <addr>        Address of the server. Default: 127.0.0.1:8787.
#   --state-dir <dir>      Where the data folder, the log, the PID file and the token are kept.
#                          Default: .bunsin in the repository (git ignores it).
#   --key-store <store>    file or keychain. Default: file (the key stays in the data folder).
#   --config <file>        A settings file (JSON) to save with config-set, for example
#                          from `bunsind config-show <profile>`.
#   --llm                  Save the LLM endpoint. The token comes from BUNSIN_LLM_TOKEN or,
#                          if that is not set, is asked for. BUNSIN_LLM_TOKEN alone turns this on.
#   --llm-wire <wire>      anthropic-messages (default), openai-chat or openai-responses.
#   --llm-base-url <url>   Default for anthropic-messages: the Claude API.
#   --llm-model <model>    Default for anthropic-messages: a Claude model.
#   --skip-llm-test        Do not send the test request (it costs tokens).
#   --connector <name>=<url>
#                          Save an MCP connector. Repeat it for more. Its token comes from
#                          BUNSIN_CONNECTOR_TOKEN_<NAME> (upper case, `-` as `_`), if set.
#   --no-build             Use the bunsind that is built already.
#   -h, --help             Show this help.
#
# start always passes --allow-stdio-connectors to bunsind serve, so the profile can save
# command (stdio) connectors. With it, every profile owner can run any program as your user.
#
# Other commands:
#   app      Opens the desktop window with the saved address and token, so it does not ask
#            for them (macOS). It builds the window on first use.
#   token    Prints only the access token, for example: scripts/setup.sh token | pbcopy
#   stop     Stops the server (SIGTERM; it waits for running requests).
#   status   Shows whether the server runs, the status of the profile, the address and the token.
#   env      Prints the export lines for the command line: eval "$(scripts/setup.sh env)"
#   logs     Follows the server log.
#
# The tools of a connector are not approved and no permission rule is set: read the tool list,
# then follow "Permission rules" in the README (bunsind tools <profile> <name> --approve, rule-set).
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

usage() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; }
say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command=start
case "${1:-}" in
    start | app | token | stop | status | env | logs) command=$1; shift ;;
esac

default_profile=$(id -un | tr 'A-Z_.' 'a-z--' | tr -cd 'a-z0-9-' | cut -c1-40)
profile=""
owner=""
listen=""
state_dir=${BUNSIN_STATE_DIR:-"$repo/.bunsin"}
key_store="file"
config=""
llm=false
[ -n "${BUNSIN_LLM_TOKEN:-}" ] && llm=true
llm_wire=anthropic-messages
llm_base_url=""
llm_model=""
llm_test=true
connectors=()
build=true
serve_extra=()

need_value() { if [ $# -lt 2 ] || [ -z "$2" ]; then die "$1 needs a value"; fi; }
while [ $# -gt 0 ]; do
    case "$1" in
        --profile) need_value "$@"; profile=$2; shift ;;
        --owner) need_value "$@"; owner=$2; shift ;;
        --listen) need_value "$@"; listen=$2; shift ;;
        --state-dir) need_value "$@"; state_dir=$2; shift ;;
        --key-store) need_value "$@"; key_store=$2; shift ;;
        --config) need_value "$@"; config=$2; shift ;;
        --llm) llm=true ;;
        --llm-wire) need_value "$@"; llm_wire=$2; llm=true; shift ;;
        --llm-base-url) need_value "$@"; llm_base_url=$2; llm=true; shift ;;
        --llm-model) need_value "$@"; llm_model=$2; llm=true; shift ;;
        --skip-llm-test) llm_test=false ;;
        --connector) need_value "$@"; connectors+=("$2"); shift ;;
        --no-build) build=false ;;
        -h | --help) usage; exit 0 ;;
        --) shift; serve_extra=("$@"); break ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
done

# Allow command (stdio) connectors, unless the options after -- give the flag already.
if [[ " ${serve_extra[*]+"${serve_extra[*]}"} " != *" --allow-stdio-connectors "* ]]; then
    serve_extra=(--allow-stdio-connectors "${serve_extra[@]+"${serve_extra[@]}"}")
fi

# Only start makes the folder. The other commands read what start left there.
if [ "$command" = start ]; then
    mkdir -p "$state_dir"
fi
if [ -d "$state_dir" ]; then
    state_dir=$(cd "$state_dir" && pwd)
fi
data_dir="$state_dir/data"
pid_file="$state_dir/bunsind.pid"
log_file="$state_dir/bunsind.log"
env_file="$state_dir/agent.env"
bin="${CARGO_TARGET_DIR:-"$repo/target"}/release/bunsind"

# The values of an earlier start, so that stop, status and env need no options.
saved_profile="" saved_server=""
if [ -f "$env_file" ]; then
    saved_profile=$(sed -n 's/^export BUNSIN_PROFILE=//p' "$env_file")
    saved_server=$(sed -n 's/^export BUNSIN_SERVER=//p' "$env_file")
fi
profile=${profile:-${saved_profile:-$default_profile}}
owner=${owner:-$profile}
if [ -z "$listen" ]; then
    listen=${saved_server#http://}
    listen=${listen:-127.0.0.1:8787}
fi
server="http://$listen"

running_pid() {
    local pid
    [ -f "$pid_file" ] || return 1
    pid=$(cat "$pid_file")
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        echo "$pid"
    else
        return 1
    fi
}

load_env() {
    [ -f "$env_file" ] || die "no setup in $state_dir yet: run $(me start) first"
    # shellcheck disable=SC1090
    . "$env_file"
}

write_env() {
    local token=$1
    (
        umask 077
        cat >"$env_file" <<EOF
# Written by scripts/setup.sh. The access token is secret: keep this file to yourself.
export BUNSIN_SERVER=$server
export BUNSIN_TOKEN=$token
export BUNSIN_PROFILE=$profile
export BUNSIN_DATA_DIR=$data_dir
EOF
    )
}

# The commands of this script as the user types them, with --state-dir if it is not the default.
state_opt=""
[ "$state_dir" = "$repo/.bunsin" ] || state_opt=" --state-dir '$state_dir'"
me() { printf 'scripts/setup.sh %s%s' "$1" "$state_opt"; }

# The two values that the sign-in of the window asks for. Needs load_env first.
show_connection() {
    cat <<EOF

The desktop window asks for these two values:

  Server address   $BUNSIN_SERVER
  Access token     $BUNSIN_TOKEN

The token gives full access to the profile $BUNSIN_PROFILE: keep it to yourself.
EOF
}

new_token() {
    say "create an access token for $profile"
    local token
    token=$(BUNSIN_DATA_DIR="$data_dir" "$bin" token-add "$profile" --label "scripts/setup.sh on $(hostname)")
    write_env "$token"
    echo "saved in $env_file"
}

case "$command" in
    app)
        [ "$(uname -s)" = Darwin ] || die "the desktop window needs macOS"
        load_env
        running_pid >/dev/null || die "the server is not running: start it with $(me start)"
        say "open the desktop window for $BUNSIN_PROFILE at $BUNSIN_SERVER"
        echo "The first start builds the window. This can take some minutes."
        # The window takes BUNSIN_SERVER and BUNSIN_TOKEN from the environment and skips its sign-in.
        cd "$repo"
        exec cargo run --release -p bunsin-app --locked
        ;;
    token)
        load_env
        printf '%s\n' "$BUNSIN_TOKEN"
        exit 0
        ;;
    stop)
        if pid=$(running_pid); then
            say "stop the server (PID $pid)"
            kill -TERM "$pid"
            for _ in $(seq 1 60); do
                kill -0 "$pid" 2>/dev/null || break
                sleep 1
            done
            if kill -0 "$pid" 2>/dev/null; then
                echo "the server still drains running requests (up to 300 s); PID $pid"
            else
                rm -f "$pid_file"
                echo "stopped"
            fi
        else
            rm -f "$pid_file"
            echo "the server is not running"
        fi
        exit 0
        ;;
    status)
        if pid=$(running_pid); then
            echo "server: running, PID $pid, $server"
            load_env
            "$bin" status "$BUNSIN_PROFILE" || true
            "$bin" connections "$BUNSIN_PROFILE" || true
            show_connection
        else
            echo "server: not running"
        fi
        exit 0
        ;;
    env)
        load_env
        cat <<EOF
export BUNSIN_SERVER=$BUNSIN_SERVER
export BUNSIN_TOKEN=$BUNSIN_TOKEN
export BUNSIN_PROFILE=$BUNSIN_PROFILE
EOF
        exit 0
        ;;
    logs)
        [ -f "$log_file" ] || die "no log yet: $log_file"
        exec tail -f "$log_file"
        ;;
esac

# --- start ---

case "$profile" in
    "" | *[!a-z0-9-]*) die "profile ID \`$profile\` is invalid: use 1 to 40 of a-z, 0-9 and \`-\` (--profile)" ;;
esac
[ ${#profile} -le 40 ] || die "profile ID \`$profile\` is longer than 40 characters"
[ -z "$config" ] || [ -f "$config" ] || die "no settings file: $config"
for spec in "${connectors[@]+"${connectors[@]}"}"; do
    case "$spec" in
        ?*=?*) ;;
        *) die "--connector needs <name>=<url>, not \`$spec\`" ;;
    esac
done

if $build; then
    say "build bunsind"
    (cd "$repo" && cargo build --release -p bunsin-daemon --locked)
fi
[ -x "$bin" ] || die "no bunsind at $bin: run without --no-build"

mkdir -p "$data_dir"
export BUNSIN_DATA_DIR="$data_dir"

say "profile $profile"
if out=$("$bin" profile-add "$profile" --owner "$owner" 2>&1); then
    echo "$out"
elif [[ "$out" == *"already exists"* ]]; then
    echo "profile $profile exists"
else
    die "$out"
fi

# A saved token of another profile, or of a data folder that was removed, is replaced.
if [ "$saved_profile" != "$profile" ] || [ "$saved_server" != "$server" ]; then
    new_token
fi

if pid=$(running_pid); then
    say "the server runs already (PID $pid)"
    if [ "$saved_server" != "$server" ]; then
        echo "note: it was started with $saved_server; run scripts/setup.sh stop to change the address" >&2
    fi
else
    say "start bunsind serve on $listen"
    echo "warning: --allow-stdio-connectors is on: every profile owner can run any program as $(id -un)" >&2
    nohup "$bin" serve --listen "$listen" --key-store "$key_store" \
        "${serve_extra[@]+"${serve_extra[@]}"}" >>"$log_file" 2>&1 &
    echo $! >"$pid_file"
    echo "PID $(cat "$pid_file"), log: $log_file"
fi

load_env
say "wait for the server"
ready=false
for _ in $(seq 1 60); do
    if ! running_pid >/dev/null; then
        tail -n 20 "$log_file" >&2
        die "the server stopped; see $log_file"
    fi
    if out=$("$bin" profile-list 2>&1); then
        ready=true
        break
    fi
    case "$out" in
        *401* | *nauthori* | *token*)
            # The data folder does not know the saved token any more.
            new_token
            load_env
            ;;
    esac
    sleep 0.5
done
$ready || die "the server does not answer at $server: $out"
echo "$out"

if [ -n "$config" ]; then
    say "save the settings of $config"
    "$bin" config-set "$profile" "$config"
fi

if $llm; then
    say "save the LLM endpoint ($llm_wire)"
    llm_token=${BUNSIN_LLM_TOKEN:-}
    if [ -z "$llm_token" ]; then
        [ -t 0 ] || die "set BUNSIN_LLM_TOKEN: there is no terminal to ask for the LLM token"
        read -r -s -p "LLM token: " llm_token
        echo
    fi
    llm_args=(--wire "$llm_wire")
    [ -n "$llm_base_url" ] && llm_args+=(--base-url "$llm_base_url")
    [ -n "$llm_model" ] && llm_args+=(--model "$llm_model")
    printf '%s\n' "$llm_token" | "$bin" llm-set "$profile" "${llm_args[@]}" --token-stdin
    unset llm_token
    if $llm_test; then
        say "test the LLM endpoint"
        "$bin" llm-test "$profile" ||
            echo "warning: the LLM test failed; fix the endpoint and check it with: bunsind llm-test $profile" >&2
    fi
fi

for spec in "${connectors[@]+"${connectors[@]}"}"; do
    name=${spec%%=*}
    url=${spec#*=}
    var="BUNSIN_CONNECTOR_TOKEN_$(printf '%s' "$name" | tr 'a-z-' 'A-Z_')"
    say "save the connector $name ($url)"
    if [ -n "${!var:-}" ]; then
        printf '%s\n' "${!var}" | "$bin" connector-set "$profile" "$name" --url "$url" --token-stdin
    else
        "$bin" connector-set "$profile" "$name" --url "$url"
    fi
    say "list the tools of $name"
    "$bin" tools "$profile" "$name" || echo "warning: the connector $name did not answer; check it with: bunsind tools $profile $name" >&2
done

say "done"
"$bin" status "$profile" || true
"$bin" connections "$profile" || true
show_connection
printf '\nNext:\n'
printf '  %-26s %s\n' \
    "$(me app)" "open the desktop window, already signed in (macOS)" \
    "$(me token) | pbcopy" "copy the token to paste it into the window" \
    "$(me stop)" "stop the server"
printf '\nThe server runs in the background (log: %s).\n' "$log_file"
printf 'For the command line in this shell: eval "$(%s)"\n' "$(me env)"
