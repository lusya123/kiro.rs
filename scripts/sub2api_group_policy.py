#!/usr/bin/env python3
"""Produce an explicit, reversible Sub2API routing change from a live audit.

This tool does not call the admin API or change servers. The collector must
resolve actual base URLs against live cluster labels, not account names.
"""
import argparse
import collections
import hashlib
import json
from pathlib import Path


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def requests_for(rows, restore=False):
    by_state = collections.defaultdict(list)
    for row in rows:
        state = (row["status"], row["schedulable"]) if restore else ("inactive", False)
        by_state[state].append(row["id"])
    result = []
    for (status, schedulable), ids in sorted(by_state.items()):
        ids.sort()
        for start in range(0, len(ids), 50):
            result.append({
                "method": "POST", "path": "/api/v1/admin/accounts/bulk-update",
                "body": {"account_ids": ids[start:start + 50],
                         "status": status, "schedulable": schedulable},
            })
    return result


def make_plan(snapshot):
    standard = snapshot["standard_accounts_to_preserve_but_exclude"]
    external = snapshot["external_accounts"]
    ids = [r["id"] for r in standard + external]
    if len(ids) != len(set(ids)):
        raise ValueError("duplicate account IDs or overlapping cluster membership")
    if not standard or not external:
        raise ValueError("both cluster inventories are required")
    for row in standard + external:
        if (row["platform"] != "anthropic" or type(row["schedulable"]) is not bool
                or row["status"] not in {"active", "inactive", "error"}
                or type(row["id"]) is not int or row["id"] <= 0
                or type(row["port"]) is not int or not 0 < row["port"] < 65536
                or any(type(g) is not int or g <= 0 for g in row["group_ids"])):
            raise ValueError("invalid account audit row")
    standard_ports = {a["port"] for a in standard}
    if standard_ports & {a["port"] for a in external}:
        raise ValueError("standard/external destination overlap")
    active_external = [r for r in external if r["status"] == "active" and r["schedulable"]]
    coverage = collections.Counter(g for r in active_external for g in set(r["group_ids"]))
    affected_groups = {g for r in standard if r["status"] == "active" and r["schedulable"]
                       for g in r["group_ids"]}
    uncovered = affected_groups - set(coverage)
    if uncovered:
        raise ValueError(f"no eligible external account in groups {sorted(uncovered)}")
    changes = sorted((r for r in standard if r["status"] != "inactive" or r["schedulable"]),
                     key=lambda r: r["id"])
    before = [{k: r[k] for k in ("id", "name", "platform", "port", "status", "schedulable", "group_ids")}
              for r in changes]
    plan = {
        "schema": 1,
        "policy": "Sub2API customer AWS traffic uses external compatibility accounts only",
        "snapshot_sha256": hashlib.sha256(canonical(snapshot)).hexdigest(),
        "before": before,
        "external_eligible_by_affected_group": {str(g): coverage[g] for g in sorted(affected_groups)},
        "apply": requests_for(changes),
        "rollback": requests_for(changes, restore=True),
        "preconditions": [
            "User production approval; fresh live audit matching account identities, states and group sets",
            "Compatibility image canary passed real streaming/non-streaming inference",
            "External accounts have live healthy capacity for each affected group/model",
            "Protected application, standard fleet and both Redis fingerprints recorded",
        ],
        "postconditions": [
            "Check all per-account results, not only HTTP 200; stop on any failed_ids",
            "Every standard destination is inactive and unschedulable; external states unchanged",
            "Credentials, group bindings, unrelated accounts and protected fingerprints unchanged",
            "Gateway system-reminder requests pass; inspect new request/error records",
        ],
    }
    plan["plan_sha256"] = hashlib.sha256(canonical(plan)).hexdigest()
    return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    plan = make_plan(json.loads(args.snapshot.read_text()))
    args.output.write_text(json.dumps(plan, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"accounts": len(plan["before"]), "requests": len(plan["apply"]),
                      "plan_sha256": plan["plan_sha256"]}))


if __name__ == "__main__":
    main()
