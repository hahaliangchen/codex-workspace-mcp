# Twenty-Page Context Acceptance Fixture

Fixture version: 1. Keep this file, outline.json, shared.css, and the checker identical for every compared run. Work in an isolated copy. Never use an existing conversation database or change another run's files.

## Deliverables

Create page01.html through page20.html in the workspace root, following the ordered outline. Each page is a separate document with exactly one h1 whose text equals its outline title. Its title element also equals that title. Every outline point appears as visible body text, in order. Paragraphs may explain the points, but do not replace their exact wording. Use a main element with the page class, and apply the specified layout class to the content container.

Each document declares HTML5, UTF-8, and an English document language. Reference shared.css through a stylesheet link. Use semantic headings, sections, paragraphs, and lists. Keep all presentation in the common stylesheet. Do not use inline styles, style elements, scripts, remote resources, data URLs, duplicate IDs, or dynamically generated content. Required content cannot be hidden inside a comment, a script, a head element, or an attribute. Each generated file is between 300 and 120000 UTF-8 bytes. All documents must parse with matching tags.

The initial task does not include footers. A separate user instruction will arrive after page 10 has passed verification. The checker has an explicit option to activate that instruction. Once active, only pages 11 through 20 receive a footer containing their page number and the exact text 内部资料. Pages 1 through 10 must retain no footer. Do not anticipate the instruction or apply it retroactively. Preserve the applicable original user instruction when summarizing or returning a task.

## Shared Presentation

Use a 1280 by 720 document format with at least 48 pixels of inner padding. Use a white background, dark neutral text, green section accents, and restrained gray separators. Typography is Arial or the browser's sans-serif fallback. Main headings use 36 pixels. Body text uses 18 pixels. Footer text uses 14 pixels. Text must wrap and stay inside the document content area. Do not remove outline points to make a layout fit. Do not introduce decorative illustrations or unrelated placeholder imagery.

The overview layout is a single introductory region. The columns layout places three related points in separate columns. The checklist layout presents ordered verification items. The repair-grid layout is required only for page 7. Every class used by a page must have a corresponding class selector in shared.css. If a required layout is missing, repair the common stylesheet and verify the affected page again. A missing class is a failed check, even when a browser would render readable unstyled content.

When editing shared.css, retain existing selectors and their behavior. Add the missing selector with explicit grid tracks and spacing. Do not work around the missing class by renaming the page's required layout, inserting inline styling, weakening the checker, or changing the outline. The repaired stylesheet becomes a shared resource for the remaining work. Verification of a repair must use the actual saved stylesheet rather than a report that the edit succeeded.

## Execution and Evidence

Read the outline and this specification before generating pages. A page is complete only after its saved file passes check_pages.py. Check one page immediately after writing it, using --page N. A failed check must be diagnosed and corrected before calling that page complete. After all pages are written, run the checker without --page to verify all twenty files against their current common stylesheet. Use --footer-from 11 after the user update becomes effective. The JSON output contains the exact checked file hashes and the shared stylesheet hash.

Keep the outline, title, required points, page number, and layout identity together when planning or delegating a page. A previous page's successful check is not evidence for a later page. A tool's acceptance of a write is not a completed verification. A successfully generated subset is progress, not delivery. Preserve check failures and their diagnostic text so that work can resume after compaction or interruption without repeating an already completed repair.

The page 7 repair and the page 10 instruction are deliberate acceptance events. A scripted model forces their occurrence to test protocol behavior. A real model is observed for planning, material reuse, completion, and correctness. Do not assume that a scripted model's successful run proves that a real model will plan correctly, stop rereading material, or apply the instruction after compaction.

## Reproducibility

Record the model, provider, context-window configuration, fixture hash, task ID, and code revision before a run. Use a total window of 65536 tokens unless the run manifest explicitly specifies another limit. Compare runs only with identical fixture content, model route, window, checker, and input task. Log actual provider token usage where available and identify estimates separately. Missing provider usage is unknown, not zero.

Report successful page checks, failed checks, repeated file or asset reads, compaction attempts and failures, token totals, and cached-token counts where the provider exposes them. Distinguish a deliberate final all-page recheck from repeated attempts to recover forgotten state. Keep original usage events unchanged and count each request once. Do not count both a task and its folded child as independent token consumption for the same request.

The initial baseline may finish successfully. If it does not reproduce growing per-page input cost, repeated reads, or incomplete delivery, record that result and increase fixture difficulty before fixing the comparison configuration. Do not quietly select a failed run or alter only the old version's task. Model behavior is stochastic; keep each run's manifest and outcome available for review.
