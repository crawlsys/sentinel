# Sentinel viz — behavioural spec
#
# Written in Gherkin (Given / When / Then). This file is the contract
# the viz must satisfy. The adversarial judge harness in
# `tests/judge/` reads this file, runs each scenario via Playwright
# against the live :8083 viewer + :8082 API, captures evidence, and
# reports any FAILED scenarios with screenshots.
#
# Authoring rules:
#  - Every scenario MUST be runnable end-to-end against the live
#    stack — no fixtures, no mocks. The bridge writes real data, the
#    viz must handle it.
#  - Wherever a measurement is involved, the Then clause MUST cite a
#    concrete threshold (e.g. "within 2 seconds", "under 60 nodes",
#    "starts with the character #"). No "should look good" hand-waves.
#  - User-visible behaviour only. Internal implementation details
#    (cache hit ratio, sim alphaDecay) belong in unit tests, not here.

Feature: Sentinel viz delivers live agent activity at a glance

  Background:
    Given the Rust API is reachable at http://127.0.0.1:8082
    And the Next.js viewer is reachable at http://127.0.0.1:8083
    And the bridge has written at least 100 events in the last hour

  # ---------- COLD LOAD ----------

  Scenario: Cold load reaches a populated graph in under 3 seconds
    Given I navigate to /
    When the page finishes loading
    Then within 3 seconds the loading overlay disappears
    And the status bar shows either "● ready" or "● live"
    And the graph SVG contains at least 1 node
    And the ticker contains at least 1 row

  Scenario: Cold load shows a spinner while connecting
    Given I navigate to / with a freshly-launched API
    When the very first paint happens
    Then I see a loading spinner with the text "connecting to sentinel.db"
    And the status bar reads "○ connecting"

  # ---------- TICKER ----------

  Scenario: Ticker rows carry a useful label
    Given the ticker has at least 10 rows
    Then no row label is the empty string
    And no row label is exactly "Bash" without context unless the underlying tool is Bash
    And UserPromptSubmit events show "user prompt", never blank

  Scenario: Ticker rows show a category colour dot
    Given the ticker has rows of mixed categories
    Then I see a small colour dot at the start of every row
    And the dot colour matches the row's category palette:
      | category      | hex     |
      | compute (tc)  | #3fb950 |
      | planning      | #d29922 |
      | communication | #bc8cff |
      | prompt        | #58a6ff |

  Scenario: Ticker rows show a parseable timestamp
    Given the ticker has at least 5 rows
    Then every row shows a HH:MM:SS-formatted timestamp
    And no row shows "—" as its timestamp

  Scenario: Hook events do not appear in the ticker by default
    Given the page is loaded with default options
    Then no ticker row's source event type is "sentinel.hook_ingested"
    And no ticker row's source event type is "sentinel.hook_denied"

  Scenario: Grouped rows are interactive
    Given a ticker row has a "×N" badge with N > 1
    When I click the badge
    Then the row expands to show N member rows
    And each member is independently clickable
    And clicking the badge does NOT fire onSelectNode

  # ---------- GRAPH ----------

  Scenario: Graph default-hides hook nodes
    Given the page is loaded with default options
    Then the SVG contains no node with `data-node-id` starting with "SentinelHookInvocation"

  Scenario: Tool-call nodes are coloured by category
    Given the graph contains SentinelToolCall nodes
    Then nodes with `data-category="tc"` are rendered green (#3fb950)
    And nodes with `data-category="planning"` are amber (#d29922)
    And nodes with `data-category="communication"` are purple (#bc8cff)
    And nodes with `data-category="prompt"` are blue (#58a6ff)

  Scenario: Session nodes are coloured by status
    Given the graph contains SentinelSession nodes
    Then each session node circle's fill matches its `session_status` field
    | status         | hex     |
    | firing         | #3fb950 |
    | busy           | #58a6ff |
    | idle           | #d29922 |
    | dormant        | #6e7681 |
    | dead           | #484f58 |
    | awaiting_user  | #bc8cff |

  Scenario: Per-session chain edges are visible
    Given a session has at least 3 tool calls in the window
    Then the graph contains at least 2 `next_tool_call` edges connecting that session's TCs
    And `next_tool_call` edges are visually distinct from session->TC edges
    (stroke colour OR stroke width MUST differ)

  Scenario: Click-to-pan centres the selected node
    Given the graph is populated and at least one node exists
    When I click any node in the graph
    Then the SVG transform translates within 600ms so the clicked node is within 100px of viewport centre
    And the clicked node circle gains a thicker stroke (>= 2px) in accent colour (#58a6ff)

  Scenario: Click a ticker row pans the graph to the referenced node
    Given the ticker has a row whose tool_call_id matches a node in the graph
    When I click that row
    Then the graph pans to the matching node within 600ms
    And the inspector populates within 2 seconds

  # ---------- INSPECTOR ----------

  Scenario: Inspector empty state when nothing selected
    Given I have not clicked any node or ticker row
    Then the inspector shows "click a node or ticker row to inspect"

  Scenario: Inspector header avoids the raw "SentinelX" label
    Given I have clicked a SentinelToolCall node
    Then the inspector h3 does NOT contain "SentinelToolCall"
    And the h3 contains "tool" or the actual tool name (e.g. "Bash")

  Scenario: Inspector shows recent activity for the selected session
    Given I click a session node
    Then within 2 seconds the inspector "recent activity" section is populated
    And the activity section contains at least 1 segment
    And no segment preview is blank

  Scenario: Different ticker rows fetch different activity slices
    Given two ticker rows in the same session but >30s apart
    When I click the older row
    Then the activity header reads "activity ± 60s @ HH:MM:SS" with the OLDER row's timestamp
    When I then click the newer row
    Then the activity header timestamp updates to the NEWER row's timestamp
    And the visible activity segments differ from the earlier set

  Scenario: Activity segments have category-aware colours
    Given the inspector has at least 3 activity segments
    Then segments containing tool_use Bash/Read/Write/Edit have a green left border
    And segments with TaskCreate/TaskUpdate/Plan have an amber left border
    And segments with Agent/AskUserQuestion have a purple left border
    And user_input segments have a blue left border
    And had_error segments override to a red left border

  # ---------- LIVE UPDATES ----------

  Scenario: New ticker rows appear without re-layout of the graph
    Given the graph has settled (no node has moved more than 1px in the last 200ms)
    When a new SSE event arrives that introduces a new ticker row
    Then no existing graph node's (x, y) changes by more than 20px within the next second
    And the new tool-call node, if any, pulses on arrival (radius briefly increased then returns to baseline)

  Scenario: Status bar transitions
    Given the page just loaded
    Then the status indicator progresses through "○ connecting" → "● ready" → "● live"
    And it does NOT regress backwards once "● live" is reached, unless the connection drops

  # ---------- RESILIENCE ----------

  Scenario: Backgrounding the tab does not jump the layout
    Given the graph is settled
    When the page becomes hidden for 5 seconds and then visible again
    Then no existing node's (x, y) changed by more than 50px during the hidden period
    And the graph is interactive within 500ms of becoming visible again

  Scenario: API down → user-visible error
    Given the API on :8082 is killed
    When I refresh the page
    Then within 3 seconds an error banner appears
    And the message is human-readable (no raw "TypeError" or "AbortError")

  # ---------- PERFORMANCE ----------

  Scenario: Cached /api/graph hit is fast
    Given the API cache is warm
    Then a GET /api/graph returns in under 100ms server-side

  Scenario: /api/activity TTL cache keeps inspector fast
    Given the same session has been opened in the inspector within the last 6 seconds
    Then a fresh fetchActivity for the same (sid, at_ts) completes in under 50ms

  # ---------- USER ROUND-4 PUNCH LIST ----------

  Scenario: Activity segments are never literally "{}" or blank
    Given the inspector has activity segments for a session that called TaskList
    Then no segment's preview text reads exactly "{}"
    And every segment with tool_calls renders each call with at least its tool name and a non-empty description (e.g. "list current tasks" for TaskList)
    And empty-arg calls render "(no args)" rather than "{}"

  Scenario: Status bar progresses to "● live" once SSE is delivering
    Given the API SSE stream is healthy (curl -N /api/stream emits a data: line within 1s)
    When I open the viewer and wait 4 seconds
    Then the status bar reads "● live"
    And the indicator does not flip back to "○ connecting" on transient EventSource errors so long as a message has been received within the last 30s

  Scenario: Live SSE updates do not require manual refresh
    Given the bridge writes a new event after the page is open
    When 10 seconds elapse
    Then the status bar's `seq` value has increased OR a new ticker row has appeared
    And no manual refresh was required

  Scenario: Ticker labels carry agency, not just tool name
    Given the ticker has rows of mixed event types
    Then UserPromptSubmit events have a label that is NOT empty and NOT the tool field
    And events whose tool is empty render with a sentinel_event-derived label (e.g. "user prompt", "stop"), never the empty string
    And clicking any row populates an inspector that surfaces the original tool args (via the at_ts → activity wire-up)
