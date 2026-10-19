# zilong-router

[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-db61a2?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/sudtanj)
[![PayPal](https://img.shields.io/badge/PayPal-donate-0070ba?logo=paypal&logoColor=white)](https://www.paypal.me/sudtanj)

Named after Zhao Yun (赵云 / 趙雲), courtesy name Zilong (子龙 / 子龍), the Three Kingdoms
general who carried his charge safely through an entire army at Changban (长坂): every
request gets through.

A small, single-binary AI API router written in Rust. It is a re-implementation of
[9router](https://github.com/decolua/9router) sized for a 1 GB ARM board such as the
Orange Pi Zero 3:

- One endpoint for your coding tools: OpenAI Chat Completions (`/v1/chat/completions`)
  and Anthropic Messages (`/v1/messages`), with translation between the two in both
  directions, streaming and non-streaming, including tool calls and thinking.
- Multiple accounts per provider with fill-first or round-robin selection, per-model
  cooldown locks and exponential backoff on rate limits, automatic fallback to the next
  account, and retries on 502/503/504 (the same rules as upstream). The `auto` model
  falls back (or rotates) across every connected provider.
- OAuth sign-in and free providers (Claude, GitHub Copilot, xAI, Kimi, Cline, OpenCode
  and more), plus the RTK / Caveman token saver.
- A port of the 9router web dashboard, served by the same binary with no Node and no
  build step.
- Usage and cost per request in SQLite, priced from upstream's pricing table, with
  optional offsite storage in Turso.
- Provider definitions are **copied from upstream automatically**: a weekly GitHub
  Actions job clones 9router and converts its registry to `data/providers.json`. No
  hand-maintained provider list.
- Idle memory is a few MB.

Not (yet) included: Gemini wire format, model combos, the Responses API, embeddings,
image/audio, and providers that need IDE-impersonation executors (Cursor, Windsurf, Kiro
…); see the [Roadmap](#roadmap). Providers that need those are listed in
`data/skipped.json` after each sync.

## Install on the Orange Pi

Download `zilong-router-aarch64-unknown-linux-musl.tar.gz` from the releases page (built by
CI; it is a static binary), then:

```sh
tar xzf zilong-router-aarch64-unknown-linux-musl.tar.gz
sudo install -m755 zilong-router /usr/local/bin/
sudo useradd -r -m -d /var/lib/zilong-router zilong-router
sudo install -m644 deploy/zilong-router.service /etc/systemd/system/
sudo systemctl enable --now zilong-router
```

Or with Docker: `docker run -d -p 20128:20128 -v zilong-router:/data ghcr.io/<you>/zilong-router`.

## First run

```sh
export DATA_DIR=/var/lib/zilong-router      # same dir the service uses
zilong-router providers                     # list providers from the synced registry
zilong-router connection add deepseek sk-...  --name personal
zilong-router connection add anthropic sk-ant-...
zilong-router connection add openai-compatible ollama --base-url http://nas:11434/v1 --prefix local
zilong-router apikey create --name laptop   # prints the key your tools will use
zilong-router alias set fast groq/llama-3.3-70b-versatile
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
cooling down, and reports the one it used in the `x-zilong-router-model` response header.
Candidates come from, in order of preference:

1. the request's `models` array (OpenRouter's fallback-list field), e.g.
   `{"model": "auto", "models": ["groq/llama-3.3-70b-versatile", "ds/deepseek-chat"]}`;
2. `settings.autoModels`, e.g. `zilong-router settings 'autoModels=["groq/llama-3.3-70b-versatile","ds/deepseek-chat"]'`;
3. otherwise every model of every provider with an active account: catalogue models,
   models added on the provider page, and for endpoints with neither (OpenAI/Anthropic
   compatible nodes) the list from their `GET /models`, cached for 30 minutes. Each
   provider's first model (providers by account priority) goes first, then their second, and so on.

Candidates are tried one after another until one answers; a failure (error, rate
limit, no free account) just moves on to the next model, until none are left.
With **Round Robin** on in Settings (`fallbackStrategy: "round-robin"`), each request
starts one model further down the list (after `stickyRoundRobinLimit` requests on the
same one) and the rest stay as fallbacks; with it off, `auto` always starts at the top.

Model `auto-strong` works the same way but only with models that hold up in agent work
(long system prompts, dozens of tools, several tool rounds per answer), which is what
agents like [kongming](https://github.com/sudtanj/kongming) or Claude Code need: a
small model there tends to call the wrong tool or stop answering. Providers publish no
capability data, so it goes by model family: Claude (Opus, Sonnet, Haiku 4.5), GPT-5,
GPT-4.1/4o and o3, Gemini Pro and Flash (2.5+), Grok 4, DeepSeek V3.1+/V4, Qwen
Max/Plus/Coder/235B+, Kimi, GLM 4.5+ and 5, MiniMax M2+, Mistral Large/Medium, MiMo,
Seed Pro/Code, Nemotron Ultra/Super, gpt-oss-120b and Command A. Small variants
(nano, lite, mini, air, distills, anything under 100B parameters) and unknown models
are left out. Candidates are tried strongest first (frontier models, then strong, then
capable); Round Robin rotates only among the strongest tier. To pick the models
yourself, set `settings.autoStrongModels` (or *Configure* on the Auto model card); a
request's `models` array also overrides it. `auto-coding` is another name for
`auto-strong`, for coding agents.

Model `auto-images` is the same for requests with images (screenshots, photos,
documents): only model families that take image input, best first. Claude Opus/Sonnet,
GPT-5, GPT-4.1, o3, Gemini Pro and Qwen VL Max/235B come first, then Claude Haiku,
GPT-4o, Gemini Flash, other Qwen VL, Llama 4, Grok 4, GLM V and Kimi K2.5+, then small
or older vision models (Gemma 3, Pixtral, Llama 3.2 Vision, …). Small variants drop a
tier. Text-only models and image generators are left out. Your own list goes in
`settings.autoImageModels` (or *Configure*), or the request's `models` array.

Requests from the Pi itself need no key. Requests from the LAN need one, and you can
force keys everywhere with `zilong-router settings requireApiKey=true`.

## Sign-in (OAuth) and free providers

Some providers have no API key: you sign in with your account instead. Open the provider
in **Providers** and press **Sign in with …**. Tokens are stored on the account and
refreshed automatically before they expire (and once more after a 401).

| Provider | How you sign in |
|---|---|
| Claude Code (`claude`) | Open the sign-in page, log in, then paste the address you land on (`http://localhost:…/callback?code=…`) back into the dashboard. The page itself may not load, that's expected. |
| xAI (`xai`), Cline, Cline Pass | Same: paste the callback address. xAI always lands on `http://127.0.0.1:56121/callback`. |
| GitHub Copilot, Kimi, Kilo Code, MiniMax Code (CN / global), Nous, Muse | Device code: the dashboard shows a code, you enter it on the provider's page, the dashboard notices when you approve. |

Claude Code and GitHub Copilot carry upstream's risk notice: they use a subscription session
that is not licensed for proxy use, so the account may be restricted or banned. The
dashboard shows that warning on their pages.

Behind the scenes, as upstream does: Claude sign-ins send Claude Code's identity (system
prompt, billing header, `metadata.user_id`) and rename tools so they don't clash with
Claude Code's own, restoring the names in the reply. GitHub accounts trade the GitHub token
for a Copilot token and send Claude models to Copilot's `/v1/messages`. Kimi, Cline,
MiniMax, Muse and Kilo get their client headers.

Free providers need no account: **MiMo Free** (`mimo-free`, `mmf`) fetches a free-tier
token by itself. **OpenCode Free** (`opencode`, `oc`) sends requests the way the OpenCode
v2 CLI does: its user agent and session headers, always streamed, with OpenCode's `read`
and `shell` tools declared next to yours. The free tier refuses requests without them, and
the model is told not to call them. Its free chat models (`-free` ids plus `big-pickle`)
come from `opencode.ai/zen/v1/models`, plus every free chat model on OpenCode's pricing
page that the live list leaves out (big-pickle, glm-5.3-flash, deepseek-v4.1-flash,
space-bunny, beam, ll-alpha, longcat-2.5-preview, exo, mimo-v2.6-flash, mimo-v2.5,
nemotron-3-ultra, nemotron-3.5-lightning and nemotron-3.5-super-vl), and show up in
`/v1/models` and `auto`; `union-alpha` goes to its Anthropic endpoint. Left out: the Muse
Spark models (OpenAI Responses API) and Jev (OpenCode's own `systemone` protocol). Local Ollama is best added as an OpenAI-compatible
endpoint (`http://<host>:11434/v1`).

Not ported, because they need a proprietary protocol or impersonate an IDE (see
`data/skipped.json`): Cursor, Windsurf, Kiro, Qoder, Trae, Zed, Codex (Responses API),
Gemini CLI / Antigravity (Gemini format), Devin CLI, OpenCode Zen / Go, iFlow, CodeBuddy,
Kimchi, GitLab Duo.

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
  *Available Models* chips with copy and custom models, plus one *Test all* button that
  sends a short "hi" to every listed model (three at a time, 30s each at most) and shows
  which work, their latency and the error of those that don't (with *Remove failed*).
  *Clean up* runs the same test and removes every model that fails (rate-limited ones
  are kept): models you added are deleted, catalogue ones are hidden from the page,
  `/v1/models` and `auto` until *Restore*. *Refresh models* pulls the provider's live
  model list (through its best account; OpenCode Free needs none), tests the models
  the page doesn't list yet and adds the ones that answer. *Add OpenAI Compatible* /
  *Add Anthropic Compatible* create custom endpoints with a prefix.
- **Usage**: Today / 24h / 7D / 30D / 60D / All periods, overview cards, providers and
  recent requests, tokens / requests / cost chart, provider and model breakdowns, and
  the grouped, sortable table by model, account, API key or endpoint in costs or tokens.
- **Console Log** (Debug): the last 200 lines of the router log, refreshed every 1.5s
  (polled rather than upstream's SSE stream), coloured by level, with *Clear*. Every
  answered request is logged as `endpoint provider/model via account -> status in Nms`.
- **Import from 9router** (Settings card, also in the header menu): upload the JSON from
  9router's *Settings → Download Backup*, its `~/.9router/db/data.sqlite`, or a legacy
  `~/.9router/db.json`. A preview lists the accounts per provider, API keys, custom
  models, aliases and settings found, and which providers zilong-router can't use (those
  accounts are skipped). Rows are merged by id, so nothing here is deleted and importing
  twice is harmless. Custom endpoints are converted to zilong-router's form; the old
  dashboard password and *Require login* are never imported.
- **Settings**: Local Mode card with theme switcher, a Database card (storage mode,
  location, size and, with Turso, online / offline / connecting, last sync, last error
  and a *Check connection* button that runs a query on Turso), Security
  (require login, require API key, password), Routing Strategy (round robin, sticky
  limit), logout. The sidebar foot shows the database state at all times (refreshed
  every 30s) and links to the Database card.

Upstream features that zilong-router does not implement are left out of the sidebar
rather than shown dead: Combos, Quota Tracker, Token Saver, CLI Tools, Media
Providers, Proxy Pools, Skills, tunnels and SSO. Model aliases are managed with the
CLI (`zilong-router alias`).

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
zilong-router connection list          # shows cooldown state and the last error
zilong-router connection unlock --all  # clear cooldowns
zilong-router settings fallbackStrategy=round-robin stickyRoundRobinLimit=3
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
turso db create zilong-router
turso db show zilong-router --url        # libsql://zilong-router-<org>.turso.io
turso db tokens create zilong-router
export TURSO_DATABASE_URL=libsql://zilong-router-<org>.turso.io
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
- The `zilong-router` CLI subcommands cannot work against an in-memory database; use the
  dashboard.

## Roadmap

Planned, roughly in order of interest. Nothing here is implemented yet.

- **MCP gateway (meta tools).** One MCP endpoint that fronts many MCP servers, so a coding
  tool connects once instead of configuring each server:
  - register upstream MCP servers (stdio command or Streamable HTTP/SSE URL) from the
    dashboard, with per-server enable/disable and credentials;
  - a small set of meta tools (for example search tools, describe a tool, call a tool)
    so the client sees a few tools instead of hundreds, keeping prompts short;
  - tool names namespaced per server to avoid clashes, and per-API-key access control;
  - calls and errors logged next to LLM usage in the dashboard;
  - stdio servers started on demand and stopped when idle, to fit in 1 GB.
- Gemini wire format (`/v1beta/models/...:generateContent`).
- OpenAI Responses API (`/v1/responses`).
- Model combos (one name that falls back through a list of provider/model pairs).
- Embeddings, image and audio endpoints.
- Providers that need IDE-impersonation executors (Cursor, Windsurf, Kiro, Codex,
  Gemini CLI …).

## Support the project

zilong-router is free for personal use. If it saves you time or money, you can support it:

- **[GitHub Sponsors](https://github.com/sponsors/sudtanj)**: monthly or one-time.
- **[PayPal](https://www.paypal.me/sudtanj)**: one-time donation.
- **Bitcoin**: `32Ja5bcaAAYXFjKCRmcYe3SM2RsSUCmXtQ`
- A star on GitHub helps others find it.

The same options are behind the **Donate** button in the dashboard.

**Commercial use** (in or for a business, as a hosted service, or inside a paid product) needs
a commercial license; see [License](#license). Paid setup help and custom features
(providers, deployment on your hardware) are also available. Get in touch via
[LinkedIn](https://www.linkedin.com/in/sudtanj/) or [GitHub](https://github.com/sudtanj).

## License

**Source-available: free for personal use, paid for commercial use.**

- Modifications and original code are under the [PolyForm Noncommercial License 1.0.0](LICENSE-POLYFORM-NONCOMMERCIAL.md), Copyright (c) 2026-present Sudono Tanjung. You can run, read and modify it for personal and other noncommercial use at no cost.
- **Commercial use** (in or for a business, as a hosted service, or in a commercial product) needs a paid commercial license: contact [Sudono Tanjung on LinkedIn](https://www.linkedin.com/in/sudtanj/) or [@sudtanj](https://github.com/sudtanj).
- The portions derived from [9router](https://github.com/decolua/9router) (Copyright (c) decolua and contributors) stay under the [MIT License](LICENSE-MIT), and versions published before the license change remain available under it. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
