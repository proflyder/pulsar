#!/usr/bin/env python3
"""Install a published pulsar release on a station through Home Assistant.

The station's updater is driven entirely by HA: `homeassistant.update_entity`
makes the "Firmware" entity re-read `releases/latest`, `update.install` makes
the daemon download, verify, swap and restart. This tool performs those two
steps from CI and then watches the entity until the new version is stable, so
an approved workflow run fails on its own when the install or the restart does
not take (the supervisor keeps the old binary as `.prev`).

Standard library only: the workflow runner needs no extra packages.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import quote

HTTP_TIMEOUT = 15.0


class HaError(Exception):
    """A Home Assistant request failed or the entity did not reach the goal."""


def log(message: str) -> None:
    print(message, flush=True)


class HaClient:
    def __init__(self, url: str, token: str, http_timeout: float = HTTP_TIMEOUT):
        self.base = url.rstrip("/")
        self._token = token
        self.http_timeout = http_timeout

    def _request(self, method: str, path: str, payload=None, timeout=None):
        data = None
        if payload is not None:
            data = json.dumps(payload).encode("utf-8")
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Authorization", "Bearer " + self._token)
        request.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(request, timeout=timeout or self.http_timeout) as response:
            body = response.read()
        if not body:
            return None
        return json.loads(body)

    def state(self, entity: str) -> dict:
        return self._request("GET", "/api/states/" + quote(entity, safe=""))

    def call_service(self, domain: str, service: str, payload: dict):
        return self._request("POST", f"/api/services/{domain}/{service}", payload)


def _attributes(state: dict) -> dict:
    attributes = state.get("attributes")
    return attributes if isinstance(attributes, dict) else {}


def installed_version(state: dict) -> str | None:
    value = _attributes(state).get("installed_version")
    return value if isinstance(value, str) else None


def latest_version(state: dict) -> str | None:
    value = _attributes(state).get("latest_version")
    return value if isinstance(value, str) else None


def entity_state(state: dict) -> str:
    value = state.get("state")
    return value if isinstance(value, str) else "unknown"


def _fetch_state(client: HaClient, entity: str) -> dict:
    try:
        return client.state(entity)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            raise HaError(f"entity {entity!r} does not exist in Home Assistant") from None
        raise HaError(f"GET /api/states/{entity} failed with HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise HaError(f"GET /api/states/{entity} failed: {error}") from None
    except json.JSONDecodeError:
        raise HaError(f"GET /api/states/{entity} returned invalid JSON") from None


def wait_for_latest(
    client: HaClient,
    entity: str,
    tag: str,
    *,
    poll_interval: float,
    retrigger_interval: float,
    timeout: float,
) -> None:
    """Poll until HA advertises `tag` as the latest release.

    The daemon finds `releases/latest` through GitHub's redirect, which can
    trail the creation of the release by a little. `update_entity` is repeated
    so a stale entity re-reads it instead of waiting for the 6 h timer.
    """
    deadline = time.monotonic() + timeout
    next_trigger = 0.0
    last_seen = None
    while True:
        now = time.monotonic()
        if now >= next_trigger:
            try:
                client.call_service("homeassistant", "update_entity", {"entity_id": entity})
            except (urllib.error.URLError, TimeoutError, OSError) as error:
                log(f"update_entity failed ({error}); retrying")
            except json.JSONDecodeError:
                log("update_entity returned invalid JSON; retrying")
            next_trigger = now + retrigger_interval
        try:
            state = _fetch_state(client, entity)
        except HaError as error:
            log(f"waiting for latest: {error}")
        else:
            last_seen = latest_version(state)
            if last_seen == tag:
                log(f"Home Assistant offers {tag}")
                return
        if time.monotonic() >= deadline:
            raise HaError(
                f"Home Assistant did not offer {tag} within {timeout:g} s"
                f" (latest_version={last_seen!r})"
            )
        time.sleep(min(poll_interval, max(0.0, deadline - time.monotonic())))


def _install_tolerated(client: HaClient, entity: str) -> None:
    """Call update.install; the station restarts, so a failed response is fine."""
    try:
        client.call_service("update", "install", {"entity_id": entity})
    except urllib.error.HTTPError as error:
        if error.code >= 500:
            log(f"install call returned HTTP {error.code}; the station may be restarting")
            return
        raise HaError(f"update.install failed with HTTP {error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        log(f"install call did not return ({error}); the station is probably restarting")
        return
    except json.JSONDecodeError:
        log("install call returned invalid JSON; continuing to poll the entity")
        return
    log("install call accepted")


def wait_for_install(
    client: HaClient,
    entity: str,
    tag: str,
    *,
    poll_interval: float,
    timeout: float,
) -> None:
    """Poll until the entity reports `tag` installed and is back online."""
    deadline = time.monotonic() + timeout
    last_seen = None
    while True:
        try:
            state = _fetch_state(client, entity)
        except HaError as error:
            log(f"waiting for install: {error}")
        else:
            last_seen = (entity_state(state), installed_version(state))
            if entity_state(state) != "unavailable" and installed_version(state) == tag:
                log(f"station reports {tag} installed")
                return
        if time.monotonic() >= deadline:
            raise HaError(
                f"station did not report {tag} installed within {timeout:g} s"
                f" (state={last_seen!r})"
            )
        time.sleep(min(poll_interval, max(0.0, deadline - time.monotonic())))


def watch_stability(
    client: HaClient,
    entity: str,
    tag: str,
    *,
    poll_interval: float,
    duration: float,
    unavailable_tolerance: float,
) -> None:
    """Keep watching for a crash-loop rollback by `supervise.sh`.

    A binary that starts and then exits is swapped back to `.prev` after a few
    short crashes; the entity then reports the old version again or stays
    unavailable.
    """
    deadline = time.monotonic() + duration
    unavailable_since = None
    while True:
        now = time.monotonic()
        try:
            state = _fetch_state(client, entity)
        except HaError as error:
            log(f"stability check: {error}")
            if unavailable_since is None:
                unavailable_since = now
        else:
            if entity_state(state) == "unavailable":
                if unavailable_since is None:
                    unavailable_since = now
            else:
                unavailable_since = None
                current = installed_version(state)
                if current is not None and current != tag:
                    raise HaError(
                        f"station rolled back after install: installed_version={current!r}"
                        f" instead of {tag!r} (supervise.sh restored .prev)"
                    )
        if (
            unavailable_since is not None
            and now - unavailable_since > unavailable_tolerance
        ):
            raise HaError(
                f"entity stayed unavailable for more than {unavailable_tolerance:g} s"
                " after the install"
            )
        if now >= deadline:
            log(f"{tag} stayed installed for {duration:g} s")
            return
        time.sleep(min(poll_interval, max(0.0, deadline - time.monotonic())))


def deploy(args: argparse.Namespace, token: str) -> int:
    client = HaClient(args.ha_url, token, args.http_timeout)
    entity = args.entity_id

    before = _fetch_state(client, entity)
    before_version = installed_version(before)
    log(f"{entity}: installed_version={before_version!r} state={entity_state(before)!r}")
    if before_version == args.tag:
        log(f"{args.tag} is already installed")
        return 0

    wait_for_latest(
        client,
        entity,
        args.tag,
        poll_interval=args.poll_interval,
        retrigger_interval=args.latest_retrigger,
        timeout=args.latest_timeout,
    )

    _install_tolerated(client, entity)
    wait_for_install(
        client,
        entity,
        args.tag,
        poll_interval=args.poll_interval,
        timeout=args.install_timeout,
    )
    watch_stability(
        client,
        entity,
        args.tag,
        poll_interval=args.poll_interval,
        duration=args.stability,
        unavailable_tolerance=args.unavailable_tolerance,
    )

    try:
        after = _fetch_state(client, entity)
        after_version = installed_version(after)
    except HaError:
        after_version = None
    log(f"done: before={before_version!r} after={after_version!r}")
    return 0


def parse_args(argv=None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Install a published pulsar release through Home Assistant.",
    )
    parser.add_argument("--tag", required=True, help="release tag, for example v0.5.1001")
    parser.add_argument(
        "--entity-id",
        default=None,
        help="HA update entity (default: $ENTITY_ID)",
    )
    parser.add_argument("--ha-url", default=None, help="HA base URL (default: $HA_URL)")
    parser.add_argument(
        "--token-file",
        default=None,
        help="file with the HA long-lived token (default: $HA_TOKEN)",
    )
    parser.add_argument(
        "--http-timeout",
        type=float,
        default=HTTP_TIMEOUT,
        help="per-request timeout in seconds (default: %(default)s)",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=5.0,
        help="state poll interval in seconds (default: %(default)s)",
    )
    parser.add_argument(
        "--latest-timeout",
        type=float,
        default=300.0,
        help="how long to wait for HA to offer the tag (default: %(default)s)",
    )
    parser.add_argument(
        "--latest-retrigger",
        type=float,
        default=30.0,
        help="repeat homeassistant.update_entity after this many seconds"
        " (default: %(default)s)",
    )
    parser.add_argument(
        "--install-timeout",
        type=float,
        default=600.0,
        help="how long to wait for the install to take (default: %(default)s)",
    )
    parser.add_argument(
        "--stability",
        type=float,
        default=90.0,
        help="how long the new version must stay installed (default: %(default)s)",
    )
    parser.add_argument(
        "--unavailable-tolerance",
        type=float,
        default=30.0,
        help="fail after the entity has been unavailable this long (default: %(default)s)",
    )
    return parser.parse_args(argv)


def _resolve(args: argparse.Namespace) -> tuple[str, str, str]:
    import os

    ha_url = args.ha_url or os.environ.get("HA_URL")
    entity_id = args.entity_id or os.environ.get("ENTITY_ID")
    if not ha_url:
        raise HaError("HA_URL or --ha-url is required")
    if not entity_id:
        raise HaError("ENTITY_ID or --entity-id is required")
    if args.token_file:
        try:
            with open(args.token_file, encoding="utf-8") as handle:
                token = handle.read().strip()
        except OSError as error:
            raise HaError(f"cannot read token file: {error}") from None
    else:
        token = os.environ.get("HA_TOKEN") or ""
    if not token:
        raise HaError("HA_TOKEN or --token-file is required")
    return ha_url, entity_id, token


def main(argv=None) -> int:
    args = parse_args(argv)
    try:
        ha_url, entity_id, token = _resolve(args)
        args.ha_url = ha_url
        args.entity_id = entity_id
        return deploy(args, token)
    except HaError as error:
        print(f"error: {error}", file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    sys.exit(main())
