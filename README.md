# mini-router

A small, single-binary AI API router written in Rust. It is a re-implementation of the
core of [9router](https://github.com/decolua/9router) sized for a 1 GB ARM board such
as the Orange Pi Zero 3:

- One endpoint for your coding tools: OpenAI Chat Completions (`/v1/chat/completions`)
  and Anthropic Messages (`/v1/messages`), with translation between the two in both
  directions, streaming and non-streaming, including tool calls and thinking.
- Multiple accounts per provider with fill-first or round-robin selection, per-model
  cooldown locks and exponential backoff on rate limits, automatic fallback to the next
  account, and retries on 502/503/504 (the same rules as upstream).
- Usage and cost per request in SQLite, priced from upstream's pricing table.
- Provider definitions are **copied from upstream automatically**: a weekly GitHub
  Actions job clones 9router, converts its registry to `data/providers.json` and opens a
  pull request. No hand-maintained provider list.
- No Node, no build step on the device, no dashboard process. Idle memory is a few MB.

Not (yet) included: the web dashboard, OAuth logins (Claude Code, Codex, Copilot, Kiro
…), Gemini wire format, model combos, the RTK token saver, embeddings, image/audio.
Providers that need those are listed in `data/skipped.json` after each sync.

## Install on the Orange Pi

Download `mini-router-aarch64-unknown-linux-musl.tar.gz` from the releases page (built by
CI; it is a static binary), then:

```sh
tar xzf mini-router-aarch64-unknown-linux-musl.tar.gz
sudo install -m755 mini-router /usr/local/bin/
sudo useradd -r -m -d /var/lib/mini-router mini-router
sudo install -m644 deploy/mini-router.service /etc/systemd/system/
sudo systemctl enable --now mini-router
```

Or with Docker: `docker run -d -p 20128:20128 -v mini-router:/data ghcr.io/<you>/mini-router`.

## First run

```sh
export DATA_DIR=/var/lib/mini-router        # same dir the service uses
mini-router providers                       # list providers from the synced registry
mini-router connection add deepseek sk-...  --name personal
mini-router connection add anthropic sk-ant-...
mini-router connection add openai-compatible ollama --base-url http://nas:11434/v1 --prefix local
mini-router apikey create --name laptop     # prints the key your tools will use
mini-router alias set fast groq/llama-3.3-70b-versatile
```

Point a tool at it:

| Tool | Setting |
|---|---|
| Claude Code | `ANTHROPIC_BASE_URL=http://pi:20128` `ANTHROPIC_AUTH_TOKEN=<key>` model `deepseek/deepseek-chat` |
| Codex / Cursor / Cline / OpenCode | base URL `http://pi:20128/v1`, API key `<key>`, model `anthropic/claude-sonnet-4-20250514` |

Model names are `provider/model` (`ds/deepseek-chat` with the short alias also works),
a saved alias (`fast`), a custom-endpoint prefix (`local/qwen3`), or a bare model id,
which is routed by name (`claude-*` to Anthropic, `gpt-*` to OpenAI, …).

Model `auto` (like OpenRouter's `openrouter/auto`) routes to whatever is available:
it walks candidate models until one answers, skipping models whose accounts are all
cooling down, and reports the one it used in the `x-mini-router-model` response header.
Candidates come from, in order of preference:

1. the request's `models` array (OpenRouter's fallback-list field), e.g.
   `{"model": "auto", "models": ["groq/llama-3.3-70b-versatile", "ds/deepseek-chat"]}`;
2. `settings.autoModels`, e.g. `mini-router settings 'autoModels=["groq/llama-3.3-70b-versatile","ds/deepseek-chat"]'`;
3. otherwise every model of every provider with an active account: catalogue models,
   models added on the provider page, and for endpoints with neither (OpenAI/Anthropic
   compatible nodes) the list from their `GET /models`, cached for 30 minutes. Each
   provider's first model (providers by account priority) goes first, then their second, and so on.

Candidates are tried one after another until one answers; a failure (error, rate
limit, no free account) just moves on to the next model, until none are left.

Requests from the Pi itself need no key. Requests from the LAN need one, and you can
force keys everywhere with `mini-router settings requireApiKey=true`.

## Sign-in (OAuth) and free providers

Some providers have no API key: you sign in with your account instead. Open the provider
in **Providers** and press **Sign in with …**. Tokens are stored on the account and
refreshed automatically before they expire (and once more after a 401).

| Provider | How you sign in |
|---|---|
| Claude Code (`claude`) | Open the sign-in page, log in, then paste the address you land on (`http://localhost:…/callback?code=…`) back into the dashboard. The page itself may not load, that's expected. |
| xAI (`xai`), Cline, Cline Pass | Same: paste the callback address. xAI always lands on `http://127.0.0.1:56121/callback`. |
| GitHub Copilot, Kimi, Kilo Code, MiniMax Code (CN / global), Nous, Muse | Device code: the dashboard shows a code, you enter it on the provider's page, the dashboard notices when you approve. |

Behind the scenes, as upstream does: Claude sign-ins send Claude Code's identity (system
prompt, billing header, `metadata.user_id`) and rename tools so they don't clash with
Claude Code's own, restoring the names in the reply. GitHub accounts trade the GitHub token
for a Copilot token and send Claude models to Copilot's `/v1/messages`. Kimi, Cline,
MiniMax, Muse and Kilo get their client headers.

Free providers need no account: **MiMo Free** (`mimo-free`, `mmf`) fetches a free-tier
token by itself. Local Ollama is best added as an OpenAI-compatible endpoint
(`http://<host>:11434/v1`).

Not ported, because they need a proprietary protocol or impersonate an IDE (see
`data/skipped.json`): Cursor, Windsurf, Kiro, Qoder, Trae, Zed, Codex (Responses API),
Gemini CLI / Antigravity (Gemini format), Devin CLI, OpenCode, iFlow, CodeBuddy, Kimchi,
GitLab Duo.

## Token Saver

Same feature and settings keys as 9router's Token Saver page, minus context
compression (Headroom), which needs a Python proxy and ML models:

- **RTK** (`rtkEnabled`, on by default): compacts tool results such as git diff/status/log,
  grep, find, ls, tree and build logs before they reach the model, often 60-90% fewer input tokens.
- **Caveman** (`cavemanEnabled`, `cavemanLevel` = lite | full | ultra | wenyan-lite | wenyan | wenyan-ultra):
  a terse-style system prompt for shorter answers.
- **Ponytail** (`ponytailEnabled`, `ponytailLevel` = lite | full | ultra): biases the model toward minimal code.

A client can skip all of them for one request with the header `x-9router-token-saver: off`.

## Dashboard

Open `http://pi:20128/` in a browser. The dashboard is a faithful port of 9router's
Next.js UI to one hand-written HTML/CSS/JS bundle compiled into the binary: same
macOS-style sidebar with traffic lights, same orange theme with light/dark mode, same
pages, cards, modals and wording. No Node and no build step; the bundle is about 70 KB
plus the provider logos, and it talks to a JSON API under `/api/admin`.

Pages, matching upstream's sidebar:

- **Endpoint & Key**: endpoint URLs to paste into tools, the *Require API key* switch,
  API keys with show/copy, pause, delete and per-key *Restrict access* model lists.
- **Providers**: card grid in upstream's sections (Custom, Free Tier, API Key) with
  connection counts, error codes and enable/disable toggles, status filter and header
  search; a provider page with the connections list (priority arrows, cooldown timer,
  last error, unlock, edit, delete, toggle, per-provider round-robin) and the
  *Available Models* chips with copy and custom models. *Add OpenAI Compatible* /
  *Add Anthropic Compatible* create custom endpoints with a prefix.
- **Usage**: Today / 24h / 7D / 30D / 60D / All periods, overview cards, providers and
  recent requests, tokens / requests / cost chart, provider and model breakdowns, and
  the grouped, sortable table by model, account, API key or endpoint in costs or tokens.
- **Settings**: Local Mode card with theme switcher and database path, Security
  (require login, require API key, password), Routing Strategy (round robin, sticky
  limit), logout.

Upstream features that mini-router does not implement are left out of the sidebar
rather than shown dead: Combos, Quota Tracker, Token Saver, CLI Tools, Media
Providers, Proxy Pools, Skills, tunnels and SSO. Model aliases are managed with the
CLI (`mini-router alias`).

Fonts and icons (Inter and Material Symbols) load from Google Fonts like upstream;
without internet the UI falls back to system fonts and shows icon names as text.
Provider logos are copied from upstream by the sync pipeline into `data/icons/` and
embedded by `build.rs`.

Access rules match 9router: a browser on the Pi itself is let in without a password
unless *Require login* is on; anything else must sign in. The initial password is
`123456` (or `INITIAL_PASSWORD`); change it in Settings before binding to `0.0.0.0`.
Sessions are HMAC-signed cookies valid for 7 days, signed with `JWT_SECRET` or a
random secret persisted to `$DATA_DIR/jwt-secret`.

## Operations

```sh
mini-router connection list          # shows cooldown state and the last error
mini-router connection unlock --all  # clear cooldowns
mini-router settings fallbackStrategy=round-robin stickyRoundRobinLimit=3
curl -s localhost:20128/api/health
```

Usage rows live in the `usageHistory` table of `$DATA_DIR/db/data.sqlite`; the schema
is 9router's, so an existing `~/.9router` data directory can be reused as `DATA_DIR`.

Environment variables: `PORT`, `HOSTNAME`, `DATA_DIR`, `REQUIRE_API_KEY`, `RUST_LOG`,
`HTTPS_PROXY`, `STREAM_STALL_TIMEOUT_MS`, `FETCH_CONNECT_TIMEOUT_MS`, `API_KEY_SECRET`,
`MACHINE_ID_SALT`. See `deploy/config.example.toml` for the file form.

## Keeping providers in sync with 9router

`.github/workflows/sync-upstream.yml` runs every Monday (or on demand). It shallow-clones
upstream, runs `tools/sync-upstream/convert.mjs`, which imports upstream's own registry
builders and writes:

- `data/providers.json`: providers the router can drive (OpenAI or Anthropic wire format,
  API-key auth, plus the sign-in and free providers described above),
  with base URLs, headers, aliases and model lists;
- `data/pricing.json`: the pricing tables;
- `data/skipped.json`: providers left out and why (dedicated executor, OAuth-only, other
  wire format).

If anything changed and the tests pass, the job commits the new files to `main`. The binary embeds
these files; dropping a newer `providers.json` or `pricing.json` into `DATA_DIR` overrides
the embedded copy without a rebuild.

To run the converter locally you need Node 22:

```sh
git clone --depth 1 https://github.com/decolua/9router .upstream/9router
node tools/sync-upstream/convert.mjs .upstream/9router data
```

## Building

```sh
cargo build --release                                  # native
cargo install cross && cross build --release --target aarch64-unknown-linux-musl
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests, then builds static musl
binaries for aarch64 and x86_64 and attaches them to tagged releases.

## Offsite database (Turso)

Requests keep using the local SQLite file, so latency is unaffected. With Turso
configured the router also mirrors the database to a hosted libSQL instance:

```sh
turso db create mini-router
turso db show mini-router --url          # libsql://mini-router-<org>.turso.io
turso db tokens create mini-router
export TURSO_DATABASE_URL=libsql://mini-router-<org>.turso.io
export TURSO_AUTH_TOKEN=...
```

- **Push:** every `TURSO_SYNC_SECS` (default 60) changed rows of the config tables
  (connections, keys, settings, combos, kv) and new `usageHistory` rows are sent over
  Turso's HTTP API. Only changes are written, which keeps free-tier row-write usage low.
- **Restore:** if the local database is empty at start-up (new board, wiped SD card)
  everything is pulled back from Turso before serving.
- **Single writer:** the board is the source of truth; do not run two routers against
  one Turso database. Local usage pruning (180 days) is not propagated, so Turso keeps
  the full history.
- Changes from the last interval are lost if the board dies between pushes.

### Turso as the only copy (`TURSO_ONLY=1`)

For boards with little storage, set `TURSO_ONLY=1` (needs the two Turso variables). No
database file is written: SQLite runs in RAM, is filled from Turso before the listener
opens, and every sync interval (plus once on clean shutdown) pushes changes back.

- Set `JWT_SECRET` if you want sessions to survive a token rotation; otherwise the
  session secret is derived from `TURSO_AUTH_TOKEN`. The dashboard password is stored in
  the database like everything else.
- Only the last 30 days of usage history are kept in RAM (older rows stay on Turso).
- If Turso is unreachable at start-up the router comes up empty and pushes stay paused
  until the restore succeeds, so an outage cannot wipe the remote copy.
- Power loss or a crash loses up to one sync interval; lower `TURSO_SYNC_SECS` (e.g. 10)
  to shrink that window at the cost of more row writes.
- The `mini-router` CLI subcommands cannot work against an in-memory database; use the
  dashboard.
