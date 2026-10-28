# Paired main/sub receive

Paired mode measures two antennas over the same seconds by putting the
K4D's main receiver (pan A) and sub receiver (pan B) on the same tile with
different RX inputs. It exists because sequential antenna comparisons are
minutes apart: fading, propagation and cycling local noise change between
them, so only same-instant comparisons of signal over background are
trustworthy.

This page records what was checked on hardware, the design (being
implemented; not yet in a release), and what is still unverified. See [collection.md](collection.md) for the shared survey
contract and [analysis.md](analysis.md) for `k4na analyze pairs`.

## Requirements

- A K4D or K4HD with the KRX4 sub receiver: `OM` position 4 reads `S`
  ("SUB RX (KRX4 + 2nd KDDC4)"). Without it, sub RX is single-antenna dual
  watch and cannot use a different antenna.
- The station's `allow_rx_antenna_switch = true` and `allow_sub_receiver =
  true`, the profile's `paired = true` with exactly two antennas, and the
  CLI `--allow-retune`.
- Split off (`FT0`): in split, VFO B is the transmit frequency.

## Observed on a K4D (R39, 2026-10-28)

An attended check on 20 m changed only `AR` and `AR$` (VFO A/B, span,
diversity and receiver settings untouched), recorded 40 s of pans A and B
per configuration and restored the found state exactly (readbacks
matched; `OV00` throughout). The radio was found in diversity mode with
the sub on, `FA` 14.063 MHz, `FB` 14.045 MHz, main `PA2`, sub `PA0` and
`RA$03`.

- Pan B carried the sub receiver: its floor followed `AR$`, independent of
  pan A. Both pans arrived at the same rate (596 frames each per 40 s) with
  a 96 kHz packet rate and about 90 kHz of RF overlap, so frames pair by
  arrival within one frame time.
- With both receivers on the DXC, their signal-over-background on the same
  13 carriers agreed within 0.3 dB (median).
- With both on the receive antenna, the sub (preamp off, attenuator on)
  read the same carriers 9 dB worse than the main (preamp 2): it was
  limited by its own noise, not the antenna. Receiver settings must match,
  and a large main/sub disagreement means the comparison is measuring the
  receiver.
- Main on the receive antenna and sub on the DXC: the receive antenna gave
  4-7 dB more signal over background on the same carriers at the same
  instant (one short daytime sample; not a ranking).
- Sharing an input: the main floor on RX ANT IN1 did not change (<0.1 dB)
  when the sub joined it; on ATU ANT2 it moved 0.8 dB. The sub's floor on
  RX ANT IN1 read 2.8 dB higher when the main was also on it. So each
  receiver's level can depend on the other receiver's input, and both
  inputs are part of a pair's identity.
- Floors differed by about 22 dB between antennas (gain), which says
  nothing about which antenna hears better.

## Design

**Visits.** Each paired visit tunes both receivers to one tile (`FA` and
`FB`, and `BN$` when band independence (`BI1`) lets the sub sit on another
band; `#SPN$` matching `#SPN`), sets
`SB1` if needed, routes `AR` and `AR$`, applies the profile's receiver
settings to both receivers (`$` forms for the sub; `#AVG` is shared), then
dwells while recording both pans. Each pan becomes its own visit with its
own frame-header coverage check, linked by a shared pair id and carrying
the other receiver's antenna and input. A pan whose header does not cover
the tile is recorded as coverage-unverified, as in a single-receiver visit.

**Swap.** With `swap = true` (the default), alternate cycles exchange the
inputs (main = first antenna and sub = second, then the reverse). If `d`
is the sub-minus-main difference in signal over background, the two
orientations give `d0 = antenna + receiver` and `d1 = -antenna +
receiver`, so the antenna difference is `(d0 - d1) / 2` and the receiver
difference is `(d0 + d1) / 2`. A receiver difference beyond a few dB means
the receivers are not comparable (unequal settings or one is limited by
its own noise), and the antenna estimate is flagged.

**Safety.** The sub-receiver setters are typed and range-checked like the
main ones (`SB`, `FB`, `BN$`, `#SPN$`, `AR$`, `PA$`, `RA$`, `RG$`, `GT$`,
`NB$`, `NR$`, `#NB$`); `DV`, `BI`, `AN` and `FT` are read but never set.
Their found values are journaled before the first change and put back the
same way as the main receiver's, on band change and at the end, only when
they still hold a value the collector set. The interlock watches VFO B and
the sub receiver too; a change the collector did not make stops the
survey.

**Diversity.** The survey leaves `DV` as found. In diversity mode both
receivers are controlled by VFO A ("RIT applied to VFO A affects both
receivers"), so `FB` may not move the sub; frame headers decide coverage
either way.

## Not planned yet

Using both receivers on one antenna and two bands to halve pass time is
possible on paper, but a shared input moved levels by up to 2.8 dB, so
that data would not be comparable with single-receiver history, and
whether the sub then uses its own band-pass filter is unverified. `k4na
watch` already gives about 1 s timing on one band, which serves cycling
sources better than a faster pass.

## Unverified on hardware

- Whether `FB` moves pan B when diversity is on, and pan B's center offset
  from `FB` in track mode.
- Whether `BN$` recalls per-band sub settings or inputs like `BN` does for
  the main receiver.
- Settle time after an `AR$` change (the check used 3 s).
- Main/sub agreement with matched settings on the receive antenna.
