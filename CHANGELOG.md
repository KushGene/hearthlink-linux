# Changelog

## Unreleased

### Tray

- Each service has its own submenu (status, its action, "Show in Hearthlink") with a status dot
  on its icon, instead of two lines per service in the main menu.
- New: **Upload everything now**, **Notifications** (Off / Important only / All) and
  **Start with my desktop** right in the tray menu.
- The tray icon gets an amber or red dot when a service needs attention or has an error, and
  shows when a Hearthlink update is available.
- Without a tray host, closing the window minimizes it instead of hiding it for good.

### Notifications

- Three levels instead of on/off. *Important only* (default) leaves out finished uploads and
  syncs and new versions of the Windows apps. Turned-off notifications stay off.

### Look and texts

- ForeverWowCraft: the routes show one realm per line and can be read in full.
- `hearthlink --page <service|overview|settings>` opens that page, also in a running Hearthlink.
- "1 day ago" instead of "1 days ago".
- New README with current screenshots.

- Calmer pages: shorter texts on every service page; versions, paths, logs and links moved
  into a closed **Details** card; long explanations are tooltips now.
- ForeverDB: the status says "Last upload 4 minutes ago" without the long list of counts
  (hover the row on its page for the full line). The ForeverDB_Helper row only shows while
  that old addon is still installed.
- Overview: compact service cards with status dot, a summary line ("5 of 6 services on · all
  good"), a **Turn on** button on turned-off services, and the update card only when an update
  is there.
- Settings: sections *Tray and startup*, *Notifications*, *Hearthlink updates* and *About*.
  "Start hidden in the tray" is gone: Hearthlink starts hidden from the autostart entry and
  shows its window when you open it yourself.
- No more "newer Windows app" notices for ForeverDB, ForeverWowCraft and TradeSkillMaster,
  which run the official apps and keep them current.

## 1.3.1 (2026-11-08)

### Hearthlink updates

- Add **Check for updates**, **Install update** and a restart action to Overview and Settings.
  Background checks notify only; installation always requires a click.
- Select the matching x86_64 AppImage, tar.gz, DEB, RPM or Flatpak automatically. Verify size
  and SHA-256 before installation. Local executables and AppImages are replaced atomically
  with a backup; packages use the system installer and Flatpak retains its installation scope.
- Release CI publishes an HTTPS checksum catalog alongside all five packages, allowing
  update checks without the GitHub API. The catalog is not independently signed.

### Fixes

- Enable the native clipboard backend and text selection in the Activity log.
- ForeverWowCraft: retry transient GitHub failures and fall back to the official release
  list when the latest-release endpoint times out. Downloads still require a matching SHA-256.
- "Install or update the addon" now checks for a newer helper first and also works before
  the helper is installed. Report installation failures instead of treating them as success.

## 1.3.0 (2026-11-07)

### New: TradeSkillMaster

- Hearthlink installs, updates and starts the official TradeSkillMaster app for Linux: it
  downloads the AppImage from TSM's update server (checked against the SHA-512 listed there),
  or takes a matching one from Downloads / `~/Applications`, and unpacks it, so it runs
  without FUSE/libfuse2 and from the Flatpak.
- Adds TradeSkillMaster with its icons to the app menu (`TradeSkillMaster.desktop`, the app's
  own desktop name, so its window gets the icon too). A TSM menu entry from another install is
  left alone.
- The app is tied to Hearthlink: it starts with Hearthlink and closes with it (also when
  Hearthlink is killed), and closes when the service is turned off. The app's own "Run at
  startup" entry is removed; the menu entry runs `hearthlink --open-tsm`, which opens it
  through the running Hearthlink (or starts Hearthlink).
- New versions are checked every 6 hours; the app is restarted to put one in place.
- "Open the app" on the overview and in the tray.
- Apps Hearthlink starts get the session's `WAYLAND_DISPLAY` back (Hearthlink drops it for
  itself to run through XWayland); without it Electron gave up on Wayland sessions.
- Off until turned on (the app is about 150 MB).

## 1.2.0 (2026-11-05)

### Wowhead: follows Wowhead Client 2.0.9801.37696

- In-game sync for WoW Forever: the Looter's new Sync button sends your profile and
  character data to Wowhead while you play, without closing the game. Hearthlink sees the
  saved request and sends what it asks for to wowhead.com, like the Windows client. It only
  runs while that game runs (Hearthlink now tells which WoW is running, so a running retail
  client doesn't count), needs you to be logged in, and is on by default (Options:
  "In-game sync for WoW Forever").
- With character data off (or for an excluded account), uploads now also leave out
  `wlProfileData` and `wlCharacterData` (the Looter's new Character Export part), not only
  `wlExportData`. Before, these two were sent anyway.

## 1.1.0 (2026-11-03)

### New: ForeverWowCraft (WoW Forever)

- Hearthlink installs, updates and runs the official
  [ForeverWowCraft Helper](https://github.com/ForeverWowCraft/ForeverWowCraft-Helper): it sends
  the auction prices the ForeverWowCraft Scanner addon saves, writes your professions' routes
  (`/fwc`) and the scan lists into the game, and keeps the Scanner and Plans addons installed.
- The helper is open source Python (MIT) and runs as it is: Hearthlink downloads the official
  release from GitHub, checks it against the SHA-256 GitHub lists, updates it when a new
  version is out (checked every 6 hours) and runs it with the system's Python 3.10 or newer.
  Hearthlink only does its Windows parts: tray icon, autostart and choosing the game folder.
- A Windows helper in a Wine/Proton prefix is taken over: its sender name, what it sent and
  its routes, so it carries on under the same name. The service is on by default only then.
- Its page shows the helper and addon versions, the sender name, the last prices sent and
  the routes, with "Send prices now", "Fetch routes now" and "Install or update the addon".

### Other changes

- The rpm and deb packages depend on `python3` (3.10 or newer). The Flatpak brings Python
  along.
- ForeverDB and ForeverWowCraft share how Hearthlink recognises WoW Forever game folders.

## 1.0.0 (2026-10-27)

First release: Wowhead, WowUtils (with heartbeat and RCLootCouncil loot history), wowaudit
and ForeverDB.
