# Modern User Experience & Distribution Plan

To elevate UltraSearch to a thoroughly modern, professional-grade Windows application, we will implement the following key features and distribution mechanisms.

## 1. Automatic Self-Updates
**Goal:** Ensure users are always running the latest version with zero friction.
*   **Mechanism:** Background check on application startup or scheduled interval (e.g., every 4 hours).
*   **Source:** GitHub Releases (public) or S3 bucket (private).
*   **UX:**
    *   **Silent:** Tiny notification "Update available - will be applied on restart".
    *   **Explicit:** "New version available. Update now?" dialog.
*   **Implementation:**
    *   Use a library like `sparkle-rs` or a custom `reqwest` + signature verification flow.
    *   **Installer Integration:** The installer must handle "in-place upgrades" gracefully (stopping the Service, replacing binaries, restarting Service).

## 2. System Tray Integration & Background Mode
**Goal:** Unobtrusive, always-on access.
*   **Behavior:**
    *   **Close/Minimize:** Closing the main window minimizes it to the System Tray area (configurable).
    *   **Startup:** Option to "Start minimized to tray".
*   **Tray Menu:**
    *   **Open Search:** Restores main window.
    *   **Quick Search:** (See below).
    *   **Pause Indexing:** Temporarily stop the background service (useful for gaming/benchmarks).
    *   **Settings:** Open config.
    *   **Quit:** Fully exit the application (stops Service interaction, though Service may persist as a system process).

## 3. Quick Search Bar ("Spotlight" for Windows)
**Goal:** Instant, keyboard-centric access without the full window weight.
*   **Trigger:** Global Hotkey (e.g., `Alt+Space` or `Ctrl+Shift+F`, configurable).
*   **UI:**
    *   A minimalist, floating input bar centered on screen.
    *   Displays top 5-10 results immediately.
    *   Expands to show snippets only on selection.
*   **Implementation:**
    *   Separate GPUI window with `WindowLevel::Floating` or `PopUp`.
    *   Uses the same IPC client to query the Service.

## 4. Professional Windows Installer (WiX v4 / MSIX)
**Goal:** "Bonafide" installation experience following Microsoft best practices.
*   **Service Management:** Automatically install, configure, and start the `UltraSearch Background Service` with `LocalSystem` privileges.
*   **Firewall:** Register necessary firewall rules for the IPC named pipes (if constrained) or local HTTP metrics.
*   **ACLs:** Secure the `%PROGRAMDATA%\UltraSearch` directory.
*   **Dependencies:** Check for and install VC++ Redistributables if missing.
*   **Code Signing:** Sign all binaries (and the installer itself) to avoid SmartScreen warnings (requires certificate).
*   **Tooling:**
    *   **WiX Toolset v4:** Standard XML-based definition for creating `.msi` packages. Highly flexible for Service installation.
    *   **MSIX:** Modern containerized format. Good for Store, but harder for "Windows Services" without specific capability declarations (`runFullTrust`, `localSystem`). *Recommendation: Start with WiX MSI for maximum compatibility and Service control.*

## 5. Modern UI Polish
*   **Acrylic/Mica Effects:** Use Windows 11 background materials if GPUI supports or exposes handle.
*   **Animations:** Smooth transitions for result rows and preview pane.
*   **Dark/Light Mode:** Respect system theme automatically.
