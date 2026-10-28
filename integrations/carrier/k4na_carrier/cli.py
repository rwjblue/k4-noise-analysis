"""Foreground supervisor: read-only HVAC observations plus an unchanged RF CLI."""

import argparse
import asyncio
import logging
import math
import os
import re
import signal
import sys
import time
import tomllib
from contextlib import suppress
from pathlib import Path

from .api import CarrierClient
from .display import Console
from .recorder import Recorder
from .store import Journal, JournalError, StorageFull, child_env, iso


class Refused(Exception):
    pass


class Stopped(Exception):
    pass


def duration_seconds(text: str) -> float:
    parts = re.findall(r"(\d+(?:\.\d+)?)\s*(ms|s|m|h|d)", text)
    remainder = re.sub(r"\d+(?:\.\d+)?\s*(?:ms|s|m|h|d)", "", text).strip()
    if not parts or remainder:
        raise argparse.ArgumentTypeError("use a duration such as 5m, 8h or 1h30m")
    factors = {"ms": 0.001, "s": 1, "m": 60, "h": 3600, "d": 86400}
    seconds = sum(float(n) * factors[unit] for n, unit in parts)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("duration must be positive and finite")
    return seconds


def positive(text: str) -> float:
    value = float(text)
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError("must be positive and finite")
    return value


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Read-only Carrier telemetry and K4 recording")
    commands = p.add_subparsers(dest="command", required=True)
    for name in ("record", "log"):
        sub = commands.add_parser(name)
        sub.add_argument("--duration", default="8h", help="finite recording duration (default: 8h)")
        sub.add_argument("--k4na", default="./target/debug/k4na", help="RF/import CLI executable")
        sub.add_argument("--telemetry-dir", type=Path, default=Path("output/carrier"))
        sub.add_argument("--poll-seconds", type=positive, default=30.0)
        sub.add_argument("--timeout-seconds", type=positive, default=20.0)
        sub.add_argument("--max-age-seconds", type=positive, default=600.0)
        sub.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
        sub.add_argument("--shard-bytes", type=int, default=4 * 1024 * 1024)
        sub.add_argument("--min-free-bytes", type=int, default=None)
        if name == "record":
            sub.add_argument("--config", type=Path, default=Path("config/local.toml"))
            sub.add_argument("--mode", choices=("capture", "survey"), default="capture")
            sub.add_argument("--profile", type=Path)
            sub.add_argument("--allow-retune", action="store_true")
            sub.add_argument("--without-hvac", action="store_true")
            sub.add_argument("--raw-full-rate")
            sub.add_argument("--raw-full-rate-max-bytes")
        else:
            sub.add_argument("--data", type=Path, default=Path("data"))
    sub = commands.add_parser("import", help="retry completed marks offline; no cloud or radio")
    sub.add_argument("--data", type=Path, default=Path("data"))
    sub.add_argument("--k4na", default="./target/debug/k4na")
    sub.add_argument("--telemetry-dir", type=Path, default=Path("output/carrier"))
    sub.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
    sub.add_argument("--min-free-bytes", type=int, default=1024 * 1024 * 1024)
    return p


def rf_command(args) -> list[str]:
    if args.mode == "survey" and not args.allow_retune:
        raise Refused("survey requires explicit --allow-retune; preview with k4na survey --dry-run")
    if args.mode == "capture" and args.allow_retune:
        raise Refused("--allow-retune is only valid with --mode survey")
    command = [args.k4na, args.mode, "--config", str(args.config), "--duration", args.duration]
    if args.profile:
        command.extend(["--profile", str(args.profile)])
    if args.allow_retune:
        command.append("--allow-retune")
    if args.raw_full_rate:
        command.extend(["--raw-full-rate", args.raw_full_rate])
    if args.raw_full_rate_max_bytes:
        if not args.raw_full_rate:
            raise Refused("--raw-full-rate-max-bytes requires --raw-full-rate")
        command.extend(["--raw-full-rate-max-bytes", args.raw_full_rate_max_bytes])
    return command


async def until_stopped(awaitable, stop: asyncio.Event):
    operation = asyncio.create_task(awaitable)
    stopped = asyncio.create_task(stop.wait())
    try:
        done, _ = await asyncio.wait((operation, stopped), return_when=asyncio.FIRST_COMPLETED)
        if stopped in done:
            raise Stopped
        return await operation
    finally:
        for task in (operation, stopped):
            if not task.done():
                task.cancel()
        await asyncio.gather(operation, stopped, return_exceptions=True)


async def read_once(client, timeout_seconds: float):
    async with asyncio.timeout(timeout_seconds):
        return await client.read(time.time_ns())


async def stop_radio(process):
    if process is None or process.returncode is not None:
        return
    with suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    try:
        async with asyncio.timeout(30):
            await asyncio.shield(process.wait())
    except TimeoutError:
        print(
            "RF shutdown still pending; waiting for safe restoration (not sending SIGKILL).",
            file=sys.stderr,
        )
        await process.wait()


async def supervise(
    args, stop: asyncio.Event, client_factory=CarrierClient, *, minimum_poll_seconds=5
) -> int:
    seconds = duration_seconds(args.duration)
    if args.poll_seconds < minimum_poll_seconds:
        raise Refused("--poll-seconds must be at least 5")
    if args.max_age_seconds < args.poll_seconds:
        raise Refused("--max-age-seconds must be at least --poll-seconds")
    if args.min_free_bytes is not None and args.min_free_bytes < 0:
        raise Refused("--min-free-bytes cannot be negative")
    radio_argv = None
    if args.command == "record":
        radio_argv = rf_command(args)
        try:
            with args.config.open("rb") as f:
                config = tomllib.load(f)
            data = Path(config["storage"]["root"])
            min_free = config["storage"]["min_free_bytes"]
        except OSError, ValueError, KeyError, TypeError:
            raise Refused("cannot read station storage settings; check --config") from None
        # Existing strict Rust validation remains authoritative. No radio IO.
        check = [args.k4na, "check", "--config", str(args.config)]
        if args.profile:
            check.extend(["--profile", str(args.profile)])
        process = await asyncio.create_subprocess_exec(
            *check,
            env=child_env(),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            async with asyncio.timeout(30):
                status = await until_stopped(process.wait(), stop)
        finally:
            await stop_radio(process)
        if status:
            raise Refused("station/profile validation failed; run k4na check for details")
    else:
        data = args.data
        min_free = 1024 * 1024 * 1024
    if args.min_free_bytes is not None:
        min_free = args.min_free_bytes
    disabled = getattr(args, "without_hvac", False)
    username = os.environ.get("CARRIER_USERNAME", "")
    password = os.environ.get("CARRIER_PASSWORD", "")
    if not disabled and (not username or not password):
        raise Refused("set CARRIER_USERNAME and CARRIER_PASSWORD in .env (loaded by mise)")

    radio = None
    radio_watcher = None
    client = None
    recorder = None
    origin = time.monotonic_ns()
    started_rf = False
    telemetry_failed = False
    with Journal(args.telemetry_dir, args.max_bytes, args.shard_bytes, min_free, data) as journal:
        recorder = Recorder(journal, max(args.poll_seconds * 3, args.timeout_seconds * 2))
        console = Console()
        console.line(f"HVAC source log: {journal.path}; poll interval={args.poll_seconds:g}s")

        async def import_marks():
            try:
                count = await journal.import_marks(args.k4na, data)
                if count:
                    console.line(f"HVAC marks: imported {count} completed interval(s).")
                return count
            except Exception as error:
                # Exception messages from subprocesses/libraries are never emitted.
                print(
                    f"HVAC marks pending import ({type(error).__name__}); raw data retained.",
                    file=sys.stderr,
                )
                return 0

        try:
            if disabled:
                now = time.time_ns()
                console.intervals(recorder.unavailable("disabled by --without-hvac", now, 0))
                console.unavailable("disabled by --without-hvac", now)
                print(
                    "HVAC: explicitly disabled; coverage will be marked unknown.", file=sys.stderr
                )
            else:
                client = client_factory(username, password, args.max_age_seconds)
                try:
                    readings = await until_stopped(read_once(client, args.timeout_seconds), stop)
                except Stopped:
                    raise
                except Exception:
                    raise Refused(
                        "Carrier startup failed; RF not started. Check credentials/connectivity, "
                        "or explicitly use --without-hvac."
                    ) from None
                if not readings or any(r.quality != "observed" for r in readings):
                    raise Refused(
                        "Carrier startup has missing/stale/disconnected readings; RF not started"
                    )
                now = time.time_ns()
                console.intervals(recorder.tick(readings, now, time.monotonic_ns() - origin))
                console.observations(readings, now)
                console.line(f"HVAC ready: {len(readings)} unit(s), first observation {iso(now)}")
            await import_marks()
            if stop.is_set():
                raise Stopped
            if radio_argv is not None:
                radio = await asyncio.create_subprocess_exec(
                    *radio_argv,
                    env=child_env(),
                    stdin=asyncio.subprocess.DEVNULL,
                    start_new_session=True,
                )
                started_rf = True

                async def stop_when_rf_exits():
                    await radio.wait()
                    stop.set()

                # Wake pending network reads too, not just the idle poll wait.
                radio_watcher = asyncio.create_task(stop_when_rf_exits())
                print(
                    f"Recording {args.mode}; HVAC telemetry is "
                    f"{'disabled' if disabled else 'running'}.",
                    file=sys.stderr,
                )
            deadline = time.monotonic() + seconds
            while not stop.is_set() and (radio is None or radio.returncode is None):
                # The RF CLI owns its duration (including its own preflight).
                # Only standalone telemetry uses the helper's deadline.
                remaining = deadline - time.monotonic() if radio is None else args.poll_seconds
                if remaining <= 0:
                    break
                # Wait for RF completion as well as a stop; do not leave HVAC
                # running for an extra polling interval after RF exits.
                sleepers = [asyncio.create_task(stop.wait())]
                if radio is not None:
                    sleepers.append(asyncio.create_task(radio.wait()))
                try:
                    done, _ = await asyncio.wait(
                        sleepers,
                        timeout=min(args.poll_seconds, remaining),
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    if done:
                        break
                finally:
                    for task in sleepers:
                        task.cancel()
                    await asyncio.gather(*sleepers, return_exceptions=True)
                if telemetry_failed:
                    print(
                        "HVAC: LOGGING STOPPED; RF continues without HVAC coverage.",
                        file=sys.stderr,
                    )
                    continue
                try:
                    if disabled:
                        now = time.time_ns()
                        console.intervals(
                            recorder.unavailable(
                                "disabled by --without-hvac",
                                now,
                                time.monotonic_ns() - origin,
                            )
                        )
                        console.unavailable("disabled by --without-hvac", now)
                    else:
                        try:
                            readings = await until_stopped(
                                read_once(client, args.timeout_seconds), stop
                            )
                        except Stopped:
                            raise
                        except Exception:
                            # HTTP/transport errors are retryable. A failure to
                            # persist the resulting gap is handled separately.
                            now = time.time_ns()
                            console.intervals(
                                recorder.unavailable(
                                    "cloud request failed", now, time.monotonic_ns() - origin
                                )
                            )
                            console.unavailable("cloud request failed", now)
                            print(
                                "HVAC: cloud unavailable; RF continues, retrying next poll.",
                                file=sys.stderr,
                            )
                        else:
                            now = time.time_ns()
                            console.intervals(
                                recorder.tick(readings, now, time.monotonic_ns() - origin)
                            )
                            console.observations(readings, now)
                    await import_marks()
                except Stopped:
                    break
                except Exception as error:
                    telemetry_failed = True
                    print(
                        f"HVAC logging stopped ({type(error).__name__}); RF continues. "
                        "Check telemetry storage and its byte/free-space limits.",
                        file=sys.stderr,
                    )
            await stop_radio(radio)
            return radio.returncode if radio is not None else (1 if telemetry_failed else 0)
        finally:
            # Never strand an RF process on exceptions, cancellation or signals.
            await stop_radio(radio)
            if radio_watcher is not None:
                radio_watcher.cancel()
                await asyncio.gather(radio_watcher, return_exceptions=True)
            if client is not None:
                await client.close()
            if recorder.recovered:
                try:
                    console.intervals(recorder.finish(time.time_ns()))
                    await import_marks()
                except Exception as error:
                    print(
                        f"HVAC finalization incomplete ({type(error).__name__}); "
                        "source shards/checkpoint retained for recovery.",
                        file=sys.stderr,
                    )
            elif started_rf:
                print("HVAC coverage unavailable for this recording.", file=sys.stderr)
            console.summary()


async def run(args) -> int:
    if args.command == "import":
        if not (args.data / "K4NA-DATA-ROOT").is_file():
            raise Refused("--data is not an existing k4na measurement root")
        with Journal(
            args.telemetry_dir,
            max_bytes=args.max_bytes,
            min_free_bytes=args.min_free_bytes,
            data_root=args.data,
            start_shard=False,
        ) as journal:
            count = await journal.import_marks(args.k4na, args.data)
        print(f"Imported {count} queued HVAC interval(s); no cloud or radio connection.")
        return 0
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    try:
        return await supervise(args, stop)
    except Stopped:
        return 130
    finally:
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.remove_signal_handler(sig)


def main() -> int:
    # Some upstream HTTP/GraphQL debug logs include account data. The helper
    # emits safe allowlisted equipment summaries, never account payload dumps.
    logging.disable(logging.CRITICAL)
    args = parser().parse_args()
    try:
        return asyncio.run(run(args))
    except (Refused, StorageFull, JournalError) as error:
        print(f"refused: {error}", file=sys.stderr)
        return 2
    except argparse.ArgumentTypeError as error:
        print(f"refused: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(
            f"recording failed ({type(error).__name__}); no exception payload logged",
            file=sys.stderr,
        )
        return 1
