# Dynamic work scheduling

## Execution and result are separate

Work nodes retain the three execution states `ready`, `running`, and `done`. `done` means the current task invocation returned and the host sealed its TaskReturn. It does not mean the declared expectation passed or the user's overall goal was achieved.

The host records `expectation_met` separately from execution state. A connection refusal, HTTP 404, unavailable image input, blocker, split request, or upstream problem can be a valid `done` result with a negative outcome. The Organizer uses that result to schedule follow-up work or deliver an honest unresolved answer. Flow completion records the actual outcome and expectation fact so a completed node is not displayed as proof of business success.

Write and check facts remain host-owned. A write is recorded only after a real file change; a check is successful only for the current input version and according to that tool's result. `check` work can return a non-2xx observation as a finished result, while `expectation_met=false` preserves that the declared condition did not pass.

## Organizer scheduling methods

The Organizer sees the current user goal, the current result once, a concise execution path, exact available delivery fields, relevant facts, browser upload availability and Observer advice. The complete source, logs, screenshots and old execution details remain in the task notebook and event log. They are retrieved only when a concrete task needs them.

Each scheduling decision uses one method:

- `schedule_task` creates one concrete next task. The host generates work/node IDs, appends Flow state and resolves dependencies. Pass upstream data as exact `inputs` referencing an available `work_id` or `node_id` and listed fields.
- `read_task_result` reads selected fields from an active sealed delivery when those fields are absent from the concise context. `current_result` is already present and should not be fetched again.
- `revisit_task` creates a new execution revision for a defective upstream node, preserves its old result and deprecates the old downstream branch.
- `finish_request` returns the user-facing summary, whether the overall goal was achieved, and unresolved conditions. Completed nodes or an empty queue alone do not establish goal completion.

The Organizer request streams (`stream=true`). The host assembles the SSE frames, including tool arguments split across frames and UTF-8 characters split across network chunks. It applies exactly one scheduling method, and only after `[DONE]` or a `finish_reason` arrives. A truncated stream, an upstream error frame, multiple methods or invalid arguments produce a contract or request error and no scheduler or Flow change. A complete JSON response is accepted as `response_format=json`. Timings are milliseconds after request start, and `null` means not received: `response_headers_ms`, `first_chunk_ms`, `first_delta_ms`, `first_tool_delta_ms`, `response_complete_ms`. They are stored with the request record. `organizer/progress` events (at most one per second plus phase changes) drive the frontend's "等待响应 / 正在接收决策 / 决策已完成" display. Cancellation and the overall deadline end the request while reading; keepalives do not extend the deadline.

`process.execution_narrative` is a short text in the order tasks actually ran. It shows which tasks returned and what they returned, which are history only (deprecated), the latest report of the running task (intent, not a result), and the latest known facts with sample times. Earlier observations that a later sample superseded are listed separately as history. A last section lists the remaining limitations. `process.current_facts` carries the same current HTTP and browser facts as structured data.

A Worker `report_progress` records intent on the current node and continues the same task; it never creates a TaskReturn or an Organizer call. Only rounds with no actual project operation count toward a stall. After `REPORT_ONLY_ROUND_LIMIT` such rounds, the host hands off with `handoff.stall` and `task_returned=false`. A goal that stalled twice is not accepted again (`STALLED_GOAL_REPEATED`).

A successor that references an unreturned task is rejected with `TASK_NOT_RETURNED`, and the running task, the queue and other results are left unchanged. Scheduling unrelated new work while a task is unreturned deprecates that task explicitly, recording the reason, the replacement and its last report. Other dependency errors are `TASK_DEPRECATED`, `REVISION_MISMATCH`, `FIELD_NOT_EXPORTED`, `INVALID_REFERENCE_TYPE` and `UNKNOWN_TASK`.

Simple knowledge questions use one answer task. Composite requests grow Flow one task at a time. A task that needs splitting returns its concrete child suggestions, dependencies and completion conditions; the Organizer schedules the next suitable child rather than receiving a model-authored task tree.

## Task contract combinations

| Work type | `completion` | `checks` |
| --- | --- | --- |
| Service startup, HTTP/API endpoint check | `check` | Required supported identifiers |
| PPTX upload/load, in-browser font-request inspection, screenshot/image inspection | `output` | Must be empty |
| Source writing | `write` or `write_check` | `write_check` also needs supported checks |

Keep mixed tasks separate and pass the startup/check result to the later browser task as an explicit input. The host validates these combinations before scheduling and returns the exact rejected field path with a correction instruction.

Supported check keys are `npm:<project>:<script>`, `npm-start:<project>:<script>`, `npm-install:<project>`, `program:<cargo|git|node|python>:<project>:<args-json>`, and `http-probe:<exact-local-url>`. Shell command strings are not check identifiers. Permissions and task scope must authorize each operation; do not add unrelated tests, builds or installs.

## Browser and other live resources

Task completion and live resource availability are independent. Upload evidence stays in the completed node's history. Before scheduling a consumer, the host checks that the associated browser session, page identity and upload attempt are still active. An active receipt can be reused. An expired or mismatched receipt is reported unavailable and marked `historical`; it describes only that receipt, for example an earlier browser session. The current session, page, upload attempt and load state are host resource facts in `process.current_facts.browser`, together with the producing task. A deprecated producer does not mean the page lost its document, and a later `browser_read` showing the deck loaded with its page count becomes a current fact. The Organizer schedules a fresh open/upload only when the current facts show the document is missing, the file differs, or the session closed. A stale receipt never changes the historical node back to `ready` or proves that the current page loaded the deck. `browser_document_path` is a workspace-relative `.pptx` path; a URL there is rejected with a correction.

`browser_diagnostics` returns the page's recorded requests: every fetch/XHR call with method, URL, status, time, duration and failure, plus font and resource timing entries, filterable by `url_contains` and `category`. `request_coverage` states what the record cannot see. An empty result explains that nothing matched in the observed scope, not that no request happened. A browser-observed `/api/fonts` 200 and a server-side `http_probe` 200 are separate facts.

Network, font-request and diagnostic tasks may not set `visual_goal`. When image input is unavailable, the Worker is told it did not see the screenshot and returns an `unavailable` visual result that cites the screenshot. The task is sealed with `expectation_met=false`, and the same visual goal is not accepted again (`VISUAL_INPUT_UNAVAILABLE`).

HTTP samples and project process observations also retain source identity, time and validity. Expiry or process changes affect whether a new task may reuse a sample, not whether a completed observation happened. New sampling is tied to a real state change or current-state question; it does not rewrite the old fact. A fresh sample is reused with its age. Once the host has marked the latest sample invalid (expired, process change, superseded), a new probe needs no extra reason. Its result includes `previous_sample` with that sample's time, status, age, invalidation reason and whether it was taken before or after the managed service was observed running.

## Recovery and persistence

Organizer network/timeout failures have their own bounded retry counter and do not consume contract correction attempts. Contract errors include a rule ID, field path, submitted value, expected shape and correction guidance. Only repeating the same rule, field and rejected value triggers the repeated-contract stop.

When retries are exhausted, the host persists the failure stage alongside the existing handoff. It retains split suggestions, upstream-problem details, session continuation and other prior handoff data. The saved scheduler is resumable without rerunning completed nodes. Scheduling decisions carry a host-generated call ID and are idempotent across request replay.

New scheduler and Flow snapshots include `schema_version`. Legacy snapshots deserialize with version 0, preserve their saved statuses/results, and are stamped with the current version when restored. Migration never changes an old unfinished node into a successful result.

The Observer reviews the same host input and outputs in parallel. Its suggestions are advisory, never approval gates, and are considered during a normal Organizer decision. There is no additional acknowledgement round. Its input is the node's actual state (`node_state`): reports, returned result, exported values, check outcomes, HTTP samples with URL/status/error/time, process IDs and running state, and browser page/upload/load facts. It also receives the shared current facts. Long lists are shortened, but no object key is dropped by position. Each piece of advice carries the node identity, revision and `observed_at`. Advice written before the node's newer facts reaches the Organizer marked `based_on_older_facts` and cannot override them.
