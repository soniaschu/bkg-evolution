# bkgclaw

A provider-agnostic coding-agent platform in Rust: one agent loop, an
OpenClaw-style gateway daemon, an interactive TUI, a web cockpit — running
on your own NVIDIA-NIM gateway.

**It works right now.** Every claim below is from a real run on this
machine against a live NIM gateway:

```
$ bkgclaw models
pass  nim/nvidia/nemotron-3-super-120b-a12b   ready      ← chain
pass  nim/nvidia/nemotron-3.5-lightning-30b-a3b ready      ← chain
pass  nim/meta/muse-glimmer-30b               ready      ← chain
pass  nim/nvidia/nemotron-3-nano-omni-30b-…   ready      ← chain
…

$ bkgclaw run "Erstelle hello.txt … lies sie danach mit read_file" --policy allow-mutating
✓ Die Datei `hello.txt` wurde … mit dem Inhalt **halo vom agent** erstellt und bestätigt.

$ bkgclaw run "Schreibe app.py, die 1..5 quadriert. Führe sie mit execute_command aus." --policy allow-all
✓ 25          ← the agent wrote the file, ran it, and reported the last line

$ bkgclaw gateway &
$ bkgclaw tui    # or open http://127.0.0.1:8787/ in a browser
```

## Install

```bash
cd bkgclaw
cargo build --release
export NIM_API_KEY=npk_…          # your gateway key
./target/release/bkgclaw models   # shows what is actually reachable
```

The web cockpit is a separate Dioxus (WASM) build:

```bash
cargo install dioxus-cli --locked    # or your distro's dx
cd crates/bkgclaw-web && dx build --release
cp -r target/dx/bkgclaw-web/release/web/public ../.. ../web-dist
BKGCLAW_WEB_DIR=$PWD/web-dist bkgclaw gateway
# open http://127.0.0.1:8787/
```

## Layout

```
bkgclaw-core      domain: models, tools, routing, the loop, observers,
                  streaming (SSE with per-model fallback), cost, leak scan
bkgclaw-config    settings + credentials, redaction on every read
bkgclaw-store     file-backed persistence: sessions (fork/resume), memory,
                  skills, tasks, the workspace the system prompt reads
bkgclaw-exec      real tool execution: files, shell (watchdog), web with an
                  SSRF guard, memory/skill/task tools, sub-agent plumbing
bkgclaw-gateway   the daemon: REST + WebSocket, sessions, live events,
                  approvals over the wire, the sub-agent engine loop
bkgclaw-tui       the terminal client (ratatui) — connects to or embeds
                  the gateway, same protocol as the web UI
bkgclaw-web       the browser cockpit (Dioxus 0.7 → WASM): chat with live
                  deltas, tool cards, approval buttons, tasks/memory/skills
bkgclaw-ui        Report → human text or JSON
bkgclaw-cli       argument parsing and dispatch
```

`#![forbid(unsafe_code)]` on every native crate. Zero clippy warnings.
314 tests, including full-HTTP gateway integration tests with a scripted
model (approvals, forks, cancellation, auth).

## The NIM provider

Models are **verified before they are offered**: on 2026-10-08 every model
in `GET /models` (80 entries) was asked to answer one real "say HALLO"
completion and one real tool call. 39 answered 404, 28 answered 403 for
this key, the rest timed out twice — **only the seven that answered are
registered**, and only the four that produced a well-formed tool call are
in the failover chain (`OpenAiCompatible::NIM_MODELS`, `NIM_CHAIN`).

Streaming works where the gateway allows it; where it does not (upstream
403 on the stream path, silent streams), the provider falls back to a
non-streaming request once per model and remembers — no request is wasted
twice. Reasoning deltas (`reasoning_content`) stream separately and are
shown as "thinking", never mixed into the answer.

Endpoint and credential are operator-provided:

```bash
export NIM_API_KEY=npk_…        # required
export NIM_BASE_URL=https://…   # optional, defaults to the eysho gateway
```

The key is a pointer, never a value: nothing in the source, logs, reports
or JSON output contains it (a canary test enforces this for every command).

## Commands

| Command | What it does |
|---|---|
| `models` | Every registered model, only the ones that really answer |
| `run "<task>"` | One agent task, gated tools, budget enforced |
| `chat` | Interactive REPL: continuity, slash commands, stdin approvals |
| `gateway` | The daemon: REST + WebSocket + web UI + sub-agents |
| `tui` | Terminal client; embeds a gateway when none is running |
| `init personal\|worker` | Scaffold SOUL/IDENTITY/USER/MEMORY, skills/, tasks/ |
| `tools`, `doctor`, `auth`, `providers`, `deploy`, `status`, `snapshot`, `destroy`, `exit-codes` | as before |

Global flags: `--json` (exactly one object on stdout, in every mode),
`--quiet`.

## Architecture (OpenClaw-style)

```
Browser ──┐                                   ┌── NIM gateway (streaming)
TUI ──────┼── REST + WebSocket ── bkgclaw-gateway ── failover chain ──┤
curl ─────┘        (one protocol)      │                              └── Ollama (free fallback)
                                        ├── sessions (JSONL-free, one JSON
                                        │   per session, fork = copy prefix)
                                        ├── approvals: gate says Ask →
                                        │   event to every client →
                                        │   y/n/always from any client
                                        ├── sub-agents: sessions_spawn runs
                                        │   a child loop on the same engine
                                        └── workspace: .bkgclaw/ in the cwd
                                            (SOUL, IDENTITY, USER, MEMORY,
                                             skills/, tasks/)
```

- **Every tool call passes the gate.** Risk classes are read-only /
  mutating / destructive; a policy decides. In the gateway and the REPL
  what a policy would silently deny becomes an *Ask* — a human answers
  allow / deny / always, from the TUI, the browser or curl.
- **Approvals never park a thread.** The observer awaits a tokio oneshot;
  a 180-second silence counts as *no*.
- **One turn per session at a time.** Turns work on a transcript clone and
  commit once; a cancelled turn leaves the session exactly as it was.
- **Sessions are files.** `~/.bkgclaw/sessions/*.json`, readable with `cat`,
  forkable via API or `/fork`, resumable in `chat --session`.
- **Memory is two layers.** Hot memory is `./.bkgclaw/MEMORY.md` (loaded
  into every system prompt); long-term entries live in
  `~/.bkgclaw/memory.json` behind `memory_set/get/search`.
- **Skills are markdown** (`./.bkgclaw/skills/<name>/SKILL.md`), indexed by
  frontmatter, loaded on demand via `skill_read` — progressive disclosure.
- **Tasks are markdown** (`./.bkgclaw/tasks/<slug>.md`) with a status in
  frontmatter, managed via `task_add/task_list/task_update` and shown in
  the web cockpit.
- **The exit code is a contract** (0 ok · 1 negative · 2 usage · 3
  environment · 4 internal) — unchanged.

## The eleven things this gets right

1. Only verified models are offered (live-tested, not listed).
2. Real streaming with honest per-model fallback; no fake typing.
3. Real tools: write, edit (unique-match enforced), shell with a watchdog
   and capped output, web fetch with an SSRF guard (scheme, DNS-resolve,
   private ranges, redirect re-checks), size caps everywhere.
4. Approvals over the wire, from any client, with "always" per session.
5. Cancellation that leaves nothing behind.
6. Session forking and resuming across every client.
7. Sub-agents as real child sessions on the same engine.
8. Workspace personality (SOUL/IDENTITY/USER) + two-layer memory + skills
   + tasks, all plain files.
9. One protocol for every client: TUI, browser and curl speak the same
   REST+WebSocket.
10. Credentials stay pointers; a leak scanner checks every model output
    and tool result before it is stored or shown.
11. 314 tests, zero clippy warnings, `unsafe` forbidden crate-wide.

## Verified behaviour, not claimed behaviour

The integration tests boot the real axum app on loopback with a scripted
model and drive it over real HTTP: a turn flows from message to persisted
transcript; an approval is asked over the wire, answered via REST, and only
then does the write happen; a cancelled turn writes nothing; a fork copies
the prefix; a token-protected gateway rejects bare requests. The browser
run above was done with the real NIM gateway and the real Chromium.

## Honest limits

- **Model discovery is a snapshot.** The seven registered models answered
  on 2026-10-08; the chain is re-verified by hand. A gateway-side change
  needs one curl round and a one-constant edit (`NIM_MODELS`).
- **Sub-agent steering is not mid-turn.** `sessions_steer` on a running
  child says so honestly; steering lands as the next turn.
- **No cron.** Scheduling needs a long-lived daemon loop with a timer
  wheel; the gateway is the right home, it is not built.
- **web_search is refused**, honestly: there is no search backend on the
  gateway. `web_fetch` on a known URL works.
- **Leak detection is pattern-based.** It catches credential shapes; it
  cannot catch an arbitrary secret someone invented.
- **The web client speaks one origin.** It is served by the gateway;
  pointing it at another gateway is a URL hash (`#https://host`), auth
  tokens travel in the WebSocket query where browsers allow them.

## Security

The daemon binds `127.0.0.1` by default. Set `BKGCLAW_GATEWAY_TOKEN` to
require a bearer token (header for REST, `?token=` for the browser
socket). Shell commands run through `sh -c` under a 60-second watchdog with
capped output — that is what the approval gate is for; `execute_command`
is destructive and asks a human unless explicitly granted.
