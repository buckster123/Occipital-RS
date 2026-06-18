# Occipital-RS — The Follow-Along Contract

How a human watches the agent read, and nudges it. The *tools* are Occipital's; the *window* and
the *steer routing* are ApexOS-RS's. This doc defines the seam so the two repos can be built
independently.

## Principle

Occipital never knows about Slint or agentd. It just returns, with every `web_fetch`/`web_search`
result, a **structured view payload** a consumer *may* render. A pure-MCP client ignores it; the
ApexOS desktop renders it as a follow-along browser.

## The view payload (Occipital → consumer)

Every `web_fetch` / `web_search` tool result carries, alongside the agent-facing text, a
`view` object:

```json
{
  "view": {
    "kind": "page",                  // "page" | "results"
    "url": "https://example.com/x",
    "title": "…",
    "markdown": "# … reader-mode body …",
    "links": [{"text": "Next", "url": "https://…"}],
    "fetched_at": "2026-…",
    "from_cache": true
  }
}
```

For `web_search`, `kind: "results"` with `results: [{title, url, snippet, rank}]` and the
`markdown` is a rendered result list. This mirrors how ApexOS's `display_face` / `sketch_snapshot`
tools already pass a side-channel the UI consumes directly from the `tool_requested` event —
**no new agentd event type needed**: the UI reads `view` off the Occipital tool call.

## Rendering in ApexOS-RS (Slint)

- A **"Occipital" / Web window** (new launcher tile, ⊕ to the existing `Web` launcher) with a
  reader pane + a link list + a breadcrumb of the agent's path this search.
- The reader pane renders `markdown` natively (no webview — see ApexOS gotchas). Long bodies use
  a std-widgets `ScrollView` (the linuxkms no-wheel-scroll gotcha: draggable bar, not a bare
  Flickable). Auto-scroll-to-top on each new page.
- The link list is the interactive surface: each link is a clickable row.
- A subtle "📵 from cache / 🌐 live" badge + the fetched-at age, so the human knows freshness.

The window is **read-only mirror by default** — it shows where the agent is. The one interaction
is the steer.

## The steer ("go here") — consumer → agent

When the human clicks a link (or types a URL) in the follow-along window, ApexOS routes a
**navigation hint** to agentd. Design choice, to settle in the integration phase (Phase 9):

- **Gentle, queued nudge (recommended).** ApexOS sends a normal `user_prompt`-class frame like
  *"(navigation) please look at <url> next"* on the bus. It funnels through the existing
  `TurnGate` like any user message — so it can't race a turn or wedge the session (ApexOS's
  serialized-turn invariant holds for free). The agent finishes its current step, then the hint
  is the next thing it sees; it calls `web_fetch(url)` and continues. **No new turn-control code.**
- *Rejected:* a hard interrupt mid-turn (fights the TurnGate, risks an orphaned tool_result) and
  a side-channel the model can't see (the agent wouldn't know it was steered).

So the loop is: **agent searches → human sees the reader view → human clicks → "go here" arrives
as the agent's next prompt → agent fetches it and resumes the task.** The human is a collaborator
in the agent's browsing, not a driver of a separate browser.

## What lands where

| Piece | Repo |
|-------|------|
| `view` payload in tool results | Occipital (`occipital-mcp`) |
| Slint follow-along window + reader rendering | ApexOS-RS (`ui-slint`) |
| reading the `view` off the tool event | ApexOS-RS (`ui-slint` WS client) |
| routing the click → `user_prompt` nudge | ApexOS-RS (`ui-slint` → agentd) |
| nothing | agentd (no new event/turn code — reuses `register_mcp_server` + `user_prompt`) |

The elegance: Occipital stays a clean standalone web library; ApexOS gets a rich follow-along
browser by reusing mechanisms it already has (MCP registration, the tool-event side-channel, the
TurnGate, `user_prompt`). The integration is **additive**, not invasive.
