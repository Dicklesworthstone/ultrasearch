import json

file_path = '.beads/issues.jsonl'
updates = {
    'ultrasearch-c01.3': {
        'description': 'Add floating search bar. Default hotkey Alt+Space (user-configurable in settings). Detect hotkey conflicts on startup. Dismiss on Blur or Esc.'
    },
    'ultrasearch-c01.4': {
        'description': 'Professional installer (WiX v4/MSIX). Register Service, set Named Pipe ACLs (non-admin client access), register URI handler (ultrasearch://), and handle clean uninstall.'
    },
    'ultrasearch-c01.5': {
        'description': '3-step wizard: 1. Welcome/Service concepts. 2. Drive Scope (Auto-detect fixed NTFS, ignore network/floppy). 3. Privacy (Telemetry Opt-in) & Initial Scan.'
    },
    'ultrasearch-c01.6': {
        'description': 'Implement custom context menu visually matching native style. Execute core shell verbs (Open, Properties, Show in Folder, Copy Path) via ShellExecute/InvokeVerb. Avoid complex IContextMenu hosting for now.'
    },
    'ultrasearch-c02.5': {
        'description': 'Implement local LRU tracking of (Query, FileID) pairs to boost scores of frequent selections. Store in user-profile SQLite/JSON (separate from system index).'
    },
    'ultrasearch-c03.2': {
        'description': 'Detect busy/presentation/game modes using SHQueryUserNotificationState (QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN) to pause background indexing reliably.'
    },
    'ultrasearch-c03.6': {
        'description': 'Integrate Sentry telemetry for panic capture (Service & UI). STRICTLY OPT-IN via Wizard/Settings. Scrub PII (paths, usernames) before sending.'
    },
    'ultrasearch-c04.1': {
        'description': 'Use SHGetFileInfo to retrieve HICONs. Implement HICON -> RGBA Bitmap conversion for rendering within GPUI. Cache results to texture atlas.'
    },
    'ultrasearch-c04.2': {
        'description': '[High Risk] Investigate GPUI support for OLE DragSource. If native support missing, research HWND hooking to support dragging results to Explorer.'
    }
}

lines = []
try:
    with open(file_path, 'r', encoding='utf-8') as f:
        for line in f:
            if not line.strip(): continue
            try:
                issue = json.loads(line)
                if issue['id'] in updates:
                    issue.update(updates[issue['id']])
                    issue['updated_at'] = '2025-11-22T19:05:00Z'
                lines.append(json.dumps(issue))
            except json.JSONDecodeError:
                pass

    with open(file_path, 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines) + '\n')
except FileNotFoundError:
    pass
