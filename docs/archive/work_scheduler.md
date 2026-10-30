# Dynamic work scheduling

## Execution and result are separate

Work nodes retain the three execution states `ready`, `running`, and `done`. `done` means the current task invocation returned and the host sealed its TaskReturn. It does not mean the declared expectation passed or the user's overall goal was achieved.

New scheduler results do not emit `expectation_met`. Older persisted values remain compatibility history and are not a conclusion about the user's goal. A connection refusal, HTTP 404, unavailable image input, blocker, split request, or upstream problem can be a valid `done` result. The Organizer uses the Worker return and dated host evidence to schedule follow-up work or report unresolved acceptance conditions.

Write and check facts remain host-owned. A write is recorded only after a real file change; a check is successful only for the current input version and according to that tool's result. These facts do not synthesize a Worker return or decide whether a business/user goal passed.

## Organizer scheduling methods

The Organizer sees the current user goal, the current result once, a concise execution path, exact available delivery fields, relevant facts, browser upload availability and Observer advice. The complete source, logs, screenshots and old execution details remain in the task notebook and event log. They are retrieved only when a concrete task needs them.

Each scheduling decision uses one method:

- `schedule_task` creates one concrete next task. The host generates work/node IDs, appends Flow state and resolves dependencies. Pass upstream data as exact `inputs` referencing an available `work_id` or `node_id` and listed fields.
- `read_task_result` reads selected fields from an active sealed delivery when those fields are absent from the concise context. `current_result` is already present and should not be fetched again.
- `revisit_task` creates a new execution revision for a defective upstream node, preserves its old result and deprecates the old downstream branch.
- `finish_request` returns the user-facing summary, whether the overall goal was achieved, and unresolved conditions. Done nodes or an empty queue alone do not establish goal completion.

The Organizer request streams (`stream=true`). The host assembles the SSE frames, including tool arguments split across frames and UTF-8 characters split across network chunks. It applies exactly one scheduling method, and only after `[DONE]` or a `finish_reason` arrives. A truncated stream, an upstream error frame, multiple methods or invalid arguments produce a contract or request error and no scheduler or Flow change. A complete JSON response is accepted as `response_format=json`. Timings are milliseconds after request start, and `null` means not received: `response_headers_ms`, `first_chunk_ms`, `first_delta_ms`, `first_tool_delta_ms`, `response_complete_ms`. They are stored with the request record. `organizer/progress` events (at most one per second plus phase changes) drive the frontend's "等待响应 / 正在接收决策 / 决策已完成" display. Cancellation and the overall deadline end the request while reading; keepalives do not extend the deadline.

`process.execution_narrative` is a short text in actual invocation order. It shows which tasks returned and what they returned, which are history only (deprecated), the latest report of the running task (intent, not a result), and host actions with sample times. HTTP observations are supplied as a separate chronological list; later samples do not rewrite or interpret earlier ones. Worker-reported limitations remain attached to their task. `process.current_facts` carries dated HTTP observations and current browser availability as structured data.

A Worker `report_progress` records intent on the current node and continues the same task; it never creates a TaskReturn or an Organizer call. Only rounds with no actual project operation count toward a stall. After `REPORT_ONLY_ROUND_LIMIT` such rounds, the host hands off with `handoff.stall` and `task_returned=false`. A goal that stalled twice is not accepted again (`STALLED_GOAL_REPEATED`).

A successor that references an unreturned task is rejected with `TASK_NOT_RETURNED`, and the running task, the queue and other results are left unchanged. Scheduling unrelated new work while a task is unreturned deprecates that task explicitly, recording the reason, the replacement and its last report. Other dependency errors are `TASK_DEPRECATED`, `REVISION_MISMATCH`, `FIELD_NOT_EXPORTED`, `INVALID_REFERENCE_TYPE` and `UNKNOWN_TASK`.

Simple knowledge questions use one Organizer-assigned answer task. A plain-text Worker response seals that invocation and goes back to Organizer, just like `yield_work`. `final_answer` never bypasses that handoff or completes the overall request. Only Organizer's explicit `finish_request` assesses the goal. Composite requests grow Flow one task at a time. A task that needs splitting returns its concrete child suggestions, dependencies and completion conditions; the Organizer schedules the next suitable child rather than receiving a model-authored task tree.

The host only dispatches, records and loads information in order. Program errors retain their complete original diagnostic chain; the host adds no diagnosis, recovery judgment or repair guidance. Process exit codes, output streams, probe replies and resource identities remain recorded facts. Worker return submissions are stored intact as `worker_return`, readable through `read_task_result`; context compaction changes only the projection, not the saved return.

The host records Organizer's explicit goal assessment as submitted. It neither derives that assessment from queue state nor changes unfinished child nodes into successful returns when Organizer finishes the request.

## Work scope and checks

`completion` is optional legacy metadata; it does not classify task contents or imply success. `checks` are optional supported host-observation identifiers and can coexist with browser, visual, HTTP, or writing work when that makes the task clearer. `edit_targets` and tool permissions still govern writes. Split work when distinct steps or dependencies call for it, then pass required upstream fields as explicit inputs. The host validates field types, permissions, paths, dependencies, and supported check identifiers, and reports the exact rejected field with correction guidance.

Supported check keys are `npm:<project>:<script>`, `npm-start:<project>:<script>`, `npm-install:<project>`, `program:<cargo|git|node|python>:<project>:<args-json>`, and `http-probe:<exact-local-url>`. Shell command strings are not check identifiers. Permissions and task scope must authorize each operation; do not add unrelated tests, builds or installs.

## Browser and other live resources

Task completion and live resource availability are independent. Upload evidence stays in the completed node's history. Before scheduling a consumer, the host checks that the associated browser session, page identity and upload attempt are still active. An active receipt can be reused. An expired or mismatched receipt is reported unavailable and marked `historical`; it describes only that receipt, for example an earlier browser session. The current session, page, upload attempt and load state are host resource facts in `process.current_facts.browser`, together with the producing task. A deprecated producer does not mean the page lost its document, and a later `browser_read` showing the deck loaded with its page count becomes a current fact. The Organizer schedules a fresh open/upload only when the current facts show the document is missing, the file differs, or the session closed. A stale receipt never changes the historical node back to `ready` or proves that the current page loaded the deck. `browser_document_path` is a workspace-relative `.pptx` path; a URL there is rejected with a correction.

`browser_diagnostics` returns the page's recorded requests: every fetch/XHR call with method, URL, status, time, duration and failure, plus font and resource timing entries, filterable by `url_contains` and `category`. `request_coverage` states what the record cannot see. An empty result explains that nothing matched in the observed scope, not that no request happened. A browser-observed `/api/fonts` 200 and a server-side `http_probe` 200 are separate facts.

Network, font-request, diagnostic, and visual requirements may coexist when the requested work benefits from them. If image input is unavailable, the Worker reports that limitation and explicitly returns the invocation; the Organizer can arrange another route or retry when capability changes. The completed invocation remains history, and no permanent host block is applied to that goal.

HTTP samples and project process observations retain source identity and time. Expiry or process changes affect whether a sample can be reused from cache or satisfy an unfinished host check, not whether that observation happened or whether a completed node stays done. New sampling does not rewrite the old fact. A sample reused from cache includes its age and the cache-reuse decision; no service-start timing relation is inferred.

## Recovery and persistence

Organizer network/timeout failures have their own bounded retry counter and do not consume contract correction attempts. Contract errors include a rule ID, field path, submitted value, expected shape and correction guidance. Only repeating the same rule, field and rejected value triggers the repeated-contract stop.

When retries are exhausted, the host persists the failure stage alongside the existing handoff. It retains split suggestions, upstream-problem details, session continuation and other prior handoff data. The saved scheduler is resumable without rerunning completed nodes. Scheduling decisions carry a host-generated call ID and are idempotent across request replay.

New scheduler and Flow snapshots include `schema_version`. Legacy snapshots deserialize with version 0 and are stamped with the current version when restored. A legacy worker node is normalized to `done` only when it has a saved delivery; an unfinished or resultless node is never promoted. Legacy `outcome=completed` and synthesized false flags are treated as unknown because old host versions supplied them by default. Overall goal completion is restored only from an explicit saved Organizer assessment (`goal_achieved=true`), never from done nodes or an empty queue.

The Observer reviews the same host input and outputs in parallel. Its suggestions are advisory, never approval gates, and are considered during a normal Organizer decision. There is no additional acknowledgement round. Its input is the node's actual state (`node_state`): reports, returned result, exported values, check outcomes, HTTP samples with URL/status/error/time, process IDs and running state, and browser page/upload/load facts. It also receives the shared current facts. Long lists are shortened, but no object key is dropped by position. Each piece of advice carries the node identity, revision and `observed_at`. Advice written before the node's newer facts reaches the Organizer marked `based_on_older_facts` and cannot override them.
