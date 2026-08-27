# Development Log

Running log of what changed each work session, for quickly catching up
without re-reading the whole diff history. Newest entry on top.

Also see `README.md` for current features/config, and the project memory
(`squawkline` project notes) for deeper architecture/decision context that
doesn't belong in this log.

---

## 2026-08-28 — WHOX, ISUPPORT, standard-replies, and a WeeChat-accuracy pass

**Added**
- ISUPPORT (005) parsing: `CHANTYPES`/`PREFIX`/`WHOX` now drive
  channel-detection and NAMES rank-marker stripping instead of hardcoded
  guesses.
- `standard-replies` (FAIL/WARN/NOTE) displayed legibly — previously
  invisible, since the `irc` crate has no dedicated variant for them.
- `message-tags` capability requested explicitly.
- WHOX: nicklist dims away users. Gated on `away-notify` being granted too
  (matches WeeChat's `irc_channel_check_whox` — no point WHOing if we can't
  keep the result fresh).
- JOIN line reshaped to match WeeChat's actual format: `nick [account]
  (realname) (user@host) has joined #channel`.
- Word-wrap in the chat view.

**Fixed (real bugs, not just gaps)**
- `Command::CAP`'s capability list lands in the 3rd tuple field for the
  common wire shape, not the 4th — reading only the 4th meant
  `granted_caps` was silently always empty, defeating echo-message dedup
  (visible as duplicated own messages on Libera, not OFTC).
- Sending a WHOX field-selector (`WHO #chan %nf`) as a *string* round-trips
  through the crate's lossy structured `Command::WHO` type and gets
  silently dropped — fixed by constructing `Command::Raw` directly.
- `userhost-in-names` makes NAMES entries `nick!user@host`, not bare
  nicks — we only stripped the leading rank-marker, never the trailing
  part, so the nicklist was quietly showing garbled hostmasks.

**Corrected against WeeChat source (not just assumed)**
- Nicklist does *not* show account name — checked `irc-nick.c`, WeeChat
  tracks it per-nick but never renders it in the nicklist. Removed the
  "(account)" nicklist decoration; account still surfaces via JOIN and
  `account-notify` text, which was already correct.

**Verified live** against Libera, OFTC, and the user's testnet
(`testnet.ptirc.org`, has `+H` chathistory + a real account to test
against) — chathistory replay, WHOX away-status, and the CAP-negotiation
fix all confirmed working end-to-end, not just compiling.

**Next up (Tier 2, evidence-ranked by real ircd deployment — see project
memory for the full InspIRCd/UnrealIRCd/Solanum/Ergo comparison):**
`setname`, `account-tag`, netsplit-batch special-case grouping (the batch
machinery already exists, just needs a case like `draft/multiline` got).
`monitor`/`labeled-response` are universally deployed but need more UI
work (a watch-list; request/response correlation) — bigger lifts, not
started.

---

## Earlier work (pre-log)

Summarized rather than itemized, since this predates the log:
renamed from the original `hexchat-rs` scaffold to `squawkline`; built out
multi-server support (one thread per server), the sidebar server/channel
tree, nicklist, CTCP auto-replies, and the initial IRCv3 CAP/SASL
negotiation layer. Full history is in `git log`.
