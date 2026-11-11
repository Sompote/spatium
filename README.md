# Spatium

A disk space map in the style of DaisyDisk, written in Rust (egui) with the Tellus3D look.

- **Disks**: every mounted volume with used / free space.
- **Map**: rings around the current folder, each item's angle proportional to its size. Click a folder to open it, click the centre to go up, right-click for Reveal in Finder.
- **Largest files**: the 5,000 largest files of the scan, filtered by minimum size and name.
- **Collector**: drag items onto it or press `+` (press `×` or the `✓` to take one out). Nothing is deleted until you press **Delete…** and confirm; the items then go to the Trash (Finder's Put Back works). System folders and your home folder are refused.
- **Security**: Claude Code or Codex scans this Mac (start-up items, code signatures, apps, downloads, shell start-up files, processes) or a chosen folder for malware, trojans, adware and cracked software. The agent runs read-only (Claude Code: read-only tools only, no edits or web; Codex: `--sandbox read-only`) and the app does no checking of its own. The report is saved to `$TMPDIR/spatium-security/report.txt`. It is a second opinion, not an antivirus.

Sizes are allocated disk space (blocks), hard links are counted once, symbolic links are not followed. For a whole-disk scan, give Spatium Full Disk Access in System Settings › Privacy & Security, or protected folders show as "could not be read".

```sh
./bundle.sh          # builds Spatium.app
cargo test --release # scan, trash and agent-output tests (one small test file goes to the Trash)
```

Check hooks: `SPATIUM_SCAN=<dir>` scans at start, `SPATIUM_SHOT=<file.png>` saves the window and quits, `SPATIUM_TAB=1` opens Largest files, `SPATIUM_THEME=light`.
