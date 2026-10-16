#!/usr/bin/env node
// Converts the upstream 9router provider registry + pricing tables into the JSON
// files embedded by mini-router. Runs on a dev machine or in CI (needs Node >= 20),
// never on the target board.
//
//   node tools/sync-upstream/convert.mjs [upstreamDir=.upstream/9router] [outDir=data]
//
// Only providers the Rust router can drive today are kept: plain HTTP transports in
// the "openai" or "claude" wire format that work with an API key. Providers that need
// their own executor (binary/protobuf streams, OAuth-only flows, IDE fingerprints) are
// listed in skipped.json for the PR reviewer and left out of providers.json.
//
// Upstream's open-sse tree is ESM without a package.json "type":"module", so we copy
// it to a temp dir with one and import upstream's own builders from there. That way
// provider semantics (defaults, oauth field injection, model normalisation) are
// upstream's, not ours.
import { cpSync, mkdtempSync, writeFileSync, readFileSync, rmSync, existsSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { execSync } from "node:child_process";

const upstream = resolve(process.argv[2] || ".upstream/9router");
const outDir = resolve(process.argv[3] || "data");
const src = join(upstream, "open-sse");
if (!existsSync(src)) {
  console.error(`upstream open-sse dir not found at ${src}`);
  process.exit(1);
}

const tmp = mkdtempSync(join(tmpdir(), "mini-router-upstream-"));
try {
  cpSync(src, join(tmp, "open-sse"), { recursive: true });
  writeFileSync(join(tmp, "package.json"), JSON.stringify({ type: "module" }));
  const imp = (p) => import(pathToFileURL(join(tmp, "open-sse", p)).href);

  const [{ default: REGISTRY }, schema, modelSchema, pricing] = await Promise.all([
    imp("providers/registry/index.js"),
    imp("providers/schema.js"),
    imp("providers/models/schema.js"),
    imp("providers/pricing.js"),
  ]);

  // Providers whose upstream code has a dedicated executor (open-sse/executors/*.js);
  // they need Rust work beyond config and are filtered out for now.
  const SPECIAL_EXECUTORS = new Set(["antigravity","azure","bedrock","bedrock-xai","codebuddy-cn","codebuddy-intl","codex","commandcode","cursor","devin-cli","gemini-cli","github","grok-cli","grok-web","iflow","kimchi","kiro","mimo-free","minimax-code","minimax-code-global","ollama-local","opencode","opencode-go","opencode-zen","perplexity-web","qoder","qoder-cn","trae","vertex","vertex-partner","windsurf","xiaomi-mimo","xiaomi-tokenplan","zed"]);
  // Upstream-special providers mini-router implements natively (src/oauth): their
  // sign-in, token refresh and request quirks are ported, so keep them.
  const RISK_NOTICE = "⚠️ Risk Notice: This provider uses a subscription/OAuth session not officially licensed for proxy/router use. Account may be restricted or banned. Use at your own risk.";
  const NATIVE = new Set(["claude", "cline", "clinepass", "github", "kilocode", "kimi", "mimo-free", "minimax-code", "minimax-code-global", "muse", "nous", "opencode", "xai"]);
  const SUPPORTED_FORMATS = new Set(["openai", "claude"]);
  const skipped = [];
  const skip = (e, reason) => skipped.push({ id: e.id, name: e.display?.name || e.id, reason });
  const acceptsApiKey = (e) => e.noAuth || e.authType === "apikey" || (e.authModes || []).includes("apikey") || (!e.oauth && e.category !== "oauth");
  const OAUTH_INJECT = ["clientId", "clientSecret", "tokenUrl"];
  const providers = [];
  for (const e of REGISTRY) {
    if (!e.transport) { skip(e, "no LLM transport (media-only provider)"); continue; }
    if (!NATIVE.has(e.id) && (SPECIAL_EXECUTORS.has(e.id) || e.transport.executor)) { skip(e, "needs a dedicated executor"); continue; }
    if (!SUPPORTED_FORMATS.has(e.transport.format || "openai")) { skip(e, `wire format \"${e.transport.format}\" not implemented`); continue; }
    if (!NATIVE.has(e.id) && !acceptsApiKey(e)) { skip(e, "OAuth-only login flow"); continue; }
    // Upstream still offers its natively-ported providers with a risk banner.
    if (e.display?.deprecated && !NATIVE.has(e.id)) { skip(e, "marked deprecated upstream"); continue; }
    // Keep only what the Rust router needs; media/UI-only fields are dropped.
    const transport = e.transport ? { ...e.transport } : null;
    if (transport) {
      if (!transport.format) transport.format = schema.PROVIDER_DEFAULTS.format;
      if (e.oauth) for (const f of OAUTH_INJECT) if (transport[f] === undefined && e.oauth[f] !== undefined) transport[f] = e.oauth[f];
      // Functions cannot be serialised (e.g. header hooks); drop them.
      for (const [k, v] of Object.entries(transport)) if (typeof v === "function") delete transport[k];
    }
    const models = (e.models || []).map(modelSchema.normalizeModel).filter((m) => {
      const kind = m.kind || m.type || "llm";
      return kind === "llm";
    });
    providers.push({
      id: e.id,
      alias: e.alias || e.id,
      aliases: e.aliases || [],
      category: e.category,
      authType: e.authType || (e.oauth ? "oauth" : "apikey"),
      authModes: e.authModes || null,
      noAuth: !!e.noAuth,
      priority: e.priority ?? 999,
      display: e.display ? { name: e.display.name, color: e.display.color, textIcon: e.display.textIcon, icon: e.display.icon, website: e.display.website, apiKeyUrl: e.display.notice?.apiKeyUrl || e.display.notice?.signupUrl || null, deprecated: !!e.display.deprecated, deprecationNotice: e.display.deprecationNotice === "RISK_NOTICE" ? RISK_NOTICE : e.display.deprecationNotice || null } : null,
      transport,
      transports: e.transports || null,
      oauth: e.oauth || null,
      models,
      passthroughModels: !!e.passthroughModels,
      thinkingConfig: e.thinkingConfig || null,
    });
  }
  providers.sort((a, b) => a.id.localeCompare(b.id));

  const upstreamCommit = (() => {
    try { return execSync("git rev-parse HEAD", { cwd: upstream }).toString().trim(); } catch { return null; }
  })();
  const upstreamVersion = (() => {
    try { return JSON.parse(readFileSync(join(upstream, "package.json"), "utf8")).version; } catch { return null; }
  })();

  mkdirSync(outDir, { recursive: true });
  const meta = { upstreamVersion, upstreamCommit, generatedAt: new Date().toISOString().slice(0, 10) };
  writeFileSync(join(outDir, "providers.json"), JSON.stringify({ meta, providers }, null, 1) + "\n");
  writeFileSync(join(outDir, "skipped.json"), JSON.stringify({ meta, skipped: skipped.sort((a, b) => a.id.localeCompare(b.id)) }, null, 1) + "\n");

  const rows = (obj) => Object.entries(obj).map(([model, p]) => ({ model, ...p }));
  writeFileSync(join(outDir, "pricing.json"), JSON.stringify({
    meta,
    freeNamespaces: pricing.FREE_MODEL_NAMESPACES,
    models: rows(pricing.MODEL_PRICING),
    providers: Object.fromEntries(Object.entries(pricing.PROVIDER_PRICING || {}).map(([prov, m]) => [prov, rows(m)])),
    patterns: pricing.PATTERN_PRICING.map((x) => ({ pattern: x.pattern, ...x.pricing })),
  }, null, 1) + "\n");


  // Provider logos: copy upstream's PNG for every shipped provider (plus the two
  // synthetic compatible endpoints) into data/icons/, dropping stale files.
  const ICON_ALIASES = { "perplexity-agent": "perplexity", "gitlab-duo": "gitlab", "vercel-ai-gateway": "vercel", "ollama-search": "ollama", "minimax-code": "minimax", "minimax-code-global": "minimax" };
  const iconsDir = join(outDir, "icons");
  rmSync(iconsDir, { recursive: true, force: true });
  mkdirSync(iconsDir, { recursive: true });
  let copied = 0;
  for (const id of [...providers.map(p => p.id), "oai-cc", "anthropic-m"]) {
    const iconSrc = join(upstream, "public", "providers", `${ICON_ALIASES[id] || id}.png`);
    if (existsSync(iconSrc)) { cpSync(iconSrc, join(iconsDir, `${id}.png`)); copied++; }
  }
  console.log(`copied ${copied} provider icons`);
  console.log(`wrote ${providers.length} providers (${skipped.length} skipped, see skipped.json) (${providers.filter(p => p.transport).length} with transport), ` +
    `${Object.keys(pricing.MODEL_PRICING).length} model prices, ${pricing.PATTERN_PRICING.length} patterns ` +
    `from 9router ${upstreamVersion} @ ${upstreamCommit?.slice(0, 8)}`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
