import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("policy", Path(__file__).parents[1] / "sub2api_group_policy.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


def account(i, port, status="active", schedulable=True, groups=(1,)):
    return dict(id=i, port=port, name=f"account-{i}", platform="anthropic",
                status=status, schedulable=schedulable, group_ids=list(groups))


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = dict(standard_accounts_to_preserve_but_exclude=[account(1, 52000), account(2, 52001, schedulable=False)],
                             external_accounts=[account(3, 25000)])

    def test_excludes_unschedulable_active_accounts_without_altering_credentials_or_groups(self):
        original = copy.deepcopy(self.snapshot)
        plan = policy.make_plan(self.snapshot)
        self.assertEqual(plan["apply"][0]["body"], {"account_ids": [1, 2], "status": "inactive", "schedulable": False})
        self.assertEqual(self.snapshot, original)
        self.assertEqual(len(plan["rollback"]), 2)
        for req in plan["apply"] + plan["rollback"]:
            self.assertEqual(set(req["body"]), {"account_ids", "status", "schedulable"})

    def test_refuses_to_remove_last_capacity_from_a_group(self):
        self.snapshot["standard_accounts_to_preserve_but_exclude"][0]["group_ids"].append(2)
        with self.assertRaisesRegex(ValueError, "groups.*2"):
            policy.make_plan(self.snapshot)

    def test_disabled_external_does_not_count_as_capacity(self):
        self.snapshot["external_accounts"][0]["schedulable"] = False
        with self.assertRaisesRegex(ValueError, "eligible external"):
            policy.make_plan(self.snapshot)

    def test_overlapping_ids_or_ports_fail_closed(self):
        for field in ("id", "port"):
            with self.subTest(field=field):
                snap = copy.deepcopy(self.snapshot)
                snap["external_accounts"][0][field] = snap["standard_accounts_to_preserve_but_exclude"][0][field]
                with self.assertRaises(ValueError):
                    policy.make_plan(snap)

    def test_completed_policy_is_noop(self):
        for r in self.snapshot["standard_accounts_to_preserve_but_exclude"]:
            r.update(status="inactive", schedulable=False)
        self.assertEqual(policy.make_plan(self.snapshot)["apply"], [])

    def test_rollback_preserves_each_original_state(self):
        plan = policy.make_plan(self.snapshot)
        restored = {i: (r["body"]["status"], r["body"]["schedulable"])
                    for r in plan["rollback"] for i in r["body"]["account_ids"]}
        self.assertEqual(restored, {1: ("active", True), 2: ("active", False)})

    def test_batches_use_explicit_bounded_ids(self):
        self.snapshot["standard_accounts_to_preserve_but_exclude"] = [account(i, 50000 + i) for i in range(10, 111)]
        plan = policy.make_plan(self.snapshot)
        self.assertEqual([len(r["body"]["account_ids"]) for r in plan["apply"]], [50, 50, 1])
        self.assertTrue(all("filters" not in r["body"] for r in plan["apply"]))


if __name__ == "__main__":
    unittest.main()
