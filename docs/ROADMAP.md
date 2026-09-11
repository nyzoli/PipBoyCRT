# Roadmap

PipBoyCRT is a Pip-Boy: things you glance at, not apps you live in. Every
module is one file plus one registry line (see `adding-a-module.md`), so the
list below is an invitation as much as a plan. Ideas that already exist as
stand-alone terminal programs (file managers, editors, full mail clients) are
deliberately out of scope — run those in the TERM tab instead.

## Candidates

- **SERVICES** — HTTP health checks for your own servers: status, response-time
  sparkline, TLS certificate expiry, an alert in the header when something is down.
- **CALENDAR** — an ICS feed or a local `.ics`: today's and tomorrow's events,
  the next one wired to the quest timer.
- **LOG** — a Pip-Boy data log: every module's events on one timeline (mail
  arrived, track saved, speed test, timer, syslog error) so you can see what
  happened while you were away.
- **CAPS** — a few stock / currency / crypto quotes with sparklines, no API key.
- **INVENTORY** — disk usage per top-level folder, the biggest files, scanned on demand.
- **WORKSHOP** — pending Windows / winget updates, last reboot, SMART status.
- **GITHUB** — notifications, review requests and CI status through the `gh`
  CLI (thin, like MAIL through Himalaya).
- **QUESTS** — `- [ ]` items from NOTES as a quest log with XP, feeding S.P.E.C.I.A.L.
- **HOLOTAPE** — voice memos and podcast episodes with their transcripts.
- **RADIATION** — a Geiger-counter style security glance: open ports, open Wi-Fi,
  Defender status, syslog errors.
- **BATTERY** — charge/discharge history, estimated time left, cycles and health.
- **MAP** — OpenStreetMap tiles rendered in braille around your location (the
  riskiest one; last).

## Decided against

- A file manager module: Far Manager (or any other) runs fine in its own
  cool-retro-term window; the Pip-Boy has nothing to add.
- Composing, deleting or sending mail from the MAIL tab: it stays read-only on
  purpose — use the Himalaya CLI or your mail client for that.
- An ASSISTANT chat module on the Messages API: the TERM tab running the
  `claude` CLI covers it without any API key in the config.
