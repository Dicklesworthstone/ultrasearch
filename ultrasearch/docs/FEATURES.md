# UltraSearch Features (Quick Reference)

## Spotlight-style Quick Search
- Toggle with `Alt+Space` (warning: PowerToys Run may conflict).
- Recent search history dropdown when the query is empty.
- Keyboard navigation: Up/Down to move, Enter to open, Esc to dismiss.
- Query highlighting in results (name and path).

## Keyboard Shortcuts Overlay / Help Panel
- Open with `F1`, `Ctrl+/`, or `Cmd+/` (also via the Help chip in the header).
- Click outside or press Esc to close.
- Groups:
  - Navigation: Ctrl/Cmd+K focus search; Up/Down move selection; Enter open; Ctrl+1/2/3 switch modes.
  - Actions: Ctrl/Cmd+C copy path; Ctrl+Shift+C copy file; Ctrl+Shift+O open folder; Alt+Enter properties.
  - System: Alt+Space quick search; Ctrl/Cmd+Q quit.
- Includes callouts for tray states, update flow, and setup docs.

## Tray & Updates
- Tray tooltip states: Idle, Indexing, Update available, Offline.
- Update flow: Check -> Download -> Restart to Update (opt-in toggle in Update panel).
- Update panel shows status, release notes, and actions.

## GraalVM (Extractous) Provisioning
- See `docs/GRAALVM_SETUP.md` for download URL + SHA256 and setup steps.
- `content-extractor` build guard enforces GraalVM CE 23.x when `extractous_backend` is enabled; smoke test runs only if `GRAALVM_HOME`/`JAVA_HOME` are set.

## IPC Self-Healing
- Named pipe client retries up to 5 times with 100ms backoff (service missing or busy); on successful reconnect, UI shows a green “Reconnected to service” toast.
- Retry successes are counted for telemetry; warnings logged on failures/timeouts.

## Status & Metrics
- Scheduler queue depth, active workers, and content jobs enqueued/dropped surfaced via metrics/status.

## Onboarding
- Three-step wizard with drive selection (content toggle per drive), privacy opt-in, and initial scan kick-off.
- Prefers fixed NTFS volumes; falls back to all detected drives if none match.
