#!/usr/bin/env bash
# Read-only health check for the Rakazo bridge surfaces. Sends no credentials,
# follows no redirects, accepts no TLS overrides, and never mutates server state.
set -u

usage() {
	cat <<'EOF'
Usage: scripts/healthcheck.sh [--base URL] [--bridge URL] [--chat URL] [--json]

Checks /healthz then /readyz on each requested surface, proves the app rejects an
unauthenticated read-only RPC, and prints one line per check:
OK|FAIL|SKIP <surface> <path> <code|error>

  --base URL     Rakazo app surface (for example https://agents.fpl.dev)
                 Also probes POST /rpc/me without a token; 401 is the pass.
  --bridge URL   private bridge surface (for example http://10.118.0.11:18789)
  --chat URL     optional agent entry/conversation page; without it the check
                 reports "SKIP chat not configured"
  --json         emit one machine-readable summary line instead of text lines
  --help         show this text

Every request uses --connect-timeout 3 --max-time 10 so a supervisor can bound it.
Exits 0 only if every requested check passed, 1 if any failed, 2 on usage errors.

The app surface is public; the bridge surface is private and reachable only over
the operator transport. There is no public bridge endpoint, so a bridge check
that fails from an operator workstation is expected, not a service fault.
EOF
}

usage_error() {
	printf 'healthcheck: %s\n\n' "$1" >&2
	usage >&2
	exit 2
}

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

status=0
json_checks=()
json_skips=()

json_escape() {
	local s=$1
	s=${s//\\/\\\\}
	s=${s//\"/\\\"}
	s=${s//$'\n'/ }
	s=${s//$'\t'/ }
	printf '%s' "$s"
}

emit() {
	local result=$1 surface=$2 path=$3 detail=$4 ok=true

	[ "$result" = "ok" ] || ok=false
	if [ "$result" = "fail" ]; then
		status=1
	fi
	if [ "$result" = "skip" ]; then
		json_skips+=("$(json_escape "$surface $path $detail")")
	else
		json_checks+=("{\"surface\":\"$(json_escape "$surface")\",\"path\":\"$(json_escape "$path")\",\"ok\":$ok,\"detail\":\"$(json_escape "$detail")\"}")
	fi
	printf '%s %s %s %s\n' "$(printf '%s' "$result" | tr '[:lower:]' '[:upper:]')" "$surface" "$path" "$detail"
}

request() {
	# request <method> <url> [curl args...]; sets $code and $rc, stderr in $tmpdir/stderr
	local method=$1 url=$2
	shift 2
	code=$(curl -sS --connect-timeout 3 --max-time 10 -X "$method" \
		-o /dev/null -w '%{http_code}' "$@" "$url" 2>"$tmpdir/stderr")
	rc=$?
}

request_error() {
	tr '\n' ' ' <"$tmpdir/stderr" | sed -e 's/  */ /g' -e 's/ $//'
}

check_health() {
	local surface=$1 base=$2 path=$3

	request GET "${base%/}${path}"
	if [ "$rc" -ne 0 ]; then
		emit fail "$surface" "$path" "$(request_error)"
	elif [ "$code" = "200" ]; then
		emit ok "$surface" "$path" "$code"
	else
		emit fail "$surface" "$path" "$code"
	fi
}

check_auth() {
	# Unauthenticated POST to a read-only RPC method. Rejected with 401 before any
	# handler runs, so this never mutates state; it proves auth is enforced (not 5xx).
	local surface=$1 base=$2 path="/rpc/me"

	request POST "${base%/}${path}" \
		-H "content-type: application/json" --data '{"json":null}'
	if [ "$rc" -ne 0 ]; then
		emit fail "$surface" "$path" "$(request_error)"
	elif [ "$code" = "401" ]; then
		emit ok "$surface" "$path" "$code"
	else
		emit fail "$surface" "$path" "expected 401 got $code"
	fi
}

check_chat() {
	local surface=$1 url=$2

	if [ -z "$url" ]; then
		emit skip "$surface" "/agent-chat" "not configured"
		return
	fi
	request GET "$url"
	if [ "$rc" -ne 0 ]; then
		emit fail "$surface" "$url" "$(request_error)"
	elif [ "$code" -ge 200 ] && [ "$code" -lt 400 ]; then
		emit ok "$surface" "$url" "$code"
	else
		emit fail "$surface" "$url" "$code"
	fi
}

surface() {
	local name=$1 base=$2

	check_health "$name" "$base" "/healthz"
	check_health "$name" "$base" "/readyz"
}

[ "$#" -eq 0 ] && usage_error "no surfaces requested"

base_url=""
bridge_url=""
chat_url=""
json=false

while [ "$#" -gt 0 ]; do
	case "$1" in
	--base)
		[ "$#" -ge 2 ] || usage_error "--base requires a URL"
		base_url=$2
		shift 2
		;;
	--bridge)
		[ "$#" -ge 2 ] || usage_error "--bridge requires a URL"
		bridge_url=$2
		shift 2
		;;
	--chat)
		[ "$#" -ge 2 ] || usage_error "--chat requires a URL"
		chat_url=$2
		shift 2
		;;
	--json)
		json=true
		shift
		;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		usage_error "unknown argument: $1"
		;;
	esac
done

[ -n "$base_url" ] || [ -n "$bridge_url" ] || usage_error "at least one of --base or --bridge is required"

# Text output goes to stdout; in JSON mode it is captured and replaced by one line.
if $json; then
	exec 3>&1
	exec 1>/dev/null
fi

[ -n "$base_url" ] && surface "base" "$base_url"
[ -n "$base_url" ] && check_auth "base" "$base_url"
[ -n "$bridge_url" ] && surface "bridge" "$bridge_url"
check_chat "chat" "$chat_url"

if $json; then
	exec 1>&3 3>&-
	joined_checks=""
	for entry in "${json_checks[@]+"${json_checks[@]}"}"; do
		joined_checks+="${joined_checks:+,}${entry}"
	done
	joined_skips=""
	for entry in "${json_skips[@]+"${json_skips[@]}"}"; do
		joined_skips+="${joined_skips:+,}\"${entry}\""
	done
	ok=true
	[ "$status" -eq 0 ] || ok=false
	printf '{"ok":%s,"exit":%d,"checks":[%s],"skipped":[%s],"readonly":true}\n' \
		"$ok" "$status" "$joined_checks" "$joined_skips"
fi

exit "$status"
