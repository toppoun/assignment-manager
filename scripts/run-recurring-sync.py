#!/usr/bin/env python3
"""Run one approved Moodle sync and record only safe, fixed diagnostic fields."""

import argparse
from datetime import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


STATUS_CODES = {"success": 0, "failed": 1, "blocked": 2, "partial": 3, "busy": 4}
ERROR_GROUPS = {
    "network": {
        "network_failed", "notion_network_failed", "dns_lookup", "dns_timeout",
        "dns_policy", "tcp_connect", "tls_handshake", "tls_protocol",
        "tls_certificate_chain", "tls_hostname", "tls_certificate_time",
        "tls_certificate_other", "tls_certificate_revoked", "tls_revocation_unconfirmed",
        "http_exchange", "connection_stage_unknown",
        "request_stage_unknown", "request_timeout_stage_unknown", "body_timeout", "body_read",
    },
    "authentication": {
        "reauth_required", "http_authentication", "notion_authentication",
        "notion_access_denied", "writer_mismatch", "missing", "unavailable", "invalid",
    },
    "write_blocked": {
        "write_not_approved", "policy_changed", "change_limit", "target_mismatch",
        "state_missing_or_invalid", "state_error", "state_path_unavailable", "plan_changed",
        "reconciliation_required", "unattributed_pending_create", "protected_field_change",
        "course_rules_changed", "invalid_course_rules", "potential_duplicate",
        "deferred_items", "recurring_not_configured", "incomplete_snapshot", "notion_conflict",
        "configuration_invalid", "configuration_required", "moodle_not_configured",
        "moodle_policy_invalid", "invalid_feed", "non_moodle_configuration", "cli_digest_mismatch",
    },
    "service": {"notion_rate_limited", "notion_server_unavailable", "http_server"},
    "response": {
        "notion_invalid_response", "invalid_source_data", "http_unexpected_status",
        "http_unexpected_metadata", "body_size_limit", "body_utf8", "body_encoding",
        "ics_parse", "candidate_limit", "candidate_validation", "moodle_uid_duplicate",
        "course_processing", "course_processing_limit",
    },
    "runner": {"cli_unavailable", "cli_result_invalid", "moodle_client_unavailable"},
    "busy": {"target_busy"},
}
ERROR_CATEGORIES = {
    error: category for category, errors in ERROR_GROUPS.items() for error in errors
}
ALERT_AFTER = 3
LOG_MAX_BYTES = 256 * 1024
LOG_BACKUPS = 3


def timestamp():
    return datetime.now().astimezone().isoformat(timespec="seconds")


def result(status, error=None, candidates=None, created=None, updated=None, cli_exit_code=None):
    if error is not None and error not in ERROR_CATEGORIES:
        error = "unclassified"
    category = ERROR_CATEGORIES.get(error, "other")
    if status == "success":
        category = None
    elif status == "busy":
        category = "busy"
    elif status == "blocked" and category == "other":
        category = "write_blocked"
    return {
        "status": status,
        "error": error,
        "category": category,
        "moodle_candidates": candidates,
        "created": created,
        "updated": updated,
        "exit_code": STATUS_CODES[status],
        "cli_exit_code": cli_exit_code,
    }


def count(value):
    if type(value) is not int or value < 0:
        raise ValueError("invalid count")
    return value


def parse_report(raw, returncode):
    try:
        report = json.loads(raw)
        status = report["status"]
        moodle = report["moodle"]
        webclass = report["webclass"]
        if (type(report["schema_version"]) is not int or report["schema_version"] != 1
                or report["mode"] != "apply" or STATUS_CODES[status] != returncode
                or webclass["acquisition"] not in {"disabled", "not_attempted"}
                or webclass["application"] != "not_attempted"):
            raise ValueError("invalid report")
        candidates = moodle["candidates"]
        applied = moodle["applied"]
        if status == "success" and (moodle["acquisition"] != "success"
                                    or moodle["application"] != "applied"
                                    or candidates is None or applied is None):
            raise ValueError("invalid success report")
        error = report["error"] or moodle["error"]
        if error is not None and not isinstance(error, str):
            raise ValueError("invalid diagnostic")
        return result(
            status, error,
            None if candidates is None else count(candidates),
            None if applied is None else count(applied["create"]),
            None if applied is None else count(applied["update"]),
            returncode,
        )
    except (KeyError, TypeError, ValueError, RecursionError):
        return result("failed", "cli_result_invalid", cli_exit_code=returncode)


def run_sync(args):
    try:
        config_bytes = args.config.read_bytes()
        config = json.loads(config_bytes)
    except (OSError, ValueError, RecursionError):
        return result("blocked", "configuration_invalid")
    if not isinstance(config, dict):
        return result("blocked", "configuration_invalid")
    policy = config.get("recurring_sync")
    if policy is None:
        return result("blocked", "recurring_not_configured")
    if (config.get("webclass") is not False or not isinstance(policy, dict)
            or "webclass" in policy or not isinstance(policy.get("moodle"), dict)
            or not isinstance(config.get("moodle"), dict)):
        return result("blocked", "non_moodle_configuration")

    # The installer places the signed release under its content digest.
    if args.cli.name != "assignment-manager" or not re.fullmatch(r"[a-f0-9]{64}", args.cli.parent.name):
        return result("blocked", "cli_digest_mismatch")
    try:
        with args.cli.open("rb") as binary:
            digest = hashlib.sha256()
            for chunk in iter(lambda: binary.read(65536), b""):
                digest.update(chunk)
        if args.cli.is_symlink() or digest.hexdigest() != args.cli.parent.name:
            return result("blocked", "cli_digest_mismatch")
        # Pass the checked bytes to the CLI so an edit cannot enable HOPPII
        # between the source check and the CLI opening its config.
        with tempfile.TemporaryDirectory(dir=args.state_dir, prefix="config-") as directory:
            snapshot = Path(directory) / "config.json"
            snapshot.write_bytes(config_bytes)
            run = subprocess.run(
                [str(args.cli), "--config", str(snapshot), "sync", "recurring", "--apply"],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                check=False,
            )
    except OSError:
        return result("failed", "cli_unavailable")
    return parse_report(run.stdout, run.returncode)


def read_alert_state(path):
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
        failures = count(state["failures"])
        sent = state["attempted"]
        if type(sent) is not bool:
            raise ValueError("invalid alert state")
        return failures, sent
    except (FileNotFoundError, KeyError, TypeError, ValueError, RecursionError):
        return 0, False


def save_alert_state(path, failures, attempted):
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False,
    ) as temp:
        pending = Path(temp.name)
        try:
            temp.write(json.dumps({"failures": failures, "attempted": attempted}))
            temp.flush()
            os.fsync(temp.fileno())
            pending.replace(path)
        finally:
            pending.unlink(missing_ok=True)


def update_alert(path, record, notifier):
    failures, attempted = read_alert_state(path)
    notification = None
    if record["status"] == "busy":
        return failures, notification
    if record["status"] == "success":
        save_alert_state(path, 0, False)
        return 0, notification
    failures += 1
    notify = notifier is not None and failures >= ALERT_AFTER and not attempted
    save_alert_state(path, failures, attempted or notify)
    if notify:
        try:
            sent = subprocess.run(
                [str(notifier), record["category"], str(failures)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                timeout=10, check=False,
            )
            notification = "submitted" if sent.returncode == 0 else "failed"
        except (OSError, subprocess.TimeoutExpired):
            notification = "failed"
    return failures, notification


def append_log(directory, record):
    log = directory / "runs.jsonl"
    line = (json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n").encode("ascii")
    if log.exists() and log.stat().st_size + len(line) > LOG_MAX_BYTES:
        for index in range(LOG_BACKUPS, 0, -1):
            old = log if index == 1 else log.with_suffix(f".jsonl.{index - 1}")
            if old.exists():
                old.replace(log.with_suffix(f".jsonl.{index}"))
    with log.open("ab") as file:
        file.write(line)


def absolute(value):
    path = Path(value)
    if not path.is_absolute():
        raise argparse.ArgumentTypeError("absolute paths required")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True, type=absolute)
    parser.add_argument("--config", required=True, type=absolute)
    parser.add_argument("--state-dir", required=True, type=absolute)
    parser.add_argument("--notifier", type=absolute)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        args.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        info = args.state_dir.stat()
        if args.state_dir.is_symlink() or info.st_uid != os.getuid() or info.st_mode & 0o777 != 0o700:
            raise OSError("state directory must be private")
        with (args.state_dir / "runner.lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return 4
            started = timestamp()
            record = run_sync(args)
            finished = timestamp()
            failures, notification = update_alert(
                args.state_dir / "alerts.json", record, args.notifier,
            )
            append_log(args.state_dir, {
                "schema_version": 1, "started_at": started, "finished_at": finished,
                **record, "consecutive_failures": failures, "notification": notification,
            })
            return record["exit_code"]
    except OSError:
        print("recurring runner: local state or log unavailable", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
