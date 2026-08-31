# Changelog

All notable changes to [UltraSearch](https://github.com/Dicklesworthstone/ultrasearch) are documented in this file.

UltraSearch is a high-performance, memory-efficient desktop search engine for Windows. It combines NTFS MFT enumeration (Everything-style instant filename search) with full-text content indexing via Tantivy, using a multi-process architecture: an always-on service, short-lived index workers, and a GPU-accelerated GPUI desktop UI.

**Versioning note:** Tags v0.2.0 through v0.2.37 were rapid CI iteration tags (many are single-commit CI fixes). The **v1.4.5** release is the first stable, published GitHub Release with downloadable Windows binaries. Tags without a corresponding GitHub Release are marked **(tag only)**.

---

## [Unreleased]

## [v1.5.1] -- 2026-08-31

### Fixed

- **`UltraSearchService` now acts on service controls (#3).** The application
  (config load, initial volume scan, scheduler) runs on a worker thread while
  the service main thread waits; STOP/SHUTDOWN reports StopPending immediately
  (30s wait hint) and the stop is bounded (20s grace, then Stopped is reported
  and process exit tears down a worker stuck in a long scan). Previously the
  app ran on the service main thread, so `sc stop` returned 1061 and the MSI's
  `ServiceControl` action failed with error 1921 (uninstall exit 1603).
  Landed in 0672c65, restructured in 2abe4ed.

### Added

- The MSI installer ships in the release again now that uninstall works
  (v1.5.0 deliberately withheld it).

## [v1.5.0] -- 2026-08-25

Released on GitHub with the five binaries and no installer (see the v1.5.0
release notes); the installer was withheld pending #3.

### Infrastructure

- Refactor CI into focused jobs and add release workflow with SHA-256 checksums ([`9439154`](https://github.com/Dicklesworthstone/ultrasearch/commit/9439154f4ce9831e3a2cedf7077f8c88fad3d5d1)) -- 2026-01-17
- Relax clippy to warnings instead of errors in CI ([`0f2042b`](https://github.com/Dicklesworthstone/ultrasearch/commit/0f2042b6513eb68e9d0f0680d01b92c5c9788b2c)) -- 2026-01-24

### Licensing

- Update license to MIT with OpenAI/Anthropic Rider ([`1468c63`](https://github.com/Dicklesworthstone/ultrasearch/commit/1468c63ec2afb751e7720af5e3db8a51489112dd)) -- 2026-02-21
- Add MIT License to repository ([`6d9b35d`](https://github.com/Dicklesworthstone/ultrasearch/commit/6d9b35dcb0e01cb6ecd7231e4e593de2e3804933)) -- 2026-01-21

### Documentation & Metadata

- Add GitHub social preview image (1280x640) ([`eb35227`](https://github.com/Dicklesworthstone/ultrasearch/commit/eb35227d371d1c7c5d8153841e05c8eebb229ce0)) -- 2026-02-21
- Add CASS (Cross-Agent Session Search) tool reference to AGENTS.md ([`5067e25`](https://github.com/Dicklesworthstone/ultrasearch/commit/5067e254eebfebb03a7ccb8f93a18832b947c1fe)) -- 2026-02-25
- Update AGENTS.md with project context ([`560ff70`](https://github.com/Dicklesworthstone/ultrasearch/commit/560ff7077161000ff2e3f316684745dab548cc62)) -- 2026-01-18

---

## [v1.4.5] -- 2025-12-09

**First stable release with published binaries.** This is a GitHub Release with Windows x64 executables: `service.exe`, `cli.exe`, `index-worker.exe`. It represents the culmination of all v0.2.x development plus post-v0.2.37 feature work on the installer, launcher, UI polish, and indexing pipeline.

### Release Checksums

| Binary | SHA-256 |
|--------|---------|
| `service.exe` | `6D6721FDCC9BB12B1470AC7B55F98BB33D6F8F6423427424E17F044612FC7710` |
| `cli.exe` | `634AC90652AB29FD00B887A3E10EC89307293EC93E74A785B4D2ED47538DFEF8` |
| `index-worker.exe` | `A3D9C0FFE100BDBD7B2E63D33E1F772FE8D8867182B54C86E3074FD9C1C70B83` |

### Bug Fixes

- Fix worker spawn paths, config `%PROGRAMDATA%` env expansion, and worker logging ([`4ef1c14`](https://github.com/Dicklesworthstone/ultrasearch/commit/4ef1c143a41ebbd6a6f8a4653bc308a900296b57)) -- 2025-12-08
- Fix clippy let-and-return lint in `default_config_path` ([`c535d21`](https://github.com/Dicklesworthstone/ultrasearch/commit/c535d217d873343fd76d8633059e781afdc1d97e)) -- 2025-12-09
- Fix idle detection when no volumes are selected ([`96f926f`](https://github.com/Dicklesworthstone/ultrasearch/commit/96f926ff50f18c6224b18f8f590baa37b70c0ec9)) -- 2025-11-29

### Service & Indexing

- Track content job sizes and expose progress counters to UI/IPC ([`199826c`](https://github.com/Dicklesworthstone/ultrasearch/commit/199826c34a6c22f398d1387fb1d30255fb1b4c49)) -- 2025-11-29
- Hard-cap indexing workers and drop process priorities for background-respectful behavior ([`bd96a2a`](https://github.com/Dicklesworthstone/ultrasearch/commit/bd96a2a4885b6dc85702c9348549db4e958e8c0c)) -- 2025-11-29
- Add rescan request plumbing for on-demand re-indexing ([`2b5d1ed`](https://github.com/Dicklesworthstone/ultrasearch/commit/2b5d1ed18ef37d98470898956999be09d6a0e635)) -- 2025-11-29

### UI & UX

- Richer indexing progress display with stronger primary styling ([`a7cd6cb`](https://github.com/Dicklesworthstone/ultrasearch/commit/a7cd6cbe84b66976dd6ee8344c56609c914698d7)) -- 2025-11-29
- Add tray minimize, progress surfaces, and clearer onboarding flow ([`304515d`](https://github.com/Dicklesworthstone/ultrasearch/commit/304515dff89539df8f3db33e7912fd19d2059f1c)) -- 2025-11-29
- Strengthen accent color and keep onboarding feedback ([`78b45d7`](https://github.com/Dicklesworthstone/ultrasearch/commit/78b45d758bfef1778fe244a15db24b08aeec4c33)) -- 2025-11-29
- Normalize onboarding source ending ([`160c5e2`](https://github.com/Dicklesworthstone/ultrasearch/commit/160c5e2ba072f834c0ce0494d5fd42184193695d)) -- 2025-11-29

### Installer & Launcher

- Add product icon and clean shortcut handling ([`0996cc7`](https://github.com/Dicklesworthstone/ultrasearch/commit/0996cc765fbbfb7bb2aa17f0ad45e7093ac055fb)) -- 2025-11-29
- Hide consoles by default, add `--show-console` flag to launcher ([`86b4ca6`](https://github.com/Dicklesworthstone/ultrasearch/commit/86b4ca606da14b0ecba3441e4c014bb9ef339a6f)) -- 2025-11-29
- Clean background priority cast for clippy ([`c923b43`](https://github.com/Dicklesworthstone/ultrasearch/commit/c923b43b1c44766158bfff95dc0a7a7abdeb0484)) -- 2025-11-29

---

## v0.2.11 through v0.2.37 -- 2025-11-24 to 2025-11-26 (CI iteration tags)

Rapid CI iteration focused on getting MSI (Windows Installer) packaging working with `cargo-wix` inside a Cargo workspace with wildcard dependencies. Tags v0.2.11 through v0.2.22 have corresponding GitHub Releases (all empty/no release notes); tags v0.2.23 through v0.2.37 are tag-only with no GitHub Release.

These are grouped here by capability rather than listed individually, since each tag is typically a single CI pipeline fix.

### MSI Packaging (cargo-wix workspace integration)

The core challenge: `cargo-wix` does not natively support Cargo workspaces with wildcard dependency versions. Over 25 iterations refined the approach from naive invocation to a working strategy of building a temporary flattened manifest with explicit dependency versions.

**Key milestones:**

- **v0.2.11/v0.2.12** ([`6c57dd1`](https://github.com/Dicklesworthstone/ultrasearch/commit/6c57dd15f4db12739de6fc3026654e0b0abe1b45)): First MSI packaging attempts -- determine `msi_built` via file existence (both tags point to same commit) -- 2025-11-24
- **v0.2.13** ([`6a6fa89`](https://github.com/Dicklesworthstone/ultrasearch/commit/6a6fa89ea43243c8da0df2c7cf113fc3b70b8c0b)): Log wix outputs and detect MSI recursively -- 2025-11-24
- **v0.2.14** ([`54dad5d`](https://github.com/Dicklesworthstone/ultrasearch/commit/54dad5d777fda563ad4bd0e35dd966560ac8ed67)): Always upload MSI artifact and log path -- 2025-11-24
- **v0.2.15** ([`d54dc62`](https://github.com/Dicklesworthstone/ultrasearch/commit/d54dc624654f7f23e30581178b3f8c1ee5737969)): Pass `ui.exe` bin-path to cargo wix -- 2025-11-24
- **v0.2.16** ([`ecc66d2`](https://github.com/Dicklesworthstone/ultrasearch/commit/ecc66d2c91c40c0278e0a7f9e435fd559b3d77a3)): Fix MSI build bin-path for cargo wix -- 2025-11-24
- **v0.2.17** ([`284b85a`](https://github.com/Dicklesworthstone/ultrasearch/commit/284b85ab5b1638f2354ae7150284b01ac9a42844)): Fix cargo wix invocation to use manifest path -- 2025-11-24
- **v0.2.18** ([`b65b386`](https://github.com/Dicklesworthstone/ultrasearch/commit/b65b38683893d113aec879ef1eb4289d4a38b809)): Use default manifest for cargo wix invocation -- 2025-11-24
- **v0.2.19** ([`8907b04`](https://github.com/Dicklesworthstone/ultrasearch/commit/8907b04d346b3dda90073c98322fbfedbe72b3c2)): Set `CARGO_MANIFEST_PATH` for cargo wix -- 2025-11-24
- **v0.2.20** ([`006ff41`](https://github.com/Dicklesworthstone/ultrasearch/commit/006ff41aceb5152064101316e7eb8b9977e0b1f4)): Call cargo wix with manifest path positional -- 2025-11-24
- **v0.2.21** ([`aecad3e`](https://github.com/Dicklesworthstone/ultrasearch/commit/aecad3eaeea4a70c6dd5a7220c796ab924b880ee)): Run cargo wix from workspace root with ui package -- 2025-11-24
- **v0.2.22** ([`3f48488`](https://github.com/Dicklesworthstone/ultrasearch/commit/3f484882f16c841136baefa2dda5c3554741ff76)): Build cargo wix with copied manifest preserving relative paths -- 2025-11-24
- **v0.2.23** ([`cf96041`](https://github.com/Dicklesworthstone/ultrasearch/commit/cf9604195901073966c1615a918035fc164e4702)): Fail fast when `ui.exe` missing; run cargo wix inside copied manifest -- 2025-11-24
- **v0.2.24** ([`7863ae5`](https://github.com/Dicklesworthstone/ultrasearch/commit/7863ae5e6ae9a8cc41c026028bd8060250399516)): Invoke cargo wix from ui crate without temp manifest -- 2025-11-24
- **v0.2.25** ([`4c09f98`](https://github.com/Dicklesworthstone/ultrasearch/commit/4c09f98afc5cd95c92cc3e2db2253f9cd791c9e2)): MSI job: use workspace target dir, `cargo wix --no-build`, always publish MSI if present -- 2025-11-25
- **v0.2.26** ([`6b66a3f`](https://github.com/Dicklesworthstone/ultrasearch/commit/6b66a3ff120c864bc00e95dfc603dbc24b11f29a)): Build temp manifest with explicit versions and use manifest-path -- 2025-11-25
- **v0.2.27** ([`28de678`](https://github.com/Dicklesworthstone/ultrasearch/commit/28de678511d3d2c8f8e1513672772f0e4dbedc81)): Include `-p ui` with temp manifest to avoid workspace deps -- 2025-11-25
- **v0.2.28** ([`8cc825c`](https://github.com/Dicklesworthstone/ultrasearch/commit/8cc825cd0b70d29c4363826758fbf0cf3684a8f2)): Run cargo wix from temp dir without manifest-path arg -- 2025-11-25
- **v0.2.29** ([`ce9d127`](https://github.com/Dicklesworthstone/ultrasearch/commit/ce9d127ec959b17e273535d339b1312e1bb87ea6)): Run `cargo wix -p ui` directly with default manifest -- 2025-11-25
- **v0.2.30** ([`e9efd87`](https://github.com/Dicklesworthstone/ultrasearch/commit/e9efd875b880362f842f02e717477550386d2a4e)): Install cargo-wix 0.5.0 (workspace-aware) -- 2025-11-25
- **v0.2.31** ([`b5b5734`](https://github.com/Dicklesworthstone/ultrasearch/commit/b5b5734d94b9e46ca685c44b9e7dbc92c8528634)): Patch `Cargo.toml` deps in-place during wix run; install cargo-wix from git -- 2025-11-25
- **v0.2.32** ([`bff37dd`](https://github.com/Dicklesworthstone/ultrasearch/commit/bff37ddb93c5bb5a48e4b807f956efbb703fdc9c)): Fix cargo-wix install to specify package name -- 2025-11-25
- **v0.2.33** ([`46dca16`](https://github.com/Dicklesworthstone/ultrasearch/commit/46dca16527b33ca2e45ac79887cf97e6dbb1fc06)): Use temp manifest with workspace deps flattened and dummy bin -- 2025-11-26
- **v0.2.34** ([`dc8eb81`](https://github.com/Dicklesworthstone/ultrasearch/commit/dc8eb818edcf3eaed8a8cad97f5ac097e3fcea26)): Build temp single-package manifest, verbose wix -- 2025-11-26
- **v0.2.35** ([`096cb69`](https://github.com/Dicklesworthstone/ultrasearch/commit/096cb69be3ce5066051d7ce7c3d421bda3d2137f)): Build MSI with standalone ui manifest (working approach) -- 2025-11-26
- **v0.2.36** ([`f6ae110`](https://github.com/Dicklesworthstone/ultrasearch/commit/f6ae110971f50c7596ac58d0ee3bc5d343e20373)): Fix temp manifest quoting for cargo-wix -- 2025-11-26
- **v0.2.37** ([`3247352`](https://github.com/Dicklesworthstone/ultrasearch/commit/3247352dbd3db39be84eeea5cf3a75ef07a2e184)): Add retry logic to GraalVM download in CI -- 2025-11-26

### CI Pipeline Hardening (pre-MSI)

Earlier tags in this range addressed foundational CI stability before the MSI packaging effort began:

- Make MSI packaging optional when wix/cargo-wix fails ([`0194a1d`](https://github.com/Dicklesworthstone/ultrasearch/commit/0194a1d0a93a12b5226a22063ebda38e39dec379)) -- 2025-11-24
- Grant `contents:write` for release publish and soften MSI requirement ([`f471903`](https://github.com/Dicklesworthstone/ultrasearch/commit/f47190302e3f01da90c893fc2beedf3e10faa828)) -- 2025-11-24
- Install cargo-wix from git to support workspace deps ([`820c440`](https://github.com/Dicklesworthstone/ultrasearch/commit/820c44021138c469d4371322e889c9600d08bd22)) -- 2025-11-24
- Generate temp manifest for cargo-wix without workspace deps ([`bcbcced`](https://github.com/Dicklesworthstone/ultrasearch/commit/bcbcced13e3986dd08fb5d4fb660e469378b36e9)) -- 2025-11-24
- Tolerate preinstalled wix version ([`9cf8718`](https://github.com/Dicklesworthstone/ultrasearch/commit/9cf8718d27b8fbfd5b8b2b0c90ae4eeb9f7d34a3)) -- 2025-11-24

### E2E Smoke Test Stabilization

- Simplify e2e smoke to binary presence check ([`d32b3d7`](https://github.com/Dicklesworthstone/ultrasearch/commit/d32b3d7663f8b23456eaa2692c692f8428b8ec46)) -- 2025-11-24
- Make e2e smoke tolerate startup scan ([`a0396dd`](https://github.com/Dicklesworthstone/ultrasearch/commit/a0396ddff1fa6566798a35870a449b97b2b2c305)) -- 2025-11-24
- Capture service logs and wait longer for IPC ([`71fdfa6`](https://github.com/Dicklesworthstone/ultrasearch/commit/71fdfa683ea35eeca63df64e1292cfb54662937e)) -- 2025-11-24
- Give CLI status longer to connect ([`850da9a`](https://github.com/Dicklesworthstone/ultrasearch/commit/850da9a58779b32a9d50995641ddee07374b9416)) -- 2025-11-24
- Retry CLI status in e2e smoke ([`0e579cd`](https://github.com/Dicklesworthstone/ultrasearch/commit/0e579cddb4a4a6a30fd51b1fb517642dc5688cac)) -- 2025-11-24
- Avoid reserved `pid` variable in Windows smoke ([`675d189`](https://github.com/Dicklesworthstone/ultrasearch/commit/675d189214764d9303b34696e7c3f15b64aa6954)) -- 2025-11-24
- Fix Windows paths for service/ui binaries and release artifacts ([`0812866`](https://github.com/Dicklesworthstone/ultrasearch/commit/081286676989b4939e9c97291390055d9146de17)) -- 2025-11-24
- Wrap `env::set_var` in e2e harness for nightly compatibility ([`fadfda8`](https://github.com/Dicklesworthstone/ultrasearch/commit/fadfda8422e0f9f49abf776ad170c1f8ba996f46)) -- 2025-11-24
- Clamp `sample_duration` to >=1ms for scheduler metrics test ([`3f89131`](https://github.com/Dicklesworthstone/ultrasearch/commit/3f89131aab49233d96b31de2db498b726e090523)) -- 2025-11-24

---

## v0.2.7 through v0.2.10 -- 2025-11-23 (tag only)

CI environment configuration for Windows-only builds with GraalVM.

- **v0.2.10** ([`807dce8`](https://github.com/Dicklesworthstone/ultrasearch/commit/807dce857c6a54c75c33cbbb148ccfb59fc33271)): Increase Windows timeouts and stabilize `CARGO_TARGET_DIR`
- **v0.2.9** ([`4939732`](https://github.com/Dicklesworthstone/ultrasearch/commit/4939732203092ce82012f225dac8c44d70ff179c)): Fetch GraalVM zip for Windows CI jobs
- **v0.2.8** ([`d96685c`](https://github.com/Dicklesworthstone/ultrasearch/commit/d96685c4411c803a6a2a967e5bfd0baa334c0dab)): Install GraalVM CE 23 for Windows CI jobs
- **v0.2.7** ([`31a0530`](https://github.com/Dicklesworthstone/ultrasearch/commit/31a0530b02c0bb77a9915f8e4ae05bf0469e265a)): Drop linux/mac CI checks; keep Windows-only pipeline

---

## v0.2.6 -- 2025-11-23 (tag only)

### Bug Fix

- Fix disk sampler refresh and remove unreachable fallback in scheduler ([`6647675`](https://github.com/Dicklesworthstone/ultrasearch/commit/66476751b5ecd4177da7641d288759f23694dcf9))

---

## v0.2.2 through v0.2.5 -- 2025-11-23 (tag only)

CI dependency and build tooling fixes for the multi-OS pipeline.

- **v0.2.5** ([`1d2a0cd`](https://github.com/Dicklesworthstone/ultrasearch/commit/1d2a0cdd2bcca9c1ab40ba7fd2e577afeced172f)): Install GTK deps for GPUI builds
- **v0.2.4** ([`33085eb`](https://github.com/Dicklesworthstone/ultrasearch/commit/33085eb22eae917d6d419a2db2e70da14467a1e8)): Install glib deps and simplify CI cache keys
- **v0.2.3** ([`be8a82d`](https://github.com/Dicklesworthstone/ultrasearch/commit/be8a82d7500fd2d978b24e0057480224214dacad)): Fix CI cache hash glob
- **v0.2.2** ([`d9484bb`](https://github.com/Dicklesworthstone/ultrasearch/commit/d9484bb17ed67872ff0a917e5792c0b9d43e0a64)): Install mold linker for linux CI jobs

---

## v0.2.1 -- 2025-11-23 (tag only)

### Testing

- Isolate scheduler enqueue dropped counter test ([`d01a652`](https://github.com/Dicklesworthstone/ultrasearch/commit/d01a6521880f1af23e410452dcf0c0d92eb9c35d))

---

## v0.2.0 -- 2025-11-23

**First tagged milestone.** Multi-OS CI pipeline, richer UI help panel, gated MSI packaging attempt. This tag marks the point where the codebase was considered feature-complete enough for automated builds. It contains the full initial development history from project inception (2025-11-20) through stabilization.

### Desktop UI (GPUI Application)

The GPU-accelerated desktop client, built on GPUI (Zed's rendering framework), went from stub to a full modern desktop experience in this release.

- Modern Desktop Experience: system tray, Spotlight-style quick search (`Alt+Space`), theming ([`d3929e4`](https://github.com/Dicklesworthstone/ultrasearch/commit/d3929e48e096075616efd984a950b7cafa7cb130)) -- 2025-11-22
- Help panel with keyboard shortcuts overlay (`F1`, `Ctrl+/`) and feature highlights ([`8908735`](https://github.com/Dicklesworthstone/ultrasearch/commit/89087359c9fab9334800feb2677763256396694e)) -- 2025-11-23
- Richer help markdown and finalized help panel copy ([`b3b148b`](https://github.com/Dicklesworthstone/ultrasearch/commit/b3b148b423ad723b412f31b2814a4f8f67952420), [`6e5ed37`](https://github.com/Dicklesworthstone/ultrasearch/commit/6e5ed37c9a097ecea34a251997248f5dade83add)) -- 2025-11-23
- GPUI action system integration with accessibility improvements ([`6578a43`](https://github.com/Dicklesworthstone/ultrasearch/commit/6578a43b511e3bb57c39c590cc12fca6973c907f)) -- 2025-11-22
- Results table with sortable columns, keyboard navigation, search debouncing ([`5252710`](https://github.com/Dicklesworthstone/ultrasearch/commit/5252710a1dc7883f08eb5108c92750bf2c814edf), [`3ab4e5c`](https://github.com/Dicklesworthstone/ultrasearch/commit/3ab4e5c936cbae9129f6b4848a7e799db3b3fb83)) -- 2025-11-21
- Search input bar and preview pane with file manager integration ([`f013ca8`](https://github.com/Dicklesworthstone/ultrasearch/commit/f013ca81c2f8c668852a1562df5dec7aeb940096)) -- 2025-11-21
- GPUI app model and IPC bridge ([`bebff24`](https://github.com/Dicklesworthstone/ultrasearch/commit/bebff247959c15c3714bdd799077378524148dcd)) -- 2025-11-21
- Update panel with opt-in check, download, restart flow ([`ee757eb`](https://github.com/Dicklesworthstone/ultrasearch/commit/ee757eb48cb697079398c1eba6ea19e64adfe0ae)) -- 2025-11-23
- Background jobs, quick search polish, and progress surfaces ([`965f6f6`](https://github.com/Dicklesworthstone/ultrasearch/commit/965f6f6ba8a09cd2ee8747de44efc8a5f586b7c3)) -- 2025-11-23
- Cancelable background tasks and mouse hover affordances ([`c17ceaf`](https://github.com/Dicklesworthstone/ultrasearch/commit/c17ceaff3296df5469ff935d4b7453c1a9ec24b0)) -- 2025-11-21
- Preview scrolling and retry accessibility ([`ea1eab8`](https://github.com/Dicklesworthstone/ultrasearch/commit/ea1eab838047e2356cb5bdf9bd3e05bedfd1ea13)) -- 2025-11-22
- Onboarding and status surfaces ([`eac4c3a`](https://github.com/Dicklesworthstone/ultrasearch/commit/eac4c3a755dd1184b4df536761f1465a60da3e87), [`faeb2d0`](https://github.com/Dicklesworthstone/ultrasearch/commit/faeb2d0ffed4fcff1c9601dff3228309f312a348)) -- 2025-11-23
- Harden search UX, hover reset, and window defaults ([`0898509`](https://github.com/Dicklesworthstone/ultrasearch/commit/0898509d801adc3b73698c150e1acbdbc946f7b4)) -- 2025-11-22
- WiX packaging metadata for MSI installer ([`221e7be`](https://github.com/Dicklesworthstone/ultrasearch/commit/221e7be0448cce4cc009d7f15dad773a5ac6bd62)) -- 2025-11-22
- Fix GPUI integration and complete Modern UX modules ([`0ca9d1b`](https://github.com/Dicklesworthstone/ultrasearch/commit/0ca9d1baa7c481cd11ad79aa17aa1f1a0d534559)) -- 2025-11-22
- Provide tokio runtime for async status/search tasks ([`beceee7`](https://github.com/Dicklesworthstone/ultrasearch/commit/beceee710f842e40fb13adfc42217ff88a87b3fd)) -- 2025-11-22
- Refine reconnect logic and onboarding/help panels ([`d1d3969`](https://github.com/Dicklesworthstone/ultrasearch/commit/d1d3969c258f2b83afebbba483b197f0864baa0f)) -- 2025-11-23

### Windows Service (Background Daemon)

The always-on service that owns NTFS handles, manages scheduling, and serves IPC queries.

- Native Windows Service management via SCM ([`b6b310b`](https://github.com/Dicklesworthstone/ultrasearch/commit/b6b310bd2d8f47466c23e758865cc8e3c012952b)) -- 2025-11-22
- Windows service skeleton implementation ([`797c4ef`](https://github.com/Dicklesworthstone/ultrasearch/commit/797c4ef33a9514a78bdeb696d2bcf29dcd9fd0a0)) -- 2025-11-21
- Named pipe IPC server with hardened ACL security ([`c6d2641`](https://github.com/Dicklesworthstone/ultrasearch/commit/c6d2641e11c17ffe6a359c66e3b0c98945c88c92), [`68d1220`](https://github.com/Dicklesworthstone/ultrasearch/commit/68d122026636f23d6c4d41a9bfe5369f3d87a0ac)) -- 2025-11-22, 2025-11-21
- SearchHandler and SchedulerRuntime abstractions ([`4f2d34a`](https://github.com/Dicklesworthstone/ultrasearch/commit/4f2d34a143a79e8033acb67b1f9712e0e7042368)) -- 2025-11-21
- IPC dispatch with search handler and status routing ([`9934c32`](https://github.com/Dicklesworthstone/ultrasearch/commit/9934c3203ec9d520654488c8613a29107c428721)) -- 2025-11-21
- Metrics HTTP server for observability ([`89dc102`](https://github.com/Dicklesworthstone/ultrasearch/commit/89dc10292c76df3f0261420cf70cb2b735d74a94)) -- 2025-11-21
- Content queue metrics plumbed into status/IPC responses ([`8e55927`](https://github.com/Dicklesworthstone/ultrasearch/commit/8e5592759b8ac8894554474f3026177e8ef79702)) -- 2025-11-23
- Config-aware volume filtering and improved polling watcher ([`a7c0776`](https://github.com/Dicklesworthstone/ultrasearch/commit/a7c0776daada30bf15eb6bb18728ed29f1255523)) -- 2025-11-23
- Meta-ingest module for NTFS metadata batch ingestion ([`ac8f25f`](https://github.com/Dicklesworthstone/ultrasearch/commit/ac8f25f2a557f6a900a4441e6160d09ea16debbd), [`b9c4bcc`](https://github.com/Dicklesworthstone/ultrasearch/commit/b9c4bcca9f0cfa2d1354a3b618401472d16a0467)) -- 2025-11-21
- Config-driven JSON file logging with hourly/daily rotation ([`3224d71`](https://github.com/Dicklesworthstone/ultrasearch/commit/3224d712ca616b7e9b0b30653147400ed996f85c), [`829364f`](https://github.com/Dicklesworthstone/ultrasearch/commit/829364f6247a8aaa698b6762d239933586e3ab3e)) -- 2025-11-21
- Reloadable configuration support ([`4446f43`](https://github.com/Dicklesworthstone/ultrasearch/commit/4446f4311e5d1415defc64bb7b8652c468f6fabe)) -- 2025-11-21
- StatusProvider abstraction and BasicStatusProvider ([`e13aa64`](https://github.com/Dicklesworthstone/ultrasearch/commit/e13aa64bb602e4412cb83b1bee903ece242eb717), [`dcb002a`](https://github.com/Dicklesworthstone/ultrasearch/commit/dcb002a995585a259056aa2767d1ac47e02a37a3)) -- 2025-11-21
- SchedulerRuntime for job queue management and worker spawning ([`ac7bab8`](https://github.com/Dicklesworthstone/ultrasearch/commit/ac7bab8c34a12e23e8777eadf6c40120c202aaf1)) -- 2025-11-21
- Diagnostic logging in Search Handler ([`393ebda`](https://github.com/Dicklesworthstone/ultrasearch/commit/393ebdae2a44cc98eafd7b31aac84e63424df740)) -- 2025-11-22
- Stabilize IPC security and module exports ([`c7232d0`](https://github.com/Dicklesworthstone/ultrasearch/commit/c7232d02498fc559b4703b5d9eb4d841ca198a44)) -- 2025-11-22
- Stub out unused polling watcher to avoid panic risk ([`5fcb73b`](https://github.com/Dicklesworthstone/ultrasearch/commit/5fcb73b90792a6c5ba6405e487a991ab6114df5c)) -- 2025-11-23
- Bump prometheus crate to 0.14 ([`19fe44a`](https://github.com/Dicklesworthstone/ultrasearch/commit/19fe44a190804b1ccae4012b49773b331055c460)) -- 2025-11-22

### NTFS Watcher & Metadata Index

Everything-style fast filename search powered by direct MFT enumeration and Tantivy indexing.

- MFT enumeration via `usn-journal-rs` for instant filename search ([`f8e0a1d`](https://github.com/Dicklesworthstone/ultrasearch/commit/f8e0a1d67d318537322019cd10163cce3534c823)) -- 2025-11-21
- Windows volume discovery and handle management ([`1c41503`](https://github.com/Dicklesworthstone/ultrasearch/commit/1c415036af2abe6178c6edf4768cd4d032c360c2)) -- 2025-11-21
- USN watcher stub for filesystem change notifications ([`a695e93`](https://github.com/Dicklesworthstone/ultrasearch/commit/a695e93de549c64399b92a794426d7c692ec0dd4)) -- 2025-11-21
- FileMeta to MetaDoc conversion for watcher integration ([`2f8ff23`](https://github.com/Dicklesworthstone/ultrasearch/commit/2f8ff230adadb268628e277a375b2e5abe5c3918)) -- 2025-11-21
- Metadata cache with path reconstruction ([`f5ae31a`](https://github.com/Dicklesworthstone/ultrasearch/commit/f5ae31ab5ccc3183a314632eb5fff05bcefcc915)) -- 2025-11-21
- FST (Finite State Transducer) index for fast prefix lookups and volume state persistence ([`6e1f5e4`](https://github.com/Dicklesworthstone/ultrasearch/commit/6e1f5e4591656de1835343862218882d8c2137c9)) -- 2025-11-21
- Regression test for document field mapping ([`ffbbd63`](https://github.com/Dicklesworthstone/ultrasearch/commit/ffbbd6336ebbc35f8c7c7e7501607e4940cbcc36)) -- 2025-11-21

### Content Indexing (Tantivy Full-Text Search)

Full-text search over extracted file contents using Tantivy.

- Tiered content index implementation ([`e7851c4`](https://github.com/Dicklesworthstone/ultrasearch/commit/e7851c4da9c45330c0e6560d5026d2a322968f99)) -- 2025-11-21
- Batch job processing and content index integration in workers ([`ab7656b`](https://github.com/Dicklesworthstone/ultrasearch/commit/ab7656b47ea37115c33140c7c4fe105795dda1ec)) -- 2025-11-21
- MVP CLI for single-file content extraction in workers ([`925aa4f`](https://github.com/Dicklesworthstone/ultrasearch/commit/925aa4f4ae2ae32a996d1e605872934d11b98d39)) -- 2025-11-21
- IndexWriter exposed publicly for worker integration ([`a7d9026`](https://github.com/Dicklesworthstone/ultrasearch/commit/a7d90260f557a9bb4f8544cdf61a724543097780)) -- 2025-11-21
- Helper function for single document insertion ([`9134888`](https://github.com/Dicklesworthstone/ultrasearch/commit/91348885893fe3b1d40643961de6180d0b963fe6)) -- 2025-11-21
- Empty job file validation and serde defaults ([`c2e6daf`](https://github.com/Dicklesworthstone/ultrasearch/commit/c2e6dafdf3758dbd0d3622642138649e45249219)) -- 2025-11-21

### Content Extraction

Multiple backends for extracting text from documents.

- Extractous backend for PDF/Office documents with optional feature flag ([`1306394`](https://github.com/Dicklesworthstone/ultrasearch/commit/1306394647fa2492414433647f74f25d3a4b00da), [`1a66f1c`](https://github.com/Dicklesworthstone/ultrasearch/commit/1a66f1cb77bbf93c973a0336496e469dfbf0da24)) -- 2025-11-21
- Windows IFilter COM bridge for legacy document formats ([`8a6c5aa`](https://github.com/Dicklesworthstone/ultrasearch/commit/8a6c5aa4392205b0df3c070b5923c6aa9ce680fa)) -- 2025-11-21
- GraalVM CE 23.x build-time enforcement when extractous feature enabled ([`e305f08`](https://github.com/Dicklesworthstone/ultrasearch/commit/e305f0805b040bc72c71c07090e3b67d0ae57869)) -- 2025-11-23
- Stack builder methods for flexible backend selection ([`6570207`](https://github.com/Dicklesworthstone/ultrasearch/commit/6570207652653fa9fabcdce1ff873dff7857a2f8)) -- 2025-11-21
- Let extractous advertise support without size cap ([`b764360`](https://github.com/Dicklesworthstone/ultrasearch/commit/b764360bed96a93da92fb96d3fa7bdc7e8457d40)) -- 2025-11-22

### Scheduler (Idle-Aware Background Indexing)

Background content indexing that only runs when the system is idle.

- Adaptive policy with stateful design integrated into runtime ([`228a5b4`](https://github.com/Dicklesworthstone/ultrasearch/commit/228a5b48023ff80556cf8f0195ef61c7228c2ec1), [`bb26d4c`](https://github.com/Dicklesworthstone/ultrasearch/commit/bb26d4c37adc37a1b7a46dc583b77a6136ad7df6)) -- 2025-11-21
- Job queues and runtime loop ([`59909dd`](https://github.com/Dicklesworthstone/ultrasearch/commit/59909dd7d9c0c1d32eca948ab5d85b9625213613)) -- 2025-11-21
- Real disk I/O monitoring via Windows PDH API, replacing sysinfo ([`341865d`](https://github.com/Dicklesworthstone/ultrasearch/commit/341865d271ac4ccdec8ee83bbeecdaf84fd2a6e8), [`16a4bbd`](https://github.com/Dicklesworthstone/ultrasearch/commit/16a4bbd4b4b01203346b65974dbfd935f651455b)) -- 2025-11-21
- Idle detection state machine for Windows with platform timer ([`4b1f961`](https://github.com/Dicklesworthstone/ultrasearch/commit/4b1f9618fc371672227a3ad25a5f57a913410984)) -- 2025-11-21
- Testable job eligibility policies ([`19ef4af`](https://github.com/Dicklesworthstone/ultrasearch/commit/19ef4af79d509213ec2b63cf6391dfa06cb8e9fb)) -- 2025-11-21
- Content queue counters surfaced to UI ([`c6bc90c`](https://github.com/Dicklesworthstone/ultrasearch/commit/c6bc90cb51be26c277bba4a6a675da78242096bf)) -- 2025-11-23
- Power save mode exposed in config ([`6f26e0e`](https://github.com/Dicklesworthstone/ultrasearch/commit/6f26e0e1982b7e5c5accd1829d90527d0605d015)) -- 2025-11-22
- Module extraction for idle detection and metrics subsystems ([`fb166e5`](https://github.com/Dicklesworthstone/ultrasearch/commit/fb166e549494c7a2d4287f7bfe4548bfad933fa8), [`9b3999f`](https://github.com/Dicklesworthstone/ultrasearch/commit/9b3999fd54ec109403881c6cca3c11f5a6bf72ea)) -- 2025-11-21

### IPC Protocol (Named Pipes)

Communication layer between service, CLI, and UI processes.

- Named pipe client with retry/backoff and distributed tracing ([`cfedc91`](https://github.com/Dicklesworthstone/ultrasearch/commit/cfedc91da0071d2374c96b4476310fa97e0c13cb)) -- 2025-11-21
- Named pipe client implementation ([`ed8f5b8`](https://github.com/Dicklesworthstone/ultrasearch/commit/ed8f5b8d3b6d175842745b004b8c7a9dea843b7c)) -- 2025-11-21
- Complete message schema with Duration serialization fixes ([`1e1f98e`](https://github.com/Dicklesworthstone/ultrasearch/commit/1e1f98e769266fc00045903ec3846d5d2e15e779), [`f657625`](https://github.com/Dicklesworthstone/ultrasearch/commit/f657625e259c4b1c577f6b074f7b813f93d31626), [`ace967b`](https://github.com/Dicklesworthstone/ultrasearch/commit/ace967be3ea33c24bd760ee828d38915197d6d32)) -- 2025-11-21
- SearchRequest wire format with fluent builder methods ([`78fe1de`](https://github.com/Dicklesworthstone/ultrasearch/commit/78fe1de58a70f26cdbd5b980dd4575152ea57133)) -- 2025-11-21
- Self-healing reconnect logic for service restarts and pipe-busy states ([`d1d3969`](https://github.com/Dicklesworthstone/ultrasearch/commit/d1d3969c258f2b83afebbba483b197f0864baa0f)) -- 2025-11-23
- IPC decoding made exact and echo ids handled safely ([`246804b`](https://github.com/Dicklesworthstone/ultrasearch/commit/246804b7ce9d3aedc8f0e6d830ae4bcc936c6ec8)) -- 2025-11-22
- Windows IPC server, priority control, and status helpers ([`468e449`](https://github.com/Dicklesworthstone/ultrasearch/commit/468e44958b47fc308f6f16a24ed0611170224712)) -- 2025-11-21
- Global metrics handle and IPC snapshot helper for status endpoint ([`b30b569`](https://github.com/Dicklesworthstone/ultrasearch/commit/b30b5690fbcdf15654b1b19d87394a0b60ab1378)) -- 2025-11-21

### CLI (Command-Line Interface)

Debug and administration CLI using clap.

- Full clap-based debug CLI with Search and Status commands ([`b3e7ef5`](https://github.com/Dicklesworthstone/ultrasearch/commit/b3e7ef5f8c3a843a5408005d6bc95295f7c9ed71)) -- 2025-11-21
- JSON output support and formatted result display ([`0844c76`](https://github.com/Dicklesworthstone/ultrasearch/commit/0844c764f0ce725f22f640e72eb267277c63ea90)) -- 2025-11-21
- Content queue metrics in status output ([`db1fdbb`](https://github.com/Dicklesworthstone/ultrasearch/commit/db1fdbbf638f9d98e7e8f27349cf4f3d84abe470)) -- 2025-11-23
- Pagination and timeout in IPC request dispatcher ([`af4391d`](https://github.com/Dicklesworthstone/ultrasearch/commit/af4391db41c1e20aab8d7fa919cd674c05efa1eb)) -- 2025-11-21
- Config hardening and enhanced IPC client with full feature set ([`0348ff7`](https://github.com/Dicklesworthstone/ultrasearch/commit/0348ff7b53264dae1217e29a6482de1bbb1114d0), [`f894990`](https://github.com/Dicklesworthstone/ultrasearch/commit/f894990ffd61694037ba19e52a55f605a2a129cd)) -- 2025-11-21
- Colored output for search and status commands ([`473af27`](https://github.com/Dicklesworthstone/ultrasearch/commit/473af27fadc66c918898301934a840d0aa6c4fd9)) -- 2025-11-21

### Launcher

One-click runner that starts the service and UI together.

- One-click runner for service + UI with hidden console ([`3174df1`](https://github.com/Dicklesworthstone/ultrasearch/commit/3174df1682db218c35fe2235f5b9a2f425957a8d)) -- 2025-11-22
- Fix env loading and binary path resolution ([`6e0a38e`](https://github.com/Dicklesworthstone/ultrasearch/commit/6e0a38ef4d1c0aad664890231c4bd56e6f386cc4)) -- 2025-11-22
- Tolerate slow startup and surface service failures ([`cf58f02`](https://github.com/Dicklesworthstone/ultrasearch/commit/cf58f0257606095e2f0c1d152828762ded00f1a5)) -- 2025-11-22
- Hide service console window ([`a8faa5e`](https://github.com/Dicklesworthstone/ultrasearch/commit/a8faa5e0678106dde1a7e206a4207d97b571912a)) -- 2025-11-22

### Installer

WiX-based Windows installer packaging.

- WiX configuration and build script for MSI packaging ([`6e79005`](https://github.com/Dicklesworthstone/ultrasearch/commit/6e79005f711ddb18d278088943a0cc9355187209)) -- 2025-11-23

### Semantic Index

Experimental vector similarity search.

- HNSW vector index configured with hnsw_rs for insert/search ([`5fbb6ae`](https://github.com/Dicklesworthstone/ultrasearch/commit/5fbb6ae0138b47f4b48e475bbd18aa57b74e8605)) -- 2025-11-22
- Pin Hnsw lifetime to satisfy hnsw_rs 0.3 API ([`96129fa`](https://github.com/Dicklesworthstone/ultrasearch/commit/96129fac28fb8d6b91ad96d6814937b51d3e0230)) -- 2025-11-22

### Unified Search Pipeline

Combined filename and content search under a single query API.

- Unified search pipeline combining metadata and content results ([`682adca`](https://github.com/Dicklesworthstone/ultrasearch/commit/682adcae0e9bfda61703216d1133e61891830794)) -- 2025-11-21

### Core Types & Serialization

Shared type system and high-performance serialization layer.

- FileMeta helpers with Display/FromStr implementations ([`dbe40b9`](https://github.com/Dicklesworthstone/ultrasearch/commit/dbe40b955bd3b66bc1bea6b026b79c306886caf9)) -- 2025-11-20
- PartialEq and Eq derives on FileMeta for testability ([`80b2066`](https://github.com/Dicklesworthstone/ultrasearch/commit/80b20662ac5846dbdf52dd3fb5f7b2fdca514304)) -- 2025-11-21
- DocKey hex format width fix ([`7c045a7`](https://github.com/Dicklesworthstone/ultrasearch/commit/7c045a7ebd41ffbe289e5f53f7b60e546b51b5f5)) -- 2025-11-21
- Bincode deserialization with defensive size limits ([`f5b8e2e`](https://github.com/Dicklesworthstone/ultrasearch/commit/f5b8e2efad302b744691d56f6f69175d4357861f)) -- 2025-11-21
- memmap2 support for memory-mapped file access ([`fa56eae`](https://github.com/Dicklesworthstone/ultrasearch/commit/fa56eaefabde88bd3499021a000669a9ae9e0a02)) -- 2025-11-21
- rkyv zero-copy serialization with validator lifetime fix ([`cffc672`](https://github.com/Dicklesworthstone/ultrasearch/commit/cffc67279b11af502121098fc6f35604a9ee3803)) -- 2025-11-21
- Serialization helpers with tests ([`e303908`](https://github.com/Dicklesworthstone/ultrasearch/commit/e303908972b971fe477588f6d41ca30b484f5bf5)) -- 2025-11-21

### Configuration

- Config loading in CLI and UI entry points ([`b635254`](https://github.com/Dicklesworthstone/ultrasearch/commit/b6352544e2f7ffbd6d811aaf04dc9b51c606f041)) -- 2025-11-21
- Telemetry and volume fields with content queue metric tracking ([`45a48bb`](https://github.com/Dicklesworthstone/ultrasearch/commit/45a48bb73326e315e57241ba8da4fa001db48eab)) -- 2025-11-23

### Build & Toolchain

- Migrate to Rust 2024 edition on nightly with wildcard dependency tracking ([`5c42260`](https://github.com/Dicklesworthstone/ultrasearch/commit/5c4226094b22fad6cef0ce30cc0d57b462c100ee)) -- 2025-11-21
- Add Justfile for developer workflow automation ([`ad19f40`](https://github.com/Dicklesworthstone/ultrasearch/commit/ad19f40708a8123ae685ee7740b74185ca4a9a9a)) -- 2025-11-21
- Tantivy 0.22+ API compatibility updates ([`d72fb43`](https://github.com/Dicklesworthstone/ultrasearch/commit/d72fb4356aa634545ffc977a8ff2465a40bdd93f), [`20328b2`](https://github.com/Dicklesworthstone/ultrasearch/commit/20328b2036a1c88fdda7d7ba1f397e0e9affdc59)) -- 2025-11-21
- Complete wildcard dependency migration and add windows-sys workspace dependency ([`d6118a9`](https://github.com/Dicklesworthstone/ultrasearch/commit/d6118a910bf82a9ee3c771130e6afaa3bc9ec22d), [`100fac1`](https://github.com/Dicklesworthstone/ultrasearch/commit/100fac1f467f8f0e1aaef33258af9978c1836820)) -- 2025-11-21
- Tame local build resource usage on Windows ([`3381bd2`](https://github.com/Dicklesworthstone/ultrasearch/commit/3381bd20d049413d569f448b66cd6a02aae563f5)) -- 2025-11-21

### CI

- Multi-OS CI workflow with fmt, clippy, test, and gated MSI artifact ([`b3b148b`](https://github.com/Dicklesworthstone/ultrasearch/commit/b3b148b423ad723b412f31b2814a4f8f67952420)) -- 2025-11-23
- Windows CI workflow for fmt/clippy/tests and MSI artifact ([`b714c03`](https://github.com/Dicklesworthstone/ultrasearch/commit/b714c03262f77d2cf461b3099eb23d57bd4ae627)) -- 2025-11-22

### Testing

- Windows e2e smoke test with opt-in bootstrap hooks ([`960d1e5`](https://github.com/Dicklesworthstone/ultrasearch/commit/960d1e5955c204a8efc107ec494f1799b16b0ada), [`1629da6`](https://github.com/Dicklesworthstone/ultrasearch/commit/1629da6d000cc483994b22539148d74cf5b87713)) -- 2025-11-21
- Mockable scheduler disk sampling for testability ([`1629da6`](https://github.com/Dicklesworthstone/ultrasearch/commit/1629da6d000cc483994b22539148d74cf5b87713)) -- 2025-11-21
- Meta-index regression test for document field mapping ([`ffbbd63`](https://github.com/Dicklesworthstone/ultrasearch/commit/ffbbd6336ebbc35f8c7c7e7501607e4940cbcc36)) -- 2025-11-21
- Gate DiskCounter to Windows and quiet test tracing noise ([`78e30a8`](https://github.com/Dicklesworthstone/ultrasearch/commit/78e30a8cffe16c1e919a6967e6172a3aa4d92c3f)) -- 2025-11-21
- Fix Windows platform glue and clean lints ([`888f7dc`](https://github.com/Dicklesworthstone/ultrasearch/commit/888f7dcc961d8cc07bcabb05e3d25e7a6c251ab2)) -- 2025-11-21

---

## Pre-v0.2.0 -- 2025-11-20

Project bootstrapped from scratch with multi-agent development in a single day.

### Workspace Architecture (16 crates)

| Crate | Purpose |
|-------|---------|
| `core-types` | Shared type system (`FileMeta`, `DocKey`, enums) |
| `core-serialization` | Bincode/rkyv/memmap2 serialization layer |
| `ipc` | Named pipe protocol (messages, client, server) |
| `ntfs-watcher` | MFT enumeration and USN journal tailing |
| `meta-index` | Tantivy metadata index with FST prefix search |
| `content-index` | Tantivy full-text content index |
| `content-extractor` | Extraction backends (Extractous, IFilter, OCR) |
| `semantic-index` | HNSW vector similarity index |
| `scheduler` | Idle-aware job scheduling with adaptive policy |
| `service` | Windows service daemon (SCM, IPC server, metrics) |
| `index-worker` | Short-lived content extraction worker process |
| `cli` | Clap-based command-line interface |
| `ui` | GPUI desktop application |
| `launcher` | One-click service + UI runner |

### First Commits

- Initial project structure and configuration ([`4c0c109`](https://github.com/Dicklesworthstone/ultrasearch/commit/4c0c109bc8137cd478cb40f1a48a713ca9a921f6)) -- 2025-11-20
- Pin Rust nightly toolchain and configure optimized build profiles ([`13261ac`](https://github.com/Dicklesworthstone/ultrasearch/commit/13261ac0a65932892eb6149356a604979dd291c0)) -- 2025-11-20
- Initialize workspace structure and dependency graph ([`82953b3`](https://github.com/Dicklesworthstone/ultrasearch/commit/82953b364feae9fa08e5444bc0e94028a3ce18e3)) -- 2025-11-20
- Core type system and configuration infrastructure ([`d3bd4b2`](https://github.com/Dicklesworthstone/ultrasearch/commit/d3bd4b25744ec72a927888f6ba16461b9454507c)) -- 2025-11-20
- NTFS watcher and index subsystem stubs ([`59a0ef4`](https://github.com/Dicklesworthstone/ultrasearch/commit/59a0ef44f1b3c476601f95377ae019ec08a37983)) -- 2025-11-20
- Service, worker, and IPC infrastructure stubs ([`addaf5e`](https://github.com/Dicklesworthstone/ultrasearch/commit/addaf5e4d2ae084b2c8e1d0fe4bd99d6b9dfa142)) -- 2025-11-20
- UI and CLI application stubs ([`12b9136`](https://github.com/Dicklesworthstone/ultrasearch/commit/12b91361d68c1d8d3d1d08068f42dc1cbcae1c06)) -- 2025-11-20
- Security documentation and planning artifacts ([`93a4ba5`](https://github.com/Dicklesworthstone/ultrasearch/commit/93a4ba548d69df975aab7fcb17cd3f1311a6e6e5)) -- 2025-11-20

---

## Link Reference

[Unreleased]: https://github.com/Dicklesworthstone/ultrasearch/compare/v1.4.5...HEAD
[v1.4.5]: https://github.com/Dicklesworthstone/ultrasearch/compare/v0.2.37...v1.4.5
