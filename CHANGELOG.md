# Changelog

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
