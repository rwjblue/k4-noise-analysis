# Survey recovery journal

Status: implemented in `crates/k4na-collector/src/recovery/` and
`k4na recovery`. Tested against the simulator, including a survey process
killed with SIGKILL. **Not yet exercised on a physical K4.**

A survey changes radio state: `BN`, `FA`, `#SPN`, `#FXT`, optionally `AR`,
and the receiver measurement settings of the profile
([measurement.md](measurement.md)). If the process dies (crash, SIGKILL,
power loss, host sleep), those changes stay in the radio and in its
per-band memories. The journal records enough to put them back later,
without trusting anything that changed since.

## What is journaled

One file per survey session: `<storage.recovery_dir>/<session_id>.jsonl`
(default `<storage.root>/recovery`). Each line is `<check> <json>`, where
`check` is 16 hex digits of SHA-256 over the JSON. Every append is
fsynced before the call returns. Entries, in the order a survey writes
them:

| Entry | Meaning |
| --- | --- |
| `begin` | Session, profile, program version, CAT meta-mode, and the full original readback. Captures the original band. |
| `capture` | First arrival in a scope: the found values of the fields the survey may change there. |
| `intent` | Changes about to be sent: scope, field, from, to, wire. Written **before** the setters. |
| `verified` | The full readback after the setters were confirmed. Clears in-flight intents. |
| `restored` | Fields put back to their captured original. |
| `note` | Informational. |
| `end` | The survey's own restoration outcome, message and manual steps. |
| `acknowledged` | The operator accepted the radio's state. |

Scopes follow the radio's memories, because their storage scope is not
documented ([measurement.md](measurement.md#storage-scope)):

- `session`: the operating band (`BN`).
- `band N`: that band's own `FA`, `#SPN`, `#FXT` and RX input `AR`.
- `band N, input AR`: receiver settings with that band and RX input.

A field the collector changed and has not put back keeps its first
captured original, even if the survey returns to that scope.

## Restoration rule

For each changed field, the current readback decides:

- equal to the captured original: already back;
- equal to a value the collector intended (the last verified one, or the
  one before it when a crash fell between the intent and the send):
  restore it;
- anything else: someone changed it. **Leave it** and report a conflict;
- unreadable, or an original the reviewed setter whitelist cannot send
  (30 m's `#SPN5000`, a setting basic CAT mode cannot express): report
  it with a manual instruction.

Nothing is sent if the radio's band is not one the collector left it on
(someone changed bands) or the CAT meta-mode differs from the journal's
(values cannot be compared). Otherwise each band with changes is settled
in turn, the current band first: the band is selected with `BN` alone and
verified, each RX-input context's receiver settings are put back
(selecting that input), then the band's own memory. Finally the original
band is selected and its memory, input context and the session band are
checked. Every restoration is journaled with intent and verification, so
an interrupted restore can be retried safely. The engine sends only the
typed, whitelisted setters; it never sends `AN`, `K4n`, transmit, mode
or any free-form CAT text.

The survey uses the same engine for clean ends (duration, signal,
storage stop, no usable spectrum). It never restores after transmit,
operator activity, an unconfirmed readback, lost CAT, or a journal write
failure: those leave the radio alone and the journal pending.

## Survey gate

A survey refuses to start while any journal in the recovery directory is
`pending` (changes may still be in the radio), `running` (another survey
holds its lock) or `corrupt`. The check runs before any network use and
again, atomically, when the new journal is created. `survey --dry-run`
reports the block.

A running survey holds an exclusive OS lock on its journal, so a recovery
cannot race it, and a crashed process never leaves a stale lock. If a
journal write fails, the survey makes no further radio changes.

## Commands

~~~text
k4na recovery list --config my.toml
k4na recovery show <session> --config my.toml
k4na recovery restore <session> --dry-run --config my.toml        # GET only
k4na recovery restore <session> --allow-retune --config my.toml
k4na recovery acknowledge <session> --note "fixed by hand" --config my.toml
~~~

`<session>` may be a unique prefix. `restore` needs the same consent as a
survey: `--allow-retune` and `collection.allow_retune = true`. Its exit
code is 0 when complete, 3 on a conflict and 1 when something else is
left (see the printed manual steps). `acknowledge` accepts the radio as it
is and unblocks surveys; for a corrupt journal it renames the file to
`*.acknowledged-corrupt` instead.

## Integrity

- A torn final line (crash mid-append) is dropped: the setter it
  announced was never sent. `resume` truncates it before appending.
- A bad line followed by good ones, or a sequence gap, is corruption.
  Automatic restore refuses; restore the radio by hand, then acknowledge.
- Journals are **not** measurement data. They are outside retention and
  the byte quota, and never pruned.

## Size bound

Journals sit outside the quotas, so they bound themselves. The ledger
keeps at most two intended values per field. When a journal passes
256 KiB it is rewritten as the minimal entry sequence that replays to the
same ledger. The rewrite is checked by replay, written to a locked side
file, fsynced and renamed over the journal, then the directory is
fsynced. A crash at any point leaves either complete journal. An
accelerated 4-hour simulated survey (3,892 visits) ended with a 126 KB
journal.

## Not yet verified

- Restoration against a physical K4: no measurement setter or recovery
  restore has been sent to the operator's K4D.
- Whether the K4 stores receiver settings globally, per band, or per band
  and input. The scope order above is correct for each case.
