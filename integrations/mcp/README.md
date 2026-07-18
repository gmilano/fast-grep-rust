# fast-grep MCP integration

## Status

A minimal MCP server wrapping fast-grep is planned. This file describes the
intended interface for when it is implemented.

## Why MCP

Model Context Protocol allows an agent to call fast-grep as a structured tool
without shell escaping, with typed inputs and outputs.

## Planned tool definition

```json
{
  "name": "fgr_search",
  "description": "Indexed regex search optimised for coding agents. Returns compact output with path printed once per file.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "pattern":  { "type": "string", "description": "Regex or literal pattern" },
      "path":     { "type": "string", "description": "Directory to search" },
      "format":   { "type": "string", "enum": ["compact", "jsonl", "grep"], "default": "compact" },
      "max_results": { "type": "integer", "description": "Cap total matches" },
      "max_files":   { "type": "integer", "description": "Cap number of files" },
      "indexed":  { "type": "boolean", "description": "Use .fgr index if present" }
    },
    "required": ["pattern", "path"]
  }
}
```

## Workaround until MCP server is available

Use fast-grep directly via a shell tool in your MCP configuration:

```json
{
  "tools": [
    {
      "name": "grep",
      "type": "shell",
      "command": "fgr --format jsonl --max-results 100 -- \"$pattern\" \"$path\""
    }
  ]
}
```

## Contributing

If you build an MCP server for fast-grep, contributions are welcome. The server
should: accept the tool definition above, call `fgr` as a subprocess, forward
stdout to the tool result, forward stderr to a `logs` field, and respect the
`indexed` flag by appending `--index .fgr` when true.
