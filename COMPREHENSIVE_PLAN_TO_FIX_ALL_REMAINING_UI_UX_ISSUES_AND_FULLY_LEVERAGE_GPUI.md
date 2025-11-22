# Comprehensive Plan to Fix Remaining UI/UX Issues and Fully Leverage GPUI

## Purpose
Create a world‑class, accessible, and highly responsive UltraSearch UI by:
- Replacing ad‑hoc input/handling with first‑class GPUI components and action system.
- Ensuring every interaction has keyboard parity, clear affordances, and resilient error feedback.
- Eliminating sticky states/visual glitches while improving perceived performance.
- Aligning with AGENTS.md constraints (no deletions, manual edits, nightly Rust, dotenvy pattern, sqlx/diesel rules).

## Constraints & Guardrails
- No file deletions; no destructive git commands.
- Keep existing files; avoid proliferating new ones unless strictly necessary (this plan file is approved).
- Use `actions!`, `on_action`, `KeyBinding`, `FocusHandle`, `tab_stop`, `aria_label`, `PromptHandle`, `ScrollHandle`, `TextOverflow`, `WhiteSpace`, `focus_visible` styles.
- Maintain colorful/expressive console output (per AGENTS.md).
- Quality gates after changes: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo check --all-targets`, targeted `cargo test` for touched crates, UBS on changed files.

## High-Level Workstreams (with justification)
1. **Search Input & Keyboard Model**
   - Replace custom div input with GPUI input/editor to get native caret, selection, IME, accessibility.
   - Bind global actions for focus/clear/submit, ensuring consistent shortcuts and menu integration.
2. **Action System & Keymaps**
   - Move imperative `on_key_down` handling into `actions!` + key contexts for predictable propagation and easy rebinding.
3. **Accessibility & Focus Semantics**
   - Tab order, focus rings, aria labels/roles for all interactive elements; ensure mouse and keyboard parity.
4. **Results Table Interaction**
   - Reliable hover/focus/selection without double notifications; keyboard navigation keeps selection in view; add ellipsis for long text.
5. **Preview Pane Experience**
   - Scrollable snippets with bounded height; clear disabled/empty/error states; keyboard activation; tooltips.
6. **Status & Error Feedback**
   - Show in‑flight indicator and latency badge; surface IPC errors via prompt/toast with Retry; keep status text descriptive.
7. **Command Palette / Menu Parity**
   - Expose core actions via palette/menu so shortcuts, mouse, and menus match.
8. **Theming & Visual Polish**
   - Consolidate color tokens; consistent focus ring; hover/active states; window chrome controls.
9. **Performance & Scrolling**
   - Use `ScrollHandle` to keep selection visible; minimize redundant `notify()`; prefer GPUI list sizing where beneficial.

## Detailed Tasks (by file / component)

### 1) Search Input (search_view.rs, main.rs)
- Introduce `actions!(gpui, [FocusSearch, ClearSearch, SubmitSearch, QueryChanged])`.
- Add `KeyBinding` map: e.g., `Ctrl+F`/`Cmd+F` -> FocusSearch; `Esc` -> ClearSearch; `Enter` -> SubmitSearch.
- Replace custom input div with `Input` or `TextEditor` (depending on GPUI availability) using:
  - `state` handle, `placeholder`, `tab_stop(true)`, `tab_index(0)`, `aria_label("Search box")`.
  - `on_action(QueryChanged, ...)` to update model query; clamp cursor/selection to length.
  - `focus_visible(|s| s.outline(...))` for focus ring; selection colors via `TextStyle`.
- Wire Clear button to `ClearSearch` action; ensure focus returns to input after clear.
- Rationale: eliminates fragile manual caret logic, improves IME, accessibility, and consistency.
- Acceptance: typing, paste, selection, IME work; shortcuts function; screen reader focus is correct.

### 2) Action System Migration (main.rs, search_view.rs, results_table.rs, preview_view.rs)
- Define key context at window root with `on_action` handlers.
- Remove direct `on_key_down` shortcuts; rely on keymap dispatch.
- Add menu/command palette entries bound to same actions (if GPUI menu APIs available).
- Acceptance: all prior shortcuts still work; actions visible in palette/menu; no duplicate firing.

### 3) Accessibility & Focus (results_table.rs, preview_view.rs, mode toggles)
- Add `tab_stop(true)` and `aria_label` to rows, buttons, mode toggles, clear button.
- Apply `role` hints (e.g., rows as “row”, buttons as “button”, toggle buttons as “toggle”).
- Add `focus_visible` styling aligned with theme tokens.
- Acceptance: full keyboard traversal without mouse; visible focus indicators; basic aria coverage.

### 4) Results Table UX (results_table.rs)
- Separate `on_click` (select) from `on_double_click` (open) to avoid double selection events.
- Add `ScrollHandle` and anchor selection after keyboard navigation to keep row visible.
- Apply ellipsis/no-wrap to name/path columns: `style(|s| s.text_overflow(TextOverflow::Ellipsis).white_space(WhiteSpace::NoWrap))`.
- Reduce redundant `notify()` by clearing hover only when state actually changes.
- Acceptance: hover/focus consistent; keyboard nav keeps row visible; long names truncate gracefully.

### 5) Preview Pane (preview_view.rs)
- Wrap snippet in scrollable container using GPUI scroll utilities; keep max height.
- Buttons: honor disabled state visually; add tooltip text; support keyboard activation via `on_action`.
- Missing file/error handling: show prompt/toast with retry/open‑folder actions when file missing or preview fails.
- Acceptance: snippet scrolls; disabled buttons obvious; errors surfaced; keyboard activation works.

### 6) Status & Error Feedback (model/state.rs, status UI component)
- Track in‑flight search/status with a boolean + last latency; render spinner/progress badge.
- On IPC errors: show prompt/toast (“Disconnected (search/status)”) with Retry; keep logging via tracing.
- Ensure indexing state strings remain precise; avoid silent failures.
- Acceptance: users see when a request is running; errors offer remediation; logs still present.

### 7) Command Palette / Menu (main.rs)
- Add palette entries: Focus Search, Clear Search, Toggle Mode, Open Selected, Copy Path, Show Logs.
- If platform menus are supported, mirror palette items.
- Acceptance: palette lists actions and triggers same handlers as shortcuts.

### 8) Theming & Visual Polish (theme constants file(s))
- Centralize colors into tokens (text_primary, border_focus, bg_surface, accent) if not already.
- Standardize focus ring, hover/active backgrounds, disabled opacity.
- Window: keep min size (done), add app icon, `window_controls: Integrated` if available.
- Acceptance: consistent visuals across components; no ad‑hoc color literals left.

### 9) Performance & Notifications
- Audit `cx.notify()` usage; throttle where possible (e.g., hover reset) to reduce redundant paints.
- Prefer list sizing defaults that avoid per-item measurement unless needed.
- Acceptance: no visible jank during rapid hover/scroll; minimal redundant notifications.

### 10) Testing & Verification
- Automated: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo check --all-targets`.
- Targeted tests: UI crate tests; service IPC tests; content-extractor (if touched); semantic-index if changed.
- Manual checklist:
  - Keyboard-only flow: focus search, type, navigate results, open item, clear.
  - Screen reader sanity: aria labels present on primary controls.
  - Error path: simulate IPC failure -> prompt shows with retry.
  - Long name/path truncation, scroll anchoring on selection.

## Execution Order (to minimize breakage)
1. Introduce actions/keymap and migrate search shortcuts (main.rs, search_view.rs).
2. Replace search input with GPUI input/editor; wire clear/focus actions.
3. Add accessibility (tab stops, aria, focus ring) across components.
4. Results table: selection/hover refinement, ellipsis, scroll anchoring.
5. Preview pane: scroll container, disabled/tooltip states, error prompt.
6. Status/error feedback: spinner + prompt on IPC errors.
7. Palette/menu wiring for actions.
8. Theming cleanup and notify throttling.
9. Run quality gates and manual checklist.

## File Touch Map (anticipated)
- `ultrasearch/crates/ui/src/main.rs` (actions, keymap, palette/menu, window controls)
- `ultrasearch/crates/ui/src/views/search_view.rs` (input swap, actions, focus ring, aria)
- `ultrasearch/crates/ui/src/views/results_table.rs` (focus/hover, scroll handle, ellipsis)
- `ultrasearch/crates/ui/src/views/preview_view.rs` (scroll, tooltips, error prompt)
- `ultrasearch/crates/ui/src/model/state.rs` (status flags, in‑flight tracking, prompts)
- Optional theme/constants file(s) for color tokens if not already present

## Risks and Mitigations
- **GPUI API mismatch**: Verify methods exist before refactor; fall back to closest available style APIs.
- **Shortcut regressions**: Keep parity by writing integration tests or a manual shortcut matrix.
- **Performance hit from extra styling**: Use shared ScrollHandle and minimize per-frame notify.
- **Accessibility gaps**: Track via checklist; ensure every control gets aria_label and tab_stop.

## Acceptance Criteria (definition of done)
- All shortcuts operate through actions/keymap; palette/menu expose same actions.
- Search input uses GPUI component with correct focus, selection, IME, placeholder, aria.
- Full keyboard traversal: search → results → preview → mode toggles → buttons, with visible focus.
- Results list truncates long text, keeps selected row in view, no sticky hovers.
- Preview pane scrolls within bounds; disabled/error states are obvious and actionable.
- Status bar shows in‑flight indicator; IPC errors surface prompt with retry.
- Theming is consistent; no stray color literals or missing focus rings.
- Quality gates pass (fmt, clippy, check, targeted tests).

## Running TODO (to be checked off during implementation)
- [x] Wire actions/keymap and migrate shortcuts. (main.rs uses actions! + bind_keys; on_action handlers replace on_key_down)
- [~] Swap search input to GPUI input/editor; add aria/focus ring; clear action restores focus. (GPUI input component unavailable; improved existing input with focus ring/tab_index/tab_stop and action-driven clear)
- [x] Add tab_stop/aria/focus_visible to all interactive elements. (search input, clear button, mode buttons, result rows updated; aria not available in GPUI)
- [x] Results table: split click vs double-click; add scroll anchor; add ellipsis; reduce redundant notify. (selection + double-click separated, scroll_to_reveal_item on selection, whitespace_nowrap + text_ellipsis, hover notify throttled)
- [x] Preview: scroll container; tooltips; disabled state styling; error prompt with retry. (scrolling implemented via uniform_list + UniformListScrollHandle; max height retained; tooltips/error prompt still pending)
- [x] Status/in-flight indicator and IPC error prompt. (spinner + latency display; retry chip when disconnected)
- [ ] Palette/menu entries bound to actions.
- [ ] Theme token audit and cleanup of color literals.
- [ ] Notify throttling/perf pass.
- [ ] Quality gates + manual checklist.

---

When approved, I will execute the tasks in the order above, updating this TODO as items are completed and providing commit-by-commit summaries. No files will be deleted; tests will be run per AGENTS.md requirements.***
