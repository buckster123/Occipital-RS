# Occipital-RS — The Politeness Contract

This is the differentiator. A raw `curl`-style agent tool gets a node blocked, rate-limited, or
fingerprinted as a bot within minutes. Occipital's fetch layer is engineered to be a **good web
citizen first**. The stance: **be honest and identifiable, pace conservatively.** We do *not*
cloak, rotate fake identities, or evade detection — we simply behave like a considerate reader.

All of this lives in `occipital::fetch`, in front of the one shared `reqwest` client. Nothing
above it can bypass it (the only network door).

## The rules

1. **Per-domain rate limiting.** A token-bucket per registrable domain (eTLD+1), default
   `OCCIPITAL_RATE_PER_DOMAIN = 0.5` req/s (1 every 2s). Bursts capped at the bucket size.
2. **Jitter.** Every inter-request delay gets ±30% randomization, so traffic never looks like a
   metronome (the #1 bot tell).
3. **Global concurrency cap.** `OCCIPITAL_MAX_CONCURRENCY = 4` in-flight fetches total, so a
   broad search can't fan out into a hammer.
4. **Honest user-agent.** `OCCIPITAL_USER_AGENT` defaults to
   `Occipital/<ver> (+https://github.com/buckster123/Occipital-RS; ApexOS web reader)` — it says
   what it is and how to contact. Configurable, never randomized per-request.
5. **robots.txt awareness.** `OCCIPITAL_RESPECT_ROBOTS = 1` (default): fetch + cache each
   domain's robots.txt, honor `Disallow` + `Crawl-delay` (raises that domain's min interval).
   A disallowed URL returns a clean "blocked by robots" note, not an error.
6. **Backoff that listens.** On `429`/`503`, honor `Retry-After` if present, else exponential
   backoff (base 2s, cap 60s, ±jitter). Repeated 429s raise that domain's interval for the
   session (adaptive politeness).
7. **Conditional GET.** Stale cache entries refresh with `If-None-Match`/`If-Modified-Since`; a
   `304` costs the origin almost nothing. The cache itself is the biggest politeness win —
   re-asked queries make **zero** live requests.
8. **Caps on appetite.** Per-`web_search`, fetch at most `top_n` results (default 5); per-page
   body size cap (default 2 MB, truncate with a note); total per-turn fetch budget so one task
   can't crawl the web.
9. **Deliberate writes only.** A POST happens solely from an explicit `web_submit`/`web_click`
   — never from extraction, cache refresh, or retry logic. It is **never replayed
   automatically** (not idempotent: a 429/503/transport error returns honestly instead), its
   result page is never cached, and it rides the same robots gate + rate budget as every read.

## What we deliberately do NOT do

- No headless browser / JS execution (also keeps it pure-Rust + Nano-able). Pages that *require*
  JS return what static HTML yields + a note; the agent can ask a keyed provider instead.
- No UA rotation, header spoofing, cookie/session farming, CAPTCHA solving, or proxy rotation to
  evade blocks. If a site says no, we respect it. (A node *operator* may set their own UA/headers
  via config — that's their call on their own infra, not a built-in evasion feature.)
- No login-walled or paywalled content bypass.

## Sessions & identity (Phase 15)

Cookies are **off by default** (`OCCIPITAL_COOKIES=0`) — with them off no jar exists and nothing
is stored or sent. Enabled, the node keeps **one jar: one identity**, so multi-step flows and
*operator-provisioned* logins work. The boundary is unchanged: sessions are for continuity, never
for farming or rotating identities. The honest user-agent is **not** overridable by per-domain
header rules (`user-agent`, `cookie`, `host`, `content-length` are refused with a warning), a
cross-site `Domain` attribute on a `Set-Cookie` is rejected outright, `Secure` cookies never
travel over http, and `OCCIPITAL_PROXY` is topology (one proxy, logged at startup), not rotation.
Cookie values are credentials: the jar is written `0600` and the CLI redacts them.

## Why this also makes the agent *better*

Politeness and quality are the same axis here. Rate-limiting + caching means the agent's web
access is predictable and cheap; reader-mode means its context fills with signal not chrome;
respecting blocks means a node never burns its IP reputation mid-task. A blocked or throttled
agent is a *worse* agent — good citizenship is self-interest.

## Tunables (per node)

| Var | Default | Effect |
|-----|---------|--------|
| `OCCIPITAL_RATE_PER_DOMAIN` | `0.5` | req/s per domain (token bucket) |
| `OCCIPITAL_MAX_CONCURRENCY` | `4` | global in-flight fetches |
| `OCCIPITAL_RESPECT_ROBOTS` | `1` | honor robots.txt + crawl-delay |
| `OCCIPITAL_USER_AGENT` | honest default | identify the reader |
| `OCCIPITAL_FETCH_TIMEOUT_SECS` | `30` | per-request timeout (Nano-safe) |
| `OCCIPITAL_MAX_BODY_BYTES` | `2_000_000` | per-page body cap |
| `OCCIPITAL_SEARCH_TOP_N` | `5` | results fetched per search |
| `OCCIPITAL_COOKIES` | `0` | opt-in session jar (one jar, one identity) |
| `OCCIPITAL_COOKIES_FILE` | `<data_dir>/occipital/cookies.json` | persisted jar (`0600`) |
| `OCCIPITAL_HEADERS_FILE` | unset | per-domain extra headers (UA not overridable) |
| `OCCIPITAL_PROXY` | unset | explicit proxy — topology, never rotation |

## Testing politeness

The rate limiter, jitter bounds, backoff schedule, and robots parsing are **pure** and
unit-tested without network (mock clock + `Fetcher` trait). An integration test asserts that N
queued requests to one domain are spaced ≥ the configured interval.
