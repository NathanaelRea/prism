#!/bin/sh
set -eu

IFS= read -r hello
printf '{"type":"hello_ack","hello":{"protocol":{"major":1,"minor":0},"features":[],"extension_id":"acme.shell/extension","extension_revision":"fixture-v1","sdk_version":"shell-fixture","package_id":"acme.shell","platform":"portable-shell","executable_digest":"%s"}}\n' "$PRISM_EXTENSION_EXECUTABLE_DIGEST"
IFS= read -r describe
describe_id="$(printf '%s' "$describe" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
printf '{"type":"description","id":"%s","descriptor":{"implementations":[{"id":"acme.shell/echo","class":"action","inputs":[],"outputs":[],"capabilities":[],"effect_boundary":"unbrokered"}],"artifact_schemas":[],"input_support":[],"renderers":[],"triggers":[],"notification_channels":[]}}\n' "$describe_id"

while IFS= read -r request; do
  id="$(printf '%s' "$request" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')"
  case "$request" in
    *'"type":"execute"'*)
      attempt_id="$(printf '%s' "$request" | sed -n 's/.*"attempt_id":"\([^"]*\)".*/\1/p')"
      generation="$(printf '%s' "$request" | sed -n 's/.*"generation":\([0-9][0-9]*\).*/\1/p')"
      printf '{"type":"execute_result","id":"%s","result":{"attempt_id":"%s","generation":%s,"outcome":{"status":"succeeded","outputs":{"language":"shell"}}}}\n' "$id" "$attempt_id" "$generation"
      ;;
    *'"type":"ping"'*)
      printf '{"type":"pong","id":"%s"}\n' "$id"
      ;;
    *'"type":"shutdown"'*)
      printf '{"type":"shutdown_ack","id":"%s"}\n' "$id"
      exit 0
      ;;
  esac
done
