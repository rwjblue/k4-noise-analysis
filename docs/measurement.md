# Receiver measurement settings

Status: implemented for surveys and tested against the simulator. **No
measurement setter has been sent to a physical K4 yet.** Sources: the K4 Programmer's Reference rev. D4 (public HTML from
ftp.elecraft.com), GET-only `k4na probe` readbacks from a K4D on
firmware R39, and the per-visit state recorded by earlier live runs. Any
row marked *unverified* is an assumption the collector must check by
readback, never trust.

## CAT meta-mode

`K4;` reads the protocol meta-mode: `K40` (basic) or `K41` (advanced).
In basic mode the documented K4 response formats of `GT`, `NB`, `PA` and
`RA` (among others) are replaced by K3-compatible ones. The operator's
K4D reads `K40`, and Ham2K HaLo sends `K40;K31;` when it connects.

The collector **never sends `K4n`**: the reference does not say whether
meta-mode is per connection, so changing it could break other clients'
parsing. Instead it reads the mode at preflight, uses a codec for that
mode, includes `K4` in the interlock (a change mid-survey is operator
activity), and refuses to change any setting the mode cannot express or
read back unambiguously.

## Settings that can affect stored spectra

The LAN spectrum is computed by the panadapter DDC from the main
receiver's ADC samples. Settings ahead of the ADC change every bin;
receiver DSP settings change audio, not the pan, unless the pan follows
them.

| Setting | CAT (advanced) | Basic readback (K4D R39) | Affects pan data | Controllable |
| --- | --- | --- | --- | --- |
| Preamp | `PAnm` n 0-3, m on | `PAn` (observed 1, 2) | yes, ahead of ADC | yes; basic sets `PAn` (*unverified*) |
| Attenuator | `RAnnm` 0-21 dB step 3 | `RA00` (K3 on/off form) | yes, ahead of ADC | advanced: any; basic: off only |
| RF gain | `RG-nn` 0-60 | `RG-00` | *unverified*; DSP scalar after the ADC | yes |
| AGC | `GTn` 0 off 1 slow 2 fast | `GT002` fast, `GT004` slow | no (audio) | advanced: any; basic: slow/fast |
| Noise blanker | `NBnnm` | `NBm` | only with pan NB on auto | on/off, level kept |
| Noise reduction | `NRnnm` | `NRnnm` (`010`) | no (audio) | on/off, level kept |
| Auto/manual notch | `NAn`, `NMnnnnm` | same | no (audio) | recorded only |
| Pan noise blanker | `#NBn` 0 off 1 on 2 auto | `#NB0` | yes (DDC) | yes |
| Pan NB level | `#NBLn` 0-14 | `#NBL8` | yes when pan NB is active | recorded only |
| Pan averaging | `#AVGnn` 1-20 | `#AVG2` | yes (smoothing) | yes |
| Pan span | `#SPNn` 6000-368000 | `#SPN40000` (30 m stored 5000) | packet rate | yes (survey) |
| Pan ref/scale/auto-ref | `#REF`, `#SCL`, `#AR` | `#REF-121`, `#SCL47`, `#AR1206-041` | *unverified*; display | recorded only |
| Peak mode, FPS | `#PKM`, `#FPS` | `#PKM0`, `#FPS15` | peak: *unverified*; FPS: frame count only | recorded only |

All controllable settings are receive-path settings. None selects a
transmit antenna, keys, tunes or changes transmit power.

## Storage scope

The K4 keeps per-band memories that `BN` recalls. Observed on the K4D:
`#SPN` and `FA` per band (80 m 92000, 40 m 22000, 30 m 5000 stored span),
the preamp differs per band (80/30 m `PA1`, 20 m `PA2`) and `#REF` moves
with auto-ref. Whether the preamp, attenuator, RF gain, AGC and noise
settings are stored per band, per band and RX input, or globally is not
documented and is **unverified**; `#FXT`, `#NB` and `#AVG` may be global.

The collector therefore assumes no scope. It records each band's
recalled state when it arrives, and each RX-input context's state when
that input is first selected. Before switching inputs it puts the current
context back; before leaving a band it puts the band back. Restoring in
that order is correct whether a setting is global, per band or per band
and input.

## Measurement profiles in a survey

A survey profile's `[measurement]` table chooses each setting as a value
or `"as-found"`. Built-in defaults put the front end and DSP that alter
the spectrum in a known state (attenuator off, RF gain 0, NB, NR and pan
NB off) and leave preamp, AGC and averaging as found. `[[measurement.override]]`
entries refine by `band`, `antenna`, or both; the most specific value
wins per field.

~~~toml
[measurement]
preamp = "pre1"
agc = "fast"

[[measurement.override]]
antenna = "dxc"
preamp = "off"
~~~

For each tile, after `BN`, `FA` and the antenna's `AR`, the survey:

1. resolves the settings for the band and antenna;
2. plans the setters from the current readback in the radio's CAT
   meta-mode. A context is measured only if every target is a
   whitelisted setter **and** every found value it would change can be
   sent back. Otherwise the tile is skipped with a `measurement_skipped`
   event (and counted in the report), and nothing is changed: for
   example, an attenuator level in basic mode;
3. journals the found values and the intent, sends the setters, and
   verifies the readback (a mismatch stops the survey);
4. records a `measurement_applied` event.

Before switching RX input the current input's settings are put back, and
before leaving a band its memory is put back
([recovery.md](recovery.md)). The interlock watches every measurement
key, so an operator change mid-survey stops it without further setters.
`survey --dry-run` prints what each antenna's settings would change on
the current band.

Each survey visit stores in `measurement_json` the resolved settings
(including `as-found` fields), the CAT meta-mode, and the effective
values verified by readback as `verified.<prefix>` (raw readback text in
that mode, for example `"verified.PA": "0"`). Its state readback
includes every measurement key, so the comparability segment id (a hash
of band, antenna, port, pan, rate, versions and the non-frequency state)
changes whenever the verified settings differ, and is the same across
cycles and sessions when they do not. `as-found` and an explicit setting
with the same readback share a segment. Earlier visits keep their
segment. Passive visits have no `measurement_json`.

The pan span follows the same rule: a band that recalls a span the
whitelist cannot send back (30 m's `#SPN5000`) is surveyed at that span,
with a `warning`, rather than changed and left changed.
