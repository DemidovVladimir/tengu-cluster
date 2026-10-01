#!/bin/sh
# Engine-matrix [[mcp_servers]] fixture (tests/fixtures/engine_matrix/open/*.toml):
# one tool, `token`, that answers "mcp token: $MATRIX_TOKEN". The config maps
# MATRIX_TOKEN = "$TENGU_MATRIX_MCP_TOKEN"; whoever spawns the server (a
# run-agent child, or a Claude Code step's tengu bridge) resolves that from
# its inherited env. Answers initialize / tools/list / tools/call; ignores
# notifications.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"matrix","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"token","description":"Return the engine-matrix MCP token.","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"mcp token: %s"}]}}\n' "$id" "$MATRIX_TOKEN" ;;
  esac
done
