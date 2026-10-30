# v8.0.42 regression inputs

These inputs join the named existing suites through `src/pin42.rs`. They
use the same Rust entry points, Go overlays, readers and strict comparisons
as every other case. Registry copies carry the original and translated
requests unchanged. Fixture names identify the behavior and its controls.

- `thinking.json`: suffix validation, capability refusals and clamping,
  source precedence, native Responses updates and configured registry lookup.
  `refusal-*` IDs isolate the two user-visible thinking refusal changes.
  Registry lookup feeds the resolved-model helper on both sides; this does
  not exercise the executor's private `apply_request` or credential selection.
- `schema.json`: reference conjunctions, scope, annotations, numeric spellings
  and cleaning a Claude tool schema twice (`repeat_clean`). Both outputs
  remain in the comparison.
- `arguments.json`: open/closed schemas, empty and placeholder continuations,
  completion transitions and translated-schema evidence. Continuations omit
  the function name so they continue the existing call. `trace_events` retains
  each input chunk's output separately, including empty output, so an early
  argument delta cannot hide in a concatenated stream.
- `signatures.json`: call boundaries, carriers, bypass policy and repeated
  sanitizing (`repeat_sanitize`), with both sanitized outputs compared.
- `tools.json`: mixed builtin/function tools, controls, filtering and search.
- `terminal.json`, `call-ids.json`, `feedback.json`: Gemini terminal output,
  usage timing, Unicode, literal supplied IDs and candidate-bearing feedback
  without a finish reason. Candidate-less prompt-block classification, endpoint
  routing and catalog freshness need executor/catalog oracles and are skipped.
- `usage.json`, `speed.json`: Interactions parsing, public merge sequences,
  stream-buffer sequences and Claude reported speed. Every intermediate
  detail and its token breakdown is retained, rather than only the final sum.
- `outcomes.json`, `search.json`: Claude exhaustion/length, complete/partial
  items and deferred search histories across tool outputs and user boundaries.
- `commentary.json`: streamed/done-only commentary, following messages,
  reasoning signatures and final/tool order.

The usage helper's `merge` and `sequence` modes invoke product helpers, not a
second accounting implementation. `merge` parses each step with its named
product parser before calling the public merge function; `sequence` invokes
the corresponding stream buffer observer (Interactions uses native latest
count replacement). Neither mode observes outbound reporter/queue records.
HTTP statuses are not returned by the existing thinking helper: its complete
body and error outcome/message are compared. Transport status and selected
credential behavior remain product unit/mock coverage.
